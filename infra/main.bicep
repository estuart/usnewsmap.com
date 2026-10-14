// US News Map: lean hosting profile (08 §8.1, ADR-0006). No IaaS.
//
// Deployed as the deployment stack usnm-{env} by scripts/bootstrap.sh, with
// parameters from infra/main.bicepparam. Creates the project
// resource group and the empty Spot resource group the backfill launcher
// will use, then the platform inside the project group.

targetScope = 'subscription'

import { guardrailPolicyNames } from 'guardrails.bicep'
import { checkedEnvName, legacyProdSuffix, names } from 'naming.bicep'
import { lakeOpen, nodeIpList, publicAccountIds } from 'public-accounts.bicep'

@minLength(2)
@maxLength(5)
@description('Environment name: prod, dev, or x plus 1-4 lowercase letters or digits for a short-lived experiment (ADR-0017; AZURE_ENV_NAME in the environment\'s settings).')
param environmentName string

@description('Region; East US 2 has ACI Spot (preview).')
param location string = 'eastus2'

@description('The VNet\'s address space (USNM_VNET_ADDRESS_PREFIX in the environment\'s settings): a private IPv4 block in CIDR, /26 or larger, holding snet-cae (its first /27) and snet-pe (the /28 after that). No default: private addresses aren\'t committed (scripts/ci/no-private-addresses.sh).')
param vnetAddressPrefix string

@description('A public API image, used only while useAcr is off. Empty (the default) skips the API until CI has pushed to the registry and useAcr is on.')
param apiImage string = ''

@description('Extra origins allowed by API CORS. The app\'s own hostname and, with dnsZoneName, the domain and its www are always allowed.')
param allowedOrigins array = []

@description('0 lets dev scale to zero; production keeps one warm replica.')
param apiMinReplicas int = toLower(environmentName) == 'prod' ? 1 : 0

@description('fixtures: the API serves its baked synthetic data. quickwit: reference data from Blob and a read-only Quickwit sidecar over qw-index (needs published indexes).')
@allowed(['fixtures', 'quickwit'])
param searchBackend string = 'fixtures'

@description('Run the Quickwit searcher as its own Container App, usnm-search-ca-{env}, with internal ingress, instead of a sidecar in each API replica (#267): a new API image, and so a site change, then doesn\'t restart it. Needs searchBackend quickwit. Off (the default) keeps the sidecar; turning it off again deletes the searcher app (docs/operations.md, "Searcher app").')
param searcherApp bool = false

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

@description('How the API renders a page-language lang= filter (#289, 05 §5.5.8): smaller (the side with fewer pages, the default) or match (page_language:IN always), for measuring one against the other. Both find the same pages; empty leaves the API\'s default (docs/configuration.md, USNM_PAGE_LANGUAGE_FILTER).')
@allowed(['', 'smaller', 'match'])
param pageLanguageFilter string = ''

@description('List a newspaper\'s pages without a search word (#274): /v1/hits with lccn and no q, and the site\'s "Browse pages". Off until its cost on the full index is measured: a list with no word reads every split for the title\'s pages (docs/configuration.md, USNM_BROWSE_PAGES).')
param browsePages bool = false

@description('Give each ingest release the Congress pages curated under curated/congress/ (#229, the ingest job\'s --congress): an index set of their own, rebuilt when the curated pages change and kept otherwise. Clearing it makes the next release publish a version without them (docs/operations.md, "Congress pages").')
param congress bool = false

@description('Deploy the Congress fetch job, usnm-cong-caj-<env> (#229): manual, one replica of `usnm-ingest congress-globe`, which fetches the Congressional Globe from UNT at its crawl delay into curated/congress/. Set archiveAccountId first so every response is kept (docs/operations.md, "Congress pages").')
param congressJob bool = false

@description('Deploy the Serial Set fetch job, usnm-sset-caj-<env> (#229 phase 2): manual, one replica of `usnm-ingest congress-serialset`, which fetches the Serial Set from GovInfo (MODS and PDFs, about a request a second; the PDFs are read and deleted) into curated/congress/. Set archiveAccountId first so the MODS and page texts are kept (docs/operations.md, "Congress pages").')
param serialSetJob bool = false

