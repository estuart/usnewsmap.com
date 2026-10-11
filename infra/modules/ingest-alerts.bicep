// Log search alerts on the ingest and backfill jobs (09 §9.2), and one on the
// Japanese OCR jobs (jaocr-job-failed), evaluated on
// the Log Analytics workspace against the resource-specific Container Apps
// tables (ContainerAppConsoleLogs, ContainerAppSystemLogs). Severity 2
// (the progress check and the Japanese OCR jobs 3), email through the environment's action group.
// Stateful: one notification when a condition starts, resolved when it
// clears.
//
// Each rule splits by a few columns (dimensions), which the email lists, so
// it says which execution and why. Every dimension combination is its own
// alert, fired and resolved apart, so the columns are ones that stay the
// same while a condition lasts (the execution, a reason, a time that has
// passed), never a count that changes at each check.
//
// The queries read the JSON lines usnm-ingest writes ("command failed",
// "building index", "release progress", "published", "curated", "claimed",
// "curate progress", "curation finished"); change them together with the
// log messages. Lines that could carry the managed
// identity endpoint's secret are dropped first.

param location string
param tags object
param nameSuffix string
param workspaceId string
param actionGroupId string
@description('Alert on the OCR pickup job, usnm-ocrsync-caj-<env> (with the on-prem OCR nodes, 12 §12.5 Step 8).')
param ocrSync bool = false

// Every error exit of usnm-ingest ends with one "command failed" line: the
// reliable signal, with the error text. A replica killed without writing it
// (out of memory, the 24 h replica timeout, a failed image pull) shows only
// in the platform's events: the job's backoff or deadline, or a container
// that exited non-zero.
//
// Left out: a full run that stopped because titles-sync left titles
// (`outcome: titles_left` on the line; images from before that field are
// matched by the error's last words). That stop is expected while LoC rate
// limits title records, and starting the job again continues it; the
// ingest-not-progressing rule watches for it repeating without progress.
// The platform's events for a replica that wrote its own line, and the
// job's backoff event right after one, only repeat it and are left out too.
//
// Split by Execution (the job execution, from the replica's name; the job
// for a job-level event that names none) and Reason: the error, cut to 180
// characters, with URL query strings, key=value secrets and long opaque
// tokens masked. The window is 30 minutes so that the line a platform event
// repeats is still in it; only the last 15 minutes are reported.
var jobFailed = '''
let lines = ContainerAppConsoleLogs
    | where ContainerName in ("ingest", "curate")
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Log has "failed"
    | extend j = parse_json(Log)
    | where tostring(j.level) == "ERROR" and tostring(j.fields.message) == "command failed"
    | extend Detail = tostring(j.fields.error), Outcome = tostring(j.fields.outcome)
    | extend Expected = Outcome == "titles_left"
        or (isempty(Outcome) and Detail has "nothing was released: start the job again")
    | project TimeGenerated, Job = coalesce(ContainerAppName, JobName), Replica = ContainerGroupName,
        Detail, Expected;
let platform = ContainerAppSystemLogs
    | where JobName startswith "caj-usnm-ingest-" or JobName startswith "caj-usnm-backfill-"
        or ContainerAppName startswith "caj-usnm-ingest-" or ContainerAppName startswith "caj-usnm-backfill-"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Reason in ("BackoffLimitExceeded", "DeadlineExceeded")
        or (Log contains "terminated with exit code" and Log !contains "exit code '0'")
    | project TimeGenerated, Job = coalesce(JobName, ContainerAppName), Replica = ReplicaName,
        Detail = strcat(Reason, ": ", Log), Backoff = Reason == "BackoffLimitExceeded";
let replicaEvents = platform
    | where isnotempty(Replica)
    | join kind=leftanti lines on Replica;
let ends = union (lines | project TimeGenerated, Job), (replicaEvents | project TimeGenerated, Job);
let jobEvents = platform
    | where isempty(Replica)
    | extend k = 1
    | join kind=leftouter (ends | project EndAt = TimeGenerated, EndJob = Job, k = 1) on k
    | summarize Explained = countif(EndJob == Job and EndAt between ((TimeGenerated - 5m) .. TimeGenerated))
        by TimeGenerated, Job, Replica, Detail, Backoff
    | where not(Backoff and Explained > 0);
union (lines | where not(Expected)), replicaEvents, jobEvents
| where TimeGenerated > ago(15m)
| extend Execution = iff(isempty(Replica), Job, extract(@"^(.+)-[a-z0-9]+$", 1, Replica))
| extend Reason = replace_regex(Detail, @"\?[^\s'\x22]+", "?…")
| extend Reason = replace_regex(Reason, @"(?i)(sig|signature|token|secret|password|key|code)=[^\s&,;]+", @"\1=***")
| extend Reason = replace_regex(Reason, @"[A-Za-z0-9+/_=-]{40,}", "***")
| extend Reason = iff(strlen(Reason) > 180, strcat(substring(Reason, 0, 179), "…"), Reason)
| project TimeGenerated, Execution, Reason
'''

