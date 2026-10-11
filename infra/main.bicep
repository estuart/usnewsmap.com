// US News Map: lean hosting profile (08 §8.1, ADR-0006). No IaaS.
//
// Deployed as the deployment stack usnm-{env} by scripts/bootstrap.sh, with
// parameters from infra/main.bicepparam. Creates the project
// resource group and the empty Spot resource group the backfill launcher
// will use, then the platform inside the project group.

targetScope = 'subscription'

import { guardrailPolicyNames } from 'guardrails.bicep'

@minLength(1)
@maxLength(16)
@description('Environment name, e.g. dev or prod (AZURE_ENV_NAME in the environment\'s settings).')
param environmentName string

@description('Region; East US 2 has ACI Spot (preview).')
param location string = 'eastus2'

@description('A public API image, used only while useAcr is off. Empty (the default) skips the API until CI has pushed to the registry and useAcr is on.')
param apiImage string = ''

@description('Extra origins allowed by API CORS and the tiles account. The app\'s own hostname and, with dnsZoneName, the domain and its www are always allowed.')
param allowedOrigins array = []

@description('0 lets dev scale to zero; production keeps one warm replica.')
param apiMinReplicas int = toLower(environmentName) == 'prod' ? 1 : 0

@description('fixtures: the API serves its baked synthetic data. quickwit: reference data from Blob and a read-only Quickwit sidecar over qw-index (needs published indexes).')
@allowed(['fixtures', 'quickwit'])
param searchBackend string = 'fixtures'

@description('Pull images from the private registry (08 §8.6). Turn on once CI has pushed them there.')
param useAcr bool = false

@description('Image tag CI pushed to the registry (main or a commit sha).')
param imageTag string = 'main'

@description('GitHub repository that deploys this environment, as owner/name. Its GitHub Environment of the same name may use the CI identity.')
param githubRepo string = 'tgoodyear/usnewsmap.com'

@description('The same repository as `owner@ownerId/name@repoId` (GitHub\'s immutable-ID OIDC subject format); scripts/bootstrap.sh looks it up.')
param githubRepoIds string = 'tgoodyear@116683/usnewsmap.com@1389862972'

@description('Create the ingest and backfill jobs (needs useAcr: the ingest image is only in the private registry).')
param ingestJobs bool = false

@description('Weekly ingest schedule, UTC cron (e.g. "17 3 * * 1"). Empty: run the job manually.')
param ingestCron string = ''

@description('Parallel curation workers in the backfill job.')
param backfillWorkers int = 4

@description('Make every ingest run a full release (a new, merged base). For a one-off rebuild; clear it afterwards.')
param ingestFull bool = false

@description('Index American Stories\' text with LoC\'s (#218): the first ingest run with it builds a full base. Keep it on afterwards; turning it off makes the next release publish a version whose searches leave the text out (docs/operations.md, "American Stories\' text").')
param americanStories bool = false

@description('Search American Stories\' text on a version built with it (05 §5.5.4). False makes the API search LoC\'s text alone, whatever current.json says, without a release; the provision restarts the API, which warms up again (docs/operations.md, "American Stories\' text").')
param americanStoriesSearch bool = true

@description('Search the Congress pages (#229) on a version whose current.json names a Congress index set. False makes the API leave them unloaded and answer source=congress with 422, without a release; the provision restarts the API (docs/configuration.md, USNM_CONGRESS_SEARCH).')
param congressSearch bool = true

@description('Give the pages LoC ships without text (the Japanese pages our OCR reads, 04 §4.8) a main-index document with the Latin-script text our OCR read on them, so English searches reach their English ads and sections (#203). The next release that builds a main index adds them; clearing it drops them at the next full release (docs/operations.md, "Japanese OCR: mixed pages and English text").')
param jaLatin bool = false

@description('Weigh our own OCR from curated/ocr-local (the on-prem nodes\' output, #296) against LoC\'s text: on the pages of the batches a release indexes, ours replaces LoC\'s where rule v1 prefers it (12 §12.5 Step 9). Published batches take it at the next full release (docs/operations.md, "Our OCR in releases").')
param ocrLocal bool = false
@description('Lay the next full rebuild\'s base out by decade (#123): every split holds one decade, so a date-limited search skips the others. Only a full base takes it; deltas follow the published version, and a full rebuild without it lays nothing out by decade (docs/operations.md, "Full rebuild").')
param partitionDecade bool = false

@description('Search the next full rebuild\'s texts, LoC\'s and American Stories\', in one field (#283, 05 §5.5.7), so a search looks each word up once. Only a full base takes it; deltas follow the published version, and a full rebuild without it searches a field per text again (docs/operations.md, "Full rebuild").')
param singleTextField bool = false

@description('Add a memory-optimized E4 workload profile (4 vCPU / 32 GiB, no minimum nodes) for full index rebuilds (#172). The environment pays the Dedicated plan management fee while it exists: turn it on for a rebuild, off afterwards (docs/operations.md, "Full rebuild").')
param dedicatedProfile bool = false

@description('Run the ingest job on the E4 profile (needs dedicatedProfile). Turn this off and provision before turning dedicatedProfile off: a profile in use can\'t be removed.')
param ingestOnDedicated bool = false

