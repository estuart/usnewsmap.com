// Azure Monitor workbooks (08 §8.1.2): "usnewsmap pipeline" for the ingest
// and backfill jobs and "usnewsmap API" for the API, over the workspace
// this environment already writes to, and "usnm OCR nodes (<env>)" for the
// on-prem OCR nodes (#296), over the nodes' own workspace
// usnm-ocr-log-<env> (ocr-node-monitoring.bicep). They are saved
// dashboards; they have no charge of their own.
//
// Their definitions are infra/workbooks/*.json in the portal's own format
// (Advanced Editor, gallery template). The files query the workspace named
// by the placeholder __WORKSPACE_ID__, replaced here with this
// environment's (the nodes' workspace for the OCR nodes workbook); the API
// workbook's replica CPU chart reads the container app's platform metrics,
// named by __API_APP_ID__. To change one, edit it in the portal, copy its
// JSON back into the file with the resource ids turned back into the
// placeholders (and, in ocr-nodes.json, the cap back into __CAP_GB__), and
// provision. sourceId attaches each workbook to Application Insights, so
// it's listed under the component's Workbooks: appi-usnm-<env> for the
// pipeline and API workbooks, the nodes' usnm-ocr-appi-<env> for the OCR
// nodes workbook, whose telemetry it reads (that component's tables are in
// the nodes' workspace). Each is also a resource of the group (type Azure
// Workbook).

param location string
param tags object
param workspaceId string
param appInsightsId string
@description('Environment name, in the OCR nodes workbook\'s title.')
param env string

@description('Create the pipeline workbook (the ingest and backfill jobs exist).')
param pipeline bool

@description('Create the API workbook (the API app exists).')
param api bool

@description('The API\'s container app, for its platform metrics; empty without the API.')
param apiAppName string = ''

@description('The searcher\'s own app (#267), whose CPU the API workbook\'s replica CPU chart then shows; empty while the searcher is the API\'s sidecar.')
param searcherAppName string = ''

@description('Create the OCR nodes workbook (the on-prem OCR nodes\' monitoring exists).')
param ocrNodes bool = false

@description('The nodes\' Log Analytics workspace usnm-ocr-log-<env> (resource id); needed with ocrNodes.')
param ocrNodeWorkspaceId string = ''

@description('The nodes\' Application Insights component usnm-ocr-appi-<env> (resource id); needed with ocrNodes.')
param ocrNodeAppInsightsId string = ''

@description('The nodes\' workspace\'s daily cap in GB, which the OCR nodes workbook charts the daily volume against.')
param ocrNodeDailyCapGb string = ''

var placeholder = '__WORKSPACE_ID__'
var apiAppPlaceholder = '__API_APP_ID__'
// Only the replica CPU chart names an app: the one the searcher runs in.
var apiAppId = empty(apiAppName)
  ? ''
  : resourceId('Microsoft.App/containerApps', empty(searcherAppName) ? apiAppName : searcherAppName)
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

// One page on the OCR nodes: CPU, memory, disk and network per node,
// heartbeat gaps, the worker's warnings, errors and pages per hour, and the
// workspace's daily volume against its cap (__CAP_GB__, the same figure as
// the workspace's own cap).
resource ocrNodesWorkbook 'Microsoft.Insights/workbooks@2023-06-01' = if (ocrNodes) {
  name: guid(resourceGroup().id, 'usnm-workbook-ocr-nodes')
  location: location
  tags: tags
  kind: 'shared'
  properties: {
    displayName: 'usnm OCR nodes (${env})'
    category: 'workbook'
    sourceId: toLower(ocrNodeAppInsightsId)
    version: 'Notebook/1.0'
    serializedData: replace(
      replace(loadTextContent('../workbooks/ocr-nodes.json'), placeholder, ocrNodeWorkspaceId),
      '__CAP_GB__',
      ocrNodeDailyCapGb
    )
  }
}
