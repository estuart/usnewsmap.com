# ADR-0009: Entra identity only, no shared keys

- **Status:** Accepted
- **Date:** 2026-09
- **Strengthens:** ADR-0008 and 08 §8.2, which disabled keys on the data services only

## Context

The data services already refused keys: Blob (`allowSharedKeyAccess: false`) and Cosmos (`disableLocalAuth: true`), both enforced by policy. A review found shared keys elsewhere in the stack:

- the Container Apps environment sent logs to Log Analytics with the workspace's shared key;
- Log Analytics and Application Insights still accepted key-based ingestion;
- the Static Web App deployed with its deployment token (resolved by ADR-0010, below).

The maintainer made the rule foundational: **every authentication is by an Entra identity authorized with Azure RBAC.**

## Decision

1. **Every caller is an Entra principal.**
   - Workloads use managed identities: user-assigned (`id-usnm-app`, `id-usnm-ingest`), or system-assigned where a component can't pick a user-assigned one (the Quickwit sidecar and writer, 08 §8.2).
   - CI uses a user-assigned identity with a GitHub OIDC federated credential (`id-usnm-ci-{env}`).
   - People use their own Entra accounts.
   - Every grant is an Azure RBAC role assignment (or a Cosmos data-plane role assignment), scoped as narrowly as Azure allows.
2. **No shared secrets of any kind.** That means no account keys, no connection strings with keys, no SAS (account, service or user-delegation), no workspace keys or instrumentation-key ingestion, no registry admin user or repository tokens, and no stored secrets in GitHub. If a service offers a switch to turn local auth off, it's off:

   | Service                    | Setting                                                                                          |
   | -------------------------- | ------------------------------------------------------------------------------------------------ |
   | Storage (data, tiles)      | `allowSharedKeyAccess: false`                                                                    |
   | Cosmos DB                  | `disableLocalAuth: true`, `disableKeyBasedMetadataWriteAccess: true`                             |
   | Log Analytics              | `features.disableLocalAuth: true`                                                                |
   | Application Insights       | `DisableLocalAuth: true`                                                                         |
   | Container Registry         | `adminUserEnabled: false`, no anonymous pull                                                     |
   | Container Apps environment | logs through a diagnostic setting (`appLogsConfiguration: azure-monitor`), not the workspace key |

   Short-lived tokens that Entra issues for a principal count as Entra authentication, because they carry that principal's RBAC. These are OAuth access tokens, and the registry refresh token that `az acr login` exchanges for an Entra token.

   One exception ([ADR-0015](0015-onprem-ocr-workers.md)): onboarding an on-prem OCR node to Azure Arc uses a client secret. It belongs to a service principal that `scripts/ocr-node/onboard.sh` creates for one run, with only Azure Connected Machine Onboarding on the environment's group; it expires after 2 hours, is written nowhere but a root-only file on tmpfs during the connect, and the principal is deleted when the script exits.

3. **Enforced, not just configured.**
   - Azure Policy **denies** each "local auth on" setting above on the project resource groups (`infra/guardrails.bicep`).
   - CI fails any change that reintroduces a key pattern (`scripts/ci/no-shared-keys.sh`, in the `infra` job).
   - The Rust storage client refuses URLs that carry a SAS query.

## The Static Web App deployment token (resolved)

Static Web Apps has no Entra-only deploy path: every deploy path ends in the site's deployment token ([Azure/static-web-apps#1359](https://github.com/Azure/static-web-apps/issues/1359)). For a while that was the one exception to rule 2. [ADR-0010](0010-site-served-by-the-api.md) removed it: the API app serves the site from its image, and the Static Web App is gone.

## The ingest scratch share (exception, proposed in PR #74)

The Quickwit writer in `caj-usnm-ingest` needs more disk than a Container Apps replica has to merge an index into large splits (08 §8.4). Container Apps mounts Azure Files in two ways: SMB, which needs the account key in the environment's storage definition, and NFS, which has no authentication at all and is authorized by network. There is no Entra-authorized volume. The share is NFS, so it breaks rule 1 (every caller an Entra principal) without breaking rule 2 (no shared secrets): there is no key to leak, and shared key access is off on its account.

What reaches it, and what that allows:

- Only the VNet, through `pe-usnm-file`; public network access is disabled. The Container Apps environment has one subnet, so the API app's replicas can reach the share as well as the ingest job. NFS trusts the Unix ids a client presents, and root squash is off (the job's init container runs as root to hand a directory to the pipeline's user), so a compromised replica in the environment could read or change the writer's files.
- The share holds only the writer's working data during a release: the write-ahead log, splits being built and merges, all from public LoC text. No secrets, and the writer removes the previous run's data when it starts.
- The worst case is a compromised API replica changing an index while a release builds it. The release still checks the document count before it publishes, but not the content.

Hardening, if wanted: run the ingest jobs in a second Container Apps environment with its own subnet, and allow NFS (port 2049) to the endpoint from that subnet only.

The search experiment's share (`qw-search`, #251) is the same exception for a test, in `dev` only: a second NFS share in the same account, behind the same endpoint, holding a copy of a test index (public LoC text) for one searcher to read as local files. It exists only while `USNM_SEARCH_LOCAL_MODE` is `nfs`. The same limits apply: anything in the environment's subnet could read or change the copy, so a searcher reading it could be fed a changed index. If production ever served an index from such a share, that would need its own decision here.

## Consequences

- Nothing in the stack can be reached with a leaked key, because no service here accepts one.
- Portal features that rely on keys stop working. For example, the Log Analytics "agents" key blade, and the legacy Application Insights ingestion by instrumentation key. Telemetry, if the API ever sends it, must use the Azure Monitor OpenTelemetry exporter with a managed identity (Monitoring Metrics Publisher on the component).
- The Quickwit fallback in 08 §8.2 changes. If the sidecar can't authenticate with its managed identity, it can't fall back to a user-delegation SAS. The fallback is a localhost proxy in the `api` container that adds the app identity's bearer token.

## Addendum: BOINC server exemptions (ADR-0019)

The BOINC server's Azure Files mount takes the account key, and its Container Apps environment takes the Log Analytics shared key. Two Waiver exemptions, one per policy (shared-key storage and monitor local-auth), are group-wide within the dedicated BOINC resource group, which must hold only BOINC resources and gets the same guardrail assignments. `infra/main.bicep` writes them only when `deployBoincGroup` (setting `USNM_BOINC_GROUP`) is true; it is false by default. The definitions are not weakened, and the keys live only in the platform configuration.
