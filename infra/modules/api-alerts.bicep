// Alerts on the public API and site (09 §9.2), email through the
// environment's action group. Deployed with the API when alert emails are
// set.
//
// - API 5xx, aggregate latency and slow searches: log search alerts on the workspace,
//   reading what usnm-api exports to Application Insights: requests
//   (AppRequests, one row per request except the health probes; the
//   request name is "<method> <route template>") and, for slow searches,
//   the api.slow_searches metric (AppMetrics). Stateful: one
//   notification when a condition starts, resolved when it clears. Each
//   splits by a column or two (dimensions) that the email lists: a kind and
//   the route or endpoint with the most of it. Route names are templates
//   ("GET /v1/aggregate"), never paths or query strings, so no search text.
//   Every combination is its own alert, so a second route overtaking the
//   first during an incident sends one more email. Every query reads only
//   rows from the API's own component (its _ResourceId, appInsightsId):
//   anyone with Monitoring Metrics Publisher on a component can write rows
//   under any role name, so a row naming usnm-api from any other component
//   that sends to the workspace can't trip these alerts.
// - Availability: an Application Insights standard test of the site's home
//   page from three US locations, with a metric alert when two of the three
//   fail. The site and the API are one container app, so the home page
//   exercises DNS, TLS, ingress and the app; /readyz is already probed by
//   Container Apps and failing searches raise the 5xx alert. Only when the
//   site's hostname has a bound certificate (the test checks TLS). Results are written by the
//   availability service itself; the component's DisableLocalAuth only
//   restricts what clients send, and Microsoft's list of scenarios that
//   don't work with Entra-only ingestion doesn't include availability tests.
//   Each run is billed ($0.0005 per location per run in East US 2): every
//   15 minutes from 3 locations is about $4 a month.
// - Search capacity (#238): when to revisit a second searcher. The owner
//   deferred a Quickwit cluster on 10 October 2026 until, for a week,
//   either the p90 of api.search_queue_wait_seconds stays above a few
//   seconds (3 s here) or the replica's CPU stays above 3 vCPU outside
//   releases. A log search alert can read at most 2 days, so neither rule
//   sees a week: the queue-wait rule emails once a day while the last two
//   days were both over, and the CPU rule once per episode of a 6-hour
//   average over 3 vCPU; the API workbook's capacity tiles chart both by
//   day over two weeks, which is where the week is judged. Severity 4:
//   planning signals, not incidents.

param location string
param tags object
param nameSuffix string
param workspaceId string
param appInsightsId string
param actionGroupId string

@description('The API\'s container app (ca-usnm-<env>), for the replica CPU alert.')
param apiAppName string

@description('The searcher\'s own app (usnm-search-ca-<env>, #267), whose CPU the replica CPU alert then watches; empty while the searcher is the API\'s sidecar.')
param searcherAppName string = ''

@description('Create the replica CPU alert: the app keeps a replica running (minReplicas above 0), so its CPU has a sample every minute.')
param replicaCpu bool = false

@description('Site home page to test, e.g. https://usnewsmap.com/; empty skips the test.')
param siteUrl string = ''

// A string: the setting arrives as text, and only these values are valid
// for a standard test.
@description('Seconds between availability test runs from each location.')
@allowed(['300', '600', '900'])
param availabilityFrequency string = '900'

// At least 5 server errors in 10 minutes that are also more than 2% of the
// requests. The count floor keeps one or two failures on a quiet site from
// paging; the ratio keeps a handful among many requests from paging. Slow
// searches are left out: a search longer than a visitor waits gets a 202
// and carries on (06 §6.3.5), and the 503s for a search that ran past its
// 2-minute limit (/errors/backend-timeout) or found every computation slot
// taken (/errors/busy) are counted apart. The request span names the
// problem type in usnm.problem. Ten or more of those in 10 minutes still
// raise this alert: a searcher that accepts searches but never answers
// shows up only as timeouts and busy refusals. Fewer are left to the slow
// search alert below. One row for each of the two conditions met, split by
// Kind (which one) and TopRoute (the route and status with the most of that
// kind), so each fires and clears on its own.
var serverErrors = '''
let requests = AppRequests
    | where _ResourceId =~ "__COMPONENT_ID__" and AppRoleName == "usnm-api"
    | extend Problem = tostring(Properties["usnm.problem"])
    | extend Slow = Problem in ("/errors/backend-timeout", "/errors/busy"), Error = toint(ResultCode) >= 500;
let top = (slow: bool) {
    toscalar(requests
        | where Error and Slow == slow
        | summarize n = sum(ItemCount) by Name, ResultCode
        | top 1 by n desc
        | project strcat(Name, " ", ResultCode))
};
let totals = requests
    | summarize Requests = sum(ItemCount),
        Errors = sumif(ItemCount, Error and not(Slow)),
        SlowErrors = sumif(ItemCount, Error and Slow);
union
    (totals | where Errors >= 5 and Errors * 50 > Requests
        | extend Kind = "server errors", TopRoute = top(false)),
    (totals | where SlowErrors >= 10
        | extend Kind = "search timeouts and busy refusals", TopRoute = top(true))
| project Kind, TopRoute
'''