// The Japanese OCR jobs (caj-usnm-jaocr-*, and caj-usnm-jaone-* for its
// one-off measurements) are watched apart, at severity 3: a failed OCR or
// audit run is retried by starting it again and touches no published index,
// so it mustn't read as an ingest failure. A replica that died (non-zero
// exit: out of memory, a crash; not one the platform stopped because another
// replica failed), the job's backoff or deadline, and the first line of any
// Python traceback the job printed (the job's own JSON lines with an "error"
// field are reported, not failures); split by Execution and Reason as above.
var jaOcrJobFailed = '''
let platform = ContainerAppSystemLogs
    | where JobName startswith "caj-usnm-jaocr-" or JobName startswith "caj-usnm-jaone-"
        or ContainerAppName startswith "caj-usnm-jaocr-" or ContainerAppName startswith "caj-usnm-jaone-"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Reason in ("BackoffLimitExceeded", "DeadlineExceeded")
        or (Log contains "terminated with exit code" and Log !contains "exit code '0'" and Log !contains "ManuallyStopped")
    | project TimeGenerated, Job = coalesce(JobName, ContainerAppName), Replica = ReplicaName,
        Detail = strcat(Reason, ": ", Log);
let crashes = ContainerAppConsoleLogs
    | where ContainerName == "jaocr"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Log startswith_cs "Traceback"
    | project TimeGenerated, Job = coalesce(ContainerAppName, JobName), Replica = ContainerGroupName, Detail = Log;
union platform, crashes
| where TimeGenerated > ago(15m)
| extend Execution = iff(isempty(Replica), Job, extract(@"^(.+)-[a-z0-9]+$", 1, Replica))
| extend Reason = replace_regex(Detail, @"\?[^\s'\x22]+", "?…")
| extend Reason = replace_regex(Reason, @"(?i)(sig|signature|token|secret|password|key|code)=[^\s&,;]+", @"\1=***")
| extend Reason = replace_regex(Reason, @"[A-Za-z0-9+/_=-]{40,}", "***")
| extend Reason = iff(strlen(Reason) > 180, strcat(substring(Reason, 0, 179), "…"), Reason)
| project TimeGenerated, Execution, Reason
'''