@description('Run the Serial Set fetch on-prem instead of as the Azure job (#229; ADR-0008, amendment of October 2026): the data lake allows public network access from ocrNodeIps only (its firewall denies everything else; the private endpoint stays), the public-network policy exempts it, and serialSetRunner gets a grant on curated/congress/serialset/ alone. Off closes the lake again and removes the grant (docs/operations.md, "The Serial Set on-prem").')
param serialSetOnPrem bool = false

@description('Object id of the on-prem Serial Set runner\'s identity: the OCR node\'s Arc machine identity (with serialSetOnPrem). Settings only, never in the repository.')
param serialSetRunner string = ''

@description('Object id of the on-prem BOINC hand-off runner\'s identity (#469): the scan-fetch container\'s Arc machine identity, which runs `usnm-boinc export` and `collect`. It gets the ocr-sync lease (Cosmos ops), the native ledger (curated/ocr-local/, no delete), reference read, ocr-out (create, no delete) and the BOINC workspace read (modules/boinc-handoff-runner.bicep). Set it only after the BOINC stack is deployed: the workspace grant needs its workspace. Settings only, never in the repository.')
param boincHandoffRunner string = ''

@description('Give the pages LoC ships without text (the Japanese pages our OCR reads, 04 §4.8) a main-index document with the Latin-script text our OCR read on them, so English searches reach their English ads and sections (#203). The next release that builds a main index adds them; clearing it drops them at the next full release (docs/operations.md, "Japanese OCR: mixed pages and English text").')
param jaLatin bool = false

@description('Weigh our own OCR from curated/ocr-local (the on-prem nodes\' output, #296) against LoC\'s text: on the pages of the batches a release indexes, ours replaces LoC\'s where rule v1 prefers it (12 §12.5 Step 9). Published batches take it at the next full release (docs/operations.md, "Our OCR in releases").')
param ocrLocal bool = false
@description('Lay the next full rebuild\'s base out by decade (#123): every split holds one decade, so a date-limited search skips the others. Only a full base takes it; deltas follow the published version, and a full rebuild without it lays nothing out by decade (docs/operations.md, "Full rebuild").')
param partitionDecade bool = false

@description('Search the next full rebuild\'s texts, LoC\'s and American Stories\', in one field (#283, 05 §5.5.7), so a search looks each word up once. Only a full base takes it; deltas follow the published version, and a full rebuild without it searches a field per text again (docs/operations.md, "Full rebuild").')
param singleTextField bool = false

@description('Give the next full rebuild\'s pages their own languages, found from their words (#289, 05 §5.5.8): lang= then filters by page language and the counts by language count pages. Only a full base takes it; deltas follow the published version, and a full rebuild without it filters by title language again (docs/operations.md, "Full rebuild").')
param pageLanguage bool = false

@description('Pages per split of the next full rebuild\'s base, in place of the 30,000 in pages-index.yaml (#283): fewer, larger splits for a cold search to open. 0: the template\'s. Only a full base takes it; deltas keep the template\'s. A larger target needs the E4 profile\'s memory for the split uploads (08 §8.4; docs/operations.md, "Full rebuild").')
@minValue(0)
@maxValue(999999)
param splitDocs int = 0

@description('Add a memory-optimized E4 workload profile (4 vCPU / 32 GiB, no minimum nodes) for full index rebuilds (#172). The environment pays the Dedicated plan management fee while it exists: turn it on for a rebuild, off afterwards (docs/operations.md, "Full rebuild").')
param dedicatedProfile bool = false

@description('Run the ingest job on the E4 profile (needs dedicatedProfile). Turn this off and provision before turning dedicatedProfile off: a profile in use can\'t be removed.')
param ingestOnDedicated bool = false

@description('Deploy the experimental Quickwit search cluster (#239): node apps usnm-qw-{i}-ca-{env}, the bench job usnm-qw-bench-caj-{env} and their containers qw-cluster and qw-bench (docs/operations.md, "Search cluster experiment"). Needs useAcr; ignored in prod. Turning it off deletes all of it, the two containers included.')
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

