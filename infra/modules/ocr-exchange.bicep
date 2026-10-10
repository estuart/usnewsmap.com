// The on-prem OCR exchange account (#296, ADR-0015, 08 §8.3): work queues
// for the OCR nodes and the container they write their output to. The data
// lake allows private endpoints only, and the nodes are at home, so they get
// this small account instead and nothing on the lake.
//
// Public network access is on, but the firewall denies everything except
// the node IPs (the operator's home IP, from USNM_OCR_NODE_IPS, never in the
// repository) and the Container Apps subnet (through its Microsoft.Storage
// service endpoint, network.bicep). Entra only, like every other account.
// The public-network policy exempts it by resource id (main.bicep), so its
// assignment must be in place before this account is written.

param location string
param tags object
param name string
@description('Public IPv4 addresses or CIDR ranges of the OCR nodes (the operator\'s home IP). Storage rejects /31 and /32 ranges, so a /32 is written as the bare address.')
param nodeIps array = []
@description('The Container Apps subnet, allowed through its service endpoint.')
param subnetId string
@description('Object id of the Entra security group of the nodes\' Arc machine identities (usnm-ocr-nodes-<env>, scripts/ocr-node/group.sh). The group gets the node roles; empty grants none.')
param nodeGroupId string = ''
@description('The ingest identity: the cloud jobs that fill the queues and copy the output into curated.')
param ingestPrincipalId string

// Role definition ids, from
// https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/storage
// Get Messages and Delete Message only (process), and peek. Update Message,
// which renews a message's invisibility, needs messages/write, which this
// role doesn't have: the worker hides a message for a fixed time when it
// takes it, long enough for its slowest page (ADR-0015).
var queueMessageProcessor = '8a0f0c08-91a1-4084-bc3d-661d67233fed'
// Put Message only.
var queueMessageSender = 'c6a89b2d-59bc-44d0-9896-0f6e12d7b80a'
var queueDataContributor = '974c5e8b-45b9-4653-ba55-5f855dd0fb88'
var blobContributor = 'ba92f5b4-2d11-453d-a403-e96b0029c9fe'

// A worker takes from ocr-priority first, then ocr-bulk; a message seen
// five times goes to ocr-deadletter.
var workQueues = ['ocr-priority', 'ocr-bulk']
var deadLetterQueue = 'ocr-deadletter'
var outputContainer = 'ocr-out'

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: name
  location: location
  tags: tags
  kind: 'StorageV2'
  sku: { name: 'Standard_LRS' }
  properties: {
    accessTier: 'Hot'
    allowSharedKeyAccess: false
    defaultToOAuthAuthentication: true
    allowBlobPublicAccess: false
    allowCrossTenantReplication: false
    minimumTlsVersion: 'TLS1_2'
    supportsHttpsTrafficOnly: true
    publicNetworkAccess: 'Enabled'
    networkAcls: {
      defaultAction: 'Deny'
      // No trusted-service bypass: nothing but the nodes and the subnet.
      bypass: 'None'
      ipRules: [
        for ip in nodeIps: {
          value: endsWith(ip, '/32') ? replace(ip, '/32', '') : ip
          action: 'Allow'
        }
      ]
      virtualNetworkRules: [{ id: subnetId, action: 'Allow' }]
    }
  }
}

resource queueService 'Microsoft.Storage/storageAccounts/queueServices@2023-05-01' = {
  parent: account
  name: 'default'
}

resource queues 'Microsoft.Storage/storageAccounts/queueServices/queues@2023-05-01' = [
  for q in workQueues: {
    parent: queueService
    name: q
  }
]

resource deadLetter 'Microsoft.Storage/storageAccounts/queueServices/queues@2023-05-01' = {
  parent: queueService
  name: deadLetterQueue
}