@description('Deploy the experimental Quickwit search cluster (#239): node apps ca-usnm-qw-{i}, the bench job caj-usnm-qwbench-{env} and their containers qw-cluster and qw-bench (docs/operations.md, "Search cluster experiment"). Needs useAcr; ignored in prod. Turning it off deletes all of it, the two containers included.')
param searchCluster bool = false

@description('Search cluster members (0 to 4). 0 keeps the cluster\'s containers and bench job but runs no node.')
@minValue(0)
@maxValue(4)
param searchClusterNodes int = 1

@description('How many search cluster nodes, from node 0, also index (1 to 4); the others only search.')
@minValue(1)
@maxValue(4)
param searchClusterIndexers int = 1

@description('vCPU per search cluster node (1 to 4), with 2 GiB per vCPU. 2 matches the API\'s Quickwit sidecar.')
@minValue(1)
@maxValue(4)
param searchClusterNodeVcpu int = 2

@description('Search cluster experiment: searcher.max_num_concurrent_split_searches per node (0: the sidecar\'s).')
param searchClusterSplitSearches int = 0

@description('Search cluster experiment: main runtime threads per node (0: one per vCPU, as the API sidecar runs).')
@minValue(0)
param searchClusterRuntimeThreads int = 0

@description('Search cluster experiment: search pool threads per node (0: one per vCPU, as the API sidecar runs).')
@minValue(0)
param searchClusterSearchThreads int = 0

@description('Quickwit version comparison (#251): ingest image tags, comma separated, each run as a standalone searcher ca-usnm-qws-{i} over the search cluster\'s indexes (searchClusterNodeVcpu each; docs/operations.md, "Quickwit version comparison"). Empty: none.')
param searchCompareTags string = ''

@description('Local-disk test (#251): a standalone 0.9.1 searcher, ca-usnm-qwl-0, reading the search cluster\'s indexes from Blob (blob) or a copy of searchLocalIndex on an NFS share (nfs), on Consumption at the API sidecar\'s 3.75 vCPU / 7.5 GiB, or through a split cache (cache) or from a copy (copy) on its own disk, on the E4 profile (needs dedicatedProfile) for the disk (docs/operations.md, "Local-disk test"). Empty: none.')
@allowed(['', 'blob', 'cache', 'copy', 'nfs'])
param searchLocalMode string = ''

@description('Local-disk test: the index the copy mode copies.')
param searchLocalIndex string = ''

@description('Local-disk test: the split cache in GiB (cache mode).')
@minValue(1)
param searchLocalCacheGib int = 40

@description('Local-disk test, nfs mode: size in GiB of the NFS share qw-search, in the ingest scratch share\'s account (provisioned v2 SSD, about $0.10/GiB a month; 32 GiB at least). 64 holds the 1% index twice over with American Stories\' fields.')
@minValue(32)
param searchNfsGiB int = 64

@description('Retain every batch archive curation downloads, byte for byte, in a `raw` container of this environment\'s own data account (Cold tier), and curate from it instead of LoC when it holds the listed archive. It goes with the environment: turning it off deletes the container. For a set that outlives environments use archiveAccountId. Production keeps none (ADR-0006).')
param retainRaw bool = false

@description('The archival account (infra/archive/, scripts/archive-store.sh), as a resource id: curation keeps every LoC archive and listing it downloads in its `raw` container and reads archives back from it instead of LoC, through a private endpoint of this environment; the search cluster bench job reads `raw` and writes `sets`. Takes precedence over retainRaw. Empty: not used. Clearing it removes only this environment\'s endpoint and roles, never the account or its data.')
param archiveAccountId string = ''

@description('Backfill schedule, UTC cron (e.g. "0 9 2-4 10 *" while a backfill lasts). Empty: run the job manually.')
param backfillCron string = ''

@description('Size in GiB of the NFS share the ingest job\'s Quickwit writer works on (08 §8.4): 128 or more for the merge settings in pages-index.yaml; 0: the replica\'s own disk, too small to merge large indexes.')
param ingestScratchGiB int = 128

@description('Seconds an ingest release waits for its new index\'s merges before failing without publishing (08 §8.4).')
param ingestMergeTimeoutSecs int = 14400

@description('Cosmos DB free tier (one per subscription). False makes the account serverless.')
param cosmosFreeTier bool = true

@description('Log Analytics daily cap in GB; empty: 1 in prod, 0.15 elsewhere. Raise it for bulk work in a non-prod environment (the search cluster experiment writes thousands of blobs, each a log record), and clear it afterwards.')
param logDailyCapGb string = ''

@description('Comma-separated alert and budget recipients. Budget and alerts are skipped when empty.')
param alertEmails string = ''


@description('Budget start month, YYYY-MM-01. Set once per environment (a budget\'s start date cannot change); the budget is skipped when empty.')
param budgetStartDate string = ''


@description('Comma-separated Entra object ids (people or groups) that may read the search log in the searches container (scripts/searches.sh).')
param searchLogReaders string = ''

@description('Deploy Azure AI Document Intelligence for OCR of Japanese pages (#128).')
param ocr bool = false

@description('Document Intelligence tier: F0 (500 pages a month free) or S0.')
@allowed(['F0', 'S0'])
param ocrSku string = 'F0'

@description('Entra object ids (people or groups) that may call Document Intelligence, comma separated.')
param ocrUsers string = ''

@description('Deploy the Japanese OCR job (caj-usnm-jaocr, #128). Needs the usnewsmap-ja-ocr image in the registry (CI publishes it from main).')
param jaOcrJob bool = false

