// Monitoring for the on-prem OCR nodes (#296, ADR-0015 decision 8), on
// resources of their own: a Log Analytics workspace with its own daily cap,
// a workspace-based Application Insights component on it for the worker's
// telemetry, and one data collection rule for every node's Azure Monitor
// Agent, into that workspace. diagnostics.bicep sends the exchange
// account's queue logs and the rule's errors there too.
//
// Not the environment's workspace and component: Monitoring Metrics
// Publisher grants Metrics/Write and Telemetry/Write on a whole component,
// and Application Insights ingestion has no IP firewall, so a node, or
// anyone holding a node's token, can write any telemetry under any cloud
// role from anywhere. On the shared component that could be fake usnm-api
// rows, which the API alerts read, or enough of anything to fill the
// environment's 1 GB daily cap and stop its logs, telemetry and log alerts
// until the reset, as a 0.15 GB cap did on 7 October 2026. Here the most a
// node can do is fill this workspace's cap, which stops the nodes'
// monitoring and nothing else; the cap alert below reports it.
//
// The template doesn't declare the Arc machines (the agent creates each
// one when it connects), so it can't attach the rule or install the agent
// either: scripts/ocr-node/onboard.sh installs the Azure Monitor Linux
// Agent extension on a node and associates the node with this rule, and
// offboard.sh removes both. The agent is the only extension a node's
// Connected Machine agent allows (node-setup.sh).
//
// Collected:
// - Syslog, every facility the system uses, at Warning and above: kernel
//   (OOM kills), systemd (a failed worker unit), sshd, chronyd, podman's
//   containers (their stderr arrives as user.err).
// - The worker, at Info and above: its unit logs to local0
//   (SyslogFacility=local0), which nothing else on the node uses. The
//   worker writes JSON lines to stdout, so journald records all of them,
//   warnings and errors too, at info; a Warning threshold would drop every
//   worker line. They also reach Application Insights as traces while
//   telemetry works; these copies cover a worker that fails before or
//   without it (bad flags, a crash, no Monitoring Metrics Publisher).
// - Performance counters every 60 s: CPU, memory, free disk space and disk
//   throughput, network bytes.
//
// Estimated volume per node (an estimate; measure it with
// ops/queries/ocr-node-health.kql, columns PerfMB, SyslogMB and WorkerMB):
// - Perf: on node 1 (4 vCPUs, 3 mounted file systems, eth0 and lo) the
//   counters below give about 20 to 23 rows a sample: 5 processor
//   instances (4 CPUs and the total), 2 memory, 3 disk counters x 3 or 4
//   mounts, 2 network counters x 2 interfaces. Once a minute that is about
//   29,000 to 33,000 rows a day; at roughly 150 to 300 billed bytes a row,
//   4 to 10 MB a day.
// - Syslog, system: node 1 logged 372 lines at Warning and above in the
//   2.6 hours after a reboot during the engine survey (boot messages and
//   the survey containers' stderr); a few hundred to a few thousand rows a
//   day at a few hundred bytes, under 2 MB a day.
// - Syslog, worker: nothing while the queues are empty (empty polls aren't
//   logged); about one line of about 1 KB per page while it works, so 2 to
//   5 MB a day at 2,000 to 5,000 pages.
// - Heartbeat: one row a minute, free.
// - The exchange account's queue logs (diagnostics.bicep): a row of about
//   520 billed bytes per request (measured on the exchange account in
//   October 2026), so about 1.5 MB a day for an idle node's 2,880 polls; a
//   working node adds a take and a delete per page, about 5 MB a day at
//   5,000 pages.
// - The worker's Application Insights telemetry: not estimated yet; the
//   worker hasn't run against Azure.
// So roughly 8 to 25 MB a day per node before the worker's telemetry, and
// with the rest of prod inside the free 5 GB a month. The cap starts at
// 0.1 GB (main.bicep); raise it once the worker's telemetry is measured.
//
// A node can still send far more: a worker in a crash loop, RUST_LOG=debug
// (every crate's debug lines, to Syslog and to Application Insights alike)
// or a compromised node. The volume alert below fires when one node sends
// more than a tenth of the daily cap in an hour, and the cap alert when the
// workspace reaches it.
//
// Table plans: Syslog and Perf stay on Analytics. Syslog can't use Basic
// (it supports Auxiliary) and Perf can't use Auxiliary (it supports Basic).
// Basic and Auxiliary ingestion are billed from the first byte, outside the
// free 5 GB, and queries on them are billed per GB scanned; at this volume
// Analytics costs nothing, keeps log alerts possible and answers queries
// at once. Revisit if the measured volume is far above the estimate.

