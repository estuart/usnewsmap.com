// Scratch disk for the Quickwit writer in caj-usnm-ingest (08 §8.4): an NFS
// Azure Files share, mounted by the job, for the writer's write-ahead log,
// the splits it builds and the merges it runs. A Container Apps replica's
// own disk is 8 GiB by the documentation (about 19.5 GiB in practice), and
// the September 2026 releases filled it: indexers died with "No space left
// on device" and merges failed. The largest merge needs its inputs and its
// output on disk at once, about twice `split_num_docs_target` pages
// (infra/quickwit/pages-index.yaml): about 60 GB at 23 KB a page.
//
// NFS uses no keys, but no identity either: access is by network, through
// the private endpoint in the VNet, so anything in the Container Apps
// subnet can reach the share. That is an exception to ADR-0009, recorded
// there with its limits. Shared key access stays disabled. Container Apps
// can't mount NFS shares that require encryption in transit, so that is
// turned off for NFS; nothing leaves the VNet, and HTTPS stays required for
// the REST API.
//
// The same account can hold a second share, `qw-search` (`searchGiB`), for
// the search cluster experiment's `nfs` mode (#251; docs/operations.md,
// "Local-disk test"): an index copied onto it and served to a searcher as
// `file://`. The account, its private endpoint and its DNS zone are shared;
// either share can be on without the other.

param location string
param tags object
param name string
@description('Scratch share size in GiB; 0: no scratch share. 128 is the floor for the merge settings in pages-index.yaml (main.bicep keeps the setting at 0 or 128 and up): a merge of about 60 GB, plus the write-ahead log, the split cache and the split being built, about 70 GB, with room for pages that index larger. A smaller share needs a smaller split_num_docs_target.')
@minValue(0)
param sizeGiB int

@description('Size in GiB of the search experiment\'s share, qw-search; 0: none. Provisioned v2 SSD shares start at 32 GiB, with 3,000 IOPS and 100 MiB/s plus 1 IOPS and 0.1 MiB/s per GiB.')
@minValue(0)
param searchGiB int = 0
param vnetId string
param vnetName string
param peSubnetId string
param containerEnvName string

var shareName = 'qw-scratch'
var searchShareName = 'qw-search'
var zoneName = 'privatelink.file.${environment().suffixes.storage}'

resource account 'Microsoft.Storage/storageAccounts@2025-01-01' = {
  name: name
  location: location
  tags: tags
  kind: 'FileStorage'
  // Provisioned v2 SSD: billed by the GiB provisioned (about $0.10/GiB a
  // month), with baseline IOPS and throughput included.
  sku: { name: 'PremiumV2_LRS' }
  properties: {
    allowSharedKeyAccess: false
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

resource files 'Microsoft.Storage/storageAccounts/fileServices@2025-01-01' = {
  parent: account
  name: 'default'
  properties: {
    protocolSettings: { nfs: { encryptionInTransit: { required: false } } }
  }
}

resource share 'Microsoft.Storage/storageAccounts/fileServices/shares@2025-01-01' = if (sizeGiB > 0) {
  parent: files
  name: shareName
  properties: {
    enabledProtocols: 'NFS'
    // The job's init container runs as root to hand a directory to the
    // pipeline's user (uid 10001).
    rootSquash: 'NoRootSquash'
    shareQuota: sizeGiB
  }
}

resource searchShare 'Microsoft.Storage/storageAccounts/fileServices/shares@2025-01-01' = if (searchGiB > 0) {
  parent: files
  name: searchShareName
  properties: {
    enabledProtocols: 'NFS'
    // The searcher's init container runs as root to hand a directory to
    // the node's user (uid 10001), as the ingest job's does.
    rootSquash: 'NoRootSquash'
    shareQuota: max(searchGiB, 32)
  }
}

resource zone 'Microsoft.Network/privateDnsZones@2024-06-01' = {
  name: zoneName
  location: 'global'
  tags: tags
}

resource link 'Microsoft.Network/privateDnsZones/virtualNetworkLinks@2024-06-01' = {
  parent: zone
  name: '${vnetName}-link'
  location: 'global'
  tags: tags
  properties: {
    registrationEnabled: false
    virtualNetwork: { id: vnetId }
  }
}

resource pe 'Microsoft.Network/privateEndpoints@2024-05-01' = {
  name: 'pe-usnm-file'
  location: location
  tags: tags
  properties: {
    subnet: { id: peSubnetId }
    privateLinkServiceConnections: [
      {
        name: 'pe-usnm-file'
        properties: {
          privateLinkServiceId: account.id
          groupIds: ['file']
        }
      }
    ]
  }
}

resource zoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-05-01' = {
  parent: pe
  name: 'default'
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'file'
        properties: { privateDnsZoneId: zone.id }
      }
    ]
  }
}

resource containerEnv 'Microsoft.App/managedEnvironments@2025-01-01' existing = {
  name: containerEnvName
}

// The share as the environment's jobs mount it.
resource envStorage 'Microsoft.App/managedEnvironments/storages@2025-01-01' = if (sizeGiB > 0) {
  parent: containerEnv
  name: 'ingest-scratch'
  dependsOn: [share, zoneGroup, link]
  properties: {
    nfsAzureFile: {
      server: '${account.name}.file.${environment().suffixes.storage}'
      shareName: '/${account.name}/${shareName}'
      accessMode: 'ReadWrite'
    }
  }
}

// The search share as the searcher mounts it.
resource searchEnvStorage 'Microsoft.App/managedEnvironments/storages@2025-01-01' = if (searchGiB > 0) {
  parent: containerEnv
  name: 'search-share'
  dependsOn: [searchShare, zoneGroup, link]
  properties: {
    nfsAzureFile: {
      server: '${account.name}.file.${environment().suffixes.storage}'
      shareName: '/${account.name}/${searchShareName}'
      accessMode: 'ReadWrite'
    }
  }
}

output accountName string = account.name
output envStorageName string = sizeGiB > 0 ? envStorage.name : ''
output searchEnvStorageName string = searchGiB > 0 ? searchEnvStorage.name : ''
