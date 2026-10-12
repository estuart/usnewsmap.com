// Private endpoints for Blob and Cosmos in snet-pe, registered in the linked
// private DNS zones (08 §8.3).

param location string
param tags object
param subnetId string
@description('The data lake\'s Blob endpoint and its NIC\'s name (create-only; empty leaves Azure\'s generated name).')
param blobEndpointName string
param blobEndpointNic string = ''
@description('Cosmos DB\'s endpoint and its NIC\'s name (create-only; empty leaves Azure\'s generated name).')
param cosmosEndpointName string
param cosmosEndpointNic string = ''
param storageId string
param cosmosId string
param blobDnsZoneId string
param cosmosDnsZoneId string

var endpoints = [
  { name: blobEndpointName, nic: blobEndpointNic, target: storageId, group: 'blob', zone: blobDnsZoneId }
  { name: cosmosEndpointName, nic: cosmosEndpointNic, target: cosmosId, group: 'Sql', zone: cosmosDnsZoneId }
]

resource pe 'Microsoft.Network/privateEndpoints@2024-05-01' = [
  for e in endpoints: {
    name: e.name
    location: location
    tags: tags
    // The NIC's name can only be set at creation: the existing endpoints
    // were created without one, so it is set only when not empty.
    properties: union(
      {
        subnet: { id: subnetId }
        privateLinkServiceConnections: [
          {
            name: e.name
            properties: {
              privateLinkServiceId: e.target
              groupIds: [e.group]
            }
          }
        ]
      },
      empty(e.nic) ? {} : { customNetworkInterfaceName: e.nic }
    )
  }
]

resource zoneGroups 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-05-01' = [
  for (e, i) in endpoints: {
    parent: pe[i]
    name: 'default'
    properties: {
      privateDnsZoneConfigs: [
        {
          name: e.group
          properties: { privateDnsZoneId: e.zone }
        }
      ]
    }
  }
]
