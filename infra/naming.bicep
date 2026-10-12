// Resource names (ADR-0017): the only file that spells out a resource name.
//
// main.bicep imports names() and passes each module the names it creates;
// modules never build a name. ADR-0017's form is usnm-{desc}-{abbr}-{env},
// usnm-{desc}-{abbr}-{env}-{sfx} for a global type that allows hyphens, and
// usnm{fn}{abbr}{env}{sfx} for one that allows only letters and digits.
//
// `pinned` keeps a key's legacy name: each ternary below is a name that
// doesn't follow ADR-0017 yet, listed in infra/names/exceptions.json with
// the phase that renames it. Each rename PR deletes one ternary, and its
// change to infra/names/prod.json is the reviewed rename. A key without a
// ternary already follows ADR-0017 everywhere.
//
// Create-only properties (a private endpoint's NIC name, the Container Apps
// environment's infrastructure group) are '' when pinned: the existing
// resources were created without them, and the module leaves them unset.
//
// scripts/ci/check-names.py evaluates names() through
// infra/names/check.bicepparam (bicep build-params, no Azure) and checks the
// result against ADR-0017's rules, exceptions.json and prod.json.
//
// Not here: the governance names (deployment stacks usnm-{env},
// usnm-guardrails, usnm-archive; the policy definitions and assignments in
// guardrails.bicep), the shared archive (infra/archive/), child resources,
// GUID names and their frozen guid() seeds, and names made outside Bicep
// (the OCR nodes' Entra group, Arc machines). ADR-0017, "Not covered".

// The prod deployment's suffix, take(uniqueString(subscription().id,
// 'prod'), 6) in the current subscription. ADR-0017 pins the legacy names
// to it, so a prod rebuilt in another subscription comes up conventional.
@export()
var legacyProdSuffix = 'ffapzy'

// Hyphenated form: usnm-{desc}-{abbr}-{env}.
@export()
func usnmName(desc string, abbr string, env string) string => 'usnm-${desc}-${abbr}-${env}'

// Hyphenated global form: usnm-{desc}-{abbr}-{env}-{sfx}.
@export()
func usnmGlobalName(desc string, abbr string, env string, sfx string) string => '${usnmName(desc, abbr, env)}-${sfx}'

// Compact form: usnm{fn}{abbr}{env}{sfx}.
@export()
func usnmCompactName(fn string, abbr string, env string, sfx string) string => 'usnm${fn}${abbr}${env}${sfx}'

// Environment names (ADR-0017): prod, dev, or a short-lived experiment x
// plus 1-4 lowercase letters or digits. The same rule as valid_env_name in
// scripts/lib/env.sh.
@export()
func validEnvName(e string) bool =>
  e == 'prod' || e == 'dev' || (length(e) >= 2 && length(e) <= 5 && startsWith(e, 'x') && empty(filter(
    range(1, length(e) - 1),
    i => !contains('abcdefghijklmnopqrstuvwxyz0123456789', substring(e, i, 1))
  )))

// The name itself when valid; otherwise evaluating it fails, at
// `bicep build-params` time in a parameter file and before anything is
// deployed in a template, with the message below.
@export()
func checkedEnvName(e string) string =>
  validEnvName(e)
    ? e
    : string(int('The environment name must be prod, dev or x plus 1-4 lowercase letters or digits (ADR-0017), not "${e}"'))

// Function words (ADR-0017, "Function words"). A new one is added here in
// the PR that needs it, with a line in the ADR's table.
@export()
var functionWords = [
  'api'
  'archive'
  'backfill'
  'ci'
  'cong'
  'core'
  'data'
  'deploy'
  'ingest'
  'jaocr'
  'jaone'
  'ocr'
  'ocrs'
  'pipeline'
  'qw'
  'scratch'
  'search'
  'searchlog'
  'site'
  'sset'
  'state'
]

