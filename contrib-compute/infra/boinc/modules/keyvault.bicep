// Key Vault for the BOINC project secrets, read by the app's managed identity.
// Secrets are created by the operator bootstrap script, not by this template, so
// no secret value passes through a deployment parameter or a template output.
// The vault is not public: public network access is off and it is reached through a private
// endpoint in the project's VNet. The workload identity reads it from there; the operator
// needs a network path into the VNet to create the secrets (docs/design.md, section 5.2).
// Expected secret names: mysql-admin-password, mysql-runtime-password,
// upload-private-key, upload-public-key. The code-signing private key is NOT here.
param name string
param location string
param readerPrincipalId string
param logAnalyticsWorkspaceId string
param peSubnetId string
param dnsZoneId string
@description('Entra object ids (people or groups) given Key Vault Secrets Officer, so the bootstrap script can create the secrets.')
param operatorPrincipalIds array

var vaultName = take('usnm${name}kv${uniqueString(resourceGroup().id, name)}', 24)

resource vault 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: vaultName
  location: location
  properties: {
    tenantId: subscription().tenantId
    sku: { family: 'A', name: 'standard' }
    enableRbacAuthorization: true
    enableSoftDelete: true
    softDeleteRetentionInDays: 30
    enablePurgeProtection: true
    publicNetworkAccess: 'Disabled'
    networkAcls: { defaultAction: 'Deny', bypass: 'None' }
  }
}

resource endpoint 'Microsoft.Network/privateEndpoints@2024-01-01' = {
  name: 'usnm-${name}-kv-pe'
  location: location
  properties: {
    subnet: { id: peSubnetId }
    privateLinkServiceConnections: [
      { name: 'vault', properties: { privateLinkServiceId: vault.id, groupIds: [ 'vault' ] } }
    ]
  }
}

resource endpointDns 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-01-01' = {
  parent: endpoint
  name: 'default'
  properties: { privateDnsZoneConfigs: [ { name: 'vault', properties: { privateDnsZoneId: dnsZoneId } } ] }
}

// Key Vault Secrets User
var secretsUser = '4633458b-17de-408a-b874-0445c86b69e6'

resource readerRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: vault
  name: guid(vault.id, readerPrincipalId, secretsUser)
  properties: {
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', secretsUser)
    principalId: readerPrincipalId
    principalType: 'ServicePrincipal'
  }
}

// Key Vault Secrets Officer, as infra/modules/ocr-vault.bicep grants its operators.
var secretsOfficer = 'b86a8fe4-44ce-4948-aee5-eccb2c155cd7'

resource officers 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for operator in operatorPrincipalIds: {
    scope: vault
    name: guid(vault.id, operator, secretsOfficer)
    properties: {
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', secretsOfficer)
      principalId: operator
    }
  }
]

resource diag 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: vault
  name: 'to-log'
  properties: {
    workspaceId: logAnalyticsWorkspaceId
    logs: [ { categoryGroup: 'audit', enabled: true } ]
  }
}

output name string = vault.name
output uri string = vault.properties.vaultUri