@description('Quickwit version comparison (#251): ingest image tags, comma separated, each run as a standalone searcher usnm-qw-cmp-{i}-ca-{env} over the search cluster\'s indexes (searchClusterNodeVcpu each; docs/operations.md, "Quickwit version comparison"). Empty: none.')
param searchCompareTags string = ''

@description('Local-disk test (#251): a standalone 0.9.1 searcher, usnm-qw-local-ca-{env}, reading the search cluster\'s indexes from Blob (blob) or a copy of searchLocalIndex on an NFS share (nfs), on Consumption at the API sidecar\'s 3.75 vCPU / 7.5 GiB, or through a split cache (cache) or from a copy (copy) on its own disk, on the E4 profile (needs dedicatedProfile) for the disk (docs/operations.md, "Local-disk test"). Empty: none.')
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

@description('Size in GiB of the NFS share the ingest job\'s Quickwit writer works on (08 §8.4), with its account, private endpoint and DNS zone: 128 or more for a full rebuild, set for the rebuild and cleared afterwards (docs/operations.md, "Full rebuild"); 0 (the default): none, and the writer works on the replica\'s own disk, which holds a weekly delta but not a full base.')
param ingestScratchGiB int = 0

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

@description('Schedule for the OCR pickup job usnm-ocrs-caj-<env> (UTC cron; with ocrNodes and the ingest jobs, 12 §12.5 Step 8). `manual`: no schedule.')
param ocrSyncCron string = '17 * * * *'

@description('The OCR seeder\'s settings for the pickup job (12 §12.5 Step 10), by environment variable name (USNM_OCR_SEED_* and USNM_OCR_BULK_*; docs/operations.md). Seeding stays off while USNM_OCR_SEED_PLAN is empty (ADR-0015 decision 7).')
param ocrSeed object = {}

@description('Entra object ids (people or groups), comma separated, given Key Vault Secrets Officer on the OCR nodes\' Key Vault, usnmocrkv<env><suffix> (with ocrNodes): the operators who set and read the fleet\'s secrets, such as the Red Hat activation key. Settings only, never in the repository (#295).')
param ocrKeyVaultAdmins string = ''

@description('Assign the guard-rail policies (defined by the usnm-guardrails stack, infra/guardrails.bicep) to the resource groups.')
param deployPolicies bool = true

@description('Create the dedicated resource group for the BOINC server stack (ADR-0019), named by infra/naming.bicep, which must hold only BOINC resources. True: creates the group, assigns the guardrails to it and waives the shared-key storage and monitor local-auth policies for every resource in that group. False: no group and no exemption.')
param deployBoincGroup bool = false

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

@description('The issue number of the experiment this environment is up for (#275; docs/operations.md, "Dev environment lifecycle"). Tags the project resource group with `experiment` and `started`. scripts/lib/env.sh requires it for dev. Empty: no tags.')
param experiment string = ''

@description('When the experiment started, UTC (ISO 8601), set by scripts/lib/env.sh.')
param experimentStarted string = ''

// A name outside ADR-0017's closed list fails here, before anything is
// deployed (main.bicepparam checks it at build-params time too).
var env = checkedEnvName(environmentName)
var tags = {
  project: 'usnewsmap'
  environment: environmentName
}
var emails = filter(map(split(alertEmails, ','), e => trim(e)), e => !empty(e))
var suffix = take(uniqueString(subscription().id, env), 6)
// Every resource name (ADR-0017), from infra/naming.bicep. Only the current
// prod (its suffix in its subscription, legacyProdSuffix) keeps its legacy
// names; dev, experiments and a prod rebuilt in another subscription come
// up in ADR-0017's form (#309; dev held nothing when this changed).
var pinnedNames = suffix == legacyProdSuffix
var n = names(env, suffix, pinnedNames)
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
// matter for the API's CORS.
var appName = n.apiApp
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

// The experiment marker is on the project group alone: scripts/dev-status.sh
// reads it there, and a new `started` doesn't touch every resource.
resource rg 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: n.rg
  location: location
  tags: empty(experiment) ? tags : union(tags, { experiment: experiment, started: experimentStarted })
}