param location string
param tags object
@description('Environment name, the suffix of the alert rules\' names.')
param nameSuffix string
@description('The nodes\' Log Analytics workspace, usnm-ocr-log-<env>.')
param workspaceName string
@description('The nodes\' Application Insights component, usnm-ocr-appi-<env>.')
param appInsightsName string
@description('The nodes\' data collection rule, usnm-ocr-dcr-<env>.')
param ruleName string
@description('The nodes\' workspace\'s daily ingestion cap in GB. ops/queries/ocr-node-health.kql has the same figure (cap_gb).')
param dailyCapGb string = '0.1'
@description('Object id of the nodes\' Entra group (usnm-ocr-nodes-<env>, ADR-0015); empty grants it nothing.')
param nodeGroupId string = ''
@description('The environment\'s action group; no alerts when empty.')
param actionGroupId string = ''

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: workspaceName
  location: location
  tags: tags
  properties: {
    sku: { name: 'PerGB2018' }
    retentionInDays: 30
    workspaceCapping: { dailyQuotaGb: json(dailyCapGb) }
    // Entra only (ADR-0009): the agent sends with the machine's identity,
    // and queries need an Entra token.
    features: { disableLocalAuth: true }
  }
}

resource appInsights 'Microsoft.Insights/components@2020-02-02' = {
  name: appInsightsName
  location: location
  tags: tags
  kind: 'web'
  properties: {
    Application_Type: 'web'
    WorkspaceResourceId: workspace.id
    DisableIpMasking: false
    // Entra only (ADR-0009): the worker sends with the node's Arc identity
    // (Monitoring Metrics Publisher, below), never the instrumentation key.
    DisableLocalAuth: true
  }
}

// Monitoring Metrics Publisher: sends any telemetry (traces too) to the
// component. The worker exports with each node's Arc identity, a member of
// the nodes' group. This component only: the group has nothing on the
// environment's (ADR-0015 decision 8).
var metricsPublisher = '3913510d-42f4-4e42-8a64-420c390055eb'

resource nodeTelemetry 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (!empty(nodeGroupId)) {
  scope: appInsights
  name: guid(appInsights.id, nodeGroupId, metricsPublisher)
  properties: {
    principalId: nodeGroupId
    principalType: 'Group'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', metricsPublisher)
  }
}

// Warning and above. Every facility the node uses except local0, which is
// the worker's (below). mail, news, uucp and lpr are unused here.
var systemFacilities = ['auth', 'authpriv', 'cron', 'daemon', 'kern', 'syslog', 'user']
var warningAndAbove = ['Warning', 'Error', 'Critical', 'Alert', 'Emergency']
var infoAndAbove = ['Info', 'Notice', 'Warning', 'Error', 'Critical', 'Alert', 'Emergency']

// Linux counter names, each instance (*), as in Microsoft's Linux examples.
// Network bytes are totals since boot; a query takes differences for a
// rate.
var counters = [
  '\\Processor(*)\\% Processor Time'
  '\\Memory\\% Used Memory'
  '\\Memory\\Used Memory MBytes'
  '\\Logical Disk(*)\\% Free Space'
  '\\Logical Disk(*)\\Disk Read Bytes/sec'
  '\\Logical Disk(*)\\Disk Write Bytes/sec'
  '\\Network(*)\\Total Bytes Received'
  '\\Network(*)\\Total Bytes Transmitted'
]