// At least 3 searches in an hour took longer than a visitor waits (outcome
// ok, timeout or error, counted once per search however often its visitor
// asked again), or were refused because every computation slot was taken
// (busy). Cold searches after a new index version is published can trip it.
// Severity 3, checked every 15 minutes; it emails the same action group as
// the other alerts. Split by the endpoint and outcome with the most.
var slowSearches = '''
let slow = AppMetrics
    | where _ResourceId =~ "__COMPONENT_ID__" and AppRoleName == "usnm-api" and Name == "api.slow_searches";
let top = slow
    | summarize n = sum(Sum) by Endpoint = tostring(Properties["endpoint"]), Outcome = tostring(Properties["outcome"])
    | top 1 by n desc;
slow
| summarize Searches = sum(Sum)
| where Searches >= 3
| extend Endpoint = toscalar(top | project Endpoint), Outcome = toscalar(top | project Outcome)
| project Endpoint, Outcome
'''

// p95 of /v1/aggregate above 3 s over 15 minutes, counting only answered
// requests (5xx have their own alert). A cold query takes 2-7 s and a warm
// one about 0.15 s, so a single cold search can push a small sample's p95
// over 3 s. It takes at least 20 requests and at least 3 slower than 3 s
// (so more than one or two cold searches) before this fires. Right after a
// new index version is published every search is cold, so a busy hour then
// can trip it; that is worth knowing but not urgent (severity 3). Not
// split: the route is fixed and the numbers change at every check.
var slowAggregate = '''
AppRequests
| where _ResourceId =~ "__COMPONENT_ID__" and AppRoleName == "usnm-api"
| where Name in ("GET /v1/aggregate", "GET /api/v1/aggregate")
| where toint(ResultCode) < 500
| summarize Requests = sum(ItemCount), Slow = sumif(ItemCount, DurationMs > 3000),
    P95 = percentile(DurationMs, 95)
| where Requests >= 20 and Slow >= 3 and P95 > 3000
'''

// p90 of the wait for a computation slot above 3 s on each of the last two
// days (24-hour spans back from the check, not calendar days), with at
// least 20 searches that needed a slot in each. AppMetrics keeps a
// histogram's sum, count, min and max per export interval (30 s), not its
// buckets, so there is no exact percentile: p90 above 3 s means more than
// 10% of the searches waited over 3 s, and a row's searches count as over
// when the row's mean wait is. At today's traffic a row holds one search
// or a few, so this is close to exact. Searches refused as busy count with
// the time they waited. The rule reads 2 days, the most a log search alert
// can, once a day, and is stateless (Azure doesn't resolve rules checked
// less often than every 12 hours), so it emails every day the condition
// holds: seven in a row is the week #238 asks for.
var queueWait = '''
AppMetrics
| where _ResourceId =~ "__COMPONENT_ID__" and AppRoleName == "usnm-api" and Name == "api.search_queue_wait_seconds"
| where ItemCount > 0
| extend Day = toint((now() - TimeGenerated) / 1d)
| where Day in (0, 1)
| summarize Searches = sum(ItemCount), Over = sumif(ItemCount, Sum / ItemCount > 3.0) by Day
| where Searches >= 20 and Over * 10 > Searches
| summarize Days = count()
| where Days == 2
'''

