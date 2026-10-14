// Log Analytics workspace. Daily cap keeps the bill bounded (docs/design.md, cost).
param workspaceName string
param location string
@description('Daily ingestion cap in GB. At $2.30/GB retail this bounds the worst case.')
param dailyCapGb int = 1
@description('Principals that read the logs (the stack operators), granted Log Analytics Reader.')
param operatorPrincipalIds array = []

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: workspaceName
  location: location
  properties: {
    sku: { name: 'PerGB2018' }
    retentionInDays: 30
    workspaceCapping: { dailyQuotaGb: dailyCapGb }
  }
}

// Log Analytics Reader, so an operator can read job logs (ContainerAppConsoleLogs_CL).
var logReader = '73c42c96-874c-492b-b04d-ab87d138a893'

resource readers 'Microsoft.Authorization/roleAssignments@2022-04-01' = [
  for operator in operatorPrincipalIds: {
    scope: workspace
    name: guid(workspace.id, operator, logReader)
    properties: {
      roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', logReader)
      principalId: operator
    }
  }
]

output id string = workspace.id
output customerId string = workspace.properties.customerId