resource spotRg 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: n.spotRg
  location: location
  tags: tags
}

resource boincRg 'Microsoft.Resources/resourceGroups@2024-03-01' = if (deployBoincGroup) {
  name: n.boincRg
  location: location
  tags: tags
}

module monitoring 'modules/monitoring.bicep' = {
  scope: rg
  name: 'monitoring'
  params: {
    location: location
    tags: tags
    workspaceName: n.workspace
    appInsightsName: n.appInsights
    actionGroupName: n.actionGroup
    actionGroupShortName: n.actionGroupShortName
    capAlertName: n.logCapAlert
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
    name: n.registry
    ciIdentityId: identities.outputs.ciId
    ciPrincipalId: identities.outputs.ciPrincipalId
    pullPrincipalIds: [identities.outputs.appPrincipalId, identities.outputs.ingestPrincipalId]
  }
}

module network 'modules/network.bicep' = {
  scope: rg
  name: 'network'
  params: {
    location: location
    tags: tags
    name: n.vnet
    addressPrefix: vnetAddressPrefix
    storageServiceEndpoint: ocrNodes
  }
}

module identities 'modules/identities.bicep' = {
  scope: rg
  name: 'identities'
  params: {
    location: location
    tags: tags
    appName: n.apiIdentity
    ingestName: n.ingestIdentity
    ciName: n.ciIdentity
    githubRepo: githubRepo
    githubRepoIds: githubRepoIds
    githubEnvironment: env
  }
}

// The data lake opens to the node IPs only for the on-prem Serial Set
// runner (serialSetOnPrem); the public-network policy then exempts it by
// resource id, like the accounts below. Provision writes the policy's
// exemptions before the stack (policy-exemptions.bicep, #336).
var dataLakeOpen = lakeOpen(serialSetOnPrem, ocrNodeIps)

module storage 'modules/storage.bicep' = {
  scope: rg
  name: 'storage'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: n.dataLake
    allowedIps: dataLakeOpen ? nodeIpList(ocrNodeIps) : []
  }
}

// The on-prem Serial Set runner's grant: curated/congress/serialset/ only.
module serialSetRunnerGrant 'modules/serialset-runner.bicep' = if (serialSetOnPrem && !empty(trim(serialSetRunner))) {
  scope: rg
  name: 'serialset-runner'
  params: {
    storageName: storage.outputs.name
    principalId: trim(serialSetRunner)
    roleLabel: n.serialSetWriterRole
  }
}

// The on-prem BOINC hand-off runner's grants (#469). Needs the exchange
// account (ocrNodes), whose node writer role it reuses on ocr-out.
module boincHandoffRunnerGrant 'modules/boinc-handoff-runner.bicep' = if (ocrNodes && !empty(trim(boincHandoffRunner))) {
  scope: rg
  name: 'boinc-handoff-runner'
  dependsOn: [ocrExchange]
  params: {
    storageName: storage.outputs.name
    exchangeName: exchangeName
    cosmosName: cosmos.outputs.name
    principalId: trim(boincHandoffRunner)
    roleLabel: n.boincLedgerRole
  }
}

// The workspace comes from the BOINC stack (scripts/provision-boinc.sh), a
// later deployment than boincRg: set boincHandoffRunner only once that stack
// is deployed, or this assignment fails on a missing workspace.
module boincHandoffLogs 'modules/boinc-handoff-logs.bicep' = if (deployBoincGroup && !empty(trim(boincHandoffRunner))) {
  scope: resourceGroup(n.boincRg)
  name: 'boinc-handoff-logs'
  dependsOn: [boincRg]
  params: {
    workspaceName: n.boincWorkspace
    principalId: trim(boincHandoffRunner)
  }
}

// The on-prem OCR exchange account (#296, ADR-0015) allows public network
// access behind a firewall, so the public-network policy exempts it by
// resource id, and its assignment goes first.
var exchangeName = n.ocrExchange

