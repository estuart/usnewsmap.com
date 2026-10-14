// Key Vault for the BOINC project secrets, read by the app's managed identity.
// The vault is not public: public network access is off and it is reached through a private
// endpoint in the project's VNet (docs/design.md, section 5.2).
//
// Who writes what:
// - this template: mysql-admin-password (a new random value each deployment, from main.bicep)
//   and code-sign-public-key (the PUBLIC half of the offline key, from the settings, if set).
//   Both go through Azure Resource Manager, which needs no network path into the VNet.
// - the secrets job (secrets-job.bicep), inside the VNet with its own identity:
//   mysql-runtime-password, upload-private-key and upload-public-key, each only if absent.
// The code-signing private key is NOT here and never reaches Azure.
param vaultName string
param endpointName string
param location string
param readerPrincipalId string
@description('The secrets job\'s identity: Key Vault Secrets Officer, to create the secrets that must not rotate.')
param writerPrincipalId string
param logAnalyticsWorkspaceId string
param peSubnetId string
param dnsZoneId string
@description('Entra object ids (people or groups) given Key Vault Secrets Officer, to inspect or repair secrets by hand.')
param operatorPrincipalIds array
@secure()
param mysqlAdminPassword string
param codeSignPublicKey string = ''

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
  name: endpointName
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

resource writerRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: vault
  name: guid(vault.id, writerPrincipalId, secretsOfficer)
  properties: {
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', secretsOfficer)
    principalId: writerPrincipalId
    principalType: 'ServicePrincipal'
  }
}

resource adminPassword 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: vault
  name: 'mysql-admin-password'
  properties: { value: mysqlAdminPassword, contentType: 'text/plain' }
}

resource codeSignPublic 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = if (!empty(codeSignPublicKey)) {
  parent: vault
  name: 'code-sign-public-key'
  properties: { value: codeSignPublicKey, contentType: 'text/plain' }
}

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