@description('Parallel replicas of the Japanese OCR job.')
@minValue(1)
@maxValue(8)
param jaOcrReplicas int = 2

@description('Deploy the on-prem OCR exchange account (stusnmo<suffix>, #296, ADR-0015): work queues and an output container for OCR nodes outside Azure, behind a firewall that allows nodeIps and the Container Apps subnet.')
param ocrNodes bool = false

@description('Public IPv4 addresses or CIDR ranges the OCR nodes connect from, comma separated (the operator\'s home IP). Only in the environment\'s settings, never in the repository.')
param ocrNodeIps string = ''

@description('Object id of the OCR nodes\' Entra security group, usnm-ocr-nodes-<env> (scripts/ocr-node/group.sh creates it and saves its id). The group gets the node roles on the exchange account and Monitoring Metrics Publisher on the nodes\' own Application Insights component, usnm-ocr-appi-<env>; onboard.sh adds each node\'s Arc identity to it.')
param ocrNodeGroupId string = ''

@description('Schedule for the OCR pickup job usnm-ocrsync-caj-<env> (UTC cron; with ocrNodes and the ingest jobs, 12 §12.5 Step 8). `manual`: no schedule.')
param ocrSyncCron string = '17 * * * *'

@description('The OCR seeder\'s settings for the pickup job (12 §12.5 Step 10), by environment variable name (USNM_OCR_SEED_* and USNM_OCR_BULK_*; docs/operations.md). Seeding stays off while USNM_OCR_SEED_PLAN is empty (ADR-0015 decision 7).')
param ocrSeed object = {}

@description('Entra object ids (people or groups), comma separated, given Key Vault Secrets Officer on the OCR nodes\' Key Vault, usnmocrkv<env><suffix> (with ocrNodes): the operators who set and read the fleet\'s secrets, such as the Red Hat activation key. Settings only, never in the repository (#295).')
param ocrKeyVaultAdmins string = ''

@description('Assign the guard-rail policies (defined by the usnm-guardrails stack, infra/guardrails.bicep) to the resource groups.')
param deployPolicies bool = true

@description('Seconds between availability test runs of the site and API from each of 3 locations (tests exist only with dnsZoneName and alert emails). 300, 600 or 900. Each run costs $0.0005: 300 is about $13 a month per URL.')
@allowed(['300', '600', '900'])
param availabilityFrequency string = '900'

@description('Public DNS zone for the site (e.g. usnewsmap.com); empty skips it. Delegate the domain to the NAME_SERVERS output.')
param dnsZoneName string = ''
@description('More TXT values at the zone apex, keyed by zone name (site-verification tokens). They are public, so they live in git.')
param dnsApexTxtValues object = {}

@description('Managed certificate for api.{dnsZoneName}, recorded by scripts/bootstrap.sh after it issues one.')
param apiCertificateId string = ''

@description('Managed certificate for {dnsZoneName} itself (the site), recorded by scripts/bootstrap.sh.')
param siteCertificateId string = ''

@description('Managed certificate for www.{dnsZoneName}, recorded by scripts/bootstrap.sh.')
param wwwCertificateId string = ''

// Resource names must be lowercase (Cosmos, storage).
var env = toLower(environmentName)
var tags = {
  project: 'usnewsmap'
  environment: environmentName
}
var emails = filter(map(split(alertEmails, ','), e => trim(e)), e => !empty(e))
var suffix = take(uniqueString(subscription().id, env), 6)
// The Quickwit image (ADR-0013, 08 §8.6). An environment listed in
// quickwit-image.json runs our build of Quickwit from its own registry,
// pushed there by scripts/build-quickwit.sh; any other runs upstream
// v0.9.1, which CI copies into the registry with its digest unchanged. CI
// reads the same file for the ingest image's base.
var quickwitPins = loadJsonContent('quickwit-image.json')
var quickwitPin = any(quickwitPins.environments)[?env] ?? quickwitPins.upstream
var quickwitUpstream = '${quickwitPins.upstream.image}:${quickwitPins.upstream.tag}@${quickwitPins.upstream.digest}'
// Where the site is served: the API app's own hostname, and the domain and
// www if there is one. The site calls the API on its own origin, so these
// matter for the tiles account's CORS (and any cross-origin API caller).
var appName = 'ca-usnm-${env}'
var appOrigin = 'https://${appName}.${containerEnv.outputs.defaultDomain}'
var domainOrigins = empty(dnsZoneName) ? [] : ['https://${dnsZoneName}', 'https://www.${dnsZoneName}']
var siteOrigins = union(allowedOrigins, domainOrigins, [appOrigin])
// Hostnames on the app, each bound once bootstrap has issued its certificate.
var customDomains = empty(dnsZoneName)
  ? []
  : [
      { name: dnsZoneName, certificateId: siteCertificateId }
      { name: 'www.${dnsZoneName}', certificateId: wwwCertificateId }
      { name: 'api.${dnsZoneName}', certificateId: apiCertificateId }
    ]

resource rg 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: 'rg-usnm-${env}'
  location: location
  tags: tags
}

resource spotRg 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: 'rg-usnm-${env}-spot'
  location: location
  tags: tags
}

