// Experimental Quickwit search cluster (#238, #239; docs/operations.md,
// "Search cluster experiment"). Deployed only with `searchCluster` on; with
// it off main.bicep skips this module, and the stack deletes what it made.
//
// - Nodes: one app per member, `ca-usnm-qw-{i}`, each exactly one replica
//   (min = max = 1) on the Consumption profile. Node 0 is the only metastore
//   (the single writer of the file-backed metastore at `azure://qw-cluster`),
//   the control plane, the janitor, an indexer and a searcher, and the root
//   that searches enter through (internal ingress on 7280, for the bench
//   job). Nodes 1 to `indexers - 1` are indexers and searchers, the rest
//   searchers only. Nothing outside the environment can reach them.
// - Addresses (#239): an app's name resolves to its service IP, which carries
//   only its ingress port, so Quickwit's UDP gossip and gRPC go to replica IPs.
//   `usnm-qwcluster node` (crates/usnm-ingest/src/cluster/node.rs) advertises
//   the replica's IP, writes it to `qw-bench/seeds/qw-{i}.json`, reads the
//   other nodes' entries as peer seeds and then becomes Quickwit (exec). A
//   node that restarts at a new IP registers again and joins through the
//   others; they learn its new address by gossip.
// - Bench job `caj-usnm-qwbench-{env}` (manual): `usnm-qwcluster` builds the
//   1% sample from `curated` (read only), loads it into the cluster, runs the
//   benchmark searches against node 0, and logs and stores its reports in
//   `qw-bench/runs/`.
//
// Storage, Entra only (ADR-0009): two containers of their own in the data
// account; nothing here can write `qw-index`, `reference` or `curated`.
// - `qw-cluster`: the cluster's metastore and splits. Quickwit 0.9 can only
//   use a system-assigned identity in Container Apps (08 §8.2), so each node's
//   system identity gets Blob Data Contributor on this container alone.
// - `qw-bench`: the seed registry, the sample documents and the reports.
//   Blob Data Contributor for `id-usnm-qwnode-{env}` (the nodes' own code:
//   seeds) and `id-usnm-qwbench-{env}` (the bench job), both user-assigned.
// - The bench identity reads `curated` and `reference` (Blob Data Reader).
// Both identities pull the ingest image (AcrPull): it has Quickwit and the
// `usnm-qwcluster` binary.
//
// Version comparison (#251; docs/operations.md, "Quickwit version
// comparison"): with `compareImages`, one standalone searcher per image,
// `ca-usnm-qws-{i}`, each the API sidecar's shape (searcher and metastore,
// polling the file-backed metastore in `qw-cluster`), with a cluster id of
// its own and no seeds, so they never join each other or the cluster. Each
// searches the same splits, the cluster's indexes, with Blob Data *Reader*
// on `qw-cluster` (as the sidecar has on `qw-index`): neither can change
// them. Load the indexes first, then set `nodes` to 0 so nothing writes
// while they search. An image is an ingest image built on another Quickwit
// (Dockerfile.ingest's QUICKWIT_IMAGE); the node wrapper writes the
// pre-0.9 searcher config when `quickwit --version` is below 0.9.
//
// Threads: every node and comparison searcher runs one main-runtime and one
// search-pool thread per vCPU unless `runtimeThreads`/`searchThreads` say
// otherwise, as the API sidecar does (4 and 4 at 3.75 vCPU).

param location string
param tags object
@description('Environment name, for the job and identity names.')
param nameSuffix string
param environmentId string
@description('The ingest image (Quickwit 0.9.1 with usnm-ingest and usnm-qwcluster): the cluster\'s nodes and the bench job.')
param image string
param registryServer string
param registryName string
param storageAccountName string
param storageBlobEndpoint string
@description('The archival account\'s Blob endpoint (USNM_ARCHIVE_ACCOUNT), for packaged sample sets; empty: none.')
param archiveBlobEndpoint string = ''
@description('Cluster members (apps ca-usnm-qw-0 to -{nodes - 1}). 0 runs no node and keeps the containers and the bench job: the idle state between runs.')
@minValue(0)
@maxValue(4)
param nodes int = 1
@description('How many of the nodes, from node 0, run an indexer. The rest search only.')
@minValue(1)
@maxValue(4)
param indexers int = 1
@description('vCPU per node, with 2 GiB of memory per vCPU (the Consumption profile\'s ratio). 2 is the API sidecar\'s size.')
@minValue(1)
@maxValue(4)
param nodeVcpu int = 2

