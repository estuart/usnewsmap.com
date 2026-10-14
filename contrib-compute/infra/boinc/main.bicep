// Hosting for the stock BOINC server that the usnm-boinc bridge (crates/usnm-boinc, ocr/boinc)
// submits to: one Container App for Apache/PHP and the stock daemons, a manual-trigger bridge
// job that runs usnm-boinc submit next to it, a job that creates the project's secrets inside
// the VNet, a private MySQL flexible server, one Azure Files share, Key Vault, a registry for
// the server images, Log Analytics and a static site. No BOINC code is built here.
//
// Deployed as the deployment stack usnm-boinc-<env> into the group usnm-boinc-rg-<env>, which
// infra/main.bicep creates (deployBoincGroup) with the guardrails and two waivers (ADR-0019,
// decision 5). Settings: env/<env>.json. Deployment: scripts/provision-boinc.sh.
//
// Two phases. Until the settings name the server image, only the base resources are deployed
// (registry, network, vault, storage, database, monitoring), so the images can be built into
// the registry (scripts/build-boinc-images.sh); with the images set, the app and the jobs too.
targetScope = 'resourceGroup'

import { checkedEnvName, usnmName, usnmCompactName, usnmGlobalName } from '../../../infra/naming.bicep'

@description('Environment: prod or dev (ADR-0017).')
param env string

param location string = resourceGroup().location

@description('The app\'s shared registry (infra/names/<env>.json), which holds the server and bridge images.')
param registryName string

@description('The shared registry\'s resource group.')
param registryResourceGroup string

@description('Server image (images/server), by digest, in the shared registry. Empty: deploy the base resources only.')
param image string = ''

@description('Bridge job image (images/bridge), by digest, in the same registry. Required with image: the server image has no usnm-boinc binary.')
param bridgeImage string = ''

@description('A signed app version baked on the server image (images/appver), by digest, in the same registry. images.appver in env/<env>.json, written by scripts/boinc-app-release.sh; empty: no publish job.')
param publishImage string = ''

@description('Run the server from the bridge image (the server image plus usnm-boinc), so the usnm_assess validator (#456) can run. Off by default.')
param assessment bool = false

@description('Deploy the gold bundle job (docs/boinc.md#build-the-gold-bundle): the bridge image running the gold-bundle step, which builds the gold bundle v1 from its publishers onto the share. Off by default.')
param goldBundle bool = false

@description('Seconds before a bridge job run is stopped. assess-stage fetches at 5 s per request, so a longer limit lets one run stage more pages.')
param bridgeTimeoutSeconds int = 1800

@secure()
@description('MySQL admin password. A new one on every deployment, written to Key Vault by this template (mysql-admin-password); nothing outside the deployment sees it.')
param mysqlAdminPassword string = 'Aa1-${newGuid()}'

@description('Public host name of the BOINC master URL, for example boinc.usnewsmap.com. It cannot change once volunteers attach.')
@minLength(4)
param masterHost string

@description('The app\'s public DNS zone (infra/env/<env>.json USNM_DNS_ZONE) and its group; empty skips the custom domain.')
param dnsZoneName string = ''
param dnsZoneResourceGroup string = ''

@description('Custom domain stage (modules/app.bicep); provision-boinc.sh steps it from 0 to 2.')
@allowed([0, 1, 2])
param customDomainStage int = 0

@description('Entra object ids (people or groups) given Key Vault Secrets Officer on the vault and Log Analytics Reader on the workspace.')
param operatorPrincipalIds array

@description('The PUBLIC half of the offline code-signing key (code_sign_public). Empty until it is made offline.')
param codeSignPublicKey string = ''

@description('Create the workbook "usnm BOINC (<env>)" over the workspace (modules/workbook.bicep). On by default in prod.')
param workbook bool = env == 'prod'

var envName = checkedEnvName(env)
var sfx = take(uniqueString(subscription().id, envName, 'boinc', location), 6)
var withApp = !empty(image)