resource rule 'Microsoft.Insights/dataCollectionRules@2023-03-11' = {
  name: ruleName
  location: location
  tags: tags
  kind: 'Linux'
  properties: {
    description: 'On-prem OCR nodes (#296): Syslog at Warning and above, the worker (local0) at Info and above, and CPU, memory, disk and network counters every 60 s, into ${workspaceName}.'
    dataSources: {
      syslog: [
        {
          name: 'system'
          streams: ['Microsoft-Syslog']
          facilityNames: systemFacilities
          logLevels: warningAndAbove
        }
        {
          name: 'worker'
          streams: ['Microsoft-Syslog']
          facilityNames: ['local0']
          logLevels: infoAndAbove
        }
      ]
      performanceCounters: [
        {
          name: 'node'
          streams: ['Microsoft-Perf']
          samplingFrequencyInSeconds: 60
          counterSpecifiers: counters
        }
      ]
    }
    destinations: {
      logAnalytics: [
        {
          name: 'workspace'
          workspaceResourceId: workspace.id
        }
      ]
    }
    dataFlows: [
      {
        streams: ['Microsoft-Syslog']
        destinations: ['workspace']
      }
      {
        streams: ['Microsoft-Perf']
        destinations: ['workspace']
      }
    ]
  }
}

// Billed MB per node in the last hour, both paths added up: what the agent
// sent (Syslog and Perf; Heartbeat is free) and every Application Insights
// table of this component, by the role instance the sender set (the
// worker's is its node name). Worker lines go both ways, so either path
// alone could stay under the threshold while the node sends twice as much.
// Only this component's rows count (__COMPONENT_ID__), whatever role they
// name. Each source falls back to an empty table, since a table with no
// rows yet may not exist. The Application Insights tables are listed by
// name: log search alert rules don't support a wildcard union such as
// App* (Microsoft). Node names are lowercased on both sides, since Log
// Analytics lowercases _ResourceId and the role instance keeps the Arc
// name's case. The threshold is a tenth of the daily cap (__CAP_GB__
// GB), so one node that keeps it up fills the cap within 10 hours; the
// estimate is under 1 MB an hour.
var nodeVolume = '''
let cap_mb = todouble("__CAP_GB__") * 1000;
let component = "__COMPONENT_ID__";
let machines = "/providers/microsoft.hybridcompute/machines/";
let node_of = (id: string) { tolower(extract(@"(?i)/machines/([^/]+)$", 1, id)) };
let agent = union isfuzzy=true
    (Syslog | where _ResourceId has machines | project Node = node_of(_ResourceId), Size = _BilledSize),
    (Perf | where _ResourceId has machines | project Node = node_of(_ResourceId), Size = _BilledSize),
    (datatable(Node: string, Size: real)[])
    | summarize AgentMB = sum(Size) / 1e6 by Node
    | extend WorkerMB = 0.0;
let worker = union isfuzzy=true AppAvailabilityResults, AppBrowserTimings, AppDependencies, AppEvents,
        AppExceptions, AppMetrics, AppPageViews, AppPerformanceCounters, AppRequests, AppSystemEvents, AppTraces,
        (datatable(_ResourceId: string, AppRoleInstance: string, _BilledSize: real)[])
    | where _ResourceId =~ component
    | summarize WorkerMB = sum(_BilledSize) / 1e6 by Node = tolower(coalesce(AppRoleInstance, "(no instance)"))
    | extend AgentMB = 0.0;
union agent, worker
| summarize AgentMB = sum(AgentMB), WorkerMB = sum(WorkerMB) by Node
| extend MB = AgentMB + WorkerMB
| where MB > cap_mb / 10
| project Node, MB = round(MB, 1), AgentMB = round(AgentMB, 1), WorkerMB = round(WorkerMB, 1)
'''

