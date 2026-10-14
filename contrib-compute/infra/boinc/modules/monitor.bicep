// Log Analytics workspace. Daily cap keeps the bill bounded (docs/design.md, cost).
param workspaceName string
param location string
@description('Daily ingestion cap in GB. At $2.30/GB retail this bounds the worst case.')
param dailyCapGb int = 1

resource workspace 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: workspaceName
  location: location
  properties: {
    sku: { name: 'PerGB2018' }
    retentionInDays: 30
    workspaceCapping: { dailyQuotaGb: dailyCapGb }
  }
}

output id string = workspace.id
output customerId string = workspace.properties.customerId
