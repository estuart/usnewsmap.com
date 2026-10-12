// Private container registry (08 §8.6). Images are never public: pulls need
// Entra ID (the app and ingest identities have AcrPull); there is no
// admin user and no anonymous pull. CI pushes as the CI identity
// (identities.bicep), which GitHub Actions signs in to with OIDC.
//
// Basic tier: private endpoints need Premium (~$50/month), so the registry
// keeps its public endpoint, which still requires a token. Images are code,
// not data; the data services stay private (ADR-0008).

param location string
param tags object
param name string
@description('The CI identity (identities.bicep), which gets AcrPush: its resource id and principal.')
param ciIdentityId string
param ciPrincipalId string
param pullPrincipalIds array

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' = {
  name: name
  location: location
  tags: tags
  sku: { name: 'Basic' }
  properties: {
    // Entra only (ADR-0009): no admin user. Anonymous pull needs Standard
    // or Premium, so Basic can't turn it on.
    adminUserEnabled: false
    publicNetworkAccess: 'Enabled'
  }
}

var acrPull = '7f951dda-4ed3-4680-a7ca-43fe172d538d'
var acrPush = '8311e382-0749-4cb8-b61a-304f252e45ec'

resource pulls 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for p in pullPrincipalIds: {
    scope: registry
    name: guid(registry.id, p, acrPull)
    properties: {
      principalId: p
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', acrPull)
    }
  }
]

resource push 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: registry
  name: guid(registry.id, ciIdentityId, acrPush)
  properties: {
    principalId: ciPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', acrPush)
  }
}

output name string = registry.name
output loginServer string = registry.properties.loginServer
