# ADR-0008: Private networking for app → data paths

- **Status:** Accepted
- **Date:** 2026-09
- **Amends:** ADR-0006, which had deferred all network isolation to the growth profile

## Context

Every app/job → storage/Cosmos path already used managed identities, with keys disabled. But the data services' public endpoints were open (Entra-only) because the ACI Spot backfill workers can't join a VNet and have no stable outbound IP. The maintainer asked for private networking on those paths and chose "private in steady state".

## Decision

- **VNet-integrated Container Apps environment** (`vnet-usnm`, `/27` delegated subnet; workload-profiles env with the Consumption profile). The public API keeps external ingress at `api.usnewsmap.com`.
- **Private endpoints** for `stusnmdata` (Blob) and `cosmos-usnm` (Sql), with linked private DNS zones. **`publicNetworkAccess: Disabled`** on both in steady state.
- **Managed identity on every path** (see the table in [08 §8.3](../08-azure-infrastructure.md#83-networking)). Keys stay disabled, and Azure Policy **denies** re-enabling them.
- **Guarded public-access window** only while a Spot backfill or full re-index runs:
  - the launcher records a window item in Cosmos with an expiry, then enables public access (still Entra-only, no IP rules);
  - it disables public access again when the work completes;
  - an hourly in-VNet `network-guard` job closes any window that is expired or wasn't recorded;
  - every change to `publicNetworkAccess` raises an alert.
- **Drop the Storage Queue.** Cosmos leases already claim work, so the queue was only a wake-up signal, and removing it avoids a third private endpoint (~$8/month).

## Alternatives

| Option                                                          | Why not                                                                                                                                                |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Identity-only (previous lean design)                            | Cheapest (−$17/mo), but data services reachable from the internet (with a valid token) at all times                                                    |
| Always private, backfill on VNet-integrated Container Apps Jobs | No public window ever, but the backfill costs ~$150–250 instead of ~$25–50, and full re-indexes cost more each time. Kept as the growth-profile option |
| IP allow-lists                                                  | ACI Spot egress IPs are unknowable, and a Consumption environment's outbound IPs can change; brittle                                                   |
| Azure Monitor Private Link Scope for telemetry                  | Adds cost and complexity for low-sensitivity telemetry; growth profile                                                                                 |

## Consequences

- About **+$17/month** (2 endpoints, 2 DNS zones, per-GB processing). The typical month is ~$72, and a press-spike month ~$114 (see [09 §9.5](../09-operations-security-cost.md#95-cost-model-monthly-usd-list-prices-confirm-with-the-azure-pricing-calculator)).
- The **2 vCPU search lever no longer fits under $80** without dropping private endpoints or raising the ceiling. This is flagged as a maintainer decision in [10 §10.4](../10-roadmap-and-risks.md#104-open-questions-for-the-maintainer).
- The Azure portal's Data Explorer and Storage Browser can't reach the data from the internet in steady state. Maintainers use the in-VNet `usnm-ingest admin` job, or open a short recorded window.
- The environment must be created with VNet integration from the start.

## Amendment, October 2026: the on-prem Serial Set runner (#229)

The Serial Set fetch (`usnm-ingest congress-serialset`) runs on an OCR node at home instead of as the Azure job `usnm-sset-caj-<env>`, so it costs no Azure compute. The subscription is over its budget, and the fetch spends most of its time waiting on GovInfo, at one request a second. Its output still goes to the data lake, which the node can't reach through the private endpoint. With `USNM_SERIALSET_ONPREM` on:

- **The data lake allows public network access from the node IPs only.** `publicNetworkAccess: Enabled`, `defaultAction: Deny`, `bypass: None`, and one IP rule per address in `USNM_OCR_NODE_IPS` (the operator's home IP, a private setting, never committed). Nothing needs a trusted-service bypass: every Azure caller uses the private endpoint, which stays. Keys stay off, so a request from that IP still needs an Entra token with a data role. With no node IPs, the account stays closed.
- **One grant, scoped by path.** The runner's identity (`USNM_SERIALSET_RUNNER`, the node's Arc machine identity, a private setting) gets a custom role, `usnm-sset-writer-role-<env>`, on the `curated` container with an ABAC condition. The role has blob read, write and create data actions and no control-plane actions, so it can't delete or reconfigure the container either. The condition allows list, read and create only under `congress/serialset/`, and no delete or move anywhere (`infra/modules/serialset-runner.bicep`). The fetch lists only its own series, so it needs nothing else.
- **No Cosmos access.** The fetch's lock (`congress-serialset`) is held in a local state file (`--state-file`) on the node, so Cosmos stays private and the node gets no Cosmos role. That lock doesn't see the Azure job's, so the Azure job must not run while the node does. If both run, attempts are create-only, so the second writer of a volume's attempt fails instead of overwriting it.
- **The policy exempts the lake by resource id** while it is open (`publicAccountIds`, `infra/public-accounts.bicep`), the same way it exempts the exchange account and the nodes' Key Vault. Provision writes the assignment before the stack (#336, 08 §8.5).

Turning `USNM_SERIALSET_ONPREM` off closes the lake, removes the grant and drops the exemption at the next provision. Turn it off once the fetch is done. Monthly updates can run the same way, or as the Azure job.

Considered and not chosen: a VPN gateway into the VNet (a new paid resource); running the job in Azure (the compute this avoids); staging the output in the exchange account, which the node can already reach, and copying it into the lake from Azure (a copy job is Azure compute again, and the copy step is new code).