@description('searcher.max_num_concurrent_split_searches on every node; 0 keeps the sidecar\'s value (infra/quickwit/searcher.yaml).')
@minValue(0)
param splitSearches int = 0

@description('Main runtime threads per node and comparison searcher (QW_TOKIO_RUNTIME_NUM_THREADS); 0: one per vCPU, as the API sidecar runs.')
@minValue(0)
param runtimeThreads int = 0

@description('Search pool threads per node and comparison searcher (RAYON_NUM_THREADS); 0: one per vCPU, as the API sidecar runs.')
@minValue(0)
param searchThreads int = 0

@description('Version comparison (#251): one standalone searcher ca-usnm-qws-{i} per image, each nodeVcpu vCPU, reading the cluster\'s indexes. Empty: none.')
@maxLength(4)
param compareImages array = []

@description('Local-disk test (#251): one standalone 0.9.1 searcher, ca-usnm-qwl-0, reading the cluster\'s indexes from Blob (blob) or from a copy of localIndex on an NFS share (nfs), on Consumption, or through a split cache on its disk (cache) or from a copy on its disk (copy), on the dedicated profile. Empty: none.')
@allowed(['', 'blob', 'cache', 'copy', 'nfs'])
param localMode string = ''

@description('The dedicated workload profile the cache and copy modes run on (the environment\'s E4); empty: those modes deploy nothing.')
param localProfile string = ''

@description('The environment storage of the NFS share the nfs mode mounts (infra/modules/ingest-scratch.bicep, qw-search); empty: that mode deploys nothing.')
param localNfsStorage string = ''

@description('The index the local-disk searcher copies (localMode copy and nfs).')
param localIndex string = ''

@description('The local-disk searcher\'s split cache in GiB (localMode cache).')
@minValue(1)
param localCacheGib int = 40

var clusterContainer = 'qw-cluster'
var benchContainer = 'qw-bench'
// What node 0 adds to the indexer and searcher every node may run.
var rootServices = ['metastore', 'control_plane', 'janitor']
// Inside the environment: node 0's internal ingress, by app name.
var rootUrl = 'http://ca-usnm-qw-0'

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: storageAccountName

  resource blobs 'blobServices' existing = {
    name: 'default'
  }
}

resource cluster 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: account::blobs
  name: clusterContainer
  properties: { publicAccess: 'None' }
}

resource bench 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: account::blobs
  name: benchContainer
  properties: { publicAccess: 'None' }
}

resource curated 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' existing = {
  parent: account::blobs
  name: 'curated'
}

resource reference 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' existing = {
  parent: account::blobs
  name: 'reference'
}

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' existing = {
  name: registryName
}

resource nodeIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: 'id-usnm-qwnode-${nameSuffix}'
  location: location
  tags: tags
}

resource benchIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: 'id-usnm-qwbench-${nameSuffix}'
  location: location
  tags: tags
}

var blobReader = '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'
var blobContributor = 'ba92f5b4-2d11-453d-a403-e96b0029c9fe'
var acrPull = '7f951dda-4ed3-4680-a7ca-43fe172d538d'

// Both identities, by position: 0 the nodes', 1 the bench job's.
var identityIds = [nodeIdentity.id, benchIdentity.id]

resource pulls 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for (id, i) in identityIds: {
    scope: registry
    name: guid(registry.id, id, acrPull)
    properties: {
      principalId: i == 0 ? nodeIdentity.properties.principalId : benchIdentity.properties.principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', acrPull)
    }
  }
]

resource benchWriters 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for (id, i) in identityIds: {
    scope: bench
    name: guid(bench.id, id, blobContributor)
    properties: {
      principalId: i == 0 ? nodeIdentity.properties.principalId : benchIdentity.properties.principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobContributor)
    }
  }
]

resource curatedReader 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: curated
  name: guid(curated.id, benchIdentity.id, blobReader)
  properties: {
    principalId: benchIdentity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
  }
}

resource referenceReader 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: reference
  name: guid(reference.id, benchIdentity.id, blobReader)
  properties: {
    principalId: benchIdentity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
  }
}

