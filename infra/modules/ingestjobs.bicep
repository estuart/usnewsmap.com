// Ingest jobs (04 §4.4, 08 §8.4): the `usnm-ingest` image as Container Apps
// Jobs inside the VNet-integrated environment, so every data path stays on
// the private endpoints with managed identities.
//
// - `caj-usnm-ingest-{env}`: `usnm-ingest run` (enqueue from LoC's listing,
//   curate, titles-sync, release into Quickwit). Weekly on `cron`, or manual
//   when it is empty. This is how batches LoC publishes after the backfill
//   reach the site. The Quickwit writer it starts authenticates with the
//   job's system-assigned identity (Blob Data Contributor on `qw-index`
//   only); the pipeline itself uses `id-usnm-ingest`.
// - `caj-usnm-backfill-{env}`: manual, for the initial corpus (or a large
//   LoC publication); `workers` parallel replicas of `usnm-ingest curate
//   --enqueue`: each enqueues LoC's listing (idempotent, safe in parallel),
//   then claims batches from the Cosmos queue until none are left or its
//   max runtime has passed. LoC's download limit sets the pace: about 2.5
//   days for the full corpus (04 §4.4), so the job is started again after
//   each execution until the queue is empty (or on `backfillCron`, while a
//   backfill lasts). Archives are streamed, so no scratch disk is needed.
// - `caj-usnm-jaocr-{env}` (when `jaOcrImage` is set): manual; `jaOcrReplicas`
//   replicas of ja-ocr/jaocr.py, which OCRs the Japanese pages LoC has no text
//   for with NDLOCR-Lite (04 §4.8, #128). Replicas claim issues through
//   create-only blobs, and pace loc.gov together. Started again until every
//   target issue has a part.
// - `ocrSyncJobName`, `usnm-ocrs-caj-{env}` (when `ocrSync`, with the
//   on-prem OCR nodes): `usnm-ingest ocr-sync` on `ocrSyncCron` (hourly),
//   one replica, as id-usnm-ingest: picks the nodes' output up from the
//   exchange account, checks it and imports it into curated/ocr-local/
//   (12 §12.5 Step 8). Named by ADR-0017 (infra/naming.bicep). With a
//   seed plan (`ocrSeed`), each run then tops `ocr-bulk` up with the
//   pages LoC read worst (12 §12.5 Step 10).
//   `ocr-enqueue`, `ocr-requeue` and `ocr-seed` run as one-off executions of
//   it (scripts/start-job.sh --job OCR_SYNC_JOB), since it has the exchange
//   account's endpoints.
// - `usnm-cong-caj-{env}` (when `congressJob`, named in main.bicep): manual, one replica of
//   `usnm-ingest congress-globe` as id-usnm-ingest (#229, 04 §4.10): fetches
//   the Congressional Globe from UNT's page OCR at UNT's 3-second crawl
//   delay into curated/congress/, keeping every response in the raw store
//   (the archive's `raw`, when set), so an execution stopped at its max
//   runtime resumes from those copies. About 105,000 pages, about 4 days:
//   started again until it reports nothing left. Named by ADR-0017. The
//   ingest job's release indexes what it wrote once `congress` is on.
// - `usnm-sset-caj-{env}` (when `serialSetJob`, named in main.bicep): manual,
//   one replica of `usnm-ingest congress-serialset` as id-usnm-ingest (#229
//   phase 2): reads GovInfo's Serial Set sitemaps, then each document's MODS
//   and PDF about a second apart, reads the PDF with pdftotext (the image's
//   poppler-utils) and deletes it, and writes a curated attempt per volume.
//   The MODS and page texts go to the raw store, so a stopped volume resumes
//   from them. About 222,000 documents and 444,000 requests, about 5 days:
//   started again until it reports nothing left.
// - `caj-usnm-jaone-{env}` (with the jaocr job): manual, one replica, the
//   same image and settings, for jaocr.py's one-off measurements (`mixed`,
//   `american-stories`) that one replica does anyway; a second replica only
//   added a way for the execution to fail. scripts/ja-ocr/start-quality.sh
//   starts those here and `quality` (which shares its chunks) on the jaocr job.
//
// The ingest job's Quickwit writer keeps its data on an NFS share
// (ingest-scratch.bicep) when `scratchStorageName` is set: indexing and
// merging a large index needs more disk than a replica has (08 §8.4).

