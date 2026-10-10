// Resource logs for every resource that has them (08 §8.1), all to the one
// Log Analytics workspace. Kept in one place so the inventory is reviewable;
// the "diagnostic settings" audit policy (infra/guardrails.bicep) flags any
// resource that slips through.
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
// - Application Insights: workspace-based, so its telemetry is already in
//   this workspace; its resource logs would store every row twice.
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
// while the queues are empty.
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