module monitoring 'modules/monitoring.bicep' = {
  scope: rg
  name: 'monitoring'
  params: {
    location: location
    tags: tags
    workspaceName: 'log-usnm-${env}'
    appInsightsName: 'appi-usnm-${env}'
    actionGroupName: 'ag-usnm-${env}'
    nameSuffix: env
    alertEmails: emails
    // Prod: 1 GB. 0.15 GB stopped all logging on 2026-10-07 (the American Stories writer's blob
    // writes reached it by 17:46 ET; nothing was logged, alerts included, until the next reset).
    // A normal prod day is about 53 MB (27 to 83 MB, 30 Sep to 9 Oct 2026), inside the free
    // 5 GB/month; the cap bounds bursts from bulk jobs. Blob write logs no longer count toward it
    // (StorageBlobLogs is on the Auxiliary plan, monitoring.bicep). Dev keeps 0.15 GB, inside the
    // free allowance at any rate (ADR-0005).
    dailyCapGb: !empty(logDailyCapGb) ? logDailyCapGb : (env == 'prod' ? '1' : '0.15')
  }
}

module registry 'modules/registry.bicep' = {
  scope: rg
  name: 'registry'
  params: {
    location: location
    tags: tags
    name: 'crusnm${env}${suffix}'
    ciIdentityName: 'id-usnm-ci-${env}'
    githubRepo: githubRepo
    githubRepoIds: githubRepoIds
    githubEnvironment: env
    pullPrincipalIds: [identities.outputs.appPrincipalId, identities.outputs.ingestPrincipalId]
  }
}

module network 'modules/network.bicep' = {
  scope: rg
  name: 'network'
  params: {
    location: location
    tags: tags
    name: 'vnet-usnm-${env}'
    storageServiceEndpoint: ocrNodes
  }
}

module identities 'modules/identities.bicep' = {
  scope: rg
  name: 'identities'
  params: {
    location: location
    tags: tags
    appName: 'id-usnm-app-${env}'
    ingestName: 'id-usnm-ingest-${env}'
  }
}

module storage 'modules/storage.bicep' = {
  scope: rg
  name: 'storage'
  params: {
    location: location
    tags: tags
    name: 'stusnmd${suffix}'
  }
}

// The tiles account serves public map data. The public-network policy exempts it
// by resource id, so its assignment must be in place before the account is written.
var tilesName = 'stusnmt${suffix}'
var tilesId = resourceId(subscription().subscriptionId, rg.name, 'Microsoft.Storage/storageAccounts', tilesName)

module tiles 'modules/tiles.bicep' = {
  scope: rg
  name: 'tiles'
  dependsOn: [policies]
  params: {
    location: location
    tags: union(tags, { 'usnm-public': 'true' })
    name: tilesName
    allowedOrigins: siteOrigins
  }
}

// The on-prem OCR exchange account (#296, ADR-0015) allows public network
// access behind a firewall, so the public-network policy exempts it by
// resource id like the tiles account, and its assignment goes first.
var exchangeName = 'stusnmo${suffix}'
var exchangeId = resourceId(subscription().subscriptionId, rg.name, 'Microsoft.Storage/storageAccounts', exchangeName)

module ocrExchange 'modules/ocr-exchange.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-exchange'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: exchangeName
    nodeIps: filter(map(split(ocrNodeIps, ','), ip => trim(ip)), ip => !empty(ip))
    subnetId: network.outputs.caeSubnetId
    nodeGroupId: trim(ocrNodeGroupId)
    ingestPrincipalId: identities.outputs.ingestPrincipalId
  }
}

// The OCR nodes' Key Vault (#296, #295): secrets the operator holds for
// setting up nodes (the Red Hat activation key), which onboard.sh reads with
// the operator's sign-in. Nodes have no access. Its firewall allows only the
// node IPs (the operator's home IP), so the public-network policy exempts it
// by resource id like the exchange account, and its assignment goes first.
// Compact name (ADR-0017): usnm{fn}{abbr}{env}{sfx}, at most 24 characters.
// The environment name is cut to 9 characters, never the suffix, which
// keeps the name unique; ADR-0017's 5-character cap makes the cut moot.
var ocrVaultName = 'usnmocrkv${take(env, 9)}${suffix}'
var ocrVaultId = resourceId(subscription().subscriptionId, rg.name, 'Microsoft.KeyVault/vaults', ocrVaultName)

module ocrVault 'modules/ocr-vault.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-vault'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: ocrVaultName
    purgeProtection: env == 'prod'
    allowedIps: filter(map(split(ocrNodeIps, ','), ip => trim(ip)), ip => !empty(ip))
    admins: filter(map(split(ocrKeyVaultAdmins, ','), a => trim(a)), a => !empty(a))
  }
}

// The OCR nodes' monitoring (ADR-0015 decision 8), apart from the
// environment's: a workspace and an Application Insights component of their
// own, so a node can't write the environment's telemetry or fill its daily
// cap; the data collection rule; the nodes' group's Monitoring Metrics
// Publisher grant on that component only; and alerts on a node sending too
// much and on the cap. onboard.sh installs the agent on each node and
// associates it with the rule.
// The nodes' workspace's daily cap in GB, which the OCR nodes workbook also
// charts against. 0.1 GB to start: about 8 to 25 MB a day per node is
// estimated before the worker's telemetry, which isn't measured yet. Raise
// it with cap_gb in ops/queries/ocr-node-health.kql.
var ocrNodeDailyCapGb = '0.1'