param location string
param tags object
param environmentId string
param image string
@description('Private registry the image comes from (pulled with the ingest identity).')
param registryServer string
param ingestIdentityId string
param ingestClientId string
param storageAccountName string
param storageBlobEndpoint string
param cosmosEndpoint string
@description('Application Insights connection string: the ingestion endpoint and instrumentation key. The jobs authenticate with id-usnm-ingest (Monitoring Metrics Publisher); the component accepts nothing else.')
param appInsightsConnectionString string
@description('The jobs\' names (infra/naming.bicep); a Container Apps job name is under 32 characters.')
@maxLength(31)
param ingestJobName string
@maxLength(31)
param backfillJobName string
@maxLength(31)
param jaOcrJobName string
@maxLength(31)
param jaAuditJobName string
@maxLength(31)
param ocrSyncJobName string
@description('Weekly schedule for the ingest job (UTC cron). Empty: manual only.')
param cron string = ''
@description('Make every ingest run a full release: a new base from every curated batch, merged (04 §4.7). For a one-off rebuild; turn it off afterwards.')
param full bool = false
@description('Index American Stories\' text with LoC\'s (04 §4.9, #218). The first run with it builds a full base; keep it on afterwards.')
param americanStories bool = false
@description('Give the pages LoC ships without text a main-index document with the Latin-script text our Japanese OCR read on them (04 §4.8, #203).')
param jaLatin bool = false
@description('Weigh our own OCR from curated/ocr-local against LoC\'s text on the pages of the batches a release indexes (12 §12.5 Step 9, #296).')
param ocrLocal bool = false
@description('Give each release the Congress pages curated under curated/congress/ (#229): their own index set, rebuilt when they change.')
param congress bool = false
@description('Deploy the Congress fetch job, `congressJobName` (#229): manual, one replica of `usnm-ingest congress-globe`.')
param congressJob bool = false
@description('The Congress fetch job\'s name (with congressJob), usnm-cong-caj-<env>; a Container Apps job name is under 32 characters (ADR-0017).')
@maxLength(31)
param congressJobName string = ''
@description('Deploy the Serial Set fetch job, `serialSetJobName` (#229 phase 2): manual, one replica of `usnm-ingest congress-serialset`.')
param serialSetJob bool = false
@description('The Serial Set fetch job\'s name (with serialSetJob), usnm-sset-caj-<env>; a Container Apps job name is under 32 characters (ADR-0017).')
@maxLength(31)
param serialSetJobName string = ''
@description('Lay a full rebuild\'s base out by decade (#123, 05 §5.5.5), so date-limited searches skip the other decades\' splits. Only a full base takes it; deltas follow the published version.')
param partitionDecade bool = false
@description('Search a full rebuild\'s texts in one field (#283, 05 §5.5.7). Only a full base takes it; deltas follow the published version.')
param singleTextField bool = false
@description('Give a full rebuild\'s pages their own languages, found from their words, so lang= filters by them (#289, 05 §5.5.8). Only a full base takes it; deltas follow the published version.')
param pageLanguage bool = false
@description('Pages per split of a full rebuild\'s base in place of the template\'s 30,000 (#283); 0: the template\'s. Only a full base takes it; deltas keep the template\'s.')
@minValue(0)
@maxValue(999999)
param splitDocs int = 0
@minValue(1)
@maxValue(32)
param workers int = 8
@description('Schedule for the backfill job (UTC cron), e.g. daily while a backfill lasts. Empty: manual only.')
param backfillCron string = ''
@description('The environment storage (an NFS share) for the writer\'s data; empty: the replica\'s own disk.')
param scratchStorageName string = ''
@description('The share\'s size in GiB: the release refuses to start with less than three quarters of it free.')
param scratchGiB int = 0
@description('An image that runs as root, to hand the share to the pipeline\'s user: the pinned Quickwit image (the ingest image\'s base).')
param rootImage string = ''
@description('Seconds a release waits for its new index\'s merges before failing without publishing (08 §8.4). Budgeted under the 24 h replica timeout below.')
@minValue(600)
// Past 4 h, a full rebuild may no longer fit the ingest job's replica timeout (below).
@maxValue(14400)
param mergeTimeoutSecs int = 14400
@description('A dedicated workload profile to run the ingest job on (the E4 profile for a full rebuild, #172); empty: Consumption.')
param ingestProfile string = ''
@description('The Japanese OCR image (usnewsmap-ja-ocr). Empty: no Japanese OCR job.')
param jaOcrImage string = ''
@minValue(1)
@maxValue(8)
param jaOcrReplicas int = 2
@description('Where curation retains the batch archives it downloads (the `raw` container URL, `retainRaw`); empty keeps none.')
param rawUrl string = ''
@description('Deploy the OCR pickup job, `ocrSyncJobName` (12 §12.5 Step 8). Needs the exchange account (ocrNodes).')
param ocrSync bool = false
@description('The OCR exchange account\'s queue endpoint (OCR_QUEUE_ENDPOINT).')
param ocrQueueEndpoint string = ''
@description('The OCR exchange account\'s blob endpoint (OCR_BLOB_ENDPOINT), with its trailing slash.')
param ocrBlobEndpoint string = ''
@description('Schedule for the OCR pickup job (UTC cron). Empty: manual only.')
param ocrSyncCron string = '17 * * * *'
@description('The OCR seeder\'s settings for the pickup job (12 §12.5 Step 10), by environment variable name: USNM_OCR_SEED_PLAN (empty: no seeding), USNM_OCR_SEED_BATCHES, USNM_OCR_SEED_LANGUAGES, USNM_OCR_SEED_YEARS, USNM_OCR_SEED_MIN_DAMAGE, USNM_OCR_SEED_MAX_PAGES, USNM_OCR_SEED_PILOT, USNM_OCR_SEED_SKIP_AMERICAN_STORIES, USNM_OCR_SEED_SOURCE (damage or fetched), USNM_OCR_SEED_MANIFEST, USNM_OCR_BULK_WINDOW, USNM_OCR_BULK_REFILL_BELOW. Empty values are left out, so the job\'s defaults apply.')
param ocrSeed object = {}

