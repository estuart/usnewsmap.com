// Two Azure Monitor workbooks (08 §8.1.2): "usnewsmap pipeline" for the
// ingest and backfill jobs and "usnewsmap API" for the API. They are saved
// dashboards over the workspace this environment already writes to; they
// have no charge of their own.
//
// Their definitions are infra/workbooks/*.json in the portal's own format
// (Advanced Editor, gallery template). The files query the workspace named
// by the placeholder __WORKSPACE_ID__, replaced here with this
// environment's; the API workbook's replica CPU chart reads the container
// app's platform metrics, named by __API_APP_ID__. To change one, edit it
// in the portal, copy its JSON back into the file with the resource ids
// turned back into the placeholders, and provision. sourceId attaches each workbook to Application Insights,
// so it's listed under the component's Workbooks; each is also a resource
// of the group (type Azure Workbook).

param location string
param tags object
param workspaceId string
param appInsightsId string

@description('Create the pipeline workbook (the ingest and backfill jobs exist).')
param pipeline bool

@description('Create the API workbook (the API app exists).')
param api bool

@description('The API\'s container app, for its platform metrics; empty without the API.')
param apiAppName string = ''

var placeholder = '__WORKSPACE_ID__'
var apiAppPlaceholder = '__API_APP_ID__'
var apiAppId = empty(apiAppName) ? '' : resourceId('Microsoft.App/containerApps', apiAppName)
// Azure stores sourceId in lowercase; matching it avoids a change on every
// deployment.
var sourceId = toLower(appInsightsId)

resource pipelineWorkbook 'Microsoft.Insights/workbooks@2023-06-01' = if (pipeline) {
  // A workbook's name is a GUID; a fixed seed keeps it stable across
  // deployments, so provisioning updates the workbook in place.
  name: guid(resourceGroup().id, 'usnm-workbook-pipeline')
  location: location
  tags: tags
  kind: 'shared'
  properties: {
    displayName: 'usnewsmap pipeline'
    category: 'workbook'
    sourceId: sourceId
    version: 'Notebook/1.0'
    serializedData: replace(loadTextContent('../workbooks/pipeline.json'), placeholder, workspaceId)
  }
}

resource apiWorkbook 'Microsoft.Insights/workbooks@2023-06-01' = if (api) {
  name: guid(resourceGroup().id, 'usnm-workbook-api')
  location: location
  tags: tags
  kind: 'shared'
  properties: {
    displayName: 'usnewsmap API'
    category: 'workbook'
    sourceId: sourceId
    version: 'Notebook/1.0'
    serializedData: replace(
      replace(loadTextContent('../workbooks/api.json'), placeholder, workspaceId),
      apiAppPlaceholder,
      apiAppId
    )
  }
}
