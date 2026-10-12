// The public-network policy's assignment alone (#336), deployed by
// scripts/lib/env.sh (provision) to the environment's project group before
// the stack.
//
// Azure checks policy at preflight against the assignments as they are
// then. When one provision both adds an account to publicAccountIds and
// opens that account, the stack's preflight sees the old list and refuses
// the whole deployment (RequestDisallowedByPolicy). A dependsOn on the
// assignment doesn't help: preflight runs before anything deploys. So this
// small deployment writes the next list first, and the stack's preflight
// then sees it. The stack still manages the assignment (main.bicep) and
// writes the same value, from the same function (public-accounts.bicep).
// The stack's deny setting is denyDelete, which allows this write.

targetScope = 'resourceGroup'

import { guardrailPolicyNames } from 'guardrails.bicep'
import { checkedEnvName, legacyProdSuffix, names } from 'naming.bicep'
import { lakeOpen, publicAccountIds } from 'public-accounts.bicep'

@description('As in main.bicep.')
param environmentName string
@description('As in main.bicep.')
param ocrNodes bool = false
@description('As in main.bicep.')
param ocrNodeIps string = ''
@description('As in main.bicep.')
param serialSetOnPrem bool = false

// The same names main.bicep computes.
var env = checkedEnvName(environmentName)
var suffix = take(uniqueString(subscription().id, env), 6)
var n = names(env, suffix, suffix == legacyProdSuffix)

resource assignment 'Microsoft.Authorization/policyAssignments@2024-04-01' = {
  name: guardrailPolicyNames.auditPublicAccess
  properties: {
    policyDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/policyDefinitions',
      guardrailPolicyNames.auditPublicAccess
    )
    enforcementMode: 'Default'
    parameters: {
      effect: { value: 'deny' }
      publicAccountIds: {
        value: publicAccountIds(subscription().subscriptionId, n, ocrNodes, lakeOpen(serialSetOnPrem, ocrNodeIps))
      }
    }
  }
}

output publicAccountIds array = assignment.properties.parameters.publicAccountIds.value