var names = {
  app: usnmName('boinc', 'ca', envName)
  bridgeJob: usnmName('boinc-bridge', 'caj', envName)
  environment: usnmName('boinc', 'cae', envName)
  files: usnmCompactName('boinc', 'st', envName, sfx)
  filesEndpoint: usnmName('boinc-files', 'pep', envName)
  identity: usnmName('boinc', 'id', envName)
  mysql: usnmGlobalName('boinc', 'mysql', envName, sfx)
  mysqlDnsZone: 'usnm-boinc-${envName}.private.mysql.database.azure.com'
  initJob: usnmName('boinc-init', 'caj', envName)
  goldBundleJob: usnmName('boinc-gold', 'caj', envName)
  publishJob: usnmName('boinc-publish', 'caj', envName)
  secretsIdentity: usnmName('boinc-secrets', 'id', envName)
  secretsJob: usnmName('boinc-secrets', 'caj', envName)
  site: usnmName('boinc', 'swa', envName)
  vault: usnmCompactName('boinc', 'kv', envName, sfx)
  vaultEndpoint: usnmName('boinc-vault', 'pep', envName)
  vnet: usnmName('boinc', 'vnet', envName)
  workspace: usnmName('boinc', 'log', envName)
  workbookTitle: 'usnm BOINC (${envName})'
}

var registryServer = '${registryName}${environment().suffixes.acrLoginServer}'

resource identity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: names.identity
  location: location
}

// The secrets job's own identity: Secrets Officer on the vault, which the app's does not need.
resource secretsIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: names.secretsIdentity
  location: location
}

module registry 'modules/registry.bicep' = {
  name: 'boinc-registry-pull'
  scope: resourceGroup(registryResourceGroup)
  params: {
    registryName: registryName
    pullPrincipalIds: [ identity.properties.principalId, secretsIdentity.properties.principalId ]
  }
}

module monitor 'modules/monitor.bicep' = {
  name: 'monitor'
  params: { workspaceName: names.workspace, location: location, operatorPrincipalIds: operatorPrincipalIds }
}

module workbookModule 'modules/workbook.bicep' = if (workbook) {
  name: 'workbook'
  params: { location: location, workspaceId: monitor.outputs.id, title: names.workbookTitle }
}

module network 'modules/network.bicep' = {
  name: 'network'
  params: { vnetName: names.vnet, mysqlDnsZoneName: names.mysqlDnsZone, location: location }
}

module vault 'modules/keyvault.bicep' = {
  name: 'vault'
  params: {
    vaultName: names.vault
    endpointName: names.vaultEndpoint
    location: location
    readerPrincipalId: identity.properties.principalId
    writerPrincipalId: secretsIdentity.properties.principalId
    peSubnetId: network.outputs.peSubnetId
    dnsZoneId: network.outputs.vaultDnsZoneId
    operatorPrincipalIds: operatorPrincipalIds
    logAnalyticsWorkspaceId: monitor.outputs.id
    mysqlAdminPassword: mysqlAdminPassword
    codeSignPublicKey: codeSignPublicKey
  }
}

module storage 'modules/storage.bicep' = {
  name: 'storage'
  params: {
    filesAccountName: names.files
    endpointName: names.filesEndpoint
    location: location
    peSubnetId: network.outputs.peSubnetId
    dnsZoneId: network.outputs.filesDnsZoneId
  }
}

module mysql 'modules/mysql.bicep' = {
  name: 'mysql'
  params: {
    serverName: names.mysql
    location: location
    subnetId: network.outputs.mysqlSubnetId
    privateDnsZoneId: network.outputs.mysqlDnsZoneId
    adminPassword: mysqlAdminPassword
  }
}

module frontend 'modules/frontend.bicep' = {
  name: 'frontend'
  params: { siteName: names.site }
}

// The Container Apps mount takes the account key and the environment's log configuration the
// workspace key (ADR-0019, decision 5, waived for this group). Both are read here, inside the
// deployment, from resources this template creates; no key is passed in, printed or committed.
resource filesAccount 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: names.files
}

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' existing = {
  name: names.workspace
}

module app 'modules/app.bicep' = if (withApp) {
  name: 'app'
  dependsOn: [ registry ]
  params: {
    environmentName: names.environment
    appName: names.app
    location: location
    acaSubnetId: network.outputs.acaSubnetId
    logAnalyticsCustomerId: monitor.outputs.customerId
    logAnalyticsSharedKey: workspace.listKeys().primarySharedKey
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: assessment ? bridgeImage : image
    // The bridge image's entry point is bridge-job; the server keeps its own.
    command: assessment ? [ '/usr/local/lib/usnm-boinc/entrypoint.sh', '--serve' ] : []
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    filesAccountName: storage.outputs.filesAccountName
    shareName: storage.outputs.shareName
    filesAccountKey: filesAccount.listKeys().keys[0].value
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    customDomainStage: empty(dnsZoneName) ? 0 : customDomainStage
  }
}

