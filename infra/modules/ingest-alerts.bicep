// Log search alerts on the pipeline's jobs (09 §9.2): the ingest and backfill
// jobs, the Japanese OCR jobs and the OCR pickup job, evaluated on the Log
// Analytics workspace against the resource-specific Container Apps tables
// (ContainerAppConsoleLogs, ContainerAppSystemLogs). Severity 2 (the
// progress check and the OCR backlog 3), email through the environment's
// action group. Every rule is checked every 15 minutes or less often (#308:
// a log alert costs $1.50 a month at 5 minutes, $0.50 at 15), and one rule
// reports every job's failures.
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
param workspaceId string
param actionGroupId string
@description('The jobs the queries watch, by their exact names (infra/naming.bicep), so the queries match no other name form.')
param ingestJobName string
param backfillJobName string
param jaOcrJobName string
param jaAuditJobName string
@description('The alert rules\' names (infra/naming.bicep): jobFailed, ingestNotProgressing, releaseStalled, backfillStalled, backfillReplicaSilent and ocrSyncBacklog.')
param alertNames object
@description('Alert on the OCR pickup job\'s backlog, usnm-ocrs-caj-<env> (with the on-prem OCR nodes, 12 §12.5 Step 8). Its failures are in the job-failed rule either way.')
param ocrSync bool = false
@description('Deploy the backfill rules (stalled, replica silent): while a backfill lasts, which is while USNM_BACKFILL_CRON schedules it (docs/operations.md). The backfill job\'s failures are in the job-failed rule either way.')
param backfill bool = false

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
// The ingest and backfill jobs' failures, as rows of TimeGenerated, Job,
// Replica and Detail, for jobFailed below.
var ingestFailures = '''
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
    | where JobName startswith "__INGEST_JOB__" or JobName startswith "__BACKFILL_JOB__"
        or ContainerAppName startswith "__INGEST_JOB__" or ContainerAppName startswith "__BACKFILL_JOB__"
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
| project TimeGenerated, Job, Replica, Detail
'''

// The Japanese OCR jobs (caj-usnm-jaocr-*, and caj-usnm-jaone-* for its
// one-off measurements): a replica that died (non-zero exit: out of memory,
// a crash; not one the platform stopped because another replica failed),
// the job's backoff or deadline, and the first line of any Python traceback
// the job printed (the job's own JSON lines with an "error" field are
// reported, not failures). The same columns as ingestFailures.
var jaOcrFailures = '''
let platform = ContainerAppSystemLogs
    | where JobName startswith "__JAOCR_JOB__" or JobName startswith "__JAONE_JOB__"
        or ContainerAppName startswith "__JAOCR_JOB__" or ContainerAppName startswith "__JAONE_JOB__"
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
| project TimeGenerated, Job, Replica, Detail
'''

// One rule for every job's failures (#308; three rules until then, each
// checked every 5 minutes): the ingest and backfill jobs, the Japanese OCR
// and audit jobs, and the OCR pickup job. A Japanese OCR, audit or pickup
// run touches no published index and is retried by starting it again, so
// the email's Execution says which job it was before anything else.
//
// Split by Execution (the job execution, from the replica's name; the job
// for a job-level event that names none) and Reason: the error, cut to 180
// characters, with URL query strings, key=value secrets and long opaque
// tokens masked. Checked every 15 minutes; the last 30 minutes are
// reported, so a line that reaches the workspace a few minutes late is
// still seen by the next check (a condition seen twice is one alert, since
// the rule is stateful), and the window is 45 minutes so that the line a
// platform event repeats, up to 5 minutes before it, is still in view.
var jobFailedTemplate = '''
let ingestEvents = () {
__INGEST_FAILURES__};
let jaocrEvents = () {
__JAOCR_FAILURES__};
let ocrsEvents = () {
__OCRS_FAILURES__};
union ingestEvents(), jaocrEvents(), ocrsEvents()
| where TimeGenerated > ago(30m)
| extend Execution = iff(isempty(Replica), Job, extract(@"^(.+)-[a-z0-9]+$", 1, Replica))
| extend Reason = replace_regex(Detail, @"\?[^\s'\x22]+", "?…")
| extend Reason = replace_regex(Reason, @"(?i)(sig|signature|token|secret|password|key|code)=[^\s&,;]+", @"\1=***")
| extend Reason = replace_regex(Reason, @"[A-Za-z0-9+/_=-]{40,}", "***")
| extend Reason = iff(strlen(Reason) > 180, strcat(substring(Reason, 0, 179), "…"), Reason)
| project TimeGenerated, Execution, Reason
'''

