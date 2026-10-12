// The settings store (#295, ADR-0006): one Azure App Configuration store
// for every environment in the subscription, holding each environment's
// private settings under its own label (dev, prod, ...): the USNM_* inputs
// that infra/env/schema.json marks private (alert addresses, reader
// principals, certificate and account resource ids, the repository's ids,
// the VNet's address space). None is a credential (ADR-0009: there are
// none); they are configuration that stays out of the public repository.
// scripts/provision.sh reads them with the operator's az sign-in and passes
// them to infra/main.bicepparam through the environment, like the committed
// settings; the template never reads or writes one.
//
// App Configuration, not Key Vault: these are non-credential settings, the
// Free tier costs nothing (1,000 requests a day, 10 MB; one provision lists
// an environment's settings in one request), and labels put every
// environment in one store, outside every environment's stack, so a
// teardown keeps them for the next stand-up.
//
// Entra ID only: local auth (access keys) is off. App Configuration Data
// Owner (read and write the key-values) goes to the operators in `admins`
// and to the account deploying the stack, so the first deployment (no list
// yet) can fill it. The role is per store, not per label: one maintainer
// operates every environment. Nothing else reads it: the CI identity only
// rolls the apps to new images (no data access, 08 §8.4), so it has no
// role here.
//
// Network: a public endpoint, Entra ID only, so laptops and GitHub runners
// reach it; the Free tier has no private endpoints. No guard-rail policy
// covers App Configuration, so none needs an exemption. The Free tier has no
// soft delete or purge protection.

param location string
param tags object
param name string
@description('Entra object ids (people or groups) given App Configuration Data Owner, besides the deploying account.')
param admins array = []

// https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/integration#app-configuration-data-owner
var dataOwner = '5ae67dd6-50cb-40e7-96ff-dc2bfa4b606b'

resource store 'Microsoft.AppConfiguration/configurationStores@2024-05-01' = {
  name: name
  location: location
  tags: tags
  sku: { name: 'free' }
  properties: {
    disableLocalAuth: true
    publicNetworkAccess: 'Enabled'
  }
}

// The operators, and the account deploying the stack, once each: union
// drops a deployer already in the list.
var ownerIds = union(admins, [deployer().objectId])

resource owners 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for owner in ownerIds: {
    scope: store
    name: guid(store.id, owner, dataOwner)
    properties: {
      principalId: owner
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', dataOwner)
    }
  }
]

output name string = store.name
output id string = store.id
output endpoint string = store.properties.endpoint
