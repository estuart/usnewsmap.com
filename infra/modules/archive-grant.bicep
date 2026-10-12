// Deployed in the archival account's group by an environment's stack: the
// environment's roles on the account's containers, each scoped to one
// container. Curation (id-usnm-ingest) writes `raw`.
//
// No environment can delete what the account holds: the delete lock covers
// only the account itself, not its blobs, so writers get a custom role that
// lists, reads and creates blobs, without delete (as the search log's,
// rbac.bicep). Azure RBAC can't make blobs write-once, so "write" still
// allows an overwrite; every writer here writes create-only (If-None-Match:
// *), and versioning keeps an overwritten version for 30 days.

param accountName string
@description('The environment, in the writer role\'s frozen guid() seed.')
param env string
@description('The writer role\'s name, before its scope, and what follows the scope\'s group (infra/naming.bicep).')
param roleLabel string
param roleScopeEnv string = ''
@description('[{container, principalId, role}]: role is reader or writer.')
param grants array

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: accountName

  resource blobs 'blobServices' existing = {
    name: 'default'
  }
}

resource containers 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' existing = [
  for g in grants: {
    parent: account::blobs
    name: g.container
  }
]

resource writer 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-archive-writer', env)
  properties: {
    // Role names are unique per tenant: include the subscription, group and environment.
    roleName: '${roleLabel} (${take(subscription().subscriptionId, 8)}/${resourceGroup().name}${roleScopeEnv})'
    description: 'An environment on the archival account: list, read and create blobs. No delete.'
    type: 'CustomRole'
    permissions: [
      {
        actions: []
        dataActions: [
          'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'
          'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write'
        ]
      }
    ]
    assignableScopes: [resourceGroup().id]
  }
}

var roles = {
  // Storage Blob Data Reader.
  reader: '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'
  writer: writer.name
}

resource assignments 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for (g, i) in grants: {
    scope: containers[i]
    name: guid(containers[i].id, g.principalId, roles[g.role])
    properties: {
      principalId: g.principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', roles[g.role])
    }
  }
]
