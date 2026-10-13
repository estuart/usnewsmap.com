// The stock BOINC server as ONE Container App: Apache/PHP, the CGI scheduler and
// upload handler, and the stock daemons (feeder, transitioner, file_deleter, db_purge,
// sample_trivial_validator, batch_collect_assimilator) under a process supervisor. They must share
// a host: the feeder and scheduler use shared memory, and every daemon uses the same
// project filesystem. So: one replica, never scaled out, never scaled to zero
// (clients poll constantly and the feeder cache is in memory).
param name string
param location string
param acaSubnetId string
param logAnalyticsCustomerId string
@secure()
param logAnalyticsSharedKey string
param identityId string
param identityClientId string
param image string
@description('Registry login server, taken from the image reference. The app pulls with its identity (AcrPull); the registry has no admin user or anonymous pull.')
param registryServer string
param keyVaultName string
param filesAccountName string
param shareName string
@secure()
param filesAccountKey string
param mysqlHost string
param databaseName string
param masterHost string
@description('vCPU for the single container. Memory must be 2 GiB per vCPU in the consumption profile.')
param cpu string = '1.0'
param memory string = '2Gi'

resource env 'Microsoft.App/managedEnvironments@2024-03-01' = {
  name: 'usnm-${name}-cae'
  location: location
  properties: {
    // Declared so the environment is a workload-profiles one (a /27 subnet is enough for it);
    // a Consumption-only environment would need a /23. Same pattern as infra/modules/containerapps-env.bicep.
    workloadProfiles: [ { name: 'Consumption', workloadProfileType: 'Consumption' } ]
    vnetConfiguration: { infrastructureSubnetId: acaSubnetId, internal: false }
    appLogsConfiguration: {
      destination: 'log-analytics'
      logAnalyticsConfiguration: { customerId: logAnalyticsCustomerId, sharedKey: logAnalyticsSharedKey }
    }
  }
}

resource projectStorage 'Microsoft.App/managedEnvironments/storages@2024-03-01' = {
  parent: env
  name: 'project'
  properties: {
    azureFile: {
      accountName: filesAccountName
      accountKey: filesAccountKey
      shareName: shareName
      accessMode: 'ReadWrite'
    }
  }
}

resource app 'Microsoft.App/containerApps@2024-03-01' = {
  name: 'usnm-${name}-app'
  location: location
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${identityId}': {} }
  }
  properties: {
    managedEnvironmentId: env.id
    workloadProfileName: 'Consumption'
    configuration: {
      activeRevisionsMode: 'Single'
      registries: [ { server: registryServer, identity: identityId } ]
      ingress: {
        external: true
        targetPort: 80
        transport: 'auto'
        allowInsecure: false
      }
    }
    template: {
      containers: [
        {
          name: 'boinc'
          image: image
          resources: { cpu: json(cpu), memory: memory }
          env: [
            { name: 'AZURE_CLIENT_ID', value: identityClientId }
            { name: 'KEY_VAULT_NAME', value: keyVaultName }
            { name: 'DB_HOST', value: mysqlHost }
            { name: 'DB_NAME', value: databaseName }
            { name: 'MASTER_HOST', value: masterHost }
            { name: 'PROJECT_DIR', value: '/project' }
          ]
          volumeMounts: [ { volumeName: 'project', mountPath: '/project' } ]
          probes: [
            {
              type: 'Liveness'
              httpGet: { path: '/usnmocr/get_project_config.php', port: 80 }
              periodSeconds: 30
              failureThreshold: 5
            }
          ]
        }
      ]
      volumes: [ { name: 'project', storageType: 'AzureFile', storageName: projectStorage.name } ]
      scale: { minReplicas: 1, maxReplicas: 1 }
    }
  }
}

output fqdn string = app.properties.configuration.ingress.fqdn
output environmentId string = env.id
output projectStorageName string = projectStorage.name
output workloadProfileName string = 'Consumption'
