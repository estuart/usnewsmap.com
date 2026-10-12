// Resource logs for one storage account's services (see diagnostics.bicep).

param storageName string
param workspaceId string
@description('Where the queue service\'s logs go instead, as a workspace resource id; empty: workspaceId.')
param queueWorkspaceId string = ''
param settingName string

resource account 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: storageName

  resource blob 'blobServices' existing = {
    name: 'default'
  }
  resource queue 'queueServices' existing = {
    name: 'default'
  }
  resource table 'tableServices' existing = {
    name: 'default'
  }
  resource file 'fileServices' existing = {
    name: 'default'
  }
}

// Reads are left out: Quickwit's split reads.
resource blobLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: account::blob
  name: settingName
  properties: {
    workspaceId: workspaceId
    logs: [
      { category: 'StorageWrite', enabled: true }
      { category: 'StorageDelete', enabled: true }
    ]
  }
}

resource queueLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: account::queue
  name: settingName
  properties: {
    workspaceId: empty(queueWorkspaceId) ? workspaceId : queueWorkspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

resource tableLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: account::table
  name: settingName
  properties: {
    workspaceId: workspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}

resource fileLogs 'Microsoft.Insights/diagnosticSettings@2021-05-01-preview' = {
  scope: account::file
  name: settingName
  properties: {
    workspaceId: workspaceId
    logs: [{ categoryGroup: 'allLogs', enabled: true }]
  }
}
