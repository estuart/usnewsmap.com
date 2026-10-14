// AcrPull for the BOINC identities on the app's shared registry (infra/names/<env>.json), which
// holds the server and bridge images (scripts/build-boinc-images.sh). Deployed by the BOINC
// stack at the registry's group, so the grant lives with the BOINC infrastructure. The worker
// image that volunteers pull is a public GHCR package (ADR-0019, addendum).
param registryName string
@description('Principals given AcrPull: the app\'s and the secrets job\'s identities.')
param pullPrincipalIds array

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' existing = {
  name: registryName
}

// https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/containers#acrpull
var acrPull = '7f951dda-4ed3-4680-a7ca-43fe172d538d'

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

output loginServer string = registry.properties.loginServer
