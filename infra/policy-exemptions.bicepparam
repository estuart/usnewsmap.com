// Parameters for infra/policy-exemptions.bicep, from the same settings as
// infra/main.bicepparam (scripts/lib/env.sh exports them for both).
using './policy-exemptions.bicep'

import { checkedEnvName } from 'naming.bicep'

param environmentName = checkedEnvName(readEnvironmentVariable('AZURE_ENV_NAME'))
param ocrNodes = bool(readEnvironmentVariable('USNM_OCR_NODES', 'false'))
param ocrNodeIps = readEnvironmentVariable('USNM_OCR_NODE_IPS', '')
param serialSetOnPrem = bool(readEnvironmentVariable('USNM_SERIALSET_ONPREM', 'false'))