// Both jobs: the platform kills a replica after replicaTimeout, which the
// ingest-job-failed alert reports as a failure. Curation stops claiming at a
// max runtime instead, and a batch already downloading then takes at most 45
// minutes more (the per-batch watchdog), so each command ends on its own.
var replicaTimeoutSecs = 86400
// The ingest job gets 48 h (#172): a full rebuild with common-word pairs
// (#154) sent only about 230 to 460 pages a second on 2026-10-05, so the
// 23.7M pages take up to about 29 h, after titles-sync (at most 8 h) and
// before the merge wait (at most 4 h): about 41 h. A release can't resume
// in a new execution, so a timeout throws the whole rebuild away. The
// platform can interrupt long replicas for maintenance (`replicaRetryLimit`
// is 0), which a longer run is more exposed to.
var ingestReplicaTimeoutSecs = 172800
// Backfill: 22 h + 45 min watchdog = 22 h 45 min, leaving 75 minutes of
// margin under the 24 h timeout for the startup enqueue and the last commit.
var backfillMaxRuntimeSecs = 79200
// Ingest run, with each step's bound counted from the start: curation stops
// claiming at 6 h (6 h 45 min at most with the watchdog), and titles-sync
// sends no request after 8 h (`--titles-max-runtime-secs`; LoC rate limits
// it, and it waits out each one-hour block until then). A full rebuild of
// the corpus then sends 23.8M pages in about 8.8 h (750 pages a second, the
// October 2026 rate) or 10.7 h at 620, and waits at most `mergeTimeoutSecs`
// (4 h; about 1.7 h expected) for its merges, 08 §8.4: 22 h 45 min at the
// slowest at that rate. With common-word pairs the rate halved, so the
// ingest job has its own 48 h timeout (`ingestReplicaTimeoutSecs`). A full run that titles-sync didn't
// finish releases nothing and fails, so the next execution resumes the sync
// before it rebuilds (docs/operations.md, "Full rebuild").
var ingestCurateMaxRuntimeSecs = 21600
var ingestTitlesMaxRuntimeSecs = 28800