// The safety net for the stop left out above. Over the last 2 days (the
// longest window a log alert reads), the ingest job's titles-sync stops
// since its last other ending (a "published" line, or any other "command
// failed"), and either:
// - "not started again": the last stop was 3 hours ago or more and the
//   ingest job has logged nothing since, so no execution picked it up; or
// - "no titles progress": 3 or more stops in a row and the last left as
//   many titles as the first (each stop says "N of M titles left").
// Split by Condition, Execution (the last stop's) and Detail; the detail
// changes only with a new stop.
var ingestNotProgressing = '''
let ingest = ContainerAppConsoleLogs
    | where ContainerName == "ingest"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET";
let ends = ingest
    | where Log has_any ("failed", "published")
    | extend j = parse_json(Log)
    | extend Message = tostring(j.fields.message), Detail = tostring(j.fields.error)
    | where Message in ("command failed", "published")
    | extend TitlesStop = Message == "command failed"
        and (tostring(j.fields.outcome) == "titles_left"
            or (isempty(tostring(j.fields.outcome)) and Detail has "nothing was released: start the job again"))
    | project TimeGenerated, TitlesStop, Execution = extract(@"^(.+)-[a-z0-9]+$", 1, ContainerGroupName),
        Left = toint(extract(@"(\d+) of \d+ titles", 1, Detail));
let reset = toscalar(ends | where not(TitlesStop) | summarize max(TimeGenerated));
let lastLine = toscalar(ingest | summarize max(TimeGenerated));
let stops = ends | where TitlesStop and (isnull(reset) or TimeGenerated > reset);
let oldest = stops | top 1 by TimeGenerated asc;
let newest = stops | top 1 by TimeGenerated desc;
stops
| summarize Stops = count(), Last = max(TimeGenerated)
| extend FirstLeft = toscalar(oldest | project Left), LastLeft = toscalar(newest | project Left),
    Execution = toscalar(newest | project Execution)
| extend Condition = case(
    Last < ago(3h) and lastLine < Last + 2m, "not started again",
    Stops >= 3 and LastLeft >= FirstLeft, "no titles progress",
    "")
| where isnotempty(Condition)
| extend Detail = strcat(Stops, " titles-sync stops in a row; ", LastLeft, " titles left at the last, ",
    FirstLeft, " at the first; the last ended ", format_datetime(Last, "yyyy-MM-dd HH:mm"), " UTC")
| project Condition, Execution, Detail
'''

// A release that started ("building index") and hasn't ended ("published"
// or "command failed") but has logged no "release progress" line for 10
// minutes: the job hung, or died without a word. Progress is logged every
// 30 s while the index builds. The window is two days, the ingest job's
// replica timeout (48 h, #172): a full release can take most of it, and a
// shorter window would lose a release's "building index" line while it
// still runs (04 §4.1.1). Split by
// Execution and Detail: the version, when it started, and the last
// progress line's time and pages sent (fixed while it is stalled).
var releaseStalled = '''
let lines = ContainerAppConsoleLogs
    | where ContainerName == "ingest"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Log has_any ("building", "progress", "published", "failed")
    | extend j = parse_json(Log)
    | extend Message = tostring(j.fields.message), Replica = ContainerGroupName;
let started = lines | where Message == "building index"
    | summarize arg_max(TimeGenerated, j) by Replica
    | project Replica, Started = TimeGenerated, Version = tostring(j.fields.version);
let ended = lines | where Message in ("published", "command failed") | summarize Ended = max(TimeGenerated) by Replica;
let progress = lines | where Message == "release progress"
    | summarize arg_max(TimeGenerated, j) by Replica
    | project Replica, LastProgress = TimeGenerated, Sent = tolong(j.fields.docs_sent), Expected = tolong(j.fields.docs_expected);
started
| join kind=leftouter ended on Replica
| join kind=leftouter progress on Replica
| where isnull(Ended) or Ended < Started
| where Started < ago(10m)
| where isnull(LastProgress) or LastProgress < ago(10m)
| extend Execution = extract(@"^(.+)-[a-z0-9]+$", 1, Replica)
| extend Detail = iff(isnull(LastProgress),
    strcat("building ", Version, " since ", format_datetime(Started, "yyyy-MM-dd HH:mm"), " UTC; no progress line yet"),
    strcat("building ", Version, " since ", format_datetime(Started, "yyyy-MM-dd HH:mm"), " UTC; last progress ",
        format_datetime(LastProgress, "yyyy-MM-dd HH:mm"), " UTC, ", Sent, " of ", Expected, " pages sent"))
| project Execution, Detail
'''

