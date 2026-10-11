// The public API and the web app it serves (08 §8.1, ADR-0009).
//
// searchBackend = 'fixtures': the API serves its baked synthetic fixtures
// (memory backend) while exercising the production paths: managed identity,
// the Blob private endpoint (persistent response cache), ingress, probes and
// scaling.
//
// searchBackend = 'quickwit': reference data comes from `reference/` and a
// read-only Quickwit searcher sidecar serves `qw-index/` on localhost (08
// §8.4.1). Quickwit 0.9 authenticates to Blob with Azure's default credential
// chain, which in Container Apps can only use the app's *system-assigned*
// identity, so the app gets one with Blob Data Reader on `qw-index` only
// (spike S-2). Our build of Quickwit (ADR-0013) keeps that credential and
// shares one of it, and one pooled HTTP client, across the process. Needs
// indexes and a current.json published by the ingest jobs.

param location string
param tags object
param name string
param environmentId string
param image string
param identityId string
param identityClientId string
param storageBlobEndpoint string
@description('Cosmos DB endpoint of the pipeline state, read (Data Reader, private endpoint) for the public status page.')
param cosmosEndpoint string
param allowedOrigins array
param storageAccountName string
param minReplicas int
param maxReplicas int = 2
@description('Proxies appending to X-Forwarded-For: 1 = Container Apps ingress only.')
param trustedProxyHops int = 1
@allowed(['fixtures', 'quickwit'])
param searchBackend string = 'fixtures'
@description('Quickwit image for the searcher sidecar, pinned by digest: our build of v0.9.1 or upstream v0.9.1 (infra/quickwit-image.json, ADR-0013).')
param quickwitImage string
@description('Private registry the images come from (pulled with the app identity); empty for a public registry.')
param registryServer string = ''
@description('Application Insights connection string (names the ingestion endpoint; not a credential). Empty: the API exports no telemetry.')
param appInsightsConnectionString string = ''
@description('The ingest job\'s cron schedule (UTC), shown on the status page as the next scheduled run. Empty: the job is started by hand.')
param ingestCron string = ''
@description('Search American Stories\' text on a version built with it. False sets USNM_AMERICAN_STORIES_SEARCH=false: LoC\'s text alone.')
param americanStoriesSearch bool = true
@description('Search the Congress pages on a version with a Congress index set (#229). False sets USNM_CONGRESS_SEARCH=false.')
param congressSearch bool = true
@description('Custom hostnames with their managed certificates, as { name, certificateId }. scripts/bootstrap.sh issues each certificate once DNS is delegated; a name with no certificate yet is left out. Declaring them here keeps a re-deploy from dropping the bindings.')
param customDomains array = []

var quickwit = searchBackend == 'quickwit'

// The searcher sidecar's node config, from infra/quickwit/searcher.yaml.
// CI applies the same file on every deploy (scripts/ci/roll-api.sh), so it
// has one source.
var quickwitConfig = replace(
  loadTextContent('../quickwit/searcher.yaml'),
  '__STORAGE_ACCOUNT__',
  storageAccountName
)

var apiEnv = [
  { name: 'USNM_RESPONSE_CACHE_URL', value: '${storageBlobEndpoint}cache' }
  // The anonymous search log (06 §6.8, ADR-0012).
  { name: 'USNM_SEARCH_LOG_URL', value: '${storageBlobEndpoint}searches' }
  { name: 'USNM_ALLOWED_ORIGINS', value: join(allowedOrigins, ',') }
  { name: 'USNM_TRUSTED_PROXY_HOPS', value: string(trustedProxyHops) }
  // Read-only pipeline state for /v1/status (the name the ingest jobs use).
  { name: 'USNM_COSMOS_ENDPOINT', value: cosmosEndpoint }
  // Selects the user-assigned identity at the managed identity endpoint, for
  // the API's own clients. The sidecar doesn't get it, and wouldn't use it:
  // Quickwit 0.9.1's managed identity credential ignores AZURE_CLIENT_ID and
  // asks for the system-assigned identity (08 §8.2).
  { name: 'AZURE_CLIENT_ID', value: identityClientId }
]
// Requests, traces and metrics go to Application Insights as id-usnm-app,
// with an Entra token (08 §8.1.2).
var telemetryEnv = empty(appInsightsConnectionString)
  ? []
  : [{ name: 'APPLICATIONINSIGHTS_CONNECTION_STRING', value: appInsightsConnectionString }]
