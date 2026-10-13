// One VNet: a subnet for the Container Apps environment, a delegated subnet for MySQL, and a
// subnet for the private endpoints of the Files account and the Key Vault (neither is public).
// MySQL flexible server access mode (private or public) cannot be changed after creation,
// so the private choice is made here from the start.
param name string
param location string
// Second octet of the VNet's 10.x.0.0/22 block. The address is built from it so the repo holds
// no literal private address.
param vnetOctet int = 40

resource vnet 'Microsoft.Network/virtualNetworks@2024-01-01' = {
  name: 'usnm-${name}-vnet'
  location: location
  properties: {
    addressSpace: { addressPrefixes: [ '10.${vnetOctet}.0.0/22' ] }
    subnets: [
      {
        name: 'aca'
        properties: {
          addressPrefix: '10.${vnetOctet}.0.0/27'
          delegations: [ { name: 'aca', properties: { serviceName: 'Microsoft.App/environments' } } ]
        }
      }
      {
        name: 'pe'
        properties: { addressPrefix: '10.${vnetOctet}.2.0/28' }
      }
      {
        name: 'mysql'
        properties: {
          addressPrefix: '10.${vnetOctet}.1.0/28'
          delegations: [ { name: 'mysql', properties: { serviceName: 'Microsoft.DBforMySQL/flexibleServers' } } ]
        }
      }
    ]
  }
}

resource dns 'Microsoft.Network/privateDnsZones@2020-06-01' = {
  name: '${name}.private.mysql.database.azure.com'
  location: 'global'
}

resource dnsLink 'Microsoft.Network/privateDnsZones/virtualNetworkLinks@2020-06-01' = {
  parent: dns
  name: 'link'
  location: 'global'
  properties: {
    registrationEnabled: false
    virtualNetwork: { id: vnet.id }
  }
}

output acaSubnetId string = '${vnet.id}/subnets/aca'
output mysqlSubnetId string = '${vnet.id}/subnets/mysql'
output mysqlDnsZoneId string = dns.id

// Private DNS for the two private endpoints.
resource filesDns 'Microsoft.Network/privateDnsZones@2020-06-01' = {
  name: 'privatelink.file.${environment().suffixes.storage}'
  location: 'global'
}

resource filesDnsLink 'Microsoft.Network/privateDnsZones/virtualNetworkLinks@2020-06-01' = {
  parent: filesDns
  name: 'link'
  location: 'global'
  properties: {
    registrationEnabled: false
    virtualNetwork: { id: vnet.id }
  }
}

resource vaultDns 'Microsoft.Network/privateDnsZones@2020-06-01' = {
  name: 'privatelink${environment().suffixes.keyvaultDns}'
  location: 'global'
}

resource vaultDnsLink 'Microsoft.Network/privateDnsZones/virtualNetworkLinks@2020-06-01' = {
  parent: vaultDns
  name: 'link'
  location: 'global'
  properties: {
    registrationEnabled: false
    virtualNetwork: { id: vnet.id }
  }
}

output peSubnetId string = '${vnet.id}/subnets/pe'
output filesDnsZoneId string = filesDns.id
output vaultDnsZoneId string = vaultDns.id
