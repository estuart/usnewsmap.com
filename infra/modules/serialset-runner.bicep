// The on-prem Serial Set runner's access to the data lake (#229, ADR-0008
// amendment of October 2026): `usnm-ingest congress-serialset` run on an OCR
// node, as that node's Arc machine identity, in place of the Azure job.
//
// One grant: a custom role with blob read and write data actions and no
// control-plane actions (Storage Blob Data Contributor would also let it
// delete or reconfigure the container, which a condition can't restrict),
// on the curated container, with a condition that leaves it only what the
// fetch does there, all of it under congress/serialset/:
// - list, with a prefix under congress/serialset/ (the fetch lists only its
//   own series, congress/curated.rs finished_in and empties_in);
// - read, write and create blobs whose path is under congress/serialset/
//   (counts.json, the parquet parts and empty.json, all create-only);
// - no delete and no move, anywhere: the role has neither data action. (A
//   condition can't say so again: Azure refuses a condition that names an
//   action the role doesn't grant, InvalidRoleAssignmentCondition.)
// Nothing on the other containers, and nothing on Cosmos: the runner holds
// its lock in a local state file (docs/operations.md, "The Serial Set
// on-prem").

param storageName string
@description('Object id of the runner\'s identity (the node\'s Arc machine identity).')
param principalId string
@description('The writer role\'s name, before its scope (infra/naming.bicep).')
param roleLabel string
var blobs = 'Microsoft.Storage/storageAccounts/blobServices/containers/blobs'
var path = 'congress/serialset/'

resource writer 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-sset-writer')
  properties: {
    // Role names are unique per tenant: include the subscription and group.
    roleName: '${roleLabel} (${take(subscription().subscriptionId, 8)}/${resourceGroup().name})'
    description: 'The on-prem Serial Set runner: list, read and create blobs. No delete, no container actions.'
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

// ABAC (condition version 2.0). Each clause either doesn't apply to the
// request's action or holds for it; both must hold.
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
  }
}

resource grant 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: account::blobService::curated
  name: guid(account::blobService::curated.id, principalId, 'usnm-sset-writer')
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    // Custom roles are addressed at subscription level wherever they're defined.
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', writer.name)
    description: 'The on-prem Serial Set runner (#229): list, read and create blobs under congress/serialset/ only; no delete.'
    conditionVersion: '2.0'
    condition: condition
  }
}

output roleAssignmentId string = grant.id
