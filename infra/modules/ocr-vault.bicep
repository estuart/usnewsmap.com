// The OCR node fleet's Key Vault (#296, #295): secrets the operator holds
// for setting up nodes, which aren't Azure credentials and have nowhere else
// to live: `rhel-activation-key` and `rhel-org-id`, the Red Hat activation
// key and organization id that onboard.sh reads with the operator's az
// sign-in and passes to node-setup.sh over ssh's stdin, and
// `rhel-offline-token`, a Red Hat API offline token for fetching RHEL
// images on the Proxmox hosts. The
// maintainer sets them with az (docs/operations.md); the template never
// writes a secret, so nothing secret is in the repository or a deployment.
//
// Access is Azure RBAC only (no access policies): the operators named in
// USNM_OCR_KEYVAULT_ADMINS get Key Vault Secrets Officer. Nodes get nothing.
//
// Network: the operator reads it from their own machine, at the home IP, so
// a private endpoint (about $7.30 a month, and unreachable from that machine
// without a VPN) doesn't fit. Public network access is on behind a firewall
// whose default action is Deny, allowing only the node IPs (the operator's
// home IP, USNM_OCR_NODE_IPS, never in the repository), with no
// trusted-service bypass, like the exchange account. The public-network
// policy exempts it by resource id (main.bicep), so its assignment must be
// in place before the vault is written.
//
// Purge protection is on in prod. Elsewhere it's off and soft delete keeps
// a deleted vault for the minimum 7 days, so teardown.sh can purge it and a
// rebuilt dev environment can use the same name (ADR-0017, open question 1).

param location string
param tags object
param name string
@description('Keep deleted vaults and secrets for 90 days and refuse purges (prod). Off: 7 days, purgeable. Both are fixed when the vault is created.')
param purgeProtection bool
@description('Public IPv4 addresses or CIDR ranges allowed through the firewall (the operator\'s home IP).')
param allowedIps array = []
@description('Entra object ids (people or groups) given Key Vault Secrets Officer: they set and read the secrets.')
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
      defaultAction: 'Deny'
      // No trusted-service bypass: nothing but the operator's IP.
      bypass: 'None'
      ipRules: [for ip in allowedIps: { value: ip }]
      virtualNetworkRules: []
    }
  }
}

resource officers 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for admin in admins: {
    scope: vault
    name: guid(vault.id, admin, secretsOfficer)
    properties: {
      principalId: admin
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', secretsOfficer)
    }
  }
]

output name string = vault.name
output id string = vault.id