// At most 4 nodes, so `indexers` past `nodes` just means all of them.
var nodeCpu = json(string(nodeVcpu))
var nodeMemory = '${nodeVcpu * 2}Gi'
// `usnm-qwcluster node` flags every node and comparison searcher shares:
// its vCPUs, then threads and concurrent split searches when set.
var tuningArgs = concat(
  ['--cpus', string(nodeVcpu)],
  runtimeThreads > 0 ? ['--runtime-threads', string(runtimeThreads)] : [],
  searchThreads > 0 ? ['--search-threads', string(searchThreads)] : [],
  splitSearches > 0 ? ['--split-searches', string(splitSearches)] : []
)
// Warnings, and the cluster's membership and the indexing pipelines at
// info: enough to follow joins, restarts and merges inside a dev
// workspace's 150 MB daily log cap, and no search text (09 §9.4.2).
var nodeRustLog = 'warn,quickwit_cluster=info,quickwit_serve=info,quickwit_serve::search_api=warn,quickwit_indexing::actors::merge_pipeline=info'
var nodeProbes = [
  {
    type: 'Startup'
    httpGet: { path: '/health/livez', port: 7280 }
    periodSeconds: 5
    failureThreshold: 60
  }
  {
    type: 'Liveness'
    httpGet: { path: '/health/livez', port: 7280 }
    periodSeconds: 30
  }
]

resource nodeApps 'Microsoft.App/containerApps@2024-03-01' = [
  for i in range(0, nodes): {
    name: 'ca-usnm-qw-${i}'
    location: location
    tags: tags
    // The user-assigned identity pulls the image and writes the seed entry;
    // Quickwit uses the system-assigned one (see above).
    dependsOn: [pulls, benchWriters]
    identity: {
      type: 'SystemAssigned,UserAssigned'
      userAssignedIdentities: { '${nodeIdentity.id}': {} }
    }
    properties: {
      environmentId: environmentId
      workloadProfileName: 'Consumption'
      configuration: {
        activeRevisionsMode: 'Single'
        registries: [{ server: registryServer, identity: nodeIdentity.id }]
        // Node 0 only: the root, for the bench job inside the environment.
        // Plain HTTP on the environment's internal network; nothing outside
        // it can connect.
        ingress: i == 0
          ? {
              external: false
              targetPort: 7280
              transport: 'http'
              allowInsecure: true
            }
          : null
      }
      template: {
        containers: [
          {
            name: 'qwnode'
            image: image
            command: ['/usr/local/bin/usnm-qwcluster']
            args: concat([
              'node'
              '--node-id'
              'qw-${i}'
              '--services'
              join(
                concat(i == 0 ? rootServices : [], i < indexers ? ['indexer'] : [], ['searcher']),
                ','
              )
              '--registry'
              '${storageBlobEndpoint}${benchContainer}'
              '--metastore'
              'azure://${clusterContainer}'
              '--index-root'
              'azure://${clusterContainer}'
              '--storage-account'
              storageAccountName
            ], tuningArgs)
            resources: { cpu: nodeCpu, memory: nodeMemory }
            env: [
              // Selects id-usnm-qwnode for the seed registry; the wrapper
              // removes it before Quickwit starts.
              { name: 'AZURE_CLIENT_ID', value: nodeIdentity.properties.clientId }
              { name: 'RUST_LOG', value: nodeRustLog }
            ]
            probes: nodeProbes
          }
        ]
        scale: { minReplicas: 1, maxReplicas: 1 }
      }
    }
  }
]

// Splits and the metastore: each node's system identity (Quickwit's).
resource clusterWriters 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for i in range(0, nodes): {
    scope: cluster
    name: guid(cluster.id, nodeApps[i].id, blobContributor)
    properties: {
      principalId: nodeApps[i].identity.principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobContributor)
    }
  }
]

// Local-disk test (#251): one standalone searcher, its splits read
// - blob: from Blob, as the API's sidecar reads them;
// - nfs: from a copy of the index on an NFS Azure Files share (qw-search,
//   provisioned v2 SSD, through the private endpoint) mounted at
//   /mnt/qwsearch, served as file://. The copy is made once, at the first
//   start, and kept on the share;
// - cache: through Quickwit's split cache on the replica's disk;
// - copy: from a copy on the replica's disk, served as file://.
// blob and nfs run on Consumption at the sidecar's 3.75 vCPU / 7.5 GiB (with
// nfs's init container, 4 / 8 in all, the Consumption maximum): the shape
// production searches with. cache and copy need more disk than Consumption's
// 8 GiB a replica, so they run on the dedicated E4 profile, whose ephemeral
// storage is about 80 GiB (Microsoft, September 2026, in
// microsoft/azure-container-apps#1779): 3.25 vCPU / 24 GiB, an E4 replica's
// most (infra/modules/ingestjobs.bicep), the disk under /work, emptied by a
// restart.
//
// The E4 profile has one node: a new revision there can't start while the
// old one holds it, and the old one keeps the traffic. In October 2026 the
// cache and copy runs were served that way by the blob revision. Between two
// E4 modes clear the mode and provision first; the bench's --variant checks
// the mode it reaches (crates/usnm-ingest/src/cluster/bench.rs, node_mode).
var localDedicated = localMode == 'cache' || localMode == 'copy'
var localOn = !empty(localMode) && (!localDedicated || !empty(localProfile)) && (localMode != 'nfs' || !empty(localNfsStorage))
var nfsMount = '/mnt/qwsearch'
// The node's user (Dockerfile.ingest) owns this directory on the share.
var nfsDir = '${nfsMount}/usnm'
var localCopyFrom = '${storageBlobEndpoint}${clusterContainer}'
var localModeArgs = localMode == 'cache'
  ? ['--split-cache-gib', string(localCacheGib)]
  : (localMode == 'copy'
      ? ['--local-copy', localIndex, '--local-copy-from', localCopyFrom, '--local-dir', '/work/index']
      : (localMode == 'nfs'
          ? ['--local-copy', localIndex, '--local-copy-from', localCopyFrom, '--local-dir', '${nfsDir}/index']
          : []))