var scratch = !empty(scratchStorageName)
// The pipeline runs as uid 10001 (Dockerfile.ingest); a new NFS share's root
// belongs to root.
var scratchDir = '/scratch/usnm'
// The init container's 0.25 vCPU / 0.5 GiB count toward the replica's
// 4 vCPU / 8 GiB on the Consumption profile. On the E4 profile a replica can
// have at most 3.25 vCPU / 26 GiB of the node's 4 vCPU / 32 GiB (the rest is
// the platform's), init container included, so the ingest container gets
// 3.0 vCPU / 25 GiB: the full rebuild with common-word pairs was killed for
// memory at 7.5 GiB (#172).
var consumptionResources = scratch ? { cpu: json('3.75'), memory: '7.5Gi' } : { cpu: json('4.0'), memory: '8Gi' }
var ingestResources = empty(ingestProfile) ? consumptionResources : { cpu: json('3.0'), memory: '25Gi' }
// The writer's tuning on the E4 profile (#172, crates/usnm-ingest/src/sink.rs
// `WriterTuning`): a 6 GiB indexing heap holds a whole 30,000-page split
// (about 2.4 GB of index), so the indexer writes splits near the target size
// and merges little; the ingest queue gets 4 GiB. The commit timeout is
// 120 s: the queue's write-ahead log is only truncated when a commit is
// published, and at 600 s it filled in about 4 minutes (15 to 18 MB/s of
// pages), so Quickwit answered "no shards available" for the rest of each
// 10 minutes and the October 6 rebuild averaged 132 pages/s. About 16 to
// 18 GiB at most with an upload and a merge, of the container's 25 GiB.
// Unset on Consumption: the template's 1 GiB and 30 s, and a 1 GiB queue.
var writerTuningEnv = empty(ingestProfile)
  ? []
  : [
      { name: 'USNM_WRITER_HEAP', value: '6GiB' }
      { name: 'USNM_WRITER_COMMIT_SECS', value: '120' }
      { name: 'USNM_WRITER_QUEUE', value: '4GiB' }
    ]
var scratchEnv = scratch
  ? [
      { name: 'USNM_WORK_DIR', value: scratchDir }
      { name: 'USNM_WORK_MIN_FREE_GIB', value: string(scratchGiB * 3 / 4) }
    ]
  : []

var env = [
  { name: 'USNM_COSMOS_ENDPOINT', value: cosmosEndpoint }
  { name: 'USNM_CURATED_URL', value: '${storageBlobEndpoint}curated' }
  { name: 'USNM_REFERENCE_URL', value: '${storageBlobEndpoint}reference' }
  // Selects id-usnm-ingest at the managed identity endpoint, for the
  // pipeline. The Quickwit writer it starts ignores it: Quickwit 0.9.1's
  // managed identity credential asks for the system-assigned identity.
  { name: 'AZURE_CLIENT_ID', value: ingestClientId }
  { name: 'RUST_LOG', value: 'info' }
  // Traces and metrics to Application Insights, signed with the identity above.
  { name: 'APPLICATIONINSIGHTS_CONNECTION_STRING', value: appInsightsConnectionString }
]
// Both jobs curate: with a raw store, they keep each archive they download.
var rawEnv = empty(rawUrl) ? [] : [{ name: 'USNM_RAW_URL', value: rawUrl }]

