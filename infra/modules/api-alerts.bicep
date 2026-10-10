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

param location string
param tags object
param nameSuffix string
param workspaceId string
param appInsightsId string
param actionGroupId string

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

var rules = [
  {
    name: 'api-server-errors'
    displayName: 'API server errors'
    description: 'usnm-api answered at least 5 requests with a 5xx in 10 minutes, more than 2% of its requests (search timeouts and busy refusals not counted), or at least 10 search timeouts and busy refusals. Kind says which, TopRoute the route and status with the most. Next: scripts/logs.sh <env> api-errors 1h for the routes, codes and the API\'s error lines; for timeouts and busy refusals, check the Quickwit sidecar (container quickwit) and whether a new index version was just published.'
    severity: 2
    frequency: 'PT5M'
    window: 'PT10M'
    query: serverErrors
    dimensions: ['Kind', 'TopRoute']
  }
  {
    name: 'api-aggregate-slow'
    displayName: 'Slow aggregate searches'
    description: 'p95 of /v1/aggregate was over 3 s in the last 15 minutes (at least 20 requests, 3 of them over 3 s). Right after a new index version is published every search is cold, so this can fire and clear on its own. Next: scripts/logs.sh <env> api-requests 1h for latency by route, and the API workbook for cache hits and backend time.'
    severity: 3
    frequency: 'PT5M'
    window: 'PT15M'
    query: slowAggregate
    dimensions: []
  }
  {
    name: 'api-searches-slow'
    displayName: 'Searches longer than a visitor waits'
    description: 'At least 3 searches in the last hour took longer than a visitor waits (USNM_SEARCH_TIMEOUT_SECS, then a 202), ran past the 2-minute limit, or were refused as busy. Endpoint and Outcome are the ones with the most. Errors other than timeouts and busy refusals count toward the API server errors alert. Next: the slow-search tiles in the API workbook (by endpoint and outcome), and scripts/logs.sh <env> api-requests 1h.'
    severity: 3
    frequency: 'PT15M'
    window: 'PT1H'
    query: slowSearches
    dimensions: ['Endpoint', 'Outcome']
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
      autoMitigate: true
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
      description: '${t.displayName} (${t.url}) failed from at least 2 of 3 locations (each after 3 tries). Next: open the URL yourself; the Availability page of the Application Insights resource shows which check failed (status code, timeout or certificate) and from where; then scripts/logs.sh <env> api-errors 1h and the container app\'s revisions and replicas.'
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