module ocrNodeMonitoring 'modules/ocr-node-monitoring.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-node-monitoring'
  params: {
    location: location
    tags: tags
    nameSuffix: env
    workspaceName: 'usnm-ocr-log-${env}'
    appInsightsName: 'usnm-ocr-appi-${env}'
    ruleName: 'usnm-ocr-dcr-${env}'
    dailyCapGb: ocrNodeDailyCapGb
    nodeGroupId: trim(ocrNodeGroupId)
    actionGroupId: monitoring.outputs.actionGroupId
  }
}

module cosmos 'modules/cosmos.bicep' = {
  scope: rg
  name: 'cosmos'
  params: {
    location: location
    tags: tags
    name: 'cosmos-usnm-${env}-${suffix}'
    freeTier: cosmosFreeTier
  }
}

module privateEndpoints 'modules/private-endpoints.bicep' = {
  scope: rg
  name: 'private-endpoints'
  params: {
    location: location
    tags: tags
    subnetId: network.outputs.peSubnetId
    storageId: storage.outputs.id
    cosmosId: cosmos.outputs.id
    blobDnsZoneId: network.outputs.blobDnsZoneId
    cosmosDnsZoneId: network.outputs.cosmosDnsZoneId
  }
}

module rbac 'modules/rbac.bicep' = {
  scope: rg
  name: 'rbac'
  params: {
    storageName: storage.outputs.name
    cosmosName: cosmos.outputs.name
    appInsightsName: monitoring.outputs.appInsightsName
    appPrincipalId: identities.outputs.appPrincipalId
    ingestPrincipalId: identities.outputs.ingestPrincipalId
    searchLogReaders: filter(map(split(searchLogReaders, ','), r => trim(r)), r => !empty(r))
  }
}

module ocrService 'modules/ocr.bicep' = if (ocr) {
  scope: rg
  name: 'ocr'
  params: {
    location: location
    tags: tags
    name: 'di-usnm-${env}-${suffix}'
    sku: ocrSku
    workspaceId: monitoring.outputs.workspaceId
    users: filter(map(split(ocrUsers, ','), u => trim(u)), u => !empty(u))
    identities: [identities.outputs.ingestPrincipalId]
  }
}

module containerEnv 'modules/containerapps-env.bicep' = {
  scope: rg
  name: 'containerapps-env'
  params: {
    location: location
    tags: tags
    name: 'cae-usnm-${env}'
    subnetId: network.outputs.caeSubnetId
    dedicatedProfile: dedicatedProfile
  }
}

// Resource logs for everything that has them: to the environment's
// workspace, except what the OCR nodes control (the exchange account's queue
// logs, the nodes' rule's errors), which goes to the nodes' workspace.
module diagnostics 'modules/diagnostics.bicep' = {
  scope: rg
  name: 'diagnostics'
  params: {
    workspaceId: monitoring.outputs.workspaceId
    workspaceName: monitoring.outputs.workspaceName
    registryName: registry.outputs.name
    vnetName: network.outputs.vnetName
    containerEnvName: containerEnv.outputs.name
    cosmosName: cosmos.outputs.name
    dataStorageName: storage.outputs.name
    tilesStorageName: tiles.outputs.name
    scratchStorageName: nfsAccount ? scratch!.outputs.accountName : ''
    exchangeStorageName: ocrNodes ? ocrExchange!.outputs.name : ''
    ocrNodeWorkspaceId: ocrNodes ? ocrNodeMonitoring!.outputs.workspaceId : ''
    ocrNodeWorkspaceName: ocrNodes ? ocrNodeMonitoring!.outputs.workspaceName : ''
    ocrNodeDcrName: ocrNodes ? ocrNodeMonitoring!.outputs.ruleName : ''
    ocrVaultName: ocrNodes ? ocrVault!.outputs.name : ''
  }
}

// A new environment has no image to pull until CI pushes to its registry.
var deployApi = useAcr || !empty(apiImage)

module api 'modules/containerapp.bicep' = if (deployApi) {
  scope: rg
  name: 'api'
  dependsOn: [rbac, privateEndpoints]
  params: {
    location: location
    tags: tags
    name: appName
    environmentId: containerEnv.outputs.id
    image: useAcr ? '${registry.outputs.loginServer}/usnewsmap-api:${imageTag}' : apiImage
    // Without the registry, the public upstream image: ours is only in the registry.
    quickwitImage: useAcr ? '${registry.outputs.loginServer}/${quickwitPin.image}@${quickwitPin.digest}' : quickwitUpstream
    registryServer: useAcr ? registry.outputs.loginServer : ''
    customDomains: customDomains
    identityId: identities.outputs.appId
    identityClientId: identities.outputs.appClientId
    storageBlobEndpoint: storage.outputs.blobEndpoint
    storageAccountName: storage.outputs.name
    cosmosEndpoint: cosmos.outputs.endpoint
    allowedOrigins: siteOrigins
    minReplicas: apiMinReplicas
    searchBackend: searchBackend
    appInsightsConnectionString: monitoring.outputs.appInsightsConnectionString
    ingestCron: ingestJobs && useAcr ? ingestCron : ''
    americanStoriesSearch: americanStoriesSearch
    congressSearch: congressSearch
  }
}

// What CI may do here: roll the API app (and so the site) onto new images.
module deployer 'modules/deployer.bicep' = {
  scope: rg
  name: 'deployer'
  params: {
    principalId: registry.outputs.ciPrincipalId
    appIdentityName: identities.outputs.appName
  }
}