var jobFailed = replace(
  replace(replace(jobFailedTemplate, '__INGEST_FAILURES__', ingestFailures), '__JAOCR_FAILURES__', jaOcrFailures),
  '__OCRS_FAILURES__',
  ocrSyncFailures
)

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
    | where JobName startswith "__BACKFILL_JOB__"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET";
let replicaStops = platform
    | where Reason in ("ContainerTerminated", "PodDeletion", "ProcessExited", "SuccessfulDelete")
    | extend Replica = iff(isnotempty(ReplicaName), ReplicaName, extract(@"(__BACKFILL_JOB__-[a-z0-9-]+)", 1, Log))
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
    name: alertNames.jobFailed
    displayName: 'Pipeline job failed'
    description: 'A replica of the ingest or backfill job, the Japanese OCR or audit job (caj-usnm-jaocr, caj-usnm-jaone) or the OCR pickup job (usnm-ocrs-caj) failed in the last 30 minutes; Execution and Reason in this email say which and why. Expected titles-sync stops are left out (ingest-not-progressing watches them). Japanese OCR, audit and pickup runs touch no published index. Next: scripts/logs.sh --env <env> --query-name job-executions --span 1d for how each replica ended, and errors-by-batch 1d for the warnings before it (for a Japanese OCR run, scripts/ja-ocr/quality-rows.sh --env <env> --execution <execution>). Fix the cause, then start the job again (or wait for the pickup\'s hourly run); the next run picks up where this one stopped.'
    severity: 2
    frequency: 'PT15M'
    // Azure accepts only 5, 10, 15, 30, 45, 60 min and up; the query reports the last 30 min
    // (two checks), and the extra lookback lets it see the 5 minutes before an event.
    window: 'PT45M'
    query: jobFailed
    dimensions: ['Execution', 'Reason']
  }
  {
    name: alertNames.ingestNotProgressing
    displayName: 'Ingest not progressing'
    description: 'The ingest job keeps stopping at the titles-sync deadline: Condition says how. "not started again": the last stop was 3 or more hours ago and no execution has run since; start the job (and check whatever restarts it). "no titles progress": 3 stops in a row left as many titles as the first; LoC may be refusing title records. Next: scripts/logs.sh --env <env> --query-name ingest-endings --span 2d for each stop and the titles left, and /v1/status (titles.pipeline.awaiting_sync).'
    severity: 3
    frequency: 'PT1H'
    window: 'P2D'
    query: ingestNotProgressing
    dimensions: ['Condition', 'Execution', 'Detail']
  }
  {
    name: alertNames.releaseStalled
    displayName: 'Release stalled'
    description: 'A release is building an index but has logged no progress for 10 minutes; Execution and Detail say which version and where it stopped. Next: scripts/logs.sh --env <env> --query-name release-progress for the last lines (disk, memory, Quickwit retries) and job-executions to see if the replica is still running.'
    severity: 2
    frequency: 'PT15M'
    window: 'P2D'
    query: releaseStalled
    dimensions: ['Execution', 'Detail']
  }
]

