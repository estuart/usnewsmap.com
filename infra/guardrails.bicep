// Guardrails (08 §8.5, ADR-0005, ADR-0008): policy definitions at
// subscription scope, assigned to each environment's resource groups by
// modules/policy-assignments.bicep. Every environment in a subscription uses
// the same definitions, so they're a stack of their own, usnm-guardrails,
// which scripts/lib/env.sh deploys before an environment's stack (ADR-0011).

targetScope = 'subscription'

@description('Recipients of the alert on dev\'s groups (#275), comma separated: scripts/lib/env.sh passes the provisioning environment\'s USNM_ALERT_EMAILS, or the ones the stack has. Empty: no budget.')
param devBudgetEmails string = ''

@description('The dev budget\'s start month, YYYY-MM-01. A budget\'s start date can\'t change; scripts/lib/env.sh keeps the stack\'s value once the budget exists.')
param devBudgetStart string = ''

@description('dev\'s region, which names the group its Container Apps environment creates: scripts/lib/env.sh passes dev\'s when dev provisions, and the stack\'s value otherwise.')
param devBudgetLocation string = 'eastus2'

// The definitions' names; main.bicep imports them to build the ids.
@export()
var guardrailPolicyNames = {
  noIaas: 'usnm-deny-iaas-compute'
  noStorageKeys: 'usnm-deny-storage-shared-key'
  noCosmosKeys: 'usnm-deny-cosmos-local-auth'
  auditPublicAccess: 'usnm-audit-data-public-network'
  auditDiagnostics: 'usnm-audit-diagnostic-settings'
  noMonitorKeys: 'usnm-deny-monitor-local-auth'
  noRegistryLocalAuth: 'usnm-deny-registry-local-auth'
}

resource noIaas 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.noIaas
  properties: {
    displayName: 'US News Map: no VMs, scale sets, AKS or Batch accounts'
    policyType: 'Custom'
    mode: 'All'
    policyRule: {
      if: {
        field: 'type'
        in: [
          'Microsoft.Compute/virtualMachines'
          'Microsoft.Compute/virtualMachineScaleSets'
          'Microsoft.ContainerService/managedClusters'
          'Microsoft.Batch/batchAccounts'
        ]
      }
      then: { effect: 'deny' }
    }
  }
}

resource noStorageKeys 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.noStorageKeys
  properties: {
    displayName: 'US News Map: storage accounts must disable shared key access'
    policyType: 'Custom'
    mode: 'Indexed'
    policyRule: {
      if: {
        allOf: [
          { field: 'type', equals: 'Microsoft.Storage/storageAccounts' }
          { field: 'Microsoft.Storage/storageAccounts/allowSharedKeyAccess', notEquals: false }
        ]
      }
      then: { effect: 'deny' }
    }
  }
}

resource noCosmosKeys 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.noCosmosKeys
  properties: {
    displayName: 'US News Map: Cosmos DB accounts must disable local (key) auth and key-based metadata writes'
    policyType: 'Custom'
    mode: 'Indexed'
    policyRule: {
      if: {
        allOf: [
          { field: 'type', equals: 'Microsoft.DocumentDB/databaseAccounts' }
          {
            anyOf: [
              { field: 'Microsoft.DocumentDB/databaseAccounts/disableLocalAuth', notEquals: true }
              // Alias from the built-in "key based metadata write access" policy.
              { field: 'Microsoft.DocumentDB/databaseAccounts/disableKeyBasedMetadataWriteAccess', notEquals: true }
            ]
          }
        ]
      }
      then: { effect: 'deny' }
    }
  }
}

resource auditPublicAccess 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.auditPublicAccess
  properties: {
    displayName: 'US News Map: data services should have public network access disabled'
    description: 'Public network access on storage and Cosmos accounts and Key Vaults, except the resources the assignment lists by resource id (the public tiles account, and the on-prem OCR exchange account and the OCR nodes\' Key Vault, which allow only their firewalls\' IPs and subnet). Audit by default; each environment assignment sets deny together with its own ids. A Spot backfill window would need a policy exemption.'
    policyType: 'Custom'
    mode: 'Indexed'
    parameters: {
      effect: {
        type: 'String'
        allowedValues: ['audit', 'deny', 'disabled']
        defaultValue: 'audit'
        metadata: {
          displayName: 'Effect'
          description: 'Audit by default, so updating this shared definition never denies an environment whose assignment has not yet listed its public accounts.'
        }
      }
      publicAccountIds: {
        type: 'Array'
        defaultValue: []
        metadata: {
          displayName: 'Public accounts'
          description: 'Resource ids of accounts and vaults that may allow public network access (behind their own firewalls, or serving public data).'
        }
      }
    }
    policyRule: {
      if: {
        allOf: [
          // Exempt by resource id, which a caller can't change, rather than by a tag they could add.
          { not: { field: 'id', in: '[parameters(\'publicAccountIds\')]' } }
          {
            anyOf: [
              {
                allOf: [
                  { field: 'type', equals: 'Microsoft.Storage/storageAccounts' }
                  { field: 'Microsoft.Storage/storageAccounts/publicNetworkAccess', notEquals: 'Disabled' }
                ]
              }
              {
                allOf: [
                  { field: 'type', equals: 'Microsoft.DocumentDB/databaseAccounts' }
                  { field: 'Microsoft.DocumentDB/databaseAccounts/publicNetworkAccess', notEquals: 'Disabled' }
                ]
              }
              {
                allOf: [
                  { field: 'type', equals: 'Microsoft.KeyVault/vaults' }
                  { field: 'Microsoft.KeyVault/vaults/publicNetworkAccess', notEquals: 'Disabled' }
                ]
              }
            ]
          }
        ]
      }
      // Each environment's assignment sets deny and its public account ids in
      // one update (main.bicep); the backfill runs inside the VNet and opens no
      // public-access window. The definition keeps its original name: renaming
      // it would delete a definition that is still assigned.
      then: { effect: '[parameters(\'effect\')]' }
    }
  }
}

