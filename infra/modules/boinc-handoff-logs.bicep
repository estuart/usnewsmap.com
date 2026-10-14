// The on-prem BOINC hand-off runner (#469) reads the BOINC jobs' logs:
// Log Analytics Reader on the BOINC group's workspace, nothing else there.
// The runner's other grants are boinc-handoff-runner.bicep.

@description('The BOINC workspace (contrib-compute/infra/boinc/main.bicep names it).')
param workspaceName string
@description('Object id of the runner\'s identity (the container\'s Arc machine identity).')
param principalId string

var logAnalyticsReader = '73c42c96-874c-492b-b04d-ab87d138a893'

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' existing = {
  name: workspaceName
}

resource read 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: workspace
  name: guid(workspace.id, principalId, logAnalyticsReader)
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', logAnalyticsReader)
  }
}
