// Azure Monitor workbook "usnm BOINC (<env>)" over the stack's workspace
// usnm-boinc-log-<env>: the server app's revisions and warnings, the jobs'
// executions and the console errors. A saved dashboard; no charge of its own.
//
// The definition is infra/workbooks/boinc.json in the portal's own format,
// with the workspace named by the placeholder __WORKSPACE_ID__, which this
// module replaces. To change it, edit it in the portal, copy its JSON back
// with the resource id turned back into the placeholder, and provision.
// sourceId attaches it to the workspace, so it's listed under the
// workspace's Workbooks.

param location string
param workspaceId string
@description('The workbook\'s title (displayName).')
param title string

var placeholder = '__WORKSPACE_ID__'

resource workbook 'Microsoft.Insights/workbooks@2023-06-01' = {
  // A fixed seed keeps the name (a GUID) stable, so provisioning updates it in place.
  name: guid(resourceGroup().id, 'usnm-workbook-boinc')
  location: location
  kind: 'shared'
  properties: {
    displayName: title
    category: 'workbook'
    // Azure stores sourceId in lowercase; matching it avoids a change on every deployment.
    sourceId: toLower(workspaceId)
    version: 'Notebook/1.0'
    serializedData: replace(loadTextContent('../../../../infra/workbooks/boinc.json'), placeholder, workspaceId)
  }
}