var ingestScratch = ingestJobs && useAcr && ingestScratchGiB > 0

// The archival account, outside this stack: an endpoint here, roles there.
var archiveAccount = empty(archiveAccountId) ? '' : last(split(archiveAccountId, '/'))

module archiveAccess 'modules/archive-access.bicep' = if (!empty(archiveAccountId)) {
  scope: rg
  name: 'archive-access'
  params: {
    location: location
    tags: tags
    name: 'pe-usnm-archive-blob'
    subnetId: network.outputs.peSubnetId
    accountId: archiveAccountId
    blobDnsZoneId: network.outputs.blobDnsZoneId
  }
}

module archiveGrant 'modules/archive-grant.bicep' = if (!empty(archiveAccountId)) {
  scope: resourceGroup(split(archiveAccountId, '/')[2], split(archiveAccountId, '/')[4])
  name: 'archive-grant-${env}'
  params: {
    accountName: archiveAccount
    env: env
    grants: concat(
      [{ container: 'raw', principalId: identities.outputs.ingestPrincipalId, role: 'writer' }],
      searchClusterOn
        ? [
            { container: 'raw', principalId: searchClusterModule!.outputs.benchPrincipalId, role: 'reader' }
            { container: 'sets', principalId: searchClusterModule!.outputs.benchPrincipalId, role: 'writer' }
          ]
        : []
    )
  }
}

module rawStore 'modules/raw-store.bicep' = if (retainRaw) {
  scope: rg
  name: 'raw-store'
  params: {
    storageAccountName: storage.outputs.name
    ingestPrincipalId: identities.outputs.ingestPrincipalId
  }
}

// The local-disk test's nfs mode puts its share in the scratch account.
var searchNfs = searchClusterOn && searchLocalMode == 'nfs'
var nfsAccount = ingestScratch || searchNfs

module scratch 'modules/ingest-scratch.bicep' = if (nfsAccount) {
  scope: rg
  name: 'ingest-scratch'
  params: {
    location: location
    tags: tags
    name: 'stusnms${suffix}'
    sizeGiB: ingestScratch ? ingestScratchGiB : 0
    searchGiB: searchNfs ? searchNfsGiB : 0
    vnetId: network.outputs.vnetId
    vnetName: network.outputs.vnetName
    peSubnetId: network.outputs.peSubnetId
    containerEnvName: containerEnv.outputs.name
  }
}

module ingest 'modules/ingestjobs.bicep' = if (ingestJobs && useAcr) {
  scope: rg
  name: 'ingest-jobs'
  dependsOn: [rbac, privateEndpoints, archiveAccess, archiveGrant]
  params: {
    location: location
    tags: tags
    environmentId: containerEnv.outputs.id
    image: '${registry.outputs.loginServer}/usnewsmap-ingest:${imageTag}'
    registryServer: registry.outputs.loginServer
    ingestIdentityId: identities.outputs.ingestId
    ingestClientId: identities.outputs.ingestClientId
    storageAccountName: storage.outputs.name
    storageBlobEndpoint: storage.outputs.blobEndpoint
    cosmosEndpoint: cosmos.outputs.endpoint
    appInsightsConnectionString: monitoring.outputs.appInsightsConnectionString
    jobNameSuffix: env
    cron: ingestCron
    full: ingestFull
    americanStories: americanStories
    jaLatin: jaLatin
    ocrLocal: ocrLocal
    partitionDecade: partitionDecade
    singleTextField: singleTextField
    workers: backfillWorkers
    backfillCron: backfillCron
    scratchStorageName: ingestScratch ? scratch!.outputs.envStorageName : ''
    scratchGiB: ingestScratch ? ingestScratchGiB : 0
    mergeTimeoutSecs: ingestMergeTimeoutSecs
    ingestProfile: ingestOnDedicated ? containerEnv.outputs.dedicatedProfileName : ''
    jaOcrImage: jaOcrJob ? '${registry.outputs.loginServer}/usnewsmap-ja-ocr:${imageTag}' : ''
    jaOcrReplicas: jaOcrReplicas
    rootImage: '${registry.outputs.loginServer}/${quickwitPin.image}@${quickwitPin.digest}'
    ocrSync: ocrNodes
    ocrQueueEndpoint: ocrNodes ? ocrExchange!.outputs.queueEndpoint : ''
    ocrBlobEndpoint: ocrNodes ? ocrExchange!.outputs.blobEndpoint : ''
    ocrSyncCron: ocrSyncCron == 'manual' ? '' : ocrSyncCron
    ocrSeed: ocrSeed
    rawUrl: !empty(archiveAccountId)
      ? 'https://${archiveAccount}.blob.${environment().suffixes.storage}/raw'
      : (retainRaw ? '${storage.outputs.blobEndpoint}${rawStore!.outputs.container}' : '')
  }
}

// The experimental search cluster (#239), off unless asked for, and never in
// prod: it is for a dev environment (docs/operations.md, "Search cluster
// experiment").
var searchClusterOn = searchCluster && useAcr && env != 'prod'