// Backfill workers have been running for the last hour (they logged in its
// first 15 minutes and in its last 15) but none has curated a batch in it.
// Not while LoC's rate limit holds downloads (a "throttled" line: every
// worker waits an hour). Workers that keep logging "curate progress" without
// finishing a batch are caught here; one that stops logging is caught by
// backfillReplicaSilent. Not split: its only columns are counts, which
// change at every check.
var backfillStalled = '''
let logs = ContainerAppConsoleLogs
    | where ContainerName == "curate"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | extend Message = tostring(parse_json(Log).fields.message);
let early = toscalar(logs | where TimeGenerated between (ago(60m) .. ago(45m)) | count);
let late = toscalar(logs | where TimeGenerated > ago(15m) | count);
let curated = toscalar(logs | where TimeGenerated > ago(60m) | where Message == "curated" | count);
let throttled = toscalar(logs | where TimeGenerated > ago(75m) | where Message startswith "throttled" | count);
print Early = early, Late = late, Curated = curated, Throttled = throttled
| where Early > 0 and Late > 0 and Curated == 0 and Throttled == 0
'''

// One backfill replica has logged nothing for 15 minutes, although it hasn't
// ended: no "curation finished" or "command failed" line, no platform event
// that stopped the replica, and no stop of its execution. Job-level stop
// events don't name the execution, so each is matched to the latest
// execution that started before it (the replica name is the execution's
// name plus a suffix). A worker logs "curate progress" every minute while it
// holds a batch (and a batch it can't finish in 45 minutes is abandoned with
// a "curation timed out" line), so 15 minutes of silence means the process or
// its runtime is stuck, or its logs stopped arriving. Only replicas that
// have logged "claimed" are checked: images from before the heartbeat were
// silent for minutes by design. The window is a day, the job's replica
// timeout. Split by Replica and Detail: its last line's time, message and
// batch (fixed while it is silent).
var backfillReplicaSilent = '''
let lines = ContainerAppConsoleLogs
    | where ContainerName == "curate"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | extend j = parse_json(Log)
    | extend Message = tostring(j.fields.message), Replica = ContainerGroupName,
        Batch = coalesce(tostring(j.fields.batch), tostring(j.span.batch));
let replicas = lines
    | summarize
        FirstLine = min(TimeGenerated),
        Watched = countif(Message in ("claimed", "curate progress")),
        Ended = countif(Message in ("curation finished", "command failed")),
        arg_max(TimeGenerated, LastMessage = Message, LastBatch = Batch)
        by Replica
    | extend LastLine = TimeGenerated
    | extend Execution = extract(@"^(.+)-[a-z0-9]+$", 1, Replica);
let platform = ContainerAppSystemLogs
    | where JobName startswith "caj-usnm-backfill-"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET";
let replicaStops = platform
    | where Reason in ("ContainerTerminated", "PodDeletion", "ProcessExited", "SuccessfulDelete")
    | extend Replica = iff(isnotempty(ReplicaName), ReplicaName, extract(@"(caj-usnm-backfill-[a-z0-9-]+)", 1, Log))
    | summarize by Replica;
let executions = replicas | summarize Started = min(FirstLine) by Execution | extend k = 1;
let executionStops = platform
    | where Reason in ("Suspended", "DeadlineExceeded", "BackoffLimitExceeded")
    | project StopAt = TimeGenerated, k = 1
    | join kind=inner executions on k
    | where Started <= StopAt
    | summarize arg_max(Started, Execution) by StopAt
    | summarize by Execution;
replicas
| where Watched > 0 and Ended == 0
| where LastLine < ago(15m)
| join kind=leftanti replicaStops on Replica
| join kind=leftanti executionStops on Execution
| extend Detail = substring(strcat("last line ", format_datetime(LastLine, "yyyy-MM-dd HH:mm"), " UTC: ", LastMessage,
    iff(isempty(LastBatch), "", strcat(" (batch ", LastBatch, ")"))), 0, 180)
| project Replica, LastLine, Detail
'''

