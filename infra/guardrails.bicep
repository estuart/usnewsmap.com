// Guardrails (08 §8.5, ADR-0005, ADR-0008): policy definitions at
// subscription scope, assigned to each environment's resource groups by
// modules/policy-assignments.bicep. Every environment in a subscription uses
// the same definitions, so they're a stack of their own, usnm-guardrails,
// which scripts/lib/env.sh deploys before an environment's stack (ADR-0011).

targetScope = 'subscription'

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
    description: 'Public network access on storage and Cosmos accounts, except the accounts the assignment lists by resource id (the public tiles account, and the on-prem OCR exchange account, which allows only its firewall\'s IPs and subnet). Audit by default; each environment assignment sets deny together with its own ids. A Spot backfill window would need a policy exemption.'
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
          description: 'Resource ids of accounts that serve public data and may allow public network access.'
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