var localResources = localDedicated ? { cpu: json('3.25'), memory: '24Gi' } : { cpu: json('3.75'), memory: '7.5Gi' }
// Root, to hand the share's directory to uid 10001 (as the ingest job's
// scratch-owner does); Microsoft's registry, no sign-in.
var nfsOwnerImage = 'mcr.microsoft.com/azurelinux/busybox:1.36'

resource localApp 'Microsoft.App/containerApps@2024-03-01' = if (localOn) {
  name: 'ca-usnm-qwl-0'
  location: location
  tags: tags
  dependsOn: [pulls]
  identity: {
    type: 'SystemAssigned,UserAssigned'
    userAssignedIdentities: { '${nodeIdentity.id}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: localDedicated ? localProfile : 'Consumption'
    configuration: {
      activeRevisionsMode: 'Single'
      registries: [{ server: registryServer, identity: nodeIdentity.id }]
      ingress: {
        external: false
        targetPort: 7280
        transport: 'http'
        allowInsecure: true
      }
    }
    template: {
      initContainers: localMode == 'nfs'
        ? [
            {
              name: 'share-owner'
              image: nfsOwnerImage
              command: ['/bin/sh']
              args: ['-c', 'mkdir -p ${nfsDir} && chown 10001:10001 ${nfsDir}']
              resources: { cpu: json('0.25'), memory: '0.5Gi' }
              volumeMounts: [{ volumeName: 'search-share', mountPath: nfsMount }]
            }
          ]
        : null
      containers: [
        {
          name: 'qwnode'
          image: image
          command: ['/usr/local/bin/usnm-qwcluster']
          args: concat([
            'node'
            '--node-id'
            'qwl-0'
            '--standalone'
            '--services'
            'searcher,metastore'
            '--metastore'
            'azure://${clusterContainer}#polling_interval=30s'
            '--index-root'
            'azure://${clusterContainer}'
            '--storage-account'
            storageAccountName
            '--cpus'
            '4'
            '--runtime-threads'
            string(runtimeThreads > 0 ? runtimeThreads : 4)
            '--search-threads'
            string(searchThreads > 0 ? searchThreads : 4)
          ], splitSearches > 0 ? ['--split-searches', string(splitSearches)] : [], localModeArgs)
          resources: localResources
          env: [
            // The wrapper's own lines too: the disk, the copy.
            { name: 'RUST_LOG', value: '${nodeRustLog},usnm_ingest=info' }
          ]
          volumeMounts: localMode == 'nfs' ? [{ volumeName: 'search-share', mountPath: nfsMount }] : null
          // A first copy of a test index takes minutes before Quickwit
          // listens: up to an hour (Container Apps allows at most 240
          // failures; 240 at 15 s).
          probes: [
            {
              type: 'Startup'
              httpGet: { path: '/health/livez', port: 7280 }
              periodSeconds: 15
              failureThreshold: 240
            }
            {
              type: 'Liveness'
              httpGet: { path: '/health/livez', port: 7280 }
              periodSeconds: 30
            }
          ]
        }
      ]
      volumes: localMode == 'nfs' ? [{ name: 'search-share', storageType: 'NfsAzureFile', storageName: localNfsStorage }] : null
      scale: { minReplicas: 1, maxReplicas: 1 }
    }
  }
}

resource localReader 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (localOn) {
  scope: cluster
  name: guid(cluster.id, 'ca-usnm-qwl-0', blobReader)
  properties: {
    principalId: localApp!.identity.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
  }
}

// Version comparison (#251): standalone searchers over the cluster's indexes.
resource compareApps 'Microsoft.App/containerApps@2024-03-01' = [
  for (compareImage, i) in compareImages: {
    name: 'ca-usnm-qws-${i}'
    location: location
    tags: tags
    dependsOn: [pulls]
    // The user-assigned identity only pulls the image; Quickwit reads with
    // the system-assigned one.
    identity: {
      type: 'SystemAssigned,UserAssigned'
      userAssignedIdentities: { '${nodeIdentity.id}': {} }
    }
    properties: {
      environmentId: environmentId
      workloadProfileName: 'Consumption'
      configuration: {
        activeRevisionsMode: 'Single'
        registries: [{ server: registryServer, identity: nodeIdentity.id }]
        // The bench job's way in, inside the environment only.
        ingress: {
          external: false
          targetPort: 7280
          transport: 'http'
          allowInsecure: true
        }
      }
      template: {
        containers: [
          {
            name: 'qwnode'
            image: compareImage
            command: ['/usr/local/bin/usnm-qwcluster']
            args: concat([
              'node'
              '--node-id'
              'qws-${i}'
              '--standalone'
              '--services'
              'searcher,metastore'
              // Read-only, polled, as the API sidecar reads qw-index.
              '--metastore'
              'azure://${clusterContainer}#polling_interval=30s'
              '--index-root'
              'azure://${clusterContainer}'
              '--storage-account'
              storageAccountName
            ], tuningArgs)
            resources: { cpu: nodeCpu, memory: nodeMemory }
            env: [
              { name: 'RUST_LOG', value: nodeRustLog }
            ]
            probes: nodeProbes
          }
        ]
        scale: { minReplicas: 1, maxReplicas: 1 }
      }
    }
  }
]

// Read only: a comparison searcher can't change the splits or the metastore.
resource compareReaders 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for (compareImage, i) in compareImages: {
    scope: cluster
    name: guid(cluster.id, compareApps[i].id, blobReader)
    properties: {
      principalId: compareApps[i].identity.principalId
      principalType: 'ServicePrincipal'
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
    }
  }
]