// The ingest job's schedule, for /v1/status's next scheduled run.
var scheduleEnv = empty(ingestCron) ? [] : [{ name: 'USNM_INGEST_CRON', value: ingestCron }]
// Only when off, so the default leaves the container's settings as they were.
var americanStoriesEnv = americanStoriesSearch
  ? []
  : [{ name: 'USNM_AMERICAN_STORIES_SEARCH', value: 'false' }]
var congressEnv = congressSearch ? [] : [{ name: 'USNM_CONGRESS_SEARCH', value: 'false' }]
// How long a start stays not ready (06 §6.6). In single-revision mode the
// old revision keeps the traffic until the new one's replicas pass their
// startup and readiness probes, so the API holds /readyz until its startup
// warm-up ends: the cap is that warm-up's budget plus 60 s (the API's own
// default, READY_CAP_MARGIN_SECS in crates/usnm-api/src/config.rs). An app
// that scales to zero has no other replica serving when one starts, and a
// visitor waits on it, so it keeps a 60 s cap. Both settings are set here,
// from the one budget, so the startup probe below always covers the cap.
var startupWarmUpSecs = 300
var holdReadyForWarmUp = minReplicas > 0
var readyCapSecs = holdReadyForWarmUp ? startupWarmUpSecs + 60 : 60
var readyEnv = [
  { name: 'USNM_PREWARM_STARTUP_BUDGET_SECS', value: string(startupWarmUpSecs) }
  { name: 'USNM_READY_CAP_SECS', value: string(readyCapSecs) }
]
// Loading the published version before the warm-up: the API retries while
// the sidecar starts, which its own probe allows 300 s.
var loadSecs = 420
var startupProbeSecs = holdReadyForWarmUp ? 20 : 10

var backendEnv = quickwit
  ? [
      { name: 'USNM_BACKEND', value: 'quickwit' }
      { name: 'USNM_QUICKWIT_URL', value: 'http://127.0.0.1:7280' }
      { name: 'USNM_REFERENCE_URL', value: '${storageBlobEndpoint}reference' }
    ]
  : [{ name: 'USNM_BACKEND', value: 'memory' }]

var apiContainer = {
  name: 'api'
  image: image
  resources: { cpu: json('0.25'), memory: '0.5Gi' }
  env: concat(backendEnv, apiEnv, telemetryEnv, scheduleEnv, americanStoriesEnv, congressEnv, readyEnv)
  probes: [
    {
      // A start loads the published version (loadSecs), then warms the
      // caches: /readyz fails until the warm-up ends or the readiness cap
      // (readyCapSecs) passes (06 §6.6). The probe allows both: 39 × 20 s =
      // 780 s with the warm-up held, 48 × 10 s = 480 s without. The longer
      // period keeps the threshold within the 48 the app has run with (the
      // API spec documents a lower maximum that Container Apps doesn't
      // apply). Failing it restarts the container; liveness and readiness
      // probing begin once it passes, and in single-revision mode the
      // previous revision keeps the traffic until then.
      type: 'Startup'
      httpGet: { path: '/readyz', port: 8080 }
      periodSeconds: startupProbeSecs
      // /readyz allows the sidecar's health check 2 s.
      timeoutSeconds: 3
      // Rounded up, so the window is never shorter than load plus cap.
      failureThreshold: (loadSecs + readyCapSecs + startupProbeSecs - 1) / startupProbeSecs
    }
    {
      type: 'Liveness'
      httpGet: { path: '/healthz', port: 8080 }
      periodSeconds: 30
    }
    {
      type: 'Readiness'
      httpGet: { path: '/readyz', port: 8080 }
      periodSeconds: 10
      timeoutSeconds: 3
    }
  ]
}