// Type abbreviations (ADR-0017, "Type abbreviations"): each one's form and
// Azure's length limit for the name. role, scopeEnv, title and shortName are
// display names or parts of them, not resource names (ADR-0017, "Display
// names"): `display: true`. The other abbreviations must equal
// TYPE_ABBREVIATIONS in crates/usnm-api/src/status/sanitize.rs, and the
// compact ones COMPACT_ABBREVIATIONS (scripts/ci/check-names.py checks).
@export()
var typeAbbreviations = {
  ag: { type: 'Microsoft.Insights/actionGroups', form: 'hyphenated', maxLength: 260 }
  alert: { type: 'Microsoft.Insights/scheduledQueryRules or metricAlerts', form: 'hyphenated', maxLength: 260 }
  appi: { type: 'Microsoft.Insights/components', form: 'hyphenated', maxLength: 255 }
  budget: { type: 'Microsoft.Consumption/budgets', form: 'hyphenated', maxLength: 63 }
  ca: { type: 'Microsoft.App/containerApps', form: 'hyphenated', maxLength: 31 }
  caj: { type: 'Microsoft.App/jobs', form: 'hyphenated', maxLength: 31 }
  cae: { type: 'Microsoft.App/managedEnvironments', form: 'hyphenated', maxLength: 32 }
  cosno: { type: 'Microsoft.DocumentDB/databaseAccounts', form: 'global', maxLength: 44 }
  cr: { type: 'Microsoft.ContainerRegistry/registries', form: 'compact', maxLength: 50 }
  dcr: { type: 'Microsoft.Insights/dataCollectionRules', form: 'hyphenated', maxLength: 64 }
  di: { type: 'Microsoft.CognitiveServices/accounts', form: 'global', maxLength: 64 }
  id: { type: 'Microsoft.ManagedIdentity/userAssignedIdentities', form: 'hyphenated', maxLength: 128 }
  kv: { type: 'Microsoft.KeyVault/vaults', form: 'compact', maxLength: 24 }
  log: { type: 'Microsoft.OperationalInsights/workspaces', form: 'hyphenated', maxLength: 63 }
  nic: { type: 'Microsoft.Network/networkInterfaces', form: 'hyphenated', maxLength: 80 }
  pep: { type: 'Microsoft.Network/privateEndpoints', form: 'hyphenated', maxLength: 64 }
  rg: { type: 'Microsoft.Resources/resourceGroups', form: 'hyphenated', maxLength: 90 }
  role: { type: 'Microsoft.Authorization/roleDefinitions roleName', form: 'hyphenated', maxLength: 512, display: true }
  scopeEnv: { type: 'part of a roleName: empty in ADR-0017\'s form', form: 'empty', maxLength: 0, display: true }
  shortName: { type: 'Microsoft.Insights/actionGroups groupShortName', form: 'shortName', maxLength: 12, display: true }
  st: { type: 'Microsoft.Storage/storageAccounts', form: 'compact', maxLength: 24 }
  title: { type: 'Microsoft.Insights/workbooks displayName', form: 'title', maxLength: 260, display: true }
  vnet: { type: 'Microsoft.Network/virtualNetworks', form: 'hyphenated', maxLength: 64 }
  webtest: { type: 'Microsoft.Insights/webtests', form: 'hyphenated', maxLength: 260 }
}

