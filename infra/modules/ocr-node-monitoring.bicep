// What the Azure Monitor Agent collects on the on-prem OCR nodes (#296,
// ADR-0015 decision 8): one data collection rule for every node, into the
// environment's workspace. The template doesn't declare the Arc machines
// (the agent creates each one when it connects), so it can't attach the
// rule or install the agent either: scripts/ocr-node/onboard.sh installs
// the Azure Monitor Linux Agent extension on a node and associates the node
// with this rule, and offboard.sh removes both. The agent is the only
// extension a node's Connected Machine agent allows (node-setup.sh).
//
// Collected:
// - Syslog, every facility the system uses, at Warning and above: kernel
//   (OOM kills), systemd (a failed worker unit), sshd, chronyd, podman's
//   containers (their stderr arrives as user.err).
// - The worker, at Info and above: its unit logs to local0
//   (SyslogFacility=local0), which nothing else on the node uses. The
//   worker writes JSON lines to stdout, so journald records all of them,
//   warnings and errors too, at info; a Warning threshold would drop every
//   worker line. They also reach Application Insights as traces while
//   telemetry works; these copies cover a worker that fails before or
//   without it (bad flags, a crash, no Monitoring Metrics Publisher).
// - Performance counters every 60 s: CPU, memory, free disk space and disk
//   throughput, network bytes.
//
// Estimated volume per node (an estimate; measure it with
// ops/queries/ocr-node-health.kql, columns PerfMB and SyslogMB):
// - Perf: on node 1 (4 vCPUs, 3 mounted file systems, eth0 and lo) the
//   counters below give about 20 to 23 rows a sample: 5 processor
//   instances (4 CPUs and the total), 2 memory, 3 disk counters x 3 or 4
//   mounts, 2 network counters x 2 interfaces. Once a minute that is about
//   29,000 to 33,000 rows a day; at roughly 150 to 300 billed bytes a row,
//   4 to 10 MB a day.
// - Syslog, system: node 1 logged 372 lines at Warning and above in the
//   2.6 hours after a reboot during the engine survey (boot messages and
//   the survey containers' stderr); a few hundred to a few thousand rows a
//   day at a few hundred bytes, under 2 MB a day.
// - Syslog, worker: nothing while the queues are empty (empty polls aren't
//   logged); about one line of about 1 KB per page while it works, so 2 to
//   5 MB a day at 2,000 to 5,000 pages.
// - Heartbeat: one row a minute, free.
// So roughly 6 to 17 MB a day per node, under 2% of prod's 1 GB daily cap
// each, and inside the free 5 GB a month with the rest of prod (about
// 1.6 GB a month).
//
// A node can still send far more: a worker in a crash loop, RUST_LOG=debug
// (every crate's debug lines, to Syslog and to Application Insights alike)
// or a compromised node. Everything lands in the workspace, so one node can
// fill the daily cap, and then the environment has no logs, telemetry or log
// alerts until the reset. The alert below fires on a node sending more than
// 25 MB in an hour, over 30 times the estimate's upper end, long before the
// cap.
//
// Table plans: Syslog and Perf stay on Analytics. Syslog can't use Basic
// (it supports Auxiliary) and Perf can't use Auxiliary (it supports Basic).
// Basic and Auxiliary ingestion are billed from the first byte, outside the
// free 5 GB, and queries on them are billed per GB scanned; at this volume
// Analytics costs nothing, keeps log alerts possible and answers queries
// at once. Revisit if the measured volume is far above the estimate.

param location string
param tags object
param name string
@description('The Log Analytics workspace the nodes send to.')
param workspaceId string
@description('Environment name, the suffix of the alert rule\'s name.')
param nameSuffix string
@description('The environment\'s action group; no alert when empty.')
param actionGroupId string = ''

// Warning and above. Every facility the node uses except local0, which is
// the worker's (below). mail, news, uucp and lpr are unused here.
var systemFacilities = ['auth', 'authpriv', 'cron', 'daemon', 'kern', 'syslog', 'user']
var warningAndAbove = ['Warning', 'Error', 'Critical', 'Alert', 'Emergency']
var infoAndAbove = ['Info', 'Notice', 'Warning', 'Error', 'Critical', 'Alert', 'Emergency']

// Linux counter names, each instance (*), as in Microsoft's Linux examples.
// Network bytes are totals since boot; a query takes differences for a
// rate.
var counters = [
  '\\Processor(*)\\% Processor Time'
  '\\Memory\\% Used Memory'
  '\\Memory\\Used Memory MBytes'
  '\\Logical Disk(*)\\% Free Space'
  '\\Logical Disk(*)\\Disk Read Bytes/sec'
  '\\Logical Disk(*)\\Disk Write Bytes/sec'
  '\\Network(*)\\Total Bytes Received'
  '\\Network(*)\\Total Bytes Transmitted'
]

