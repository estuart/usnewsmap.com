// Log Analytics with a daily cap (main.bicep sets it per environment) and
// workspace-based Application Insights (08 §8.1). Optional action group.

param location string
param tags object
param workspaceName string
param appInsightsName string
param actionGroupName string
@description('The action group\'s short name (at most 12 characters), shown in its emails and SMS.')
@maxLength(12)
param actionGroupShortName string
@description('The daily cap alert\'s name.')
param capAlertName string
@description('Alert recipients; no action group is created when empty.')
param alertEmails array = []
@description('Daily ingestion cap in GB (0.15 ≈ 4.5 GB/month, inside the free 5 GB; main.bicep raises it for prod).')
param dailyCapGb string = '0.15'

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: workspaceName
  location: location
  tags: tags
  properties: {
    sku: { name: 'PerGB2018' }
    retentionInDays: 30
    workspaceCapping: { dailyQuotaGb: json(dailyCapGb) }
    // Entra only (ADR-0009): no ingestion or queries with the workspace keys.
    features: { disableLocalAuth: true }
  }
}

// Blob writes and deletes (diagnostics.bicep) on the Auxiliary plan: the
// daily cap doesn't apply to Auxiliary tables, so a bulk job's writes can't
// stop the other logs. On 2026-10-07 the American Stories writer's blob
// writes filled the 0.15 GB cap and stopped all logging, alerts included.
// Nothing queries this table (no alert, workbook or saved query); it is the
// storage audit trail (08 §8.1.1), still queryable with full KQL, more
// slowly and at $0.005 per GB scanned. Auxiliary ingestion is $0.15/GB
// ($0.05 ingestion plus $0.10 processing) and outside the free 5 GB, which
// covers Analytics only: about $0.08 a month at the measured 18 MB a normal
// day. 30 days in all, as before (ADR-0012 cites it). A table's plan can
// change once a week.
resource blobLogsTable 'Microsoft.OperationalInsights/workspaces/tables@2025-07-01' = {
  parent: workspace
  name: 'StorageBlobLogs'
  properties: {
    plan: 'Auxiliary'
    totalRetentionInDays: 30
  }
}

// The control-plane audit trail kept a year (#231): Cosmos account and
// database changes, and the registry's image pushes, pulls and deletes.
// 30 days queryable as before, then long-term retention, read with a search
// job. About 5 MB a month between them, so a year held costs well under a
// cent a month ($0.02 per GB-month).
resource auditTables 'Microsoft.OperationalInsights/workspaces/tables@2025-07-01' = [
  for table in ['CDBControlPlaneRequests', 'ContainerRegistryRepositoryEvents']: {
    parent: workspace
    name: table
    properties: {
      plan: 'Analytics'
      retentionInDays: 30
      totalRetentionInDays: 365
    }
  }
]

resource appInsights 'Microsoft.Insights/components@2020-02-02' = {
  name: appInsightsName
  location: location
  tags: tags
  kind: 'web'
  properties: {
    Application_Type: 'web'
    WorkspaceResourceId: workspace.id
    DisableIpMasking: false
    // Entra only (ADR-0009): telemetry must be sent with a managed identity
    // (Monitoring Metrics Publisher), not the instrumentation key.
    DisableLocalAuth: true
  }
}

resource actionGroup 'Microsoft.Insights/actionGroups@2023-01-01' = if (!empty(alertEmails)) {
  name: actionGroupName
  location: 'global'
  tags: tags
  properties: {
    groupShortName: actionGroupShortName
    enabled: true
    emailReceivers: [
      for (email, i) in alertEmails: {
        name: 'email-${i}'
        emailAddress: email
        useCommonAlertSchema: true
      }
    ]
  }
}

// The daily cap reached (OverQuota): ingestion of billable data has stopped
// until the next reset, so every log-based alert is blind and a working job
// looks stalled. The workspace writes this event to its own Operation table
// (_LogOperation), which is free and not stopped by the cap: on 2026-10-07
// the event arrived at 21:46 UTC, the minute logging stopped. Checked every
// 15 minutes over the last 30, so a late row is still seen. Stateless, with
// actions muted for 30 minutes so one event sends one email: the event marks
// the moment the cap was reached, not the state, so a stateful rule would
// report "resolved" 30 minutes later while ingestion is still stopped.
// Severity 1: until the reset nothing else can alert. Next: Usage by DataType for what filled it, then raise the cap
// for the day (dailyCapGb) or stop the job.
var capReached = '''
_LogOperation
| where Category =~ "Ingestion"
| where Detail has "OverQuota"
| project TimeGenerated, Detail
'''

resource capAlert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = if (!empty(alertEmails)) {
  name: capAlertName
  location: location
  tags: tags
  kind: 'LogAlert'
  properties: {
    displayName: 'Log Analytics daily cap reached'
    description: 'The workspace ${workspaceName} reached its daily cap of ${dailyCapGb} GB: no logs, telemetry or log-based alerts until the cap resets (Auxiliary tables such as StorageBlobLogs excepted). Next: in the workspace, Usage | where IsBillable | summarize sum(Quantity) by DataType, bin(TimeGenerated, 1h) for what filled it; stop the job, or raise dailyCapGb in infra/main.bicep and provision (08 §8.1.1).'
    severity: 1
    enabled: true
    evaluationFrequency: 'PT15M'
    windowSize: 'PT30M'
    scopes: [workspace.id]
    autoMitigate: false
    muteActionsDuration: 'PT30M'
    criteria: {
      allOf: [
        {
          query: capReached
          timeAggregation: 'Count'
          operator: 'GreaterThan'
          threshold: 0
          failingPeriods: {
            numberOfEvaluationPeriods: 1
            minFailingPeriodsToAlert: 1
          }
        }
      ]
    }
    actions: { actionGroups: [actionGroup.id] }
  }
}

output workspaceId string = workspace.id
output workspaceName string = workspace.name
// The workspace's id in the Log Analytics query API (scripts/logs.sh).
output workspaceCustomerId string = workspace.properties.customerId
output appInsightsName string = appInsights.name
output appInsightsId string = appInsights.id
// The ingestion endpoint and instrumentation key, not a credential: with
// local auth disabled, ingestion needs an Entra token as well.
output appInsightsConnectionString string = appInsights.properties.ConnectionString
output actionGroupId string = empty(alertEmails) ? '' : actionGroup.id