resource ingest 'Microsoft.App/jobs@2025-01-01' = {
  name: ingestJobName
  location: location
  tags: tags
  identity: {
    type: 'SystemAssigned,UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: empty(ingestProfile) ? 'Consumption' : ingestProfile
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: empty(cron) ? 'Manual' : 'Schedule'
      manualTriggerConfig: empty(cron) ? { parallelism: 1, replicaCompletionCount: 1 } : null
      scheduleTriggerConfig: empty(cron)
        ? null
        : { cronExpression: cron, parallelism: 1, replicaCompletionCount: 1 }
      // A full rebuild of the corpus can take more than a day (#172).
      replicaTimeout: ingestReplicaTimeoutSecs
      replicaRetryLimit: 0
    }
    template: {
      initContainers: scratch
        ? [
            {
              name: 'scratch-owner'
              image: rootImage
              command: ['/bin/sh']
              args: ['-c', 'mkdir -p ${scratchDir} && chown 10001:10001 ${scratchDir}']
              resources: { cpu: json('0.25'), memory: '0.5Gi' }
              volumeMounts: [{ volumeName: 'scratch', mountPath: '/scratch' }]
            }
          ]
        : null
      containers: [
        {
          name: 'ingest'
          image: image
          args: concat(
            [
              'run'
              '--curate-max-runtime-secs'
              string(ingestCurateMaxRuntimeSecs)
              '--titles-max-runtime-secs'
              string(ingestTitlesMaxRuntimeSecs)
              '--quickwit-bin'
              '/usr/local/bin/quickwit'
              '--quickwit-metastore'
              'azure://qw-index'
              '--quickwit-index-root'
              'azure://qw-index'
            ],
            full ? ['--full'] : [],
            americanStories ? ['--american-stories'] : [],
            jaLatin ? ['--ja-latin'] : [],
            ocrLocal ? ['--ocr-local'] : [],
            congress ? ['--congress'] : [],
            partitionDecade ? ['--partition-decade'] : [],
            singleTextField ? ['--single-text-field'] : [],
            pageLanguage ? ['--page-language'] : [],
            splitDocs > 0 ? ['--split-docs', string(splitDocs)] : []
          )
          resources: ingestResources
          env: concat(env, rawEnv, scratchEnv, writerTuningEnv, [
            { name: 'QW_AZURE_STORAGE_ACCOUNT', value: storageAccountName }
            { name: 'USNM_MERGE_TIMEOUT_SECS', value: string(mergeTimeoutSecs) }
          ])
          volumeMounts: scratch ? [{ volumeName: 'scratch', mountPath: '/scratch' }] : null
        }
      ]
      volumes: scratch ? [{ name: 'scratch', storageType: 'NfsAzureFile', storageName: scratchStorageName }] : null
    }
  }
}

resource backfill 'Microsoft.App/jobs@2025-01-01' = {
  name: backfillJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      // From the environment's settings, so a deployment keeps a backfill
      // schedule instead of resetting the job to manual.
      triggerType: empty(backfillCron) ? 'Manual' : 'Schedule'
      manualTriggerConfig: empty(backfillCron) ? { parallelism: workers, replicaCompletionCount: workers } : null
      scheduleTriggerConfig: empty(backfillCron)
        ? null
        : { cronExpression: backfillCron, parallelism: workers, replicaCompletionCount: workers }
      replicaTimeout: replicaTimeoutSecs
      // A worker that dies leaves its batch leased; another replica or run
      // picks it up once the lease expires.
      replicaRetryLimit: 1
    }
    template: {
      containers: [
        {
          name: 'curate'
          image: image
          args: ['curate', '--enqueue', '--max-runtime-secs', string(backfillMaxRuntimeSecs)]
          // bzip2 decoding is single-threaded: one vCPU per worker.
          resources: { cpu: json('1.0'), memory: '2Gi' }
          env: concat(env, rawEnv)
        }
      ]
    }
  }
}

