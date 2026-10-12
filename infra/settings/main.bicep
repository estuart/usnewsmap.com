// The settings store (infra/modules/settings-store.bicep, #295): one App
// Configuration store for every environment in the subscription, each
// environment's private settings under its own label. It sits outside every
// environment's stack, in its own stack `usnm-config` and group
// `usnm-config-rg`, deployed with scripts/settings-store.sh (which
// scripts/lib/env.sh's provision runs when the store is missing), so no
// environment's provision or teardown can delete it.
//
// Names (ADR-0017): a shared resource has no environment, so its name is
// usnm-{fn}-{abbr} (`usnm-config-rg`), with the global suffix on a global
// type (`usnm-config-appcs-{sfx}`). The suffix has its own seed, like the
// archival account's, so it doesn't depend on any environment's name.
//
// No guard-rail assignment here: no guard-rail definition covers App
// Configuration, and an assignment would tie this stack to usnm-guardrails,
// which teardown.sh deletes with the last environment.

targetScope = 'subscription'

@description('Region; the environments\' (East US 2).')
param location string = 'eastus2'

@description('The store\'s name: stable per subscription.')
param name string = 'usnm-config-appcs-${take(uniqueString(subscription().id, 'usnm-config'), 6)}'

@description('Entra object ids (people or groups), comma separated, given App Configuration Data Owner on the store besides the account deploying the stack: every operator who provisions an environment, so each keeps reading the private settings whoever deploys it. scripts/settings-store.sh keeps the stack\'s value when it isn\'t given.')
param admins string = ''

var tags = {
  project: 'usnewsmap'
  environment: 'shared'
}

resource rg 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: 'usnm-config-rg'
  location: location
  tags: tags
}

module store '../modules/settings-store.bicep' = {
  scope: rg
  name: 'settings-store'
  params: {
    location: location
    tags: tags
    name: name
    admins: filter(map(split(admins, ','), a => trim(a)), a => !empty(a))
  }
}

// scripts/lib/env.sh reads and writes the settings at this endpoint.
output SETTINGS_STORE_ENDPOINT string = store.outputs.endpoint
output SETTINGS_STORE_ID string = store.outputs.id
output SETTINGS_STORE string = store.outputs.name