module searchClusterModule 'modules/searchcluster.bicep' = if (searchClusterOn) {
  scope: rg
  name: 'search-cluster'
  dependsOn: [privateEndpoints]
  params: {
    location: location
    tags: tags
    nameSuffix: env
    environmentId: containerEnv.outputs.id
    image: '${registry.outputs.loginServer}/usnewsmap-ingest:${imageTag}'
    registryServer: registry.outputs.loginServer
    registryName: registry.outputs.name
    storageAccountName: storage.outputs.name
    storageBlobEndpoint: storage.outputs.blobEndpoint
    archiveBlobEndpoint: empty(archiveAccountId) ? '' : 'https://${archiveAccount}.blob.${environment().suffixes.storage}/'
    nodes: searchClusterNodes
    indexers: searchClusterIndexers
    nodeVcpu: searchClusterNodeVcpu
    splitSearches: searchClusterSplitSearches
    runtimeThreads: searchClusterRuntimeThreads
    searchThreads: searchClusterSearchThreads
    localMode: searchLocalMode
    localProfile: containerEnv.outputs.dedicatedProfileName
    localIndex: searchLocalIndex
    localCacheGib: searchLocalCacheGib
    localNfsStorage: searchNfs ? scratch!.outputs.searchEnvStorageName : ''
    compareImages: map(
      filter(split(searchCompareTags, ','), t => !empty(trim(t))),
      t => '${registry.outputs.loginServer}/usnewsmap-ingest:${trim(t)}'
    )
  }
}

module budget 'modules/budget.bicep' = if (!empty(emails) && !empty(budgetStartDate)) {
  scope: rg
  name: 'budget'
  params: {
    name: 'budget-usnm-${env}'
    startDate: budgetStartDate
    contactEmails: emails
  }
}

// Ingest and backfill failures and stalls, from the jobs' console logs.
module ingestAlerts 'modules/ingest-alerts.bicep' = if (!empty(emails) && ingestJobs && useAcr) {
  scope: rg
  name: 'ingest-alerts'
  params: {
    location: location
    tags: tags
    nameSuffix: env
    workspaceId: monitoring.outputs.workspaceId
    actionGroupId: monitoring.outputs.actionGroupId
    ocrSync: ocrNodes
  }
}

// API server errors and latency, from the requests it exports; availability
// tests of the site and the API on the custom domain once its certificates
// are bound.
module apiAlerts 'modules/api-alerts.bicep' = if (!empty(emails) && deployApi) {
  scope: rg
  name: 'api-alerts'
  params: {
    location: location
    tags: tags
    nameSuffix: env
    workspaceId: monitoring.outputs.workspaceId
    appInsightsId: monitoring.outputs.appInsightsId
    actionGroupId: monitoring.outputs.actionGroupId
    siteUrl: !empty(dnsZoneName) && !empty(siteCertificateId) ? 'https://${dnsZoneName}/' : ''
    availabilityFrequency: availabilityFrequency
  }
}

// Dashboards: the pipeline workbook with the jobs and the API workbook with
// the API, over the workspace; the OCR nodes workbook with the nodes, over
// their own workspace. No alert emails needed.
module workbooks 'modules/workbooks.bicep' = if ((ingestJobs && useAcr) || deployApi || ocrNodes) {
  scope: rg
  name: 'workbooks'
  params: {
    location: location
    tags: tags
    workspaceId: monitoring.outputs.workspaceId
    appInsightsId: monitoring.outputs.appInsightsId
    env: env
    pipeline: ingestJobs && useAcr
    api: deployApi
    ocrNodes: ocrNodes
    ocrNodeWorkspaceId: ocrNodes ? ocrNodeMonitoring!.outputs.workspaceId : ''
    ocrNodeAppInsightsId: ocrNodes ? ocrNodeMonitoring!.outputs.appInsightsId : ''
    ocrNodeDailyCapGb: ocrNodeDailyCapGb
  }
}

// The guard-rail definitions are shared by every environment in the
// subscription and deployed by their own stack (guardrails.bicep); this
// environment only assigns them.
var guardrailPolicyIds = map(
  items(guardrailPolicyNames),
  p => subscriptionResourceId('Microsoft.Authorization/policyDefinitions', p.value)
)

module policies 'modules/policy-assignments.bicep' = if (deployPolicies) {
  scope: rg
  name: 'policy-assignments'
  params: {
    definitionIds: guardrailPolicyIds
    definitionParameters: {
      // Deny and the exemption arrive together, in the same assignment update.
      '${guardrailPolicyNames.auditPublicAccess}': {
        effect: { value: 'deny' }
        publicAccountIds: { value: ocrNodes ? [tilesId, exchangeId, ocrVaultId] : [tilesId] }
      }
    }
  }
}

module spotPolicies 'modules/policy-assignments.bicep' = if (deployPolicies) {
  scope: spotRg
  name: 'policy-assignments-spot'
  params: {
    definitionIds: guardrailPolicyIds
    // No public accounts live here.
    definitionParameters: {
      '${guardrailPolicyNames.auditPublicAccess}': { effect: { value: 'deny' } }
    }
  }
}

module dns 'modules/dns.bicep' = if (!empty(dnsZoneName)) {
  scope: rg
  name: 'dns'
  params: {
    tags: tags
    zoneName: dnsZoneName
    appFqdn: deployApi ? api!.outputs.fqdn : ''
    staticIp: containerEnv.outputs.staticIp
    verificationId: containerEnv.outputs.customDomainVerificationId
    apexTxtValues: dnsApexTxtValues[?dnsZoneName] ?? []
  }
}