resource jaOcr 'Microsoft.App/jobs@2025-01-01' = if (!empty(jaOcrImage)) {
  name: jaOcrJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: 'Manual'
      manualTriggerConfig: { parallelism: jaOcrReplicas, replicaCompletionCount: jaOcrReplicas }
      replicaTimeout: replicaTimeoutSecs
      // A replica that dies leaves its issue claimed; the claim goes stale
      // after JAOCR_CLAIM_HOURS and the next run takes it over.
      replicaRetryLimit: 1
    }
    template: {
      containers: [
        {
          name: 'jaocr'
          image: jaOcrImage
          args: ['run']
          // NDLOCR-Lite runs one page at a time on every core; about 1 GB.
          resources: { cpu: json('4.0'), memory: '8Gi' }
          env: [
            { name: 'USNM_CURATED_URL', value: '${storageBlobEndpoint}curated' }
            { name: 'USNM_REFERENCE_URL', value: '${storageBlobEndpoint}reference' }
            { name: 'AZURE_CLIENT_ID', value: ingestClientId }
            { name: 'JAOCR_REPLICAS', value: string(jaOcrReplicas) }
          ]
        }
      ]
    }
  }
}

resource jaAudit 'Microsoft.App/jobs@2025-01-01' = if (!empty(jaOcrImage)) {
  name: jaAuditJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: 'Manual'
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
      replicaTimeout: replicaTimeoutSecs
      // A retried replica takes its run's lock back (ja-ocr/solo.py).
      replicaRetryLimit: 1
    }
    template: {
      containers: [
        {
          name: 'jaocr'
          image: jaOcrImage
          args: ['mixed']
          // Started with its command's arguments (start-quality.sh); the same size as the OCR job.
          resources: { cpu: json('4.0'), memory: '8Gi' }
          env: [
            { name: 'USNM_CURATED_URL', value: '${storageBlobEndpoint}curated' }
            { name: 'USNM_REFERENCE_URL', value: '${storageBlobEndpoint}reference' }
            { name: 'AZURE_CLIENT_ID', value: ingestClientId }
            { name: 'JAOCR_REPLICAS', value: '1' }
          ]
        }
      ]
    }
  }
}

// congress-globe stops between pages at 22 h, leaving 2 h of the 24 h
// replica timeout: an item it stops writes nothing, and the next execution
// reads that item's pages back from the raw store instead of UNT.
var congressMaxRuntimeSecs = 79200

resource congressFetch 'Microsoft.App/jobs@2025-01-01' = if (congressJob) {
  name: congressJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: 'Manual'
      // One replica: UNT asks for one request every 3 seconds, and the
      // Cosmos lock `congress-globe` refuses a second execution anyway.
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
      replicaTimeout: replicaTimeoutSecs
      // Started again by hand; a retry would only repeat a failure.
      replicaRetryLimit: 0
    }
    template: {
      containers: [
        {
          name: 'congress'
          image: image
          args: ['congress-globe', '--max-runtime-secs', string(congressMaxRuntimeSecs)]
          // Mostly waiting on the crawl delay; an item's pages are held in
          // memory until it is written (about 1,500 pages, under 100 MB).
          resources: { cpu: json('0.5'), memory: '1Gi' }
          env: concat(env, rawEnv)
        }
      ]
    }
  }
}

// congress-serialset stops between documents at 22 h, like congress-globe:
// a volume it stops writes nothing, and the next execution reads that
// volume's MODS and page texts back from the raw store instead of GovInfo.
var serialSetMaxRuntimeSecs = 79200

