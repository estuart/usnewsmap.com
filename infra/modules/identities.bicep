// User-assigned managed identities (08 §8.2). The launcher identity arrives
// with the backfill launcher and network-guard jobs.
//
// The CI identity pushes images to the registry (registry.bicep gives it
// AcrPush) and rolls the API (deployer.bicep). GitHub Actions signs in as
// it with OIDC (a federated credential; no secret is stored anywhere). It
// trusts only jobs in this repository's GitHub Environment named after the
// environment, which the bootstrap script restricts to the `main` branch.

param location string
param tags object
param appName string
param ingestName string
@description('The CI identity\'s name.')
param ciName string
@description('GitHub repository allowed to deploy, as owner/name.')
param githubRepo string
@description('The same repository in GitHub\'s immutable-ID subject format, `owner@ownerId/name@repoId`; empty if unknown. Repositories may use either format, so both are trusted.')
param githubRepoIds string = ''
@description('GitHub Environment whose jobs may use the CI identity.')
param githubEnvironment string

resource app 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: appName
  location: location
  tags: tags
}

resource ingest 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: ingestName
  location: location
  tags: tags
}

resource ci 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: ciName
  location: location
  tags: tags

  // Only jobs in this repository's GitHub Environment can use it. (The
  // name predates environments; it is kept so existing identities update
  // in place.)
  resource github 'federatedIdentityCredentials' = {
    name: 'github-main'
    properties: {
      issuer: 'https://token.actions.githubusercontent.com'
      subject: 'repo:${githubRepo}:environment:${githubEnvironment}'
      audiences: ['api://AzureADTokenExchange']
    }
  }

  // The immutable-ID form of the same subject. Credentials on one identity
  // must be written one at a time.
  resource githubIds 'federatedIdentityCredentials' = if (!empty(githubRepoIds)) {
    name: 'github-ids'
    dependsOn: [github]
    properties: {
      issuer: 'https://token.actions.githubusercontent.com'
      subject: 'repo:${githubRepoIds}:environment:${githubEnvironment}'
      audiences: ['api://AzureADTokenExchange']
    }
  }
}

output appId string = app.id
output appName string = app.name
output appClientId string = app.properties.clientId
output appPrincipalId string = app.properties.principalId
output ingestId string = ingest.id
output ingestClientId string = ingest.properties.clientId
output ingestPrincipalId string = ingest.properties.principalId
output ciId string = ci.id
output ciClientId string = ci.properties.clientId
output ciPrincipalId string = ci.properties.principalId