var rules = [
  {
    name: 'ingest-job-failed'
    displayName: 'Ingest or backfill job failed'
    description: 'An ingest or backfill replica exited with an error in the last 15 minutes; Execution and Reason in this email say which and why. Expected titles-sync stops are left out (ingest-not-progressing watches them). Next: scripts/logs.sh <env> job-executions 1d for how each replica ended, and errors-by-batch 1d for the warnings before it. Fix the cause, then start the job again; the next run picks up where this one stopped.'
    severity: 2
    frequency: 'PT5M'
    // Azure accepts only 5, 10, 15, 30, 45, 60 min and up; the query itself reports the last 15 min,
    // and the extra lookback lets it see the 5 minutes before an event.
    window: 'PT30M'
    query: jobFailed
    dimensions: ['Execution', 'Reason']
  }
  {
    name: 'jaocr-job-failed'
    displayName: 'Japanese OCR or audit job failed'
    description: 'A replica of the Japanese OCR job (caj-usnm-jaocr) or its one-off audit job (caj-usnm-jaone) died or printed a traceback in the last 15 minutes; Execution and Reason say which and why. It touches no published index. Next: scripts/ja-ocr/quality-rows.sh <env> <execution> or the job\'s console logs for its last lines; fix the cause and start the run again (OCR resumes from its claims; audits rerun).'
    severity: 3
    frequency: 'PT5M'
    window: 'PT30M'
    query: jaOcrJobFailed
    dimensions: ['Execution', 'Reason']
  }
  {
    name: 'ingest-not-progressing'
    displayName: 'Ingest not progressing'
    description: 'The ingest job keeps stopping at the titles-sync deadline: Condition says how. "not started again": the last stop was 3 or more hours ago and no execution has run since; start the job (and check whatever restarts it). "no titles progress": 3 stops in a row left as many titles as the first; LoC may be refusing title records. Next: scripts/logs.sh <env> ingest-endings 2d for each stop and the titles left, and /v1/status (titles.pipeline.awaiting_sync).'
    severity: 3
    frequency: 'PT1H'
    window: 'P2D'
    query: ingestNotProgressing
    dimensions: ['Condition', 'Execution', 'Detail']
  }
  {
    name: 'release-stalled'
    displayName: 'Release stalled'
    description: 'A release is building an index but has logged no progress for 10 minutes; Execution and Detail say which version and where it stopped. Next: scripts/logs.sh <env> release-progress for the last lines (disk, memory, Quickwit retries) and job-executions to see if the replica is still running.'
    severity: 2
    frequency: 'PT15M'
    window: 'P2D'
    query: releaseStalled
    dimensions: ['Execution', 'Detail']
  }
  {
    name: 'backfill-stalled'
    displayName: 'Backfill stalled'
    description: 'Backfill workers have run for the last hour but none has curated a batch in it (and LoC is not rate limiting). Next: scripts/logs.sh <env> curation-throughput for the recent work, curate-replicas for each worker\'s stage, and errors-by-batch 2h for repeated failures on the same batches.'
    severity: 2
    frequency: 'PT15M'
    window: 'PT2H'
    query: backfillStalled
    dimensions: []
  }
  {
    name: 'backfill-replica-silent'
    displayName: 'Backfill replica silent'
    description: 'A backfill replica that is still running has logged nothing for 15 minutes; a working one logs curate progress every minute. Replica and Detail say which and its last line. Next: scripts/logs.sh <env> curate-replicas for each replica\'s last stage and heartbeat; stop the execution if it is stuck (its batch lease expires and another worker retries it).'
    severity: 2
    frequency: 'PT5M'
    window: 'P1D'
    query: backfillReplicaSilent
    dimensions: ['Replica', 'Detail']
  }
]