module customDomainDns 'modules/custom-domain-dns.bicep' = if (withApp && !empty(dnsZoneName)) {
  name: 'boinc-custom-domain-dns'
  scope: resourceGroup(dnsZoneResourceGroup)
  params: {
    zoneName: dnsZoneName
    recordName: substring(masterHost, 0, length(masterHost) - length(dnsZoneName) - 1)
    appFqdn: app!.outputs.fqdn
    verificationId: app!.outputs.verificationId
  }
}

module bridgeJob 'modules/bridge-job.bicep' = if (withApp) {
  name: 'bridge-job'
  dependsOn: [ registry ]
  params: {
    jobName: names.bridgeJob
    location: location
    environmentId: app!.outputs.environmentId
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: bridgeImage
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    projectStorageName: app!.outputs.projectStorageName
    workloadProfileName: app!.outputs.workloadProfileName
    timeoutSeconds: bridgeTimeoutSeconds
  }
}

// Project init (the schema and the project tree on the share) as its own manual job: the same
// mounts and settings as the bridge, which a per-run command override would drop. The
// entrypoint refuses to initialise twice.
module initJob 'modules/bridge-job.bicep' = if (withApp) {
  name: 'init-job'
  dependsOn: [ registry ]
  params: {
    jobName: names.initJob
    location: location
    environmentId: app!.outputs.environmentId
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: image
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    projectStorageName: app!.outputs.projectStorageName
    workloadProfileName: app!.outputs.workloadProfileName
    command: [ '/usr/local/lib/usnm-boinc/entrypoint.sh', '--init-project' ]
  }
}

// App version publish (docs/app-release.md): copies the version directory baked into publishImage
// onto the share and runs bin/update_versions. Its own job, like init, because a per-run image
// override would drop the share and the settings. Deployed whenever images.appver is set, so it stays
// across deployments and always holds the last published version.
module publishJob 'modules/bridge-job.bicep' = if (withApp && !empty(publishImage)) {
  name: 'publish-job'
  dependsOn: [ registry ]
  params: {
    jobName: names.publishJob
    location: location
    environmentId: app!.outputs.environmentId
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: publishImage
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    projectStorageName: app!.outputs.projectStorageName
    workloadProfileName: app!.outputs.workloadProfileName
    command: [ '/usr/local/lib/usnm-boinc/publish-appver.sh' ]
  }
}

// The gold bundle v1, built inside the VNet from the datasets' publishers onto the share
// (gold-bundle.sh): the bridge job's image, mounts and settings, its step fixed in the job, so a
// plain `az containerapp job start` runs it. Six hours: gold.py paces its downloads, and the
// datasets are unpacked on the share.
module goldBundleJob 'modules/bridge-job.bicep' = if (withApp && goldBundle) {
  name: 'gold-bundle-job'
  dependsOn: [ registry ]
  params: {
    jobName: names.goldBundleJob
    location: location
    environmentId: app!.outputs.environmentId
    identityId: identity.id
    identityClientId: identity.properties.clientId
    image: bridgeImage
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    mysqlHost: mysql.outputs.host
    databaseName: mysql.outputs.databaseName
    masterHost: masterHost
    projectStorageName: app!.outputs.projectStorageName
    workloadProfileName: app!.outputs.workloadProfileName
    timeoutSeconds: 21600
    cpu: '1'
    memory: '2Gi'
    extraEnv: [
      { name: 'BRIDGE_STEP', value: 'gold-bundle' }
      { name: 'GOLD_CAMPAIGN', value: 'v1-tess550' }
    ]
  }
}

module secretsJob 'modules/secrets-job.bicep' = if (withApp) {
  name: 'secrets-job'
  dependsOn: [ registry ]
  params: {
    jobName: names.secretsJob
    location: location
    environmentId: app!.outputs.environmentId
    identityId: secretsIdentity.id
    identityClientId: secretsIdentity.properties.clientId
    image: image
    registryServer: registryServer
    keyVaultName: vault.outputs.name
    workloadProfileName: app!.outputs.workloadProfileName
  }
}

output appFqdn string = withApp ? app!.outputs.fqdn : ''
output bridgeJobName string = withApp ? bridgeJob!.outputs.name : ''
output goldBundleJobName string = withApp && goldBundle ? goldBundleJob!.outputs.name : ''
output keyVaultName string = vault.outputs.name
output mysqlHost string = mysql.outputs.host
output publishJobName string = withApp && !empty(publishImage) ? publishJob!.outputs.name : ''
output registryName string = registryName
output registryServer string = registryServer
output secretsJobName string = withApp ? secretsJob!.outputs.name : ''
output staticSiteHost string = frontend.outputs.defaultHostname
