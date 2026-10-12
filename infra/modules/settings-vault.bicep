// The environment's settings vault (#295, ADR-0006): its private settings,
// the USNM_* inputs that infra/env/schema.json marks private (alert
// addresses, reader principals, certificate and account resource ids, the
// repository's ids). None is a credential (ADR-0009: there are none); they
// are configuration that stays out of the public repository.
// scripts/provision.sh reads them with the operator's az sign-in and passes
// them to infra/main.bicepparam through the environment, like the committed
// settings; the template never reads or writes a secret.
//
// Access is Azure RBAC only (no access policies), Entra ID only like every
// Key Vault. Key Vault Secrets Officer (they set and read them) goes to the
// operators in USNM_SETTINGS_VAULT_ADMINS, a stable list kept in this vault,
// so every operator keeps access whoever provisions, and to the account
// deploying the stack, so the first provision (an empty list) can fill it.
// Another deployer's assignment lasts until the next provision by someone
// else, which the stack then removes: list every operator. Nothing else
// reads it: the CI identity only rolls the apps to new images (no data
// access, 08 §8.4), so it has no role here.
//
// Network: a public endpoint with no firewall, like the registry and Log
// Analytics, so laptops and GitHub runners reach it; a private endpoint
// would cost about $7.30 a month and neither could. The public-network
// policy exempts it by resource id (main.bicep).
//
// Purge protection is on in prod. Elsewhere it's off and soft delete keeps
// a deleted vault for the minimum 7 days, so teardown.sh can purge it and a
// rebuilt dev environment can use the same name (ADR-0017, open question 1).

param location string
param tags object
param name string
@description('Keep deleted vaults and secrets for 90 days and refuse purges (prod). Off: 7 days, purgeable. Both are fixed when the vault is created.')
param purgeProtection bool
@description('Entra object ids (people or groups) given Key Vault Secrets Officer, besides the deploying account.')
param admins array = []

// https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/security#key-vault-secrets-officer
var secretsOfficer = 'b86a8fe4-44ce-4948-aee5-eccb2c155cd7'

resource vault 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: name
  location: location
  tags: tags
  properties: {
    tenantId: tenant().tenantId
    sku: { family: 'A', name: 'standard' }
    enableRbacAuthorization: true
    accessPolicies: []
    enabledForDeployment: false
    enabledForDiskEncryption: false
    enabledForTemplateDeployment: false
    enableSoftDelete: true
    softDeleteRetentionInDays: purgeProtection ? 90 : 7
    // Azure accepts only true or unset here: once on, it can't be turned off.
    enablePurgeProtection: purgeProtection ? true : null
    publicNetworkAccess: 'Enabled'
    networkAcls: {
      defaultAction: 'Allow'
      bypass: 'None'
      ipRules: []
      virtualNetworkRules: []
    }
  }
}

// The operators, and the account deploying the stack (the operator running
// provision.sh), once each: union drops a deployer already in the list.
var officerIds = union(admins, [deployer().objectId])

resource officers 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for officer in officerIds: {
    scope: vault
    name: guid(vault.id, officer, secretsOfficer)
    properties: {
      principalId: officer
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', secretsOfficer)
    }
  }
]

output name string = vault.name
