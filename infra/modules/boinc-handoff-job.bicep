// The BOINC hand-off job (#469, docs/boinc.md#one-page-from-the-scan-cache): the BOINC
// bridge image running /usr/local/lib/usnm-boinc/handoff-job.sh in the main Container Apps
// environment, which reaches Cosmos and the lake through their private endpoints and the
// exchange account through the subnet's service endpoint. It never talks to the BOINC
// server; the boinc-handoff container sits between them. Manual only:
// scripts/start-boinc-bridge.sh --target handoff writes each run's settings (HANDOFF_STEP
// and that step's) and starts it.
//
// Its own user-assigned identity, with AcrPull on the shared registry here. Its data grants
// are boinc-handoff-runner.bicep's (Cosmos ops, the ledger, reference, ocr-out) and the
// node writer role on boinc-handoff (ocr-exchange.bicep).

param location string
param tags object
param environmentId string
@description('The bridge image (images.bridge in contrib-compute/infra/boinc/env/<env>.json), by digest, in the shared registry.')
param image string
param registryName string
@maxLength(31)
param jobName string
param identityName string
@description('The exchange account\'s boinc-handoff container URL.')
param handoffUrl string
param cosmosEndpoint string
@description('The data lake\'s curated and reference container URLs.')
param curatedUrl string
param referenceUrl string
@description('The exchange account\'s ocr-out container URL.')
param outUrl string
@description('Seconds before a run is stopped. Export copies scans/ down, so a larger cohort needs longer.')
param timeoutSeconds int = 1800

// https://learn.microsoft.com/en-us/azure/role-based-access-control/built-in-roles/containers#acrpull
var acrPull = '7f951dda-4ed3-4680-a7ca-43fe172d538d'

resource identity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: identityName
  location: location
  tags: tags
}

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' existing = {
  name: registryName
}

resource pull 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: registry
  name: guid(registry.id, identity.id, acrPull)
  properties: {
    principalId: identity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', acrPull)
  }
}

resource job 'Microsoft.App/jobs@2025-01-01' = {
  name: jobName
  location: location
  tags: tags
  dependsOn: [pull]
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${identity.id}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registry.properties.loginServer, identity: identity.id }]
      triggerType: 'Manual'
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
      replicaTimeout: timeoutSeconds
      // A repeated export replays its own reservation, but nothing retries it unasked.
      replicaRetryLimit: 0
    }
    template: {
      containers: [
        {
          name: 'handoff'
          image: image
          command: ['/usr/local/lib/usnm-boinc/handoff-job.sh']
          resources: { cpu: json('0.5'), memory: '1Gi' }
          // scripts/start-boinc-bridge.sh keeps these as they are (its base settings for
          // --target handoff; scripts/test_boinc_scripts.py checks the two lists agree).
          env: [
            { name: 'AZURE_CLIENT_ID', value: identity.properties.clientId }
            { name: 'HANDOFF_URL', value: handoffUrl }
            { name: 'COSMOS_ENDPOINT', value: cosmosEndpoint }
            { name: 'CURATED_URL', value: curatedUrl }
            { name: 'REFERENCE_URL', value: referenceUrl }
            { name: 'OUT_URL', value: outUrl }
          ]
        }
      ]
    }
  }
}

output name string = job.name
output principalId string = identity.properties.principalId