resource blobService 'Microsoft.Storage/storageAccounts/blobServices@2023-05-01' = {
  parent: account
  name: 'default'
  properties: {
    // Staging data that a node can write again: a week of soft delete is
    // enough to recover from a bad copy-in, and no versioning.
    deleteRetentionPolicy: { enabled: true, days: 7 }
    containerDeleteRetentionPolicy: { enabled: true, days: 7 }
  }
}

resource output 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: blobService
  name: outputContainer
  properties: { publicAccess: 'None' }
}

// ocr-out is staging: a cloud job (ingest identity) checks each part and
// copies it into curated/ocr-local/ in the data lake, which keeps it with
// versions. The copy-in deletes what it has taken; this rule removes what's
// left after 30 days (parts it rejected, or that a stopped job never took),
// so the account never grows into a second copy of the lake.
resource lifecycle 'Microsoft.Storage/storageAccounts/managementPolicies@2023-05-01' = {
  parent: account
  name: 'default'
  properties: {
    policy: {
      rules: [
        {
          name: 'expire-ocr-out'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['${outputContainer}/'] }
            actions: {
              baseBlob: { delete: { daysAfterModificationGreaterThan: 30 } }
            }
          }
        }
      ]
    }
  }
}

// The nodes: take and delete messages on the two work queues, add to the
// dead-letter queue, and read and create blobs in ocr-out, without delete,
// so one node can't remove another's output. Azure RBAC can't make blobs
// write-once, so "write" still allows an overwrite; the worker writes
// create-only (If-None-Match: *), and the copy-in job checks every part.
// They get nothing else in this account or anywhere in the lake.
resource nodeWriter 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-ocr-node-writer')
  properties: {
    // Role names are unique per tenant: include the subscription and group.
    roleName: 'usnm OCR node writer (${take(subscription().subscriptionId, 8)}/${resourceGroup().name})'
    description: 'An on-prem OCR node: list, read and create blobs. No delete.'
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

// The nodes' grants go to one Entra security group, usnm-ocr-nodes-<env>
// (ADR-0015). scripts/ocr-node/group.sh creates it, since deployment stacks
// can't hold Microsoft Graph resources; onboard.sh adds each node's
// system-assigned identity to it and offboard.sh takes it out, so adding or
// removing a node needs no provision. Each assignment's name comes from its
// scope, the group and the role.
var nodeGrants = !empty(nodeGroupId)

resource nodeProcess 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for (q, i) in workQueues: if (nodeGrants) {
    scope: queues[i]
    name: guid(queues[i].id, nodeGroupId, queueMessageProcessor)
    properties: {
      principalId: nodeGroupId
      principalType: 'Group'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', queueMessageProcessor)
    }
  }
]

resource nodeDeadLetter 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (nodeGrants) {
  scope: deadLetter
  name: guid(deadLetter.id, nodeGroupId, queueMessageSender)
  properties: {
    principalId: nodeGroupId
    principalType: 'Group'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', queueMessageSender)
  }
}

resource nodeOutput 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (nodeGrants) {
  scope: output
  name: guid(output.id, nodeGroupId, 'usnm-ocr-node-writer')
  properties: {
    principalId: nodeGroupId
    principalType: 'Group'
    // Custom roles are addressed at subscription level wherever they're defined.
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', nodeWriter.name)
  }
}

// The ingest identity: the seeder adds pages to the queues, and someone
// inspecting or replaying dead letters needs to read and delete them, so
// Queue Data Contributor on the account. Blob Data Contributor on ocr-out,
// not Reader: the copy-in job deletes each part once it is in curated.
resource ingestQueues 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: account
  name: guid(account.id, ingestPrincipalId, queueDataContributor)
  properties: {
    principalId: ingestPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', queueDataContributor)
  }
}

resource ingestOutput 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: output
  name: guid(output.id, ingestPrincipalId, blobContributor)
  properties: {
    principalId: ingestPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobContributor)
  }
}

output id string = account.id
output name string = account.name
output queueEndpoint string = account.properties.primaryEndpoints.queue
output blobEndpoint string = account.properties.primaryEndpoints.blob