// The OCR pickup job (usnm-ocrsync-caj-*, 12 §12.5 Step 8), at severity 3:
// a failed pickup touches no published index, and the next hourly run
// continues it. Its "command failed" line, or the platform's events for a
// replica that died without one. Split by Execution and Reason as above.
var ocrSyncFailed = '''
let lines = ContainerAppConsoleLogs
    | where ContainerName == "ocrsync"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Log has "failed"
    | extend j = parse_json(Log)
    | where tostring(j.level) == "ERROR" and tostring(j.fields.message) == "command failed"
    | project TimeGenerated, Job = coalesce(ContainerAppName, JobName), Replica = ContainerGroupName,
        Detail = tostring(j.fields.error);
let platform = ContainerAppSystemLogs
    | where JobName startswith "usnm-ocrsync-caj-" or ContainerAppName startswith "usnm-ocrsync-caj-"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Reason in ("BackoffLimitExceeded", "DeadlineExceeded")
        or (Log contains "terminated with exit code" and Log !contains "exit code '0'")
    | project TimeGenerated, Job = coalesce(JobName, ContainerAppName), Replica = ReplicaName,
        Detail = strcat(Reason, ": ", Log)
    | join kind=leftanti lines on Replica;
union lines, platform
| where TimeGenerated > ago(15m)
| extend Execution = iff(isempty(Replica), Job, extract(@"^(.+)-[a-z0-9]+$", 1, Replica))
| extend Reason = replace_regex(Detail, @"\?[^\s'\x22]+", "?…")
| extend Reason = replace_regex(Reason, @"(?i)(sig|signature|token|secret|password|key|code)=[^\s&,;]+", @"\1=***")
| extend Reason = replace_regex(Reason, @"[A-Za-z0-9+/_=-]{40,}", "***")
| extend Reason = iff(strlen(Reason) > 180, strcat(substring(Reason, 0, 179), "…"), Reason)
| project TimeGenerated, Execution, Reason
'''

// Node output waiting in ocr-out for more than 72 hours: the pickup's
// last "ocr sync finished" line in the last 3 hours reports the oldest
// part it left (oldest_age_s), from the parts' creation times on the
// server. Parts left that long are refused by nothing and taken by
// nothing: a run that keeps stopping at its max runtime, or a pickup
// that can't read them.
var ocrSyncBacklog = '''
ContainerAppConsoleLogs
| where ContainerName == "ocrsync"
| where Log has "ocr sync finished"
| extend j = parse_json(Log)
| where tostring(j.fields.message) == "ocr sync finished"
| summarize arg_max(TimeGenerated, j) by ContainerAppName
| extend OldestHours = toint(tolong(j.fields.oldest_age_s) / 3600), PartsLeft = tolong(j.fields.parts_left)
| where OldestHours > 72
| project TimeGenerated, Job = ContainerAppName, Detail = strcat(PartsLeft, " parts left, the oldest ", OldestHours, " hours old")
'''

var ocrSyncRules = [
  {
    name: 'usnm-ocrsync-failed-alert-${nameSuffix}'
    displayName: 'OCR pickup failed'
    description: 'An execution of the OCR pickup job (usnm-ocrsync-caj) failed in the last 15 minutes; Execution and Reason say which and why. Nothing published changes, and the next hourly run continues. Next: the job\'s console logs for its last lines (scripts/logs.sh <env> job-executions 1d); fix the cause, then start the job again or wait for the schedule.'
    severity: 3
    frequency: 'PT5M'
    window: 'PT30M'
    query: ocrSyncFailed
    dimensions: ['Execution', 'Reason']
  }
  {
    name: 'usnm-ocrsync-backlog-alert-${nameSuffix}'
    displayName: 'OCR output waiting over 72 hours'
    description: 'The OCR pickup\'s last run left node output in ocr-out that is more than 72 hours old; Detail says how much. ocr-out deletes blobs 30 days after their last change. Next: reference status/ocr-local.json (parts_left, oldest_age_s) and the pickup\'s logs; raise --max-parts or start extra executions (scripts/start-job.sh --job OCR_SYNC_JOB -- ocr-sync).'
    severity: 3
    frequency: 'PT1H'
    window: 'PT3H'
    query: ocrSyncBacklog
    dimensions: ['Job', 'Detail']
  }
]

resource ocrSyncAlert 'Microsoft.Insights/scheduledQueryRules@2023-12-01' = [
  for r in (ocrSync ? ocrSyncRules : []): {
    name: r.name
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
      skipQueryValidation: true
      autoMitigate: true
      criteria: {
        allOf: [
          {
            query: r.query
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
      // A new workspace has no Container Apps tables until the first logs
      // arrive; the rules start working once they do.
      skipQueryValidation: true
      autoMitigate: true
      criteria: {
        allOf: [
          {
            query: r.query
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