module ocrExchange 'modules/ocr-exchange.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-exchange'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: exchangeName
    nodeIps: nodeIpList(ocrNodeIps)
    subnetId: network.outputs.caeSubnetId
    nodeGroupId: trim(ocrNodeGroupId)
    ingestPrincipalId: identities.outputs.ingestPrincipalId
    nodeWriterRoleLabel: n.ocrNodeWriterRole
  }
}

// The scan archive (#442, ADR-0018): Cold-tier copies of the keep-set scans.
// Public network access behind a firewall like the exchange account, so the
// policy exempts it by resource id and its assignment goes first.
module scanArchive 'modules/scan-archive.bicep' = if (ocrNodes) {
  scope: rg
  name: 'scan-archive'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: n.scansAccount
    nodeIps: nodeIpList(ocrNodeIps)
    subnetId: network.outputs.caeSubnetId
    nodeGroupId: trim(ocrNodeGroupId)
    writerRoleLabel: n.scansWriterRole
  }
}

// The OCR nodes' Key Vault (#296, #295): secrets the operator holds for
// setting up nodes (the Red Hat activation key), which onboard.sh reads with
// the operator's sign-in. Nodes have no access. Its firewall allows only the
// node IPs (the operator's home IP), so the public-network policy exempts it
// by resource id like the exchange account, and its assignment goes first.
// Compact name (ADR-0017): usnm{fn}{abbr}{env}{sfx}, at most 24 characters.
var ocrVaultName = n.ocrVault