var quickwitContainer = {
  name: 'quickwit'
  // Our build pools Blob connections and shares one token credential by
  // default (ADR-0013), so it needs no setting of its own. QW_AZURE_POOL=false
  // in env below would restore upstream's client (a new connection and TLS
  // handshake per request) without a new image; upstream's image ignores it.
  image: quickwitImage
  // The image has no config for this role; write it from the environment.
  command: ['/bin/sh', '-c', 'printf \'%s\\n\' "$USNM_QW_CONFIG" > /tmp/node.yaml && exec quickwit run --config /tmp/node.yaml']
  // 3.75 vCPU / 7.5 GiB: with the api container (0.25 / 0.5) the replica is at the
  // Consumption profile's 4 vCPU / 8 GiB. On the American Stories index the
  // searcher used 2.0 of 2.0 vCPU during cold searches, then 2.2 of 3.75 (#251;
  // the thread counts below).
  resources: { cpu: json('3.75'), memory: '7.5Gi' }
  env: [
    { name: 'USNM_QW_CONFIG', value: quickwitConfig }
    { name: 'QW_DISABLE_TELEMETRY', value: '1' }
    // Thread counts (#251). Quickwit 0.9.1 searches each split on one thread of
    // its rayon "search" pool, sized by RAYON_NUM_THREADS or else Rust's CPU
    // count, which rounds the 3.75 quota down to 3. Its blob downloads, TLS and
    // split opening run on the main tokio runtime, ceil(cpus / 3) threads: 1.
    // Cold searches used 2.2 of 3.75 vCPU with those defaults. QW_NUM_CPUS is
    // its own CPU count (rounded up from k8s syntax), which sizes the
    // small_tasks pool; it doesn't size the search pool. With 4 search and 2
    // runtime threads, the runtime was 96 to 99% busy through a cold-search
    // test while the search pool sat idle (#251, ops/queries/searcher-threads.kql),
    // so the runtime gets 4 too. The pools share the quota: an idle pool uses none.
    { name: 'QW_NUM_CPUS', value: '4' }
    { name: 'RAYON_NUM_THREADS', value: '4' }
    { name: 'QW_TOKIO_RUNTIME_NUM_THREADS', value: '4' }
    // The image sets QW_LISTEN_ADDRESS=0.0.0.0, which overrides the config's
    // listen_address, and Quickwit refuses 0.0.0.0 without an advertise
    // address ("listen address `0.0.0.0` is unspecified"). Pin it here.
    { name: 'QW_LISTEN_ADDRESS', value: '127.0.0.1' }
    // At info, Quickwit logs every search request with its query, which holds
    // the visitor's search text (09 §9.4.2). Those two targets log warnings
    // only; the rest keeps Quickwit's default (quickwit=info, tantivy=warn).
    {
      name: 'RUST_LOG'
      value: 'quickwit=info,quickwit_serve::search_api=warn,quickwit_search=warn,tantivy=warn'
    }
  ]
  probes: [
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
}

resource app 'Microsoft.App/containerApps@2024-03-01' = {
  name: name
  location: location
  tags: tags
  identity: {
    // Quickwit can use only the system-assigned identity (see above).
    type: quickwit ? 'SystemAssigned,UserAssigned' : 'UserAssigned'
    userAssignedIdentities: { '${identityId}': {} }
  }
  properties: {
    environmentId: environmentId
    workloadProfileName: 'Consumption'
    configuration: {
      activeRevisionsMode: 'Single'
      registries: empty(registryServer) ? [] : [{ server: registryServer, identity: identityId }]
      ingress: {
        external: true
        targetPort: 8080
        transport: 'http'
        allowInsecure: false
        customDomains: [
          for d in filter(customDomains, d => !empty(d.certificateId)): {
            name: d.name
            certificateId: d.certificateId
            bindingType: 'SniEnabled'
          }
        ]
      }
    }
    template: {
      containers: quickwit ? [apiContainer, quickwitContainer] : [apiContainer]
      scale: {
        minReplicas: minReplicas
        maxReplicas: maxReplicas
        rules: [
          {
            name: 'http'
            http: { metadata: { concurrentRequests: '50' } }
          }
        ]
      }
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

var blobReader = '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'

// Read-only, and only the index: the sidecar can't write splits or the
// metastore, and can't read anything else.
resource quickwitIndexReader 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (quickwit) {
  scope: account::blobs::index
  name: guid(account::blobs::index.id, app.id, blobReader)
  properties: {
    principalId: app.identity.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', blobReader)
  }
}

output name string = app.name
output fqdn string = app.properties.configuration.ingress.fqdn
