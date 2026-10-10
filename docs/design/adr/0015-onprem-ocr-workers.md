# ADR-0015: On-prem OCR workers with Azure Arc identities and an exchange account

- **Status:** Proposed
- **Date:** 2026-10
- **Extends:** ADR-0008 (private networking) and ADR-0009 (Entra identity only)

## Context

The language report ([`docs/reports/2026-10-language-analysis/report.md`](../../reports/2026-10-language-analysis/report.md)) scored a 10% sample of index version `pages-v20261003-1`: 2,371,337 pages from all 2,989 batches. Damage is the share of occurrences of a language's 20 most common words that LoC's OCR read one edit wrong; a page is badly damaged above 0.25. Some parts of the corpus are read badly:

- **German before the 1920s.** 57,580 sampled German pages from the 1850s to the 1910s, so about 580,000 pages in the index. The _Nebraska Staats-Zeitung_ has median damage 0.62, with 99% of 292 sampled pages badly damaged; the _Richmonder Anzeiger_ is at 0.43. The report's hypothesis is Fraktur type; no sampled page's typeface has been checked (#246).
- **The worst English batches.** `nn_kant_ver01`, `txdn_japan_ver01`, `nn_bentham_ver01`, `nn_carson_ver02` and `kyu_frenchie_ver02` have median damage 0.46 to 0.52, and 88% to 99.8% of their scored pages are badly damaged.
- **Serbian, Russian and Czech.** Serbian (the _Amerikanski Srbobran_) has median damage 0.257, with 52.7% of pages badly damaged; Russian 0.240 (41.2%); Czech 0.212 (39.2%). Part of the Serbian and Czech score may come from the metric's bias against inflected languages.

Search can't find a page whose words were misread. Re-reading these pages on cloud compute or a cloud OCR service would cost far more than the project's monthly budget (ADR-0005). Two local machines can do the work instead (#296). They are outside Azure, so they have no managed identity, and they can't reach the data lake, which allows private endpoints only (ADR-0008).

## Decision

1. **The nodes are Azure Arc-enabled servers running AlmaLinux 9 (or RHEL 10).**
   - The Connected Machine agent gives each node a system-assigned managed identity. The worker gets Entra tokens from the agent at `localhost:40342`, so no key, secret or certificate is stored on the node (ADR-0009).
   - Arc [supports](https://learn.microsoft.com/en-us/azure/azure-arc/servers/prerequisites) AlmaLinux 9 and RHEL 10 on x86-64 and not Fedora, which the first node ran. Node setup installs no OCR engine: EL9's AppStream has only Tesseract 4.1.1 and EPEL 9 has none, so the engines come as pinned container images (podman) or a source build.
   - The machines are resources in the environment's group, `rg-usnm-<env>`, outside the deployment stack. The no-IaaS policy denies VMs, scale sets, AKS and Batch; `Microsoft.HybridCompute/machines` isn't on its list, and the machines are on-prem hardware.
   - `scripts/ocr-node/onboard.sh` connects a node with a short-lived ARM token from the operator's `az` sign-in, sent on ssh's stdin and read by `azcmagent connect --config` from a root-only file on tmpfs that is deleted straight after.
2. **The agent is locked down before it connects.** `extensions.enabled false` and `guestconfiguration.enabled false`, so nobody in Azure can install an extension, run a command or apply a machine configuration on the node; `incomingconnections.enabled false`, so there's no SSH through Arc. The worker runs as the unprivileged `usnm` user in the `himds` group (needed to read the agent's token challenge), from a hardened systemd unit.
3. **A separate exchange account, `stusnmo<suffix>`, holds the queues and the nodes' output** (`infra/modules/ocr-exchange.bicep`, with `USNM_OCR_NODES`).
   - Entra only: shared keys off, public blob access off.
   - Public network access is on and the firewall's default action is Deny. It allows the node IPs (`USNM_OCR_NODE_IPS`, the operator's home IP, kept in the environment's settings and never in the repository) and the Container Apps subnet, through a Microsoft.Storage service endpoint on `snet-cae`. There's no trusted-service bypass.
   - The public-network policy exempts it by resource id in the environment's assignment, as it does the tiles account.
4. **The work queues are Azure Storage queues:** `ocr-priority`, `ocr-bulk` and `ocr-deadletter`.
   - One page per message: lccn, date, edition, sequence, batch, expected language and why it was queued. No search text.
   - A worker takes from `ocr-priority` first. It hides a message for a fixed time when it takes it, long enough for its slowest page. Renewing the hide (Update Message) needs `messages/write`, which would also let a node add or rewrite messages; the nodes don't get it. A crashed worker's page comes back when the time runs out. A message taken 5 times goes to `ocr-deadletter`.
5. **The nodes write staging data only.** Each node's identity has Storage Queue Data Message Processor on `ocr-priority` and `ocr-bulk` (take, peek and delete), Storage Queue Data Message Sender on `ocr-deadletter` (add only), and a custom role on the `ocr-out` container that lists, reads and creates blobs but can't delete them, so one node can't remove another's output. Nothing on `curated`, `reference`, the index or Cosmos. A cloud job running as `id-usnm-ingest` checks each part (schema, `doc_id` against its `PageKey`, text size), copies it to `curated/ocr-local/pages/` and deletes it from `ocr-out`; a lifecycle rule deletes anything left there after 30 days.
6. **The production worker is Rust**, a binary built from this workspace (ADR-0004), so it shares `PageKey`, `doc_id`, the Parquet overlay types and the Entra credential code with ingest. That credential code gets an Arc source: today it uses a managed identity only when `IDENTITY_HEADER` is set (Container Apps), and Arc's endpoint needs its `WWW-Authenticate` challenge instead. Python stays for the engine survey and audits.
7. **The engine survey comes first.** Before any bulk run, Node 1 compares engines on sampled pages from each group, measuring damage, character error rate on hand-transcribed crops, seconds per page and peak memory. Its results go in a draft report, and we choose the bulk engine by its gain and pages per day.

## Alternatives

| Option                                                | Why not                                                                                                                                    |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Azure Local (formerly Azure Stack HCI)                | Its own OS on validated hardware with a 32 GB ECC minimum; a VM on Proxmox can't be a node                                                 |
| A service principal with a certificate on each node   | A long-lived credential on disk at home, to rotate and to lose; Arc's identity needs none                                                  |
| Site-to-site VPN into the VNet, nodes using the lake  | A VPN gateway is billed by the hour whether used or not; an idle storage account costs only what it stores                                 |
| Service Bus queues                                    | Standard has a monthly base fee; Storage queues are billed per operation only                                                              |
| A queue in Cosmos DB                                  | Competes with the pipeline state for the free tier's request units, and needs the Cosmos private endpoint the nodes can't reach            |
| Nodes writing straight into `curated`                 | A stolen node could change the system of record; with staging only, it can at most write junk into `ocr-out`, which the copy-in job checks |
| A private endpoint for the exchange account           | Billed by the hour, and the nodes would still need a public path; the subnet's service endpoint is free                                    |
| A custom role with `messages/write` for lease renewal | Also lets a node add or rewrite messages in the work queues; a fixed hide long enough for the slowest page does the same job               |

## Consequences

- **The nodes run a supported OS**, at the cost of rebuilding Node 1 from Fedora as AlmaLinux 9. AlmaLinux 10 isn't on Arc's list yet; RHEL 10 is.
- **A home IP change cuts the nodes off** until `USNM_OCR_NODE_IPS` is updated and the environment provisioned ([operations](../../operations.md#on-prem-ocr-nodes)). The queues hold the work meanwhile, and hidden messages come back.
- **The service endpoint changes the subnet's storage traffic.** Requests from `snet-cae` to storage public endpoints leave from private addresses. The data lake and archive accounts are reached through private endpoints, so nothing they do changes; turning the endpoint on or off closes open connections to storage public endpoints ([Microsoft](https://learn.microsoft.com/en-us/azure/virtual-network/virtual-network-service-endpoints-overview)), so it's provisioned between jobs.
- **The onboarding token is the operator's.** `azcmagent connect` needs a token that can create the machine resource, and the operator's carries their Owner rights for about an hour. Root on the node during onboarding could copy it. We accept this for a node the operator has just built and that nobody else has had root on. The alternative, a service principal with only Azure Connected Machine Onboarding on the group, needs a credential and an Entra app registration, a tenant-level object the project otherwise avoids.
- **Root on a node can undo the lockdown.** Anyone who can run commands as root can change the agent's configuration ([Microsoft](https://learn.microsoft.com/en-us/azure/azure-arc/servers/security-extensions)). Its local settings protect the node from Azure, not from its own administrators. Losing a node exposes only what its identity can do: take pages, add dead letters and write to `ocr-out`.
- **No new charge for Arc.** The Arc control plane for servers has no charge; Storage queue operations, the account's storage and its logs are the new costs. `StorageQueueLogs` has a row per queue request, empty polls included, so the worker backs off while the queues are empty.
- **Arc machines aren't in the stack.** Teardown deletes them with the resource group; a node that leaves is removed with `azcmagent disconnect` and its id taken out of `USNM_OCR_NODE_PRINCIPALS`.