module ocrVault 'modules/ocr-vault.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-vault'
  dependsOn: [policies]
  params: {
    location: location
    tags: tags
    name: ocrVaultName
    purgeProtection: env == 'prod'
    allowedIps: nodeIpList(ocrNodeIps)
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
// charts against. 0.1 GB to start: about 20 to 45 MB a day per node, VM
// insights included, is estimated before the worker's telemetry, which
// isn't measured yet (ocr-node-monitoring.bicep). Raise it with cap_gb in
// ops/queries/ocr-node-health.kql.
var ocrNodeDailyCapGb = '0.1'

module ocrNodeMonitoring 'modules/ocr-node-monitoring.bicep' = if (ocrNodes) {
  scope: rg
  name: 'ocr-node-monitoring'
  params: {
    location: location
    tags: tags
    workspaceName: n.ocrWorkspace
    appInsightsName: n.ocrAppInsights
    ruleName: n.ocrDcr
    volumeAlertName: n.ocrNodeVolumeAlert
    locHoldAlertName: n.ocrLocHoldAlert
    capAlertName: n.ocrLogCapAlert
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
    name: n.cosmos
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
    blobEndpointName: n.dataEndpoint
    blobEndpointNic: n.dataEndpointNic
    cosmosEndpointName: n.stateEndpoint
    cosmosEndpointNic: n.stateEndpointNic
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
    searchLogRoleLabel: n.searchLogRole
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
    name: n.docIntel
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
    name: n.containerEnv
    infrastructureResourceGroup: n.containerEnvInfraRg
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
    scratchStorageName: nfsAccount ? scratch!.outputs.accountName : ''
    exchangeStorageName: ocrNodes ? ocrExchange!.outputs.name : ''
    scansStorageName: ocrNodes ? scanArchive!.outputs.name : ''
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
    searcherApp: searcherApp
    searcherName: n.searcherApp
    // Warm where the API keeps a replica warm (prod); elsewhere it scales to
    // zero with the API.
    searcherMinReplicas: apiMinReplicas > 0 ? 1 : 0
    appInsightsConnectionString: monitoring.outputs.appInsightsConnectionString
    ingestCron: ingestJobs && useAcr ? ingestCron : ''
    americanStoriesSearch: americanStoriesSearch
    congressSearch: congressSearch
    pageLanguageFilter: pageLanguageFilter
    browsePages: browsePages
  }
}

// What CI may do here: roll the API app (and so the site) onto new images.
module deployer 'modules/deployer.bicep' = {
  scope: rg
  name: 'deployer'
  params: {
    principalId: identities.outputs.ciPrincipalId
    appIdentityName: identities.outputs.appName
    roleLabel: n.deployRole
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
    name: n.archiveEndpoint
    nicName: n.archiveEndpointNic
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
    roleLabel: n.archiveWriterRole
    roleScopeEnv: n.archiveWriterRoleScopeEnv
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
    name: n.scratch
    endpointName: n.scratchEndpoint
    endpointNic: n.scratchEndpointNic
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
    ingestJobName: n.ingestJob
    backfillJobName: n.backfillJob
    jaOcrJobName: n.jaocrJob
    jaAuditJobName: n.jaoneJob
    ocrSyncJobName: n.ocrSyncJob
    cron: ingestCron
    full: ingestFull
    americanStories: americanStories
    jaLatin: jaLatin
    ocrLocal: ocrLocal
    congress: congress
    congressJob: congressJob
    congressJobName: n.congressJob
    serialSetJob: serialSetJob
    serialSetJobName: n.serialSetJob
    partitionDecade: partitionDecade
    singleTextField: singleTextField
    pageLanguage: pageLanguage
    splitDocs: splitDocs
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
    nodeAppNames: n.qwNodeApps
    compareAppNames: n.qwCompareApps
    localAppName: n.qwLocalApp
    benchJobName: n.qwBenchJob
    nodeIdentityName: n.qwNodeIdentity
    benchIdentityName: n.qwBenchIdentity
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
    name: n.budget
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
    workspaceId: monitoring.outputs.workspaceId
    actionGroupId: monitoring.outputs.actionGroupId
    ingestJobName: n.ingestJob
    backfillJobName: n.backfillJob
    jaOcrJobName: n.jaocrJob
    jaAuditJobName: n.jaoneJob
    alertNames: {
      // Every job's failures, under the ingest rule's name (#308).
      jobFailed: n.ingestJobFailedAlert
      ingestNotProgressing: n.ingestNotProgressingAlert
      releaseStalled: n.releaseStalledAlert
      backfillStalled: n.backfillStalledAlert
      backfillReplicaSilent: n.backfillReplicaSilentAlert
      ocrSyncBacklog: n.ocrSyncBacklogAlert
    }
    ocrSync: ocrNodes
    // While a backfill lasts, it runs on USNM_BACKFILL_CRON.
    backfill: !empty(backfillCron)
  }
}

// API server errors and latency, from the requests it exports; search
// capacity (queue wait and replica CPU, #238); availability tests of the
// site and the API on the custom domain once its certificates are bound.
module apiAlerts 'modules/api-alerts.bicep' = if (!empty(emails) && deployApi) {
  scope: rg
  name: 'api-alerts'
  params: {
    location: location
    tags: tags
    workspaceId: monitoring.outputs.workspaceId
    appInsightsId: monitoring.outputs.appInsightsId
    actionGroupId: monitoring.outputs.actionGroupId
    alertNames: {
      serverErrors: n.apiServerErrorsAlert
      aggregateSlow: n.apiAggregateSlowAlert
      searchesSlow: n.apiSearchesSlowAlert
      searchQueueWait: n.apiSearchQueueWaitAlert
      replicaCpu: n.apiReplicaCpuAlert
      siteUnavailable: n.siteUnavailableAlert
    }
    siteWebtestName: n.siteWebtest
    apiAppName: api!.outputs.name
    searcherAppName: api!.outputs.searcherName
    replicaCpu: apiMinReplicas > 0
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
    pipelineTitle: n.pipelineWorkbookTitle
    apiTitle: n.apiWorkbookTitle
    ocrNodesTitle: n.ocrNodesWorkbookTitle
    pipeline: ingestJobs && useAcr
    api: deployApi
    apiAppName: deployApi ? api!.outputs.name : ''
    searcherAppName: deployApi ? api!.outputs.searcherName : ''
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
      // Deny and the exemptions arrive together, in the same assignment
      // update. Provision writes the same value first, on its own
      // (policy-exemptions.bicep), since preflight checks the stack against
      // the assignment as it is before the stack runs (#336).
      '${guardrailPolicyNames.auditPublicAccess}': {
        effect: { value: 'deny' }
        publicAccountIds: {
          value: publicAccountIds(subscription().subscriptionId, n, ocrNodes, dataLakeOpen)
        }
      }
    }
  }
}

