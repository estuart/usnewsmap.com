// Resource logs for every resource that has them (08 §8.1), to the
// environment's Log Analytics workspace, except what the on-prem OCR nodes
// control, which goes to the nodes' own workspace (ocr-node-monitoring.bicep)
// so that a node can't fill the environment's daily cap: the exchange
// account's queue logs (every node request is a row) and the nodes' data
// collection rule's errors. Kept in one place so the inventory is
// reviewable; the "diagnostic settings" audit policy (infra/guardrails.bicep)
// flags any resource that slips through.
//
// Everything with resource logs is covered, with every category, except
// where a category would record each request and eat the workspace's daily
// cap (1 GB in prod, 0.15 GB in dev; a normal prod day is about 53 MB):
// - Blob reads (StorageRead): the public tiles and the index splits Quickwit
//   range-reads on every search. Writes and deletes are the audit trail,
//   in StorageBlobLogs on the Auxiliary plan (monitoring.bicep), outside
//   the cap: one row per write or delete request (PutBlob, or PutBlock
//   for each block and PutBlockList), so bulk jobs write many.
// - Cosmos DataPlaneRequests and the per-query/per-request statistics: every
//   ingest write. Control-plane changes are the audit trail.
// - Container Apps HTTP logs: every API request, with its query string (the
//   search text, 09 §9.4.2). usnm-api reports each request itself as one
//   Application Insights request, without the path or query.
//
// Not covered, deliberately:
// - Application Insights, the environment's and the nodes': workspace-based,
//   so their telemetry is already in a workspace; their resource logs would
//   store every row twice.
// - Resources with platform metrics only (DNS zones, private endpoints,
//   Container Apps and jobs): no logs to send, and Azure Monitor keeps their
//   metrics 93 days at no cost.
//
// Retention is the workspace's (30 days; 31 are included in the ingestion
// price, so shorter saves nothing).

param workspaceId string
param workspaceName string
param registryName string
param vnetName string
param containerEnvName string
param cosmosName string
param dataStorageName string
param tilesStorageName string
@description('The ingest scratch share\'s account (ingest-scratch.bicep); empty if there is none.')
param scratchStorageName string = ''
@description('The on-prem OCR exchange account (ocr-exchange.bicep); empty if there is none.')
param exchangeStorageName string = ''
@description('The OCR nodes\' workspace (ocr-node-monitoring.bicep), as a resource id; empty if there is none.')
param ocrNodeWorkspaceId string = ''
@description('The OCR nodes\' workspace\'s name; empty if there is none.')
param ocrNodeWorkspaceName string = ''
@description('The OCR nodes\' data collection rule (ocr-node-monitoring.bicep); empty if there is none.')
param ocrNodeDcrName string = ''
@description('The OCR nodes\' Key Vault (ocr-vault.bicep); empty if there is none.')
param ocrVaultName string = ''
@description('The settings vault (settings-vault.bicep).')
param settingsVaultName string = ''

var name = 'to-log-analytics'

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' existing = {
  name: workspaceName
}

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' existing = {
  name: registryName
}

resource vnet 'Microsoft.Network/virtualNetworks@2024-05-01' existing = {
  name: vnetName
}

resource containerEnv 'Microsoft.App/managedEnvironments@2024-03-01' existing = {
  name: containerEnvName
}

resource cosmos 'Microsoft.DocumentDB/databaseAccounts@2024-11-15' existing = {
  name: cosmosName
}

// Queries run against the workspace (LAQueryLogs) and its own health.
resource workspaceLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: workspace
  name: name
  properties: {
    workspaceId: workspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

// The OCR nodes' workspace, the same way, into itself: the operators' and
// the node alerts' queries, a few rows an hour that no node can add to.
resource ocrNodeWorkspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' existing = if (!empty(ocrNodeWorkspaceName)) {
  name: ocrNodeWorkspaceName
}

resource ocrNodeWorkspaceLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = if (!empty(ocrNodeWorkspaceName)) {
  scope: ocrNodeWorkspace
  name: name
  properties: {
    workspaceId: ocrNodeWorkspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

// Logins and image pushes and deletes.
resource registryLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: registry
  name: name
  properties: {
    workspaceId: workspaceId
    logAnalyticsDestinationType: 'Dedicated'
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

resource vnetLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: vnet
  name: name
  properties: {
    workspaceId: workspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

// The apps' and jobs' console output and the platform's events (revisions,
// scaling, restarts). The environment sends them here (appLogsConfiguration
// is azure-monitor), into ContainerAppConsoleLogs and ContainerAppSystemLogs.
resource containerEnvLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: containerEnv
  name: name
  properties: {
    workspaceId: workspaceId
    logAnalyticsDestinationType: 'Dedicated'
    logs: [
      { category: 'ContainerAppConsoleLogs', enabled: true }
      { category: 'ContainerAppSystemLogs', enabled: true }
    ]
  }
}

resource cosmosLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: cosmos
  name: name
  properties: {
    workspaceId: workspaceId
    logAnalyticsDestinationType: 'Dedicated'
    logs: [
      { category: 'ControlPlaneRequests', enabled: true }
      { category: 'PartitionKeyStatistics', enabled: true }
    ]
  }
}

// The data and tiles accounts, and the OCR exchange account when there is
// one: blob writes and deletes; the queue, table and file services log
// everything. Only the exchange account uses queues: each node's take,
// delete and empty poll is a StorageQueueLogs row, so the worker backs off
// while the queues are empty, and a node decides how many there are, so
// they go to the nodes' workspace and count under its cap. Its blob writes
// stay here, in StorageBlobLogs on the Auxiliary plan, which no cap covers
// (monitoring.bicep), so they can't stop the environment's logging.
module dataStorageLogs 'storage-diagnostics.bicep' = {
  name: 'diagnostics-${dataStorageName}'
  params: {
    storageName: dataStorageName
    workspaceId: workspaceId
    settingName: name
  }
}

module tilesStorageLogs 'storage-diagnostics.bicep' = {
  name: 'diagnostics-${tilesStorageName}'
  params: {
    storageName: tilesStorageName
    workspaceId: workspaceId
    settingName: name
  }
}

module exchangeStorageLogs 'storage-diagnostics.bicep' = if (!empty(exchangeStorageName)) {
  name: 'diagnostics-${exchangeStorageName}'
  params: {
    storageName: exchangeStorageName
    workspaceId: workspaceId
    queueWorkspaceId: ocrNodeWorkspaceId
    settingName: name
  }
}

// The ingest scratch share: deletes only. The Quickwit writer creates,
// reads and writes many files on it in a release; logging those would fill
// the daily cap. (A FileStorage account has no blob, queue or table service.)
resource scratchAccount 'Microsoft.Storage/storageAccounts@2025-01-01' existing = if (!empty(scratchStorageName)) {
  name: scratchStorageName

  resource file 'fileServices' existing = {
    name: 'default'
  }
}

resource scratchLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = if (!empty(scratchStorageName)) {
  scope: scratchAccount::file
  name: name
  properties: {
    workspaceId: workspaceId
    logs: [{ category: 'StorageDelete', enabled: true }]
  }
}

// The OCR nodes' data collection rule: errors in collecting or transforming
// what the Azure Monitor Agent sends (DCRLogErrors), its only category. Rows
// only when something fails, and what a node sends can make it fail, so
// they go to the nodes' workspace with the rest of the nodes' data.
resource ocrNodeDcr 'Microsoft.Insights/dataCollectionRules@2023-03-11' existing = if (!empty(ocrNodeDcrName)) {
  name: ocrNodeDcrName
}

resource ocrNodeDcrLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = if (!empty(ocrNodeDcrName)) {
  scope: ocrNodeDcr
  name: name
  properties: {
    workspaceId: ocrNodeWorkspaceId
    logs: [{ category: 'LogErrors', enabled: true }]
  }
}

// The OCR nodes' Key Vault: every secret read and write and every refused
// request (AuditEvent), to the environment's workspace. Only the operator
// uses it, a few requests per onboarding, and no node can reach it.
resource ocrVault 'Microsoft.KeyVault/vaults@2023-07-01' existing = if (!empty(ocrVaultName)) {
  name: ocrVaultName
}

resource ocrVaultLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = if (!empty(ocrVaultName)) {
  scope: ocrVault
  name: name
  properties: {
    workspaceId: workspaceId
    logs: [{ category: 'AuditEvent', enabled: true }]
  }
}

// The settings vault (#295): every read and write of a private setting and
// every refused request, a few per provision.
resource settingsVault 'Microsoft.KeyVault/vaults@2023-07-01' existing = if (!empty(settingsVaultName)) {
  name: settingsVaultName
}

resource settingsVaultLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = if (!empty(settingsVaultName)) {
  scope: settingsVault
  name: name
  properties: {
    workspaceId: workspaceId
    logs: [{ category: 'AuditEvent', enabled: true }]
  }
}