// Stateful, like the other log alerts: one email when a node starts sending
// too much, resolved when it stops. Split by node, which stays the same
// while it lasts.
resource volumeAlert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = if (!empty(actionGroupId)) {
  name: 'alert-usnm-ocr-node-volume-${nameSuffix}'
  location: location
  tags: tags
  kind: 'LogAlert'
  properties: {
    displayName: 'OCR node log volume high'
    description: 'An on-prem OCR node sent more than a tenth of ${workspaceName}\'s daily cap of ${dailyCapGb} GB in the last hour (the estimate is under 1 MB an hour). Kept up, it fills the cap within 10 hours, and then the nodes have no monitoring until the reset (the environment\'s own workspace isn\'t affected). Node says which; the results split the total into AgentMB (Syslog and Perf from the Azure Monitor Agent) and WorkerMB (the nodes\' Application Insights telemetry, by the role instance the sender set). Next: scripts/logs.sh <env> ocr-node-health and ocr-node-errors; on the node, check RUST_LOG in /etc/usnm/ocr-worker.local.env and journalctl -u usnm-ocr-worker; stop the worker (systemctl stop usnm-ocr-worker) if it keeps on.'
    severity: 2
    enabled: true
    evaluationFrequency: 'PT15M'
    windowSize: 'PT1H'
    scopes: [workspace.id]
    // A new workspace has no Syslog, Perf or App tables until a node first
    // sends.
    skipQueryValidation: true
    autoMitigate: true
    criteria: {
      allOf: [
        {
          query: replace(replace(nodeVolume, '__CAP_GB__', dailyCapGb), '__COMPONENT_ID__', appInsights.id)
          timeAggregation: 'Count'
          operator: 'GreaterThan'
          threshold: 0
          dimensions: [{ name: 'Node', operator: 'Include', values: ['*'] }]
          failingPeriods: {
            numberOfEvaluationPeriods: 1
            minFailingPeriodsToAlert: 1
          }
        }
      ]
    }
    actions: { actionGroups: [actionGroupId] }
  }
}

// The cap reached (OverQuota), as for the environment's workspace (#279,
// monitoring.bicep): the workspace writes the event to its own Operation
// table (_LogOperation), which is free and keeps being written when the cap
// stops everything else. Ingestion here stops until the reset: no node
// heartbeats, counters or syslog, no worker telemetry, no queue logs, and
// the volume alert is blind. Stateless, with actions muted for 30 minutes
// so one event sends one email. Severity 2, not 1 like the environment's:
// the environment's own logs and alerts go on.
var capReached = '''
_LogOperation
| where Category =~ "Ingestion"
| where Detail has "OverQuota"
| project TimeGenerated, Detail
'''

resource capAlert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = if (!empty(actionGroupId)) {
  name: 'alert-usnm-ocr-log-cap-${nameSuffix}'
  location: location
  tags: tags
  kind: 'LogAlert'
  properties: {
    displayName: 'OCR nodes\' daily cap reached'
    description: 'The OCR nodes\' workspace ${workspaceName} reached its daily cap of ${dailyCapGb} GB: no node heartbeats, counters, syslog, worker telemetry or exchange-account queue logs, and no node log alerts, until the cap resets. The environment\'s own workspace isn\'t affected. A node in a crash loop, with debug logging or tampered with can fill it. Next: in ${workspaceName}, Usage | where IsBillable | summarize sum(Quantity) by DataType, bin(TimeGenerated, 1h) for what filled it, and scripts/logs.sh <env> ocr-node-health for which node; stop that node\'s worker (systemctl stop usnm-ocr-worker), or raise the cap (dailyCapGb of ocr-node-monitoring in infra/main.bicep, and cap_gb in ops/queries/ocr-node-health.kql) and provision.'
    severity: 2
    enabled: true
    evaluationFrequency: 'PT15M'
    windowSize: 'PT30M'
    scopes: [workspace.id]
    // A new workspace may not have the table behind _LogOperation yet.
    skipQueryValidation: true
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
    actions: { actionGroups: [actionGroupId] }
  }
}

output ruleId string = rule.id
output ruleName string = rule.name
output workspaceId string = workspace.id
output workspaceName string = workspace.name
// The workspace's id in the Log Analytics query API (scripts/logs.sh).
output workspaceCustomerId string = workspace.properties.customerId
output appInsightsId string = appInsights.id
// The ingestion endpoint and instrumentation key, not a credential: with
// local auth disabled, ingestion needs an Entra token as well.
output appInsightsConnectionString string = appInsights.properties.ConnectionString