// Each key's type abbreviation, for the check.
@export()
var nameTypes = {
  actionGroup: 'ag'
  actionGroupShortName: 'shortName'
  apiAggregateSlowAlert: 'alert'
  apiApp: 'ca'
  apiIdentity: 'id'
  apiReplicaCpuAlert: 'alert'
  apiSearchQueueWaitAlert: 'alert'
  apiSearchesSlowAlert: 'alert'
  apiServerErrorsAlert: 'alert'
  apiWorkbookTitle: 'title'
  appInsights: 'appi'
  archiveEndpoint: 'pep'
  archiveEndpointNic: 'nic'
  archiveWriterRole: 'role'
  archiveWriterRoleScopeEnv: 'scopeEnv'
  backfillJob: 'caj'
  backfillReplicaSilentAlert: 'alert'
  backfillStalledAlert: 'alert'
  budget: 'budget'
  ciIdentity: 'id'
  congressJob: 'caj'
  containerEnv: 'cae'
  containerEnvInfraRg: 'rg'
  cosmos: 'cosno'
  dataEndpoint: 'pep'
  dataEndpointNic: 'nic'
  dataLake: 'st'
  deployRole: 'role'
  docIntel: 'di'
  ingestIdentity: 'id'
  ingestJob: 'caj'
  ingestJobFailedAlert: 'alert'
  ingestNotProgressingAlert: 'alert'
  jaocrJob: 'caj'
  jaoneJob: 'caj'
  logCapAlert: 'alert'
  ocrAppInsights: 'appi'
  ocrDcr: 'dcr'
  ocrExchange: 'st'
  ocrLocHoldAlert: 'alert'
  ocrLogCapAlert: 'alert'
  ocrNodeVolumeAlert: 'alert'
  ocrNodeWriterRole: 'role'
  ocrNodesWorkbookTitle: 'title'
  ocrSyncBacklogAlert: 'alert'
  ocrSyncJob: 'caj'
  ocrVault: 'kv'
  ocrWorkspace: 'log'
  pipelineWorkbookTitle: 'title'
  qwBenchIdentity: 'id'
  qwBenchJob: 'caj'
  qwCompareApps: 'ca'
  qwLocalApp: 'ca'
  qwNodeApps: 'ca'
  qwNodeIdentity: 'id'
  registry: 'cr'
  releaseStalledAlert: 'alert'
  rg: 'rg'
  scratch: 'st'
  scratchEndpoint: 'pep'
  scratchEndpointNic: 'nic'
  searchLogRole: 'role'
  searcherApp: 'ca'
  serialSetJob: 'caj'
  serialSetWriterRole: 'role'
  siteUnavailableAlert: 'alert'
  siteWebtest: 'webtest'
  spotRg: 'rg'
  stateEndpoint: 'pep'
  stateEndpointNic: 'nic'
  vnet: 'vnet'
  workspace: 'log'
}

// Keys for create-only properties: '' (unset) when pinned.
@export()
var createOnlyKeys = [
  'archiveEndpointNic'
  'containerEnvInfraRg'
  'dataEndpointNic'
  'scratchEndpointNic'
  'stateEndpointNic'
]

@export()
type Names = {
  actionGroup: string
  actionGroupShortName: string
  apiAggregateSlowAlert: string
  apiApp: string
  apiIdentity: string
  apiReplicaCpuAlert: string
  apiSearchQueueWaitAlert: string
  apiSearchesSlowAlert: string
  apiServerErrorsAlert: string
  apiWorkbookTitle: string
  appInsights: string
  archiveEndpoint: string
  archiveEndpointNic: string
  @description('Role labels: the module appends the scope, " ({subscription id, first 8}/{resource group})".')
  archiveWriterRole: string
  @description('Appended to the archive writer role\'s scope: "/{env}" in the legacy roleName, empty in ADR-0017\'s, which has the env in the label.')
  archiveWriterRoleScopeEnv: string
  backfillJob: string
  backfillReplicaSilentAlert: string
  backfillStalledAlert: string
  budget: string
  ciIdentity: string
  congressJob: string
  containerEnv: string
  containerEnvInfraRg: string
  cosmos: string
  dataEndpoint: string
  dataEndpointNic: string
  dataLake: string
  deployRole: string
  docIntel: string
  ingestIdentity: string
  ingestJob: string
  ingestJobFailedAlert: string
  ingestNotProgressingAlert: string
  jaocrJob: string
  jaoneJob: string
  logCapAlert: string
  ocrAppInsights: string
  ocrDcr: string
  ocrExchange: string
  ocrLocHoldAlert: string
  ocrLogCapAlert: string
  ocrNodeVolumeAlert: string
  ocrNodeWriterRole: string
  ocrNodesWorkbookTitle: string
  ocrSyncBacklogAlert: string
  ocrSyncJob: string
  ocrVault: string
  ocrWorkspace: string
  pipelineWorkbookTitle: string
  qwBenchIdentity: string
  qwBenchJob: string
  @description('Search cluster members, by index (searchcluster.bicep allows 4 of each).')
  qwCompareApps: string[]
  qwLocalApp: string
  qwNodeApps: string[]
  qwNodeIdentity: string
  registry: string
  releaseStalledAlert: string
  rg: string
  scratch: string
  scratchEndpoint: string
  scratchEndpointNic: string
  searchLogRole: string
  searcherApp: string
  serialSetJob: string
  serialSetWriterRole: string
  siteUnavailableAlert: string
  siteWebtest: string
  spotRg: string
  stateEndpoint: string
  stateEndpointNic: string
  vnet: string
  workspace: string
}