var rules = [
  {
    name: 'api-server-errors'
    displayName: 'API server errors'
    description: 'usnm-api answered at least 5 requests with a 5xx in 10 minutes, more than 2% of its requests (search timeouts and busy refusals not counted), or at least 10 search timeouts and busy refusals. Kind says which, TopRoute the route and status with the most. Next: scripts/logs.sh --env <env> --query-name api-errors --span 1h for the routes, codes and the API\'s error lines; for timeouts and busy refusals, check the Quickwit sidecar (container quickwit) and whether a new index version was just published.'
    severity: 2
    frequency: 'PT5M'
    window: 'PT10M'
    query: serverErrors
    dimensions: ['Kind', 'TopRoute']
    autoMitigate: true
  }
  {
    name: 'api-aggregate-slow'
    displayName: 'Slow aggregate searches'
    description: 'p95 of /v1/aggregate was over 3 s in the last 15 minutes (at least 20 requests, 3 of them over 3 s). Right after a new index version is published every search is cold, so this can fire and clear on its own. Next: scripts/logs.sh --env <env> --query-name api-requests --span 1h for latency by route, and the API workbook for cache hits and backend time.'
    severity: 3
    frequency: 'PT5M'
    window: 'PT15M'
    query: slowAggregate
    dimensions: []
    autoMitigate: true
  }
  {
    name: 'api-searches-slow'
    displayName: 'Searches longer than a visitor waits'
    description: 'At least 3 searches in the last hour took longer than a visitor waits (USNM_SEARCH_TIMEOUT_SECS, then a 202), ran past the 2-minute limit, or were refused as busy. Endpoint and Outcome are the ones with the most. Errors other than timeouts and busy refusals count toward the API server errors alert. Next: the slow-search tiles in the API workbook (by endpoint and outcome), and scripts/logs.sh --env <env> --query-name api-requests --span 1h.'
    severity: 3
    frequency: 'PT15M'
    window: 'PT1H'
    query: slowSearches
    dimensions: ['Endpoint', 'Outcome']
    autoMitigate: true
  }
  {
    name: 'api-search-queue-wait'
    displayName: 'Search capacity: searches waiting for a slot'
    description: 'On each of the last two days more than 10% of the searches that needed a computation slot waited over 3 s for one (p90 of api.search_queue_wait_seconds above 3 s), with at least 20 such searches a day. Sent once a day while it holds. #238 (deferred 10 October 2026): if it holds for a week, revisit more search capacity (a second searcher, #239, after moving Quickwit out of the API replica, #267). Next: the capacity tiles in the API workbook (daily share over 3 s and p90 estimate), and whether a new index version or a traffic spike explains it.'
    severity: 4
    frequency: 'P1D'
    window: 'P2D'
    query: queueWait
    dimensions: []
    autoMitigate: false
  }
]

resource alert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = [
  for r in rules: {
    name: 'alert-usnm-${r.name}-${nameSuffix}'
    location: location
    tags: tags
    kind: 'LogAlert'
    properties: {
      displayName: r.displayName
      description: r.description
      severity: r.severity
      enabled: true
      evaluationFrequency: r.frequency
      windowSize: r.window
      scopes: [workspaceId]
      // AppRequests exists only once the first request has been exported.
      skipQueryValidation: true
      autoMitigate: r.autoMitigate
      criteria: {
        allOf: [
          {
            // The placeholder becomes the API's component.
            query: replace(r.query, '__COMPONENT_ID__', appInsightsId)
            timeAggregation: 'Count'
            operator: 'GreaterThan'
            threshold: 0
            dimensions: [for d in r.dimensions: { name: d, operator: 'Include', values: ['*'] }]
            failingPeriods: {
              numberOfEvaluationPeriods: 1
              minFailingPeriodsToAlert: 1
            }
          }
        ]
      }
      actions: { actionGroups: [actionGroupId] }
    }
  }
]

var tests = filter(
  [
    { name: 'site', displayName: 'Site home page', url: siteUrl }
  ],
  t => !empty(t.url)
)

// East US (Virginia), North Central US (Chicago), West US (San Jose).
var testLocations = ['us-va-ash-azr', 'us-il-ch1-azr', 'us-ca-sjc-azr']

// The portal lists a test under the component that its hidden-link tag names.
var linkTag = { 'hidden-link:${appInsightsId}': 'Resource' }