resource benchJob 'Microsoft.App/jobs@2025-01-01' = {
  name: 'caj-usnm-qwbench-${nameSuffix}'
  location: location
  tags: tags
  dependsOn: [pulls, benchWriters, curatedReader, referenceReader]
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${benchIdentity.id}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: benchIdentity.id }]
      triggerType: 'Manual'
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
      // The sample reads the whole curated lake once (about 1 to 2 h).
      replicaTimeout: 21600
      replicaRetryLimit: 0
    }
    template: {
      containers: [
        {
          name: 'qwbench'
          image: image
          command: ['/usr/local/bin/usnm-qwcluster']
          // scripts/qwcluster.sh starts it with each step's arguments.
          args: ['members']
          resources: { cpu: json('4.0'), memory: '8Gi' }
          env: concat([
            { name: 'AZURE_CLIENT_ID', value: benchIdentity.properties.clientId }
            { name: 'USNM_CURATED_URL', value: '${storageBlobEndpoint}curated' }
            { name: 'USNM_REFERENCE_URL', value: '${storageBlobEndpoint}reference' }
            { name: 'USNM_QWBENCH_URL', value: '${storageBlobEndpoint}${benchContainer}' }
            { name: 'USNM_QWCLUSTER_URL', value: rootUrl }
            { name: 'USNM_QWCLUSTER_INDEX_ROOT', value: 'azure://${clusterContainer}' }
            { name: 'RUST_LOG', value: 'info' }
          ], empty(archiveBlobEndpoint) ? [] : [
            { name: 'USNM_ARCHIVE_RAW_URL', value: '${archiveBlobEndpoint}raw' }
            { name: 'USNM_ARCHIVE_SETS_URL', value: '${archiveBlobEndpoint}sets' }
          ])
        }
      ]
    }
  }
}

output nodeApps array = [for i in range(0, nodes): nodeApps[i].name]
output compareApps array = [for (compareImage, i) in compareImages: compareApps[i].name]
output localUrl string = localOn ? 'http://ca-usnm-qwl-0' : ''
// Each comparison searcher's root for `bench --cluster`, inside the environment.
output compareUrls array = [for (compareImage, i) in compareImages: 'http://${compareApps[i].name}']
output benchJobName string = benchJob.name
output rootUrl string = rootUrl
output benchPrincipalId string = benchIdentity.properties.principalId