// One key per resource, sorted. `env` is a valid environment name and
// `sfx` the deployment suffix; main.bicep passes both.
@export()
func names(env string, sfx string, pinned bool) Names => {
  actionGroup: pinned ? 'ag-usnm-${env}' : usnmName('core', 'ag', env)
  actionGroupShortName: pinned ? 'usnm' : 'usnm-${env}'
  apiAggregateSlowAlert: pinned ? 'alert-usnm-api-aggregate-slow-${env}' : usnmName('api-aggregate-slow', 'alert', env)
  apiApp: pinned ? 'ca-usnm-${env}' : usnmName('api', 'ca', env)
  apiIdentity: pinned ? 'id-usnm-app-${env}' : usnmName('api', 'id', env)
  apiReplicaCpuAlert: pinned ? 'alert-usnm-api-replica-cpu-${env}' : usnmName('api-replica-cpu', 'alert', env)
  apiSearchQueueWaitAlert: pinned
    ? 'alert-usnm-api-search-queue-wait-${env}'
    : usnmName('api-search-queue-wait', 'alert', env)
  apiSearchesSlowAlert: pinned ? 'alert-usnm-api-searches-slow-${env}' : usnmName('api-searches-slow', 'alert', env)
  apiServerErrorsAlert: pinned ? 'alert-usnm-api-server-errors-${env}' : usnmName('api-server-errors', 'alert', env)
  apiWorkbookTitle: pinned ? 'usnewsmap API' : 'usnm API (${env})'
  appInsights: pinned ? 'appi-usnm-${env}' : usnmName('core', 'appi', env)
  archiveEndpoint: pinned ? 'pe-usnm-archive-blob' : usnmName('archive-blob', 'pep', env)
  archiveEndpointNic: pinned ? '' : usnmName('archive-blob', 'nic', env)
  archiveWriterRole: pinned ? 'usnm archive writer' : usnmName('archive-writer', 'role', env)
  archiveWriterRoleScopeEnv: pinned ? '/${env}' : ''
  backfillJob: pinned ? 'caj-usnm-backfill-${env}' : usnmName('backfill', 'caj', env)
  backfillReplicaSilentAlert: pinned
    ? 'alert-usnm-backfill-replica-silent-${env}'
    : usnmName('backfill-replica-silent', 'alert', env)
  backfillStalledAlert: pinned ? 'alert-usnm-backfill-stalled-${env}' : usnmName('backfill-stalled', 'alert', env)
  budget: pinned ? 'budget-usnm-${env}' : usnmName('core', 'budget', env)
  ciIdentity: pinned ? 'id-usnm-ci-${env}' : usnmName('ci', 'id', env)
  congressJob: usnmName('cong', 'caj', env)
  containerEnv: pinned ? 'cae-usnm-${env}' : usnmName('core', 'cae', env)
  containerEnvInfraRg: pinned ? '' : usnmName('core-infra', 'rg', env)
  cosmos: pinned ? 'cosmos-usnm-${env}-${sfx}' : usnmGlobalName('state', 'cosno', env, sfx)
  dataEndpoint: pinned ? 'pe-usnm-blob' : usnmName('data-blob', 'pep', env)
  dataEndpointNic: pinned ? '' : usnmName('data-blob', 'nic', env)
  dataLake: pinned ? 'stusnmd${sfx}' : usnmCompactName('data', 'st', env, sfx)
  deployRole: pinned ? 'usnm deployer' : usnmName('deploy', 'role', env)
  docIntel: pinned ? 'di-usnm-${env}-${sfx}' : usnmGlobalName('jaocr', 'di', env, sfx)
  ingestIdentity: pinned ? 'id-usnm-ingest-${env}' : usnmName('ingest', 'id', env)
  ingestJob: pinned ? 'caj-usnm-ingest-${env}' : usnmName('ingest', 'caj', env)
  ingestJobFailedAlert: pinned ? 'alert-usnm-ingest-job-failed-${env}' : usnmName('ingest-job-failed', 'alert', env)
  ingestNotProgressingAlert: pinned
    ? 'alert-usnm-ingest-not-progressing-${env}'
    : usnmName('ingest-not-progressing', 'alert', env)
  jaocrJob: pinned ? 'caj-usnm-jaocr-${env}' : usnmName('jaocr', 'caj', env)
  jaoneJob: pinned ? 'caj-usnm-jaone-${env}' : usnmName('jaone', 'caj', env)
  logCapAlert: pinned ? 'alert-usnm-log-cap-${env}' : usnmName('core-log-cap', 'alert', env)
  ocrAppInsights: usnmName('ocr', 'appi', env)
  ocrDcr: usnmName('ocr', 'dcr', env)
  ocrExchange: pinned ? 'stusnmo${sfx}' : usnmCompactName('ocr', 'st', env, sfx)
  ocrLocHoldAlert: pinned ? 'alert-usnm-ocr-loc-hold-${env}' : usnmName('ocr-loc-hold', 'alert', env)
  ocrLogCapAlert: pinned ? 'alert-usnm-ocr-log-cap-${env}' : usnmName('ocr-log-cap', 'alert', env)
  ocrNodeVolumeAlert: pinned ? 'alert-usnm-ocr-node-volume-${env}' : usnmName('ocr-node-volume', 'alert', env)
  ocrNodeWriterRole: pinned ? 'usnm OCR node writer' : usnmName('ocr-nodewriter', 'role', env)
  ocrNodesWorkbookTitle: 'usnm OCR nodes (${env})'
  // 'ocrs', not 'ocrsync', the pickup job's first name (#386, ADR-0017).
  ocrSyncBacklogAlert: usnmName('ocrs-backlog', 'alert', env)
  ocrSyncJob: usnmName('ocrs', 'caj', env)
  ocrVault: usnmCompactName('ocr', 'kv', env, sfx)
  ocrWorkspace: usnmName('ocr', 'log', env)
  pipelineWorkbookTitle: pinned ? 'usnewsmap pipeline' : 'usnm pipeline (${env})'
  // The search cluster never runs in prod (main.bicep), so these were legacy
  // only while every environment was pinned: ADR-0017's form everywhere.
  qwBenchIdentity: usnmName('qw-bench', 'id', env)
  qwBenchJob: usnmName('qw-bench', 'caj', env)
  qwCompareApps: map(range(0, 4), i => usnmName('qw-cmp-${i}', 'ca', env))
  qwLocalApp: usnmName('qw-local', 'ca', env)
  qwNodeApps: map(range(0, 4), i => usnmName('qw-${i}', 'ca', env))
  qwNodeIdentity: usnmName('qw-node', 'id', env)
  registry: pinned ? 'crusnm${env}${sfx}' : usnmCompactName('core', 'cr', env, sfx)
  releaseStalledAlert: pinned ? 'alert-usnm-release-stalled-${env}' : usnmName('ingest-release-stalled', 'alert', env)
  rg: pinned ? 'rg-usnm-${env}' : usnmName('core', 'rg', env)
  scratch: pinned ? 'stusnms${sfx}' : usnmCompactName('scratch', 'st', env, sfx)
  scratchEndpoint: pinned ? 'pe-usnm-file' : usnmName('scratch-file', 'pep', env)
  scratchEndpointNic: pinned ? '' : usnmName('scratch-file', 'nic', env)
  searchLogRole: pinned ? 'usnm search log writer' : usnmName('searchlog', 'role', env)
  searcherApp: usnmName('search', 'ca', env)
  serialSetJob: usnmName('sset', 'caj', env)
  serialSetWriterRole: usnmName('sset-writer', 'role', env)
  siteUnavailableAlert: pinned ? 'alert-usnm-site-unavailable-${env}' : usnmName('site-unavailable', 'alert', env)
  siteWebtest: pinned ? 'webtest-usnm-site-${env}' : usnmName('site', 'webtest', env)
  // Removed in Phase 1 (ADR-0017): legacy everywhere until then.
  spotRg: 'rg-usnm-${env}-spot'
  stateEndpoint: pinned ? 'pe-usnm-cosmos' : usnmName('state-sql', 'pep', env)
  stateEndpointNic: pinned ? '' : usnmName('state-sql', 'nic', env)
  // Removed in Phase 1 (ADR-0017): legacy everywhere until then.
  vnet: pinned ? 'vnet-usnm-${env}' : usnmName('core', 'vnet', env)
  workspace: pinned ? 'log-usnm-${env}' : usnmName('core', 'log', env)
}