// Only while a backfill lasts (`backfill`, #308): the job is idle otherwise,
// and each rule costs a few dollars a year for nothing.
var backfillRules = [
  {
    name: alertNames.backfillStalled
    displayName: 'Backfill stalled'
    description: 'Backfill workers have run for the last hour but none has curated a batch in it (and LoC is not rate limiting). Next: scripts/logs.sh --env <env> --query-name curation-throughput for the recent work, curate-replicas for each worker\'s stage, and errors-by-batch 2h for repeated failures on the same batches.'
    severity: 2
    frequency: 'PT15M'
    window: 'PT2H'
    query: backfillStalled
    dimensions: []
  }
  {
    name: alertNames.backfillReplicaSilent
    displayName: 'Backfill replica silent'
    description: 'A backfill replica that is still running has logged nothing for 15 minutes; a working one logs curate progress every minute. Replica and Detail say which and its last line. Next: scripts/logs.sh --env <env> --query-name curate-replicas for each replica\'s last stage and heartbeat; stop the execution if it is stuck (its batch lease expires and another worker retries it).'
    severity: 2
    frequency: 'PT15M'
    window: 'P1D'
    query: backfillReplicaSilent
    dimensions: ['Replica', 'Detail']
  }
]

// The OCR pickup job (usnm-ocrs-caj-*, 12 §12.5 Step 8): its "command
// failed" line, or the platform's events for a replica that died without
// one. The same columns as ingestFailures. The container is still called ocrsync (the job was usnm-ocrsync-caj-{env}
// until the October 2026 rename, #309), so the console queries match both.
var ocrSyncFailures = '''
let lines = ContainerAppConsoleLogs
    | where ContainerName == "ocrsync"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Log has "failed"
    | extend j = parse_json(Log)
    | where tostring(j.level) == "ERROR" and tostring(j.fields.message) == "command failed"
    | project TimeGenerated, Job = coalesce(ContainerAppName, JobName), Replica = ContainerGroupName,
        Detail = tostring(j.fields.error);
let platform = ContainerAppSystemLogs
    | where JobName startswith "usnm-ocrs-caj-" or ContainerAppName startswith "usnm-ocrs-caj-"
    | where Log !has "IDENTITY_HEADER" and Log !has "MSI_SECRET"
    | where Reason in ("BackoffLimitExceeded", "DeadlineExceeded")
        or (Log contains "terminated with exit code" and Log !contains "exit code '0'")
    | project TimeGenerated, Job = coalesce(JobName, ContainerAppName), Replica = ReplicaName,
        Detail = strcat(Reason, ": ", Log)
    | join kind=leftanti lines on Replica;
union lines, platform
| project TimeGenerated, Job, Replica, Detail
'''

// Node output waiting in ocr-out for more than 72 hours: the pickup's
// last "ocr sync finished" line in the last 3 hours reports the oldest
// part it left (oldest_age_s), from the parts' creation times on the
// server. Parts left that long are refused by nothing and taken by
// nothing: a run that keeps stopping at its max runtime, or a pickup
// that can't read them. The latest line of any job counts, not each job's
// own, so a run of the job under its old name (usnm-ocrsync-caj-{env})
// isn't reported again after the rename.
var ocrSyncBacklog = '''
ContainerAppConsoleLogs
| where ContainerName == "ocrsync"
| where Log has "ocr sync finished"
| extend j = parse_json(Log)
| where tostring(j.fields.message) == "ocr sync finished"
| summarize arg_max(TimeGenerated, j, ContainerAppName)
| extend OldestHours = toint(tolong(j.fields.oldest_age_s) / 3600), PartsLeft = tolong(j.fields.parts_left)
| where OldestHours > 72
| project TimeGenerated, Job = ContainerAppName, Detail = strcat(PartsLeft, " parts left, the oldest ", OldestHours, " hours old")
'''

var ocrSyncRules = [
  {
    name: alertNames.ocrSyncBacklog
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
  for r in concat(rules, backfill ? backfillRules : []): {
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
      // A new workspace has no Container Apps tables until the first logs
      // arrive; the rules start working once they do.
      skipQueryValidation: true
      autoMitigate: true
      criteria: {
        allOf: [
          {
            // The placeholders become the jobs' exact names.
            query: replace(
              replace(
                replace(replace(r.query, '__INGEST_JOB__', ingestJobName), '__BACKFILL_JOB__', backfillJobName),
                '__JAOCR_JOB__',
                jaOcrJobName
              ),
              '__JAONE_JOB__',
              jaAuditJobName
            )
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
