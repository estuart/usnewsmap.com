// The BOINC hand-off's access (#469): `usnm-boinc export` and `usnm-boinc
// collect` run as the hand-off job's identity (boinc-handoff-job.bicep,
// docs/boinc.md#one-page-from-the-scan-cache). Only what those two commands
// touch:
// - Cosmos: Built-in Data Contributor on the `ops` container alone, for the
//   `ocr-sync` lease export takes and the published version it reads
//   (usnm-state's current_version and lock items). Nothing on titles,
//   batches, issues or index_runs.
// - curated: a custom role with blob read, write and create data actions and
//   no control-plane actions, with a condition that leaves it list, read and
//   create under ocr-local/ only (the native ledger, ocr_local::ROOT); no
//   delete or move anywhere (the role has neither data action).
// - reference: Storage Blob Data Reader, for {version}/batches.json and
//   titles.json, which export checks the cohort against.
// - ocr-out: the OCR nodes' writer role (list, read and create, no delete;
//   modules/ocr-exchange.bicep), where collect publishes ALTO and Parquet.
// The BOINC jobs' log workspace read is boinc-handoff-logs.bicep, in the
// BOINC group.

param storageName string
param exchangeName string
param cosmosName string
@description('Object ids given these grants: the hand-off job\'s identity.')
param principalIds array
@description('The ledger writer role\'s name, before its scope (infra/naming.bicep).')
param roleLabel string

var blobs = 'Microsoft.Storage/storageAccounts/blobServices/containers/blobs'
var path = 'ocr-local/'
var blobReader = '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'
var cosmosDataContributor = '00000000-0000-0000-0000-000000000002'

resource ledgerWriter 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-boinc-ledger-writer')
  properties: {
    // Role names are unique per tenant: include the subscription and group.
    roleName: '${roleLabel} (${take(subscription().subscriptionId, 8)}/${resourceGroup().name})'
    description: 'The BOINC hand-off: list, read and create blobs. No delete, no container actions.'
    type: 'CustomRole'
    permissions: [
      {
        actions: []
        dataActions: [
          '${blobs}/read'
          '${blobs}/write'
          '${blobs}/add/action'
        ]
      }
    ]
    assignableScopes: [resourceGroup().id]
  }
}

// ABAC (condition version 2.0), the same form as serialset-runner.bicep.
var condition = join(
  [
    '('
    ' ('
    '  !(ActionMatches{\'${blobs}/read\'} AND NOT SubOperationMatches{\'Blob.List\'})'
    '  AND !(ActionMatches{\'${blobs}/write\'})'
    '  AND !(ActionMatches{\'${blobs}/add/action\'})'
    ' )'
    ' OR'
    ' (@Resource[${blobs}:path] StringStartsWith \'${path}\')'
    ')'
    'AND'
    '('
    ' (!(ActionMatches{\'${blobs}/read\'} AND SubOperationMatches{\'Blob.List\'}))'
    ' OR'
    ' (@Request[${blobs}:prefix] StringStartsWith \'${path}\')'
    ')'
  ],
  '\n'
)

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: storageName

  resource blobService 'blobServices' existing = {
    name: 'default'

    resource curated 'containers' existing = {
      name: 'curated'
    }

    resource reference 'containers' existing = {
      name: 'reference'
    }
  }
}

resource exchange 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: exchangeName

  resource blobService 'blobServices' existing = {
    name: 'default'

    resource output 'containers' existing = {
      name: 'ocr-out'
    }
  }
}

resource cosmos 'Microsoft.DocumentDB/databaseAccounts@2024-11-15' existing = {
  name: cosmosName
}

resource ledger 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for principalId in principalIds: {
    scope: account::blobService::curated
    name: guid(account::blobService::curated.id, principalId, 'usnm-boinc-ledger-writer')
    properties: {
      principalId: principalId
      principalType: 'ServicePrincipal'
      // Custom roles are addressed at subscription level wherever they're defined.
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', ledgerWriter.name)
      description: 'The BOINC hand-off (#469): list, read and create blobs under ocr-local/ only; no delete.'
      conditionVersion: '2.0'
      condition: condition
    }
  }
]

resource reference 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for principalId in principalIds: {
    scope: account::blobService::reference
    name: guid(account::blobService::reference.id, principalId, blobReader)
    properties: {
      principalId: principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
    }
  }
]

resource output 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for principalId in principalIds: {
    scope: exchange::blobService::output
    name: guid(exchange::blobService::output.id, principalId, 'usnm-ocr-node-writer')
    properties: {
      principalId: principalId
      principalType: 'ServicePrincipal'
      // ocr-exchange.bicep's nodeWriter, in this group.
      roleDefinitionId: subscriptionResourceId(
        'Microsoft.Authorization/roleDefinitions',
        guid(resourceGroup().id, 'usnm-ocr-node-writer')
      )
    }
  }
]

resource lease 'Microsoft.DocumentDB/databaseAccounts/sqlRoleAssignments@2024-11-15' = [
  for principalId in principalIds: {
    parent: cosmos
    name: guid(cosmos.id, principalId, cosmosDataContributor, 'ops')
    properties: {
      principalId: principalId
      roleDefinitionId: '${cosmos.id}/sqlRoleDefinitions/${cosmosDataContributor}'
      scope: '${cosmos.id}/dbs/usnm/colls/ops'
    }
  }
]
