// Waivers for the BOINC server stack (ADR-0019, decision 5), deployed into
// the dedicated BOINC resource group. The Container Apps Azure Files mount
// takes the storage account key, and the environment's log configuration
// takes the workspace's shared key, so the two deny policies would otherwise
// refuse the stack.
//
// The scope is the group, not the two resources: a resource-scoped exemption
// can't exist before its target, and the policy denies the target without it.
// So the waivers are group-wide for those two policies and the group must hold only BOINC resources, and these exemptions are
// deployed before the BOINC stack. Each waives one assignment only; every
// other guardrail still applies. The policy definitions are unchanged.

@description('Full id of the shared-key storage policy assignment on this group.')
param noStorageKeysAssignmentId string
@description('Full id of the monitor local-auth policy assignment on this group.')
param noMonitorKeysAssignmentId string

resource storageExemption 'Microsoft.Authorization/policyExemptions@2022-07-01-preview' = {
  name: guid(resourceGroup().id, noStorageKeysAssignmentId)
  properties: {
    policyAssignmentId: noStorageKeysAssignmentId
    exemptionCategory: 'Waiver'
    displayName: 'BOINC group: shared key access on the Files account'
    description: 'The Container Apps Azure Files mount takes the account key. The account is private-endpoint only and the key stays in the platform configuration. The group holds only BOINC resources. ADR-0019 decision 5, ADR-0009 addendum.'
  }
}

resource monitorExemption 'Microsoft.Authorization/policyExemptions@2022-07-01-preview' = {
  name: guid(resourceGroup().id, noMonitorKeysAssignmentId)
  properties: {
    policyAssignmentId: noMonitorKeysAssignmentId
    exemptionCategory: 'Waiver'
    displayName: 'BOINC group: shared key on the Log Analytics workspace'
    description: 'The Container Apps environment log configuration takes the workspace shared key. The key stays in the platform configuration. The group holds only BOINC resources. ADR-0019 decision 5, ADR-0009 addendum.'
  }
}
