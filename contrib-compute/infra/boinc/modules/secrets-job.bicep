// The job that creates the project's lasting secrets inside the VNet, where the private
// Key Vault is reachable: mysql-runtime-password, upload-private-key and upload-public-key.
// It runs the server image's `entrypoint.sh --ensure-secrets` with its own identity (Key Vault
// Secrets Officer), creates each secret only if it is absent, and stops on a failed lookup
// instead of reading it as absence. Values are generated in the container's tmpfs and never
// printed. scripts/provision-boinc.sh starts it after each deployment; a rerun changes nothing.
param jobName string
param location string
param environmentId string
param identityId string
param identityClientId string
param image string
param registryServer string
param keyVaultName string
param workloadProfileName string

resource job 'Microsoft.App/jobs@2024-03-01' = {
  name: jobName
  location: location
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${identityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: workloadProfileName
    configuration: {
      registries: [ { server: registryServer, identity: identityId } ]
      triggerType: 'Manual'
      replicaTimeout: 600
      replicaRetryLimit: 0
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
    }
    template: {
      containers: [
        {
          name: 'secrets'
          image: image
          command: [ '/usr/local/lib/usnm-boinc/entrypoint.sh' ]
          args: [ '--ensure-secrets' ]
          resources: { cpu: json('0.25'), memory: '0.5Gi' }
          env: [
            { name: 'AZURE_CLIENT_ID', value: identityClientId }
            { name: 'KEY_VAULT_NAME', value: keyVaultName }
          ]
        }
      ]
    }
  }
}

output name string = job.name