// Every resource type in the project that has resource logs must send some
// to a workspace (the settings live in diagnostics.bicep). Audit, not
// deployIfNotExists: the template writes them, and chooses categories per
// resource to stay inside the workspace's daily cap (a remediation identity
// would need role-assignment rights and could only turn on whole category
// groups). Mode All, so storage services (child resources) are evaluated.
var loggedTypes = [
  'Microsoft.OperationalInsights/workspaces'
  'Microsoft.ContainerRegistry/registries'
  'Microsoft.Network/virtualNetworks'
  'Microsoft.App/managedEnvironments'
  'Microsoft.DocumentDB/databaseAccounts'
  'Microsoft.Insights/dataCollectionRules'
  'Microsoft.KeyVault/vaults'
  'Microsoft.Storage/storageAccounts/blobServices'
  'Microsoft.Storage/storageAccounts/queueServices'
  'Microsoft.Storage/storageAccounts/tableServices'
  'Microsoft.Storage/storageAccounts/fileServices'
]

resource auditDiagnostics 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.auditDiagnostics
  properties: {
    displayName: 'US News Map: resources with resource logs must send them to Log Analytics'
    description: 'Audit only: infra/modules/diagnostics.bicep writes the settings.'
    policyType: 'Custom'
    mode: 'All'
    policyRule: {
      if: { field: 'type', in: loggedTypes }
      then: {
        effect: 'auditIfNotExists'
        details: {
          type: 'Microsoft.Insights/diagnosticSettings'
          existenceCondition: {
            allOf: [
              { field: 'Microsoft.Insights/diagnosticSettings/workspaceId', exists: true }
              {
                count: {
                  field: 'Microsoft.Insights/diagnosticSettings/logs[*]'
                  where: { field: 'Microsoft.Insights/diagnosticSettings/logs[*].enabled', equals: 'true' }
                }
                greater: 0
              }
            ]
          }
        }
      }
    }
  }
}

// ADR-0009: nothing accepts a shared key. The aliases match the built-in
// "should block non-Azure Active Directory based ingestion" and "local admin
// account disabled" policies.
resource noMonitorKeys 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.noMonitorKeys
  properties: {
    displayName: 'US News Map: Log Analytics and Application Insights must disable local (key) auth'
    policyType: 'Custom'
    mode: 'Indexed'
    policyRule: {
      if: {
        anyOf: [
          {
            allOf: [
              { field: 'type', equals: 'Microsoft.OperationalInsights/workspaces' }
              { field: 'Microsoft.OperationalInsights/workspaces/features.disableLocalAuth', notEquals: 'true' }
            ]
          }
          {
            allOf: [
              { field: 'type', equals: 'Microsoft.Insights/components' }
              { field: 'Microsoft.Insights/components/DisableLocalAuth', notEquals: 'true' }
            ]
          }
        ]
      }
      then: { effect: 'deny' }
    }
  }
}

// Basic can't enable anonymous pull today; the deny still covers a later
// SKU change or drift.
resource noRegistryLocalAuth 'Microsoft.Authorization/policyDefinitions@2023-04-01' = {
  name: guardrailPolicyNames.noRegistryLocalAuth
  properties: {
    displayName: 'US News Map: container registries must disable the admin user and anonymous pull'
    policyType: 'Custom'
    mode: 'Indexed'
    policyRule: {
      if: {
        allOf: [
          { field: 'type', equals: 'Microsoft.ContainerRegistry/registries' }
          {
            anyOf: [
              { field: 'Microsoft.ContainerRegistry/registries/adminUserEnabled', equals: true }
              { field: 'Microsoft.ContainerRegistry/registries/anonymousPullEnabled', equals: true }
            ]
          }
        ]
      }
      then: { effect: 'deny' }
    }
  }
}

// dev holds nothing between experiments (#275; docs/operations.md, "Dev
// environment lifecycle"). An alert at $5 of actual spend in a month on its
// groups says it was left up. It lives here, not in dev's stack, so it
// exists while dev doesn't. A budget filters by exact group names: the
// project group, the Spot group, and the group the Container Apps
// environment creates for its load balancer (named by dev's template now;
// the legacy one, Azure's default, is named for dev's region). A distinct
// name from the budget dev's own stack can deploy (usnm-core-budget-dev).
var devEmails = filter(map(split(devBudgetEmails, ','), e => trim(e)), e => !empty(e))

resource devBudget 'Microsoft.Consumption/budgets@2023-11-01' = if (!empty(devEmails) && !empty(devBudgetStart)) {
  name: 'budget-usnm-dev-groups'
  properties: {
    category: 'Cost'
    amount: 5
    timeGrain: 'Monthly'
    timePeriod: { startDate: devBudgetStart }
    filter: {
      dimensions: {
        name: 'ResourceGroupName'
        operator: 'In'
        // dev's groups in ADR-0017's form (infra/naming.bicep: rg,
        // containerEnvInfraRg, spotRg), then the legacy ones, which an
        // older checkout's dev would still use.
        values: [
          'usnm-core-rg-dev'
          'usnm-core-infra-rg-dev'
          'rg-usnm-dev-spot'
          'rg-usnm-dev'
          'ME_cae-usnm-dev_rg-usnm-dev_${devBudgetLocation}'
        ]
      }
    }
    notifications: {
      actual5: {
        enabled: true
        operator: 'GreaterThanOrEqualTo'
        threshold: 100
        thresholdType: 'Actual'
        contactEmails: devEmails
      }
    }
  }
}