output AZURE_LOCATION string = location
output AZURE_RESOURCE_GROUP string = rg.name
// The custom hostname once its certificate is bound, else the app's own.
output API_URL string = !deployApi
  ? ''
  : (!empty(dnsZoneName) && !empty(apiCertificateId) ? 'https://api.${dnsZoneName}' : 'https://${api!.outputs.fqdn}')
output API_APP string = deployApi ? api!.outputs.name : ''
// The domain once its certificate is bound, else the app's own hostname.
output SITE_URL string = !deployApi
  ? ''
  : (!empty(dnsZoneName) && !empty(siteCertificateId) ? 'https://${dnsZoneName}' : 'https://${api!.outputs.fqdn}')
output CONTAINER_ENV_NAME string = 'cae-usnm-${env}'
// Set these as the domain's name servers at the registrar.
output NAME_SERVERS string = empty(dnsZoneName) ? '' : join(dns!.outputs.nameServers, ' ')
output TILES_URL string = tiles.outputs.tilesUrl
output STORAGE_ACCOUNT string = storage.outputs.name
output COSMOS_ENDPOINT string = cosmos.outputs.endpoint
output INGEST_JOB string = ingestJobs && useAcr ? ingest!.outputs.ingestJobName : ''
output BACKFILL_JOB string = ingestJobs && useAcr ? ingest!.outputs.backfillJobName : ''
output JA_OCR_JOB string = ingestJobs && useAcr ? ingest!.outputs.jaOcrJobName : ''
output JA_AUDIT_JOB string = ingestJobs && useAcr ? ingest!.outputs.jaAuditJobName : ''
// The OCR pickup job (12 §12.5 Step 8); scripts/start-job.sh --job OCR_SYNC_JOB
// runs ocr-enqueue and ocr-requeue as executions of it.
output OCR_SYNC_JOB string = ingestJobs && useAcr ? ingest!.outputs.ocrSyncJobName : ''
output ACR_NAME string = registry.outputs.name
output ACR_LOGIN_SERVER string = registry.outputs.loginServer
// For the GitHub Environment variables CI signs in with (not secrets; scripts/bootstrap.sh writes them).
output CI_CLIENT_ID string = registry.outputs.ciClientId
output AZURE_TENANT_ID string = tenant().tenantId
output AZURE_SUBSCRIPTION_ID string = subscription().subscriptionId
output APPLICATIONINSIGHTS_CONNECTION_STRING string = monitoring.outputs.appInsightsConnectionString
// The workspace's customer id, for the Log Analytics query API (scripts/logs.sh).
output LOG_ANALYTICS_WORKSPACE_ID string = monitoring.outputs.workspaceCustomerId
output OCR_ENDPOINT string = ocr ? ocrService!.outputs.endpoint : ''
// The search cluster's apps, bench job and root URL (inside the environment); empty when off.
output SEARCH_CLUSTER_APPS string = searchClusterOn ? join(searchClusterModule!.outputs.nodeApps, ' ') : ''
output SEARCH_CLUSTER_JOB string = searchClusterOn ? searchClusterModule!.outputs.benchJobName : ''
output SEARCH_CLUSTER_URL string = searchClusterOn ? searchClusterModule!.outputs.rootUrl : ''
// The version comparison's searchers and their roots, in the order of USNM_SEARCH_COMPARE_TAGS.
output SEARCH_COMPARE_APPS string = searchClusterOn ? join(searchClusterModule!.outputs.compareApps, ' ') : ''
// The local-disk test's searcher root (inside the environment); empty when off.
output SEARCH_LOCAL_URL string = searchClusterOn ? searchClusterModule!.outputs.localUrl : ''
output SEARCH_COMPARE_URLS string = searchClusterOn ? join(searchClusterModule!.outputs.compareUrls, ' ') : ''
// The on-prem OCR exchange account (#296); scripts/ocr-node/onboard.sh checks it exists.
output OCR_EXCHANGE_ACCOUNT string = ocrNodes ? ocrExchange!.outputs.name : ''
output OCR_QUEUE_ENDPOINT string = ocrNodes ? ocrExchange!.outputs.queueEndpoint : ''
output OCR_BLOB_ENDPOINT string = ocrNodes ? ocrExchange!.outputs.blobEndpoint : ''
// The OCR nodes' Key Vault: onboard.sh reads the Red Hat secrets from it.
output OCR_KEYVAULT string = ocrNodes ? ocrVault!.outputs.name : ''
// The nodes' data collection rule; onboard.sh associates each node with it.
output OCR_NODE_DCR_ID string = ocrNodes ? ocrNodeMonitoring!.outputs.ruleId : ''
// The nodes' own Application Insights component: onboard.sh passes its
// ingestion settings (not a credential: local auth is off) to the worker.
output OCR_NODE_APPINSIGHTS string = ocrNodes ? ocrNodeMonitoring!.outputs.appInsightsConnectionString : ''
output OCR_NODE_APPINSIGHTS_ID string = ocrNodes ? ocrNodeMonitoring!.outputs.appInsightsId : ''
// The nodes' workspace's customer id: scripts/logs.sh runs the ocr-node-*
// queries there.
output OCR_NODE_LOG_ANALYTICS_WORKSPACE_ID string = ocrNodes ? ocrNodeMonitoring!.outputs.workspaceCustomerId : ''
