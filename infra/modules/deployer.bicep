// What CI may do in an environment (08 §8.2): roll the API app (which also
// serves the site) onto a new image. One custom role, assigned on the
// environment's resource group:
//
// - Azure rejects role assignments scoped to a Container App itself
//   (RoleDefinitionDoesNotExist, even for built-in Contributor).
// - An app-scoped role wouldn't be enough anyway: `az containerapp update`
//   sends the app's environment and user-assigned identity back, so Azure
//   also checks join and assign rights on those.
//
// The group holds only this environment's resources. The role can't touch
// data (storage, Cosmos), keys, networking or role assignments. Assigning
// identities is granted on the API's own identity only: rights on the whole
// group would let CI attach the ingest identity, and its data access, to the
// app it controls.

@description('The CI identity.')
param principalId string
@description('The API app\'s user-assigned identity, the only one CI may assign.')
param appIdentityName string
@description('The role\'s name, before its scope (infra/naming.bicep).')
param roleLabel string

// The name predates the Container Apps rights (it once also read a Static
// Web App's deploy token); it's kept so existing environments update the
// role in place.
resource role 'Microsoft.Authorization/roleDefinitions@2022-04-01' = {
  name: guid(resourceGroup().id, 'usnm-swa-deployer')
  properties: {
    // Role names are unique per tenant: include the subscription and group.
    roleName: '${roleLabel} (${take(subscription().subscriptionId, 8)}/${resourceGroup().name})'
    description: 'CI deploys: roll Container Apps onto new images.'
    type: 'CustomRole'
    permissions: [
      {
        actions: [
          // az containerapp update / show, and its operation polling.
          'Microsoft.App/containerApps/read'
          'Microsoft.App/containerApps/write'
          'Microsoft.App/containerApps/listSecrets/action'
          'Microsoft.App/containerApps/revisions/read'
          'Microsoft.App/managedEnvironments/read'
          'Microsoft.App/managedEnvironments/join/action'
          'Microsoft.App/locations/*/read'
        ]
      }
    ]
    assignableScopes: [resourceGroup().id]
  }
}

resource assignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(resourceGroup().id, principalId, 'usnm-swa-deployer')
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    // Custom roles are addressed at subscription level wherever they're defined.
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', role.name)
  }
}

// The app's identity comes back in every update: Managed Identity Operator
// (read and assign) on that identity alone.
var identityOperator = 'f1a07417-d97a-45cb-824c-7a7467783830'

resource appIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' existing = {
  name: appIdentityName
}

resource assignIdentity 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: appIdentity
  name: guid(appIdentity.id, principalId, identityOperator)
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', identityOperator)
  }
}
