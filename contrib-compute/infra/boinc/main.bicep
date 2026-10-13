// DRAFT. Never deployed; compile and lint only (az bicep build).
// Hosting for the stock BOINC server that PR #441 (usnm-boinc, ocr/boinc) submits to:
// one Container App for Apache/PHP and the stock daemons, a manual-trigger bridge job
// that runs usnm-boinc submit next to it, a private MySQL flexible server, one Azure
// Files share, Key Vault, Log Analytics and a static site. No BOINC code is built here.
// Design and cost: docs/design.md. Deployment: scripts/boinc/bootstrap.sh.
targetScope = 'resourceGroup'

@description('Short lower-case name used in resource names.')
@minLength(3)
@maxLength(12)
param name string = 'boinc'

param location string = resourceGroup().location

@description('Server image, by digest, for example registry/name@sha256:...')
param image string

@secure()
@description('MySQL admin password. The bootstrap script generates it and keeps it in Key Vault.')
param mysqlAdminPassword string

@description('Public host name of the BOINC master URL, for example boinc.usnewsmap.com. Required: bootstrap.sh derives it from --project-url. It cannot change once volunteers attach.')
@minLength(4)
param masterHost string

@description('Resource group of the registry that holds the image (infra/modules/registry.bicep). The registry must be in this subscription.')
param registryResourceGroup string = resourceGroup().name

@description('Entra object ids (people or groups) given Key Vault Secrets Officer, so the bootstrap script can create the secrets.')
param operatorPrincipalIds array

// The registry is named by the image: <registry>.azurecr.io/<repository>@sha256:...
var registryServer = split(image, '/')[0]
var registryName = split(registryServer, '.')[0]

// Computed here, not read from the storage module, so listKeys() below can resolve at the start of the deployment.
var filesAccountName = take('usnm${name}fil${uniqueString(resourceGroup().id, name)}', 24)

resource identity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: 'usnm-${name}-id'
  location: location
}

module acrPull 'modules/acr-pull.bicep' = {
  name: 'acr-pull'
  scope: resourceGroup(registryResourceGroup)
  params: { registryName: registryName, principalId: identity.properties.principalId }
}

module monitor 'modules/monitor.bicep' = {
  name: 'monitor'
  params: { name: name, location: location }
}

module network 'modules/network.bicep' = {
  name: 'network'
  params: { name: name, location: location }
}

module vault 'modules/keyvault.bicep' = {
  name: 'vault'
  params: {
    name: name
    location: location
    readerPrincipalId: identity.properties.principalId
    peSubnetId: network.outputs.peSubnetId
    dnsZoneId: network.outputs.vaultDnsZoneId
    operatorPrincipalIds: operatorPrincipalIds
    logAnalyticsWorkspaceId: monitor.outputs.id
  }
}

module storage 'modules/storage.bicep' = {
  name: 'storage'
  params: {
    name: name
    location: location
    filesAccountName: filesAccountName
    peSubnetId: network.outputs.peSubnetId
    dnsZoneId: network.outputs.filesDnsZoneId
  }
}

module mysql 'modules/mysql.bicep' = {
  name: 'mysql'
  params: {
    name: name
    location: location
    subnetId: network.outputs.mysqlSubnetId
    privateDnsZoneId: network.outputs.mysqlDnsZoneId
    adminPassword: mysqlAdminPassword
  }
}

module frontend 'modules/frontend.bicep' = {
  name: 'frontend'
  params: { name: name }
}

// The Container Apps mount takes the account key, so it is read from the account this template
// creates (the storage module is a dependency); no key is passed in or committed.
resource filesAccount 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: filesAccountName
}

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' existing = {
  name: 'usnm-${name}-log'
}

module app 'modules/app.bicep' = {
  name: 'app'
  dependsOn: [ acrPull ]
  params: {
    name: name
    location: location
    acaSubnetId: network.outputs.acaSubnetId
    logAnalyticsCustomerId: monitor.outputs.customerId
    logAnalyticsSharedKey: workspace.listKeys().primarySharedKey
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: image
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    filesAccountName: storage.outputs.filesAccountName
    shareName: storage.outputs.shareName
    filesAccountKey: filesAccount.listKeys().keys[0].value
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
  }
}

module bridgeJob 'modules/bridge-job.bicep' = {
  name: 'bridge-job'
  dependsOn: [ acrPull ]
  params: {
    name: name
    location: location
    environmentId: app.outputs.environmentId
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: image
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    projectStorageName: app.outputs.projectStorageName
    workloadProfileName: app.outputs.workloadProfileName
  }
}

output bridgeJobName string = bridgeJob.outputs.name
output appFqdn string = app.outputs.fqdn
output staticSiteHost string = frontend.outputs.defaultHostname
output keyVaultName string = vault.outputs.name
output mysqlHost string = mysql.outputs.host