resource webtest 'Microsoft.Insights/webtests@2022-06-15' = [
  for t in tests: {
    name: 'webtest-usnm-${t.name}-${nameSuffix}'
    location: location
    tags: union(tags, linkTag)
    kind: 'standard'
    properties: {
      SyntheticMonitorId: 'webtest-usnm-${t.name}-${nameSuffix}'
      Name: t.displayName
      Description: 'GET ${t.url} expects 200 and a certificate valid for 7 more days.'
      Enabled: true
      Frequency: int(availabilityFrequency)
      Timeout: 30
      Kind: 'standard'
      // A failure counts only after three attempts fail in a row.
      RetryEnabled: true
      Locations: [for l in testLocations: { Id: l }]
      Request: {
        RequestUrl: t.url
        HttpVerb: 'GET'
        ParseDependentRequests: false
      }
      ValidationRules: {
        ExpectedHttpStatusCode: 200
        SSLCheck: true
        SSLCertRemainingLifetimeCheck: 7
      }
    }
  }
]

resource unavailable 'Microsoft.Insights/metricAlerts@2018-03-01' = [
  for (t, i) in tests: {
    name: 'alert-usnm-${t.name}-unavailable-${nameSuffix}'
    location: 'global'
    tags: union(tags, linkTag)
    properties: {
      description: '${t.displayName} (${t.url}) failed from at least 2 of 3 locations (each after 3 tries). Next: open the URL yourself; the Availability page of the Application Insights resource shows which check failed (status code, timeout or certificate) and from where; then scripts/logs.sh --env <env> --query-name api-errors --span 1h and the container app\'s revisions and replicas.'
      severity: 1
      enabled: true
      scopes: [webtest[i].id, appInsightsId]
      evaluationFrequency: 'PT1M'
      // At least one run per location in the window.
      windowSize: { '300': 'PT5M', '600': 'PT10M', '900': 'PT15M' }[availabilityFrequency]
      criteria: {
        'odata.type': 'Microsoft.Azure.Monitor.WebtestLocationAvailabilityCriteria'
        webTestId: webtest[i].id
        componentId: appInsightsId
        failedLocationCount: 2
      }
      autoMitigate: true
      actions: [{ actionGroupId: actionGroupId }]
    }
  }
]

// The app the searcher runs in: the API's, or its own (#267).
var cpuAppName = empty(searcherAppName) ? apiAppName : searcherAppName

resource apiApp 'Microsoft.App/containerApps@2024-03-01' existing = {
  name: cpuAppName
}

// The replicas' CPU (each the API and the Quickwit sidecar together, 4 vCPU
// since #251) averaging over 3 vCPU for 6 hours, checked hourly. Not split
// by replica: Azure Monitor averages only the samples a series has, so a
// release's new replica (a series of its own) would be judged on its
// minutes of warm-up alone. The app's one series has a sample from every
// running replica every minute, so a warm-up's minutes are a few of the
// window's hundreds; and only where a replica always runs (prod), since
// after scaling to zero a window could hold only a start's samples.
// Stateful: one email when it starts, one when it clears. Platform metrics
// stay in Azure Monitor 93 days; the API workbook charts them per replica.
// With the searcher in its own app (#267) the alert watches that app: one
// replica of 3.75 vCPU, which the same 3 vCPU threshold leaves 0.75 under.
resource replicaCpuAlert 'Microsoft.Insights/metricAlerts@2018-03-01' = if (replicaCpu) {
  name: 'alert-usnm-api-replica-cpu-${nameSuffix}'
  location: 'global'
  tags: tags
  properties: {
    description: 'The replicas of ${cpuAppName} averaged over 3 vCPU (of ${empty(searcherAppName) ? '4' : '3.75'}) for the last 6 hours. #238 (deferred 10 October 2026): if it keeps happening outside releases for a week, revisit more search capacity (a second searcher, #239, after moving Quickwit out of the API replica, #267). Next: the capacity tiles in the API workbook (replica CPU and the queue wait by day), and scripts/logs.sh --env <env> --query-name api-requests --span 24h for the traffic.'
    severity: 4
    enabled: true
    scopes: [apiApp.id]
    evaluationFrequency: 'PT1H'
    windowSize: 'PT6H'
    criteria: {
      'odata.type': 'Microsoft.Azure.Monitor.SingleResourceMultipleMetricCriteria'
      allOf: [
        {
          criterionType: 'StaticThresholdCriterion'
          name: 'replica-cpu'
          metricNamespace: 'Microsoft.App/containerApps'
          metricName: 'UsageNanoCores'
          timeAggregation: 'Average'
          operator: 'GreaterThan'
          // 3 vCPU in nanocores.
          threshold: 3000000000
        }
      ]
    }
    autoMitigate: true
    actions: [{ actionGroupId: actionGroupId }]
  }
}