resource serialSetFetch 'Microsoft.App/jobs@2025-01-01' = if (serialSetJob) {
  name: serialSetJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: 'Manual'
      // One replica: about one request a second to GovInfo, and the Cosmos
      // lock `congress-serialset` refuses a second execution anyway.
      manualTriggerConfig: { parallelism: 1, replicaCompletionCount: 1 }
      replicaTimeout: replicaTimeoutSecs
      // Started again by hand; a retry would only repeat a failure.
      replicaRetryLimit: 0
    }
    template: {
      containers: [
        {
          name: 'serialset'
          image: image
          args: ['congress-serialset', '--max-runtime-secs', string(serialSetMaxRuntimeSecs)]
          // Each PDF is streamed to /work (USNM_WORK_DIR) and deleted once
          // pdftotext has read it: a document's PDF is 18.5 MB on average
          // and a few hundred MB at most, within the 4 GiB of ephemeral
          // storage a 1-vCPU replica gets. A volume's pages are held in
          // memory until it is written.
          resources: { cpu: json('1.0'), memory: '2Gi' }
          env: concat(env, rawEnv)
        }
      ]
    }
  }
}

// ocr-sync stops starting imports at 3000 s, leaving 10 minutes of its
// 3600 s replica timeout for the import in hand, the status file and the
// telemetry flush. An hourly run normally takes a few minutes.
var ocrSyncReplicaTimeoutSecs = 3600
var ocrSyncMaxRuntimeSecs = 3000
var ocrSeedEnv = filter(map(items(ocrSeed), s => { name: s.key, value: string(s.value) }), e => !empty(e.value))

resource ocrSyncJob 'Microsoft.App/jobs@2025-01-01' = if (ocrSync) {
  name: ocrSyncJobName
  location: location
  tags: tags
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: { '${ingestIdentityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      registries: [{ server: registryServer, identity: ingestIdentityId }]
      triggerType: empty(ocrSyncCron) ? 'Manual' : 'Schedule'
      manualTriggerConfig: empty(ocrSyncCron) ? { parallelism: 1, replicaCompletionCount: 1 } : null
      scheduleTriggerConfig: empty(ocrSyncCron)
        ? null
        : { cronExpression: ocrSyncCron, parallelism: 1, replicaCompletionCount: 1 }
      replicaTimeout: ocrSyncReplicaTimeoutSecs
      // The next hourly run picks up where a failed one stopped; the Cosmos
      // lock keeps two from overlapping.
      replicaRetryLimit: 0
    }
    template: {
      containers: [
        {
          // Not renamed with the job (usnm-ocrsync-caj-{env} before #309):
          // the alerts and saved queries find its logs by this name.
          name: 'ocrsync'
          image: image
          args: ['ocr-sync', '--max-runtime-secs', string(ocrSyncMaxRuntimeSecs)]
          resources: { cpu: json('0.5'), memory: '1Gi' }
          env: concat(env, [
            { name: 'USNM_OCR_QUEUE_URL', value: ocrQueueEndpoint }
            { name: 'USNM_OCR_OUT_URL', value: '${ocrBlobEndpoint}ocr-out' }
            { name: 'USNM_OCR_QUARANTINE_URL', value: '${ocrBlobEndpoint}ocr-quarantine' }
          ], ocrSeedEnv)
        }
      ]
    }
  }
}

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: storageAccountName

  resource blobs 'blobServices' existing = {
    name: 'default'

    resource index 'containers' existing = {
      name: 'qw-index'
    }
  }
}

var blobContributor = 'ba92f5b4-2d11-453d-a403-e96b0029c9fe'

// The Quickwit writer node: splits and the file-backed metastore, nothing else.
resource writerIndexContributor 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: account::blobs::index
  name: guid(account::blobs::index.id, ingest.id, blobContributor)
  properties: {
    principalId: ingest.identity.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobContributor)
  }
}

output ingestJobName string = ingest.name
output backfillJobName string = backfill.name
output jaOcrJobName string = empty(jaOcrImage) ? '' : jaOcr.name
output jaAuditJobName string = empty(jaOcrImage) ? '' : jaAudit.name
output ocrSyncJobName string = ocrSync ? ocrSyncJob.name : ''
output congressJobName string = congressJob ? congressFetch.name : ''
output serialSetJobName string = serialSetJob ? serialSetFetch.name : ''