resource rule 'Microsoft.Insights/dataCollectionRules@2023-03-11' = {
  name: name
  location: location
  tags: tags
  kind: 'Linux'
  properties: {
    description: 'On-prem OCR nodes (#296): Syslog at Warning and above, the worker (local0) at Info and above, and CPU, memory, disk and network counters every 60 s.'
    dataSources: {
      syslog: [
        {
          name: 'system'
          streams: ['Microsoft-Syslog']
          facilityNames: systemFacilities
          logLevels: warningAndAbove
        }
        {
          name: 'worker'
          streams: ['Microsoft-Syslog']
          facilityNames: ['local0']
          logLevels: infoAndAbove
        }
      ]
      performanceCounters: [
        {
          name: 'node'
          streams: ['Microsoft-Perf']
          samplingFrequencyInSeconds: 60
          counterSpecifiers: counters
        }
      ]
    }
    destinations: {
      logAnalytics: [
        {
          name: 'workspace'
          workspaceResourceId: workspaceId
        }
      ]
    }
    dataFlows: [
      {
        streams: ['Microsoft-Syslog']
        destinations: ['workspace']
      }
      {
        streams: ['Microsoft-Perf']
        destinations: ['workspace']
      }
    ]
  }
}

// Billed MB per node in the last hour, both paths added up: what the agent
// sent (Syslog and Perf; Heartbeat is free) and the worker's Application
// Insights tables (role usnm-ocr-worker, instance the node). Worker lines go
// both ways, so either path alone could stay under the threshold while the
// node sends twice as much. Each table falls back to an empty one, since a
// table with no rows yet may not exist.
var nodeVolume = '''
let machines = "/providers/microsoft.hybridcompute/machines/";
let node_of = (id: string) { extract(@"(?i)/machines/([^/]+)$", 1, id) };
let worker_role = "usnm-ocr-worker";
let agent = union isfuzzy=true
    (Syslog | where _ResourceId has machines | project Node = node_of(_ResourceId), Size = _BilledSize),
    (Perf | where _ResourceId has machines | project Node = node_of(_ResourceId), Size = _BilledSize),
    (datatable(Node: string, Size: real)[])
    | summarize AgentMB = sum(Size) / 1e6 by Node
    | extend WorkerMB = 0.0;
let worker = union isfuzzy=true
    (AppTraces | where AppRoleName == worker_role | project Node = AppRoleInstance, Size = _BilledSize),
    (AppMetrics | where AppRoleName == worker_role | project Node = AppRoleInstance, Size = _BilledSize),
    (AppDependencies | where AppRoleName == worker_role | project Node = AppRoleInstance, Size = _BilledSize),
    (AppRequests | where AppRoleName == worker_role | project Node = AppRoleInstance, Size = _BilledSize),
    (AppExceptions | where AppRoleName == worker_role | project Node = AppRoleInstance, Size = _BilledSize),
    (datatable(Node: string, Size: real)[])
    | summarize WorkerMB = sum(Size) / 1e6 by Node
    | extend AgentMB = 0.0;
union agent, worker
| summarize AgentMB = sum(AgentMB), WorkerMB = sum(WorkerMB) by Node
| extend MB = AgentMB + WorkerMB
| where MB > 25
| project Node, MB = round(MB, 1), AgentMB = round(AgentMB, 1), WorkerMB = round(WorkerMB, 1)
'''

// Stateful, like the other log alerts: one email when a node starts sending
// too much, resolved when it stops. Split by node, which stays the same
// while it lasts.
resource volumeAlert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = if (!empty(actionGroupId)) {
  name: 'alert-usnm-ocr-node-volume-${nameSuffix}'
  location: location
  tags: tags
  kind: 'LogAlert'
  properties: {
    displayName: 'OCR node log volume high'
    description: 'An on-prem OCR node sent more than 25 MB to the workspace in the last hour (the data collection rule\'s estimate is under 1 MB an hour). Kept up, it fills the daily cap, and then the environment has no logs, telemetry or log alerts until the reset. Node says which; the results split the total into AgentMB (Syslog and Perf from the Azure Monitor Agent) and WorkerMB (the worker\'s Application Insights telemetry). Next: scripts/logs.sh <env> ocr-node-health and ocr-node-errors; on the node, check RUST_LOG in /etc/usnm/ocr-worker.local.env and journalctl -u usnm-ocr-worker; stop the worker (systemctl stop usnm-ocr-worker) if it keeps on.'
    severity: 2
    enabled: true
    evaluationFrequency: 'PT15M'
    windowSize: 'PT1H'
    scopes: [workspaceId]
    // A new workspace may have no Syslog or Perf table until a node first
    // sends.
    skipQueryValidation: true
    autoMitigate: true
    criteria: {
      allOf: [
        {
          query: nodeVolume
          timeAggregation: 'Count'
          operator: 'GreaterThan'
          threshold: 0
          dimensions: [{ name: 'Node', operator: 'Include', values: ['*'] }]
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

output id string = rule.id
output name string = rule.name
