// VNet with a delegated Container Apps subnet and a private-endpoint subnet,
// plus the private DNS zones the endpoints register in (08 §8.1, ADR-0008).

param location string
param tags object
param name string
@description('The VNet\'s address space: a private IPv4 block, /26 or larger. snet-cae is its first /27 and snet-pe the /28 after that. No default: private addresses aren\'t committed.')
param addressPrefix string
var caeSubnetPrefix = cidrSubnet(addressPrefix, 27, 0)
var peSubnetPrefix = cidrSubnet(addressPrefix, 28, 2)
@description('A Microsoft.Storage service endpoint on snet-cae, for the OCR exchange account\'s firewall rule (ocr-exchange.bicep).')
param storageServiceEndpoint bool = false

var dnsZones = [
  'privatelink.blob.${environment().suffixes.storage}'
  'privatelink.documents.azure.com'
]

resource vnet 'Microsoft.Network/virtualNetworks@2024-05-01' = {
  name: name
  location: location
  tags: tags
  properties: {
    addressSpace: { addressPrefixes: [addressPrefix] }
    subnets: [
      {
        name: 'snet-cae'
        properties: {
          addressPrefix: caeSubnetPrefix
          // With the OCR nodes on (#296), the subnet's traffic to storage
          // accounts' public endpoints leaves from its private addresses,
          // which the exchange account's virtual network rule allows. The
          // data lake and archive accounts are reached through private
          // endpoints, whose private DNS names resolve to the endpoints'
          // addresses, so their traffic doesn't change. No account here
          // allows the environment's public outbound IP, so no IP rule stops
          // matching. Turning it on or off closes open connections to storage
          // public endpoints from the subnet, so provision between jobs.
          serviceEndpoints: storageServiceEndpoint ? [{ service: 'Microsoft.Storage', locations: [location] }] : []
          delegations: [
            {
              name: 'containerapps'
              properties: { serviceName: 'Microsoft.App/environments' }
            }
          ]
        }
      }
      {
        name: 'snet-pe'
        properties: {
          addressPrefix: peSubnetPrefix
          privateEndpointNetworkPolicies: 'Disabled'
        }
      }
    ]
  }
}

resource zones 'Microsoft.Network/privateDnsZones@2024-06-01' = [
  for zone in dnsZones: {
    name: zone
    location: 'global'
    tags: tags
  }
]

resource links 'Microsoft.Network/privateDnsZones/virtualNetworkLinks@2024-06-01' = [
  for (zone, i) in dnsZones: {
    parent: zones[i]
    name: '${name}-link'
    location: 'global'
    tags: tags
    properties: {
      registrationEnabled: false
      virtualNetwork: { id: vnet.id }
    }
  }
]

output vnetId string = vnet.id
output vnetName string = vnet.name
output caeSubnetId string = '${vnet.id}/subnets/snet-cae'
output peSubnetId string = '${vnet.id}/subnets/snet-pe'
output blobDnsZoneId string = zones[0].id
output cosmosDnsZoneId string = zones[1].id
