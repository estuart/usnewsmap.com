# ADR-0011: One deployment stack per environment, without azd

- **Status:** Accepted
- **Date:** 2026-09
- **Amends:** ADR-0005 decision 3 ("Bicep + azd") and 08 §8.5

## Context

`azd provision` ran an incremental ARM deployment. Resources removed from the Bicep stayed in Azure and kept billing, until someone deleted them by hand. Replacing the Static Web App (ADR-0010) is an example: bootstrap had to delete it explicitly. Nothing stopped a person deleting a managed resource in the portal, either.

Azure **deployment stacks** track the resources a template deploys. They can delete resources that leave the template (`actionOnUnmanage`) and block deletes made outside the stack (`denySettings`). Deployment stacks are GA in ARM. azd supports them only as an alpha feature, switched on per machine (`azd config set alpha.deployment.stacks on`), and its documentation ties `actionOnUnmanage` to `azd down` rather than to ordinary updates. Beyond deploying, azd only stored settings and substituted them into `main.parameters.json`. The maintainer agreed to drop azd if that helped.

## Decision

- **The environment is one stack at subscription scope, `usnm-<env>`.** It covers both resource groups and everything in them, including the guard-rail policy assignments. `scripts/bootstrap.sh` and `scripts/provision.sh` deploy it with:
  ```
  az stack sub create --parameters infra/main.bicepparam \
    --action-on-unmanage deleteResources --deny-settings-mode denyDelete
  ```
  - `deleteResources`: a resource dropped from the template, or switched off by a setting, is deleted on the next deployment. Resource groups are never deleted this way; teardown removes them.
  - `denyDelete`: every managed resource, the resource groups included, gets a deny assignment against deletes by anyone. Changes and removals go through the stack. Writes are still allowed, so CI can roll the API and bootstrap can bind certificates.
- **The guard-rail policy definitions are a stack of their own, `usnm-guardrails`** (`infra/guardrails.bicep`, same settings), deployed just before the environment's stack. Every environment in a subscription assigns the same definitions. If each environment's stack managed them, two stacks would claim one resource: one environment switching them off would try to delete what the other still assigns, and fail on the other's deny assignment. The environment template builds the definition ids from the names it imports from `guardrails.bicep`.
- **No azd.**
  - **Settings:** they stay in `.azure/<env>/.env`, in the same `KEY="value"` layout, so existing environments carry over unchanged.
  - **Parameters:** `infra/main.bicepparam` reads them with `readEnvironmentVariable`. Bootstrap exports only the settings that have a value, so empty ones fall back to the parameter defaults.
  - **Outputs:** they're written back to the settings after each deployment.
  - **Commands:** `scripts/settings.sh` reads and writes settings. `scripts/provision.sh` deploys the stack alone. `scripts/teardown.sh` replaces `azd down`.
- **Teardown detaches, then deletes explicitly.** It removes the environment's stack in detach mode, then deletes the resource groups and the environment's custom role. The stack's own `deleteAll` would fail on a resource group that also holds resources the stack doesn't manage. The last environment in the subscription also deletes `usnm-guardrails` with its definitions.

## Alternatives

| Option                                                  | Why not                                                                                                                 |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| azd with `alpha.deployment.stacks`                      | Alpha. The flag is per machine, not in the repository. `actionOnUnmanage` is documented for `azd down`, not for updates |
| Incremental deployments plus cleanup steps in bootstrap | Every removal needs its own hand-written step, and nothing prevents manual deletes                                      |
| Complete-mode deployments                               | Resource-group scope only (this template is subscription scope), and no delete protection                               |
| Resource locks                                          | Protect against deletes, but don't remove resources that left the template, and block the template's own removals too   |

## Consequences

- **Removing a resource from the Bicep is a real deletion** on the next deployment. Data lives in the storage and Cosmos accounts, which are declared unconditionally. A conditional resource goes away when its setting turns it off, for example the ingest jobs, the DNS zone without `USNM_DNS_ZONE`, or the budget without alert emails.
- **Deleting a resource by hand requires a deployment with `--deny-settings-mode none` first.** Deny assignments also block Owners.
- **Resources the template doesn't declare are neither protected nor cleaned up.** Examples are the managed certificates bootstrap binds and anything created in the portal.
- **No what-if.** Deployment stacks don't support what-if yet.
- **Every deployment re-applies the whole template.** azd skipped deployments when nothing had changed.
- **Two environments in one subscription share `usnm-guardrails`.** Either environment's deployment updates it, and it stays until the last environment's teardown. The archival account's group (`usnm-archive`) assigns the definitions too, so while it exists the stack stays (#276). Its location is the first environment's region.