// ADR-0019 decision 5: the BOINC server's own group (created above; it holds
// only BOINC resources) gets the same guardrails as the others, so the
// waivers below are meaningful. No public accounts live here.
module boincPolicies 'modules/policy-assignments.bicep' = if (deployPolicies && deployBoincGroup) {
  scope: resourceGroup(n.boincRg)
  name: 'policy-assignments-boinc'
  dependsOn: [boincRg]
  params: {
    definitionIds: guardrailPolicyIds
    definitionParameters: {
      '${guardrailPolicyNames.auditPublicAccess}': { effect: { value: 'deny' } }
    }
  }
}

// The Files mount and Container Apps log configuration need keys. These two
// waivers cover every resource in the BOINC group, for those two policies
// only. Deployed before the BOINC stack, to the group itself.
module boincExemptions 'modules/boinc-exemptions.bicep' = if (deployPolicies && deployBoincGroup) {
  scope: resourceGroup(n.boincRg)
  name: 'boinc-exemptions'
  params: {
    noStorageKeysAssignmentId: resourceId(
      subscription().subscriptionId,
      n.boincRg,
      'Microsoft.Authorization/policyAssignments',
      guardrailPolicyNames.noStorageKeys
    )
    noMonitorKeysAssignmentId: resourceId(
      subscription().subscriptionId,
      n.boincRg,
      'Microsoft.Authorization/policyAssignments',
      guardrailPolicyNames.noMonitorKeys
    )
  }
  dependsOn: [boincPolicies]
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
// The searcher's own app (searcherApp); empty while the searcher is the API's sidecar.
output SEARCHER_APP string = deployApi ? api!.outputs.searcherName : ''
// The domain once its certificate is bound, else the app's own hostname.
output SITE_URL string = !deployApi
  ? ''
  : (!empty(dnsZoneName) && !empty(siteCertificateId) ? 'https://${dnsZoneName}' : 'https://${api!.outputs.fqdn}')
output CONTAINER_ENV_NAME string = containerEnv.outputs.name
// Set these as the domain's name servers at the registrar.
output NAME_SERVERS string = empty(dnsZoneName) ? '' : join(dns!.outputs.nameServers, ' ')
output STORAGE_ACCOUNT string = storage.outputs.name
output COSMOS_ENDPOINT string = cosmos.outputs.endpoint
output INGEST_JOB string = ingestJobs && useAcr ? ingest!.outputs.ingestJobName : ''
output BACKFILL_JOB string = ingestJobs && useAcr ? ingest!.outputs.backfillJobName : ''
output JA_OCR_JOB string = ingestJobs && useAcr ? ingest!.outputs.jaOcrJobName : ''
output JA_AUDIT_JOB string = ingestJobs && useAcr ? ingest!.outputs.jaAuditJobName : ''
// The OCR pickup job (12 §12.5 Step 8); scripts/start-job.sh --job OCR_SYNC_JOB
// runs ocr-enqueue and ocr-requeue as executions of it.
output OCR_SYNC_JOB string = ingestJobs && useAcr ? ingest!.outputs.ocrSyncJobName : ''
// The Congress fetch job (#229); scripts/start-job.sh --job CONGRESS_JOB.
output CONGRESS_JOB string = ingestJobs && useAcr ? ingest!.outputs.congressJobName : ''
// The Serial Set fetch job (#229 phase 2); scripts/start-job.sh --job SERIALSET_JOB.
output SERIALSET_JOB string = ingestJobs && useAcr ? ingest!.outputs.serialSetJobName : ''
output ACR_NAME string = registry.outputs.name
output ACR_LOGIN_SERVER string = registry.outputs.loginServer
// For the GitHub Environment variables CI signs in with (not secrets; scripts/bootstrap.sh writes them).
output CI_CLIENT_ID string = identities.outputs.ciClientId
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
output SCAN_ARCHIVE_ACCOUNT string = ocrNodes ? scanArchive!.outputs.name : ''
output SCAN_ARCHIVE_BLOB_ENDPOINT string = ocrNodes ? scanArchive!.outputs.blobEndpoint : ''
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
