// Data lake account (08 §8.1): flat namespace so versioning and soft delete
// work, Entra ID only (no keys or SAS), private endpoint only in steady state.

param location string
param tags object
param name string

@description('Blob containers. Access is granted per container in rbac.bicep.')
var containers = ['curated', 'reference', 'cache', 'qw-index', 'searches']

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: name
  location: location
  tags: tags
  kind: 'StorageV2'
  sku: { name: 'Standard_LRS' }
  properties: {
    accessTier: 'Hot'
    isHnsEnabled: false
    allowSharedKeyAccess: false
    defaultToOAuthAuthentication: true
    allowBlobPublicAccess: false
    allowCrossTenantReplication: false
    minimumTlsVersion: 'TLS1_2'
    supportsHttpsTrafficOnly: true
    publicNetworkAccess: 'Disabled'
    networkAcls: {
      defaultAction: 'Deny'
      bypass: 'None'
    }
  }
}

resource blobService 'Microsoft.Storage/storageAccounts/blobServices@2023-05-01' = {
  parent: account
  name: 'default'
  properties: {
    isVersioningEnabled: true
    // Three days of blob soft delete (maintainer, 12 October 2026, #245), down
    // from 14. It's the only recovery window for an old blob once deleted: the
    // version rules count from a version's creation, so a deleted curated/
    // blob older than 30 days loses its version at the next lifecycle pass.
    // But it's account-wide, so it also keeps every index split Quickwit
    // deletes after a merge or prune, at Hot prices (2 TB after the first
    // prune). Moving the index to its own account (#283) lets this go back up.
    deleteRetentionPolicy: { enabled: true, days: 3 }
    containerDeleteRetentionPolicy: { enabled: true, days: 14 }
    // Lets the tiering rules below go by last read, not creation (#245).
    lastAccessTimeTrackingPolicy: {
      enable: true
      name: 'AccessTimeTracking'
      trackingGranularityInDays: 1
      blobType: ['blockBlob']
    }
  }
}

resource blobContainers 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = [
  for c in containers: {
    parent: blobService
    name: c
    properties: { publicAccess: 'None' }
  }
]

// Versioning is account-wide; only curated/ and reference/ need history.
// searches/days/ and searches/import/ (the search log, kept indefinitely,
// ADR-0012) match no rule here, so nothing expires them.
resource lifecycle 'Microsoft.Storage/storageAccounts/managementPolicies@2023-05-01' = {
  parent: account
  name: 'default'
  // Last-access rules need the tracking policy in place first.
  dependsOn: [blobService]
  properties: {
    policy: {
      rules: [
        {
          // Cache entries are keyed by index version; versions publish at
          // most weekly, so anything older than 14 days is for a retired version.
          name: 'expire-response-cache'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['cache/'] }
            actions: {
              baseBlob: { delete: { daysAfterModificationGreaterThan: 14 } }
              version: { delete: { daysAfterCreationGreaterThan: 1 } }
            }
          }
        }
        {
          // The Quickwit metastore rewrites small JSON files often, and every
          // merge deletes splits, which versioning would otherwise keep (#245).
          // Deleted versions stay in the 3-day soft delete above, so the
          // metastore is still recoverable.
          name: 'prune-index-versions'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['qw-index/'] }
            actions: {
              version: { delete: { daysAfterCreationGreaterThan: 1 } }
            }
          }
        }
        {
          // Searches keep the live index Hot. A rollback index nobody reads
          // moves to Cool and comes back Hot on its first read. 14 days, not
          // 7: the searcher's footer cache can leave live splits unread for
          // days, and Cool bills a 30-day minimum and retrieval (#245).
          name: 'tier-index-cool'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['qw-index/'] }
            actions: {
              baseBlob: {
                tierToCool: { daysAfterLastAccessTimeGreaterThan: 14 }
                enableAutoTierToHotFromCool: true
              }
            }
          }
        }
        {
          // Read only by curation and releases. By last access, so a
          // rebuild's reads don't churn it. reference/, cache/ and searches/
          // stay Hot (tiny).
          name: 'tier-curated-cool'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['curated/'] }
            actions: {
              baseBlob: { tierToCool: { daysAfterLastAccessTimeGreaterThan: 7 } }
            }
          }
        }
        {
          name: 'prune-data-versions'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['curated/', 'reference/'] }
            actions: {
              version: { delete: { daysAfterCreationGreaterThan: 30 } }
            }
          }
        }
        {
          // The search log's staged batches (one blob per replica per flush)
          // are timed by their creation. The API copies each day, shuffled,
          // to searches/days/ an hour after it ends; these go a week after
          // they're written (STAGING_DAYS in crates/usnm-api/src/searchlog.rs),
          // then stay in soft delete for 3 days (ADR-0012 counts that).
          name: 'expire-search-staging'
          enabled: true
          type: 'Lifecycle'
          definition: {
            filters: { blobTypes: ['blockBlob'], prefixMatch: ['searches/staging/'] }
            actions: {
              baseBlob: { delete: { daysAfterModificationGreaterThan: 7 } }
              version: { delete: { daysAfterCreationGreaterThan: 1 } }
            }
          }
        }
      ]
    }
  }
}

output id string = account.id
output name string = account.name
output blobEndpoint string = account.properties.primaryEndpoints.blob
