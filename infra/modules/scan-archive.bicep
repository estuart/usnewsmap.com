// The scan archive (#442, ADR-0018): the keep-set JP2 scans on the Cold
// tier, an off-site copy and the source the BOINC clients read first.
//
// Same shape as the OCR exchange account: public network access on, a
// firewall that denies everything except the node IPs (USNM_OCR_NODE_IPS,
// never in the repository) and the Container Apps subnet (service endpoint),
// Entra only, no shared key, no SAS. The public-network policy exempts it by
// resource id (public-accounts.bicep), so that assignment goes first.
//
// Blobs are written once and not rewritten: no versioning, no blob soft
// delete (a scan can be fetched again, and kept copies would bill at Cold
// rates). The tier is Cold at the account level and on each upload; the
// lifecycle rule only moves a blob that arrived on a warmer tier.

param location string
param tags object
param name string
@description('Public IPv4 addresses or CIDR ranges of the OCR nodes. Storage rejects /31 and /32 ranges, so a /32 is written as the bare address.')
param nodeIps array = []
@description('The Container Apps subnet, allowed through its service endpoint.')
param subnetId string
@description('The BOINC stack\'s Container Apps subnet (usnm-boinc-vnet-<env>/aca), allowed through its Microsoft.Storage.Global endpoint so the bridge\'s staging steps can read the archive (#516). Empty: none.')
param boincSubnetId string = ''
@description('Principals given Storage Blob Data Reader on `scans`: the BOINC bridge\'s identity (usnm-boinc-id-<env>), which stages pages from the archive before going to LoC (ADR-0018, decision 7).')
param readerIds array = []
@description('Object id of the Entra security group of the nodes\' Arc machine identities (usnm-ocr-nodes-<env>). It gets the writer role; empty grants none.')
param nodeGroupId string = ''
@description('The scan writer role\'s name, before its scope (infra/naming.bicep).')
param writerRoleLabel string

var scansContainer = 'scans'

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: name
  location: location
  tags: tags
  kind: 'StorageV2'
  sku: { name: 'Standard_LRS' }
  properties: {
    accessTier: 'Cold'
    allowSharedKeyAccess: false
    defaultToOAuthAuthentication: true
    allowBlobPublicAccess: false
    allowCrossTenantReplication: false
    minimumTlsVersion: 'TLS1_2'
    supportsHttpsTrafficOnly: true
    publicNetworkAccess: 'Enabled'
    networkAcls: {
      defaultAction: 'Deny'
      bypass: 'None'
      ipRules: [
        for ip in nodeIps: {
          value: endsWith(ip, '/32') ? replace(ip, '/32', '') : ip
          action: 'Allow'
        }
      ]
      virtualNetworkRules: concat(
        [{ id: subnetId, action: 'Allow' }],
        empty(boincSubnetId) ? [] : [{ id: boincSubnetId, action: 'Allow' }]
      )
    }
  }
}

resource blobService 'Microsoft.Storage/storageAccounts/blobServices@2023-05-01' = {
  parent: account
  name: 'default'
  properties: {
    isVersioningEnabled: false
    deleteRetentionPolicy: { enabled: false }
    // A deleted container can be restored for a week.
    containerDeleteRetentionPolicy: { enabled: true, days: 7 }
  }
}

resource scans 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: blobService
  name: scansContainer
  properties: { publicAccess: 'None' }
}

// Fallback: an upload that arrived on Hot or Cool (a client that left out
// x-ms-access-tier) is moved to Cold the next day. It never deletes and
// never archives.
resource lifecycle 'Microsoft.Storage/storageAccounts/managementPolicies@2023-05-01' = {
  parent: account
  name: 'default'
  properties: {
    policy: {
      rules: [
        {
          name: 'scans-to-cold'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['${scansContainer}/'] }
            actions: {
              baseBlob: { tierToCold: { daysAfterModificationGreaterThan: 1 } }
            }
          }
        }
      ]
    }
  }
}

// The writers: list, read and create blobs in `scans`, no delete. Azure RBAC
// can't make blobs write-once, so the uploader writes create-only
// (If-None-Match: *) and the role has no delete.
resource writer 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-scans-writer')
  properties: {
    // Role names are unique per tenant: include the subscription and group.
    roleName: '${writerRoleLabel} (${take(subscription().subscriptionId, 8)}/${resourceGroup().name})'
    description: 'The scan archive uploader: list, read and create blobs. No delete.'
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

resource nodeWrite 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (!empty(nodeGroupId)) {
  scope: scans
  name: guid(scans.id, nodeGroupId, 'usnm-scans-writer')
  properties: {
    principalId: nodeGroupId
    principalType: 'Group'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', writer.name)
  }
}

// Storage Blob Data Reader, built in.
var blobReader = '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'

resource read 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for p in readerIds: {
    scope: scans
    name: guid(scans.id, p, 'usnm-scans-reader')
    properties: {
      principalId: p
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
    }
  }
]

output id string = account.id
output name string = account.name
output blobEndpoint string = account.properties.primaryEndpoints.blob
output container string = scans.name
