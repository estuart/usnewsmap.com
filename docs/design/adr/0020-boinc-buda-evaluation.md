# ADR-0020: Retain separate BOINC apps while evaluating BUDA

- **Status:** Proposed; source review and Mac launch smoke completed, server spike pending
- **Date:** 2026-10-14
- **Extends:** [ADR-0019](0019-boinc-server-hosting.md)
- **Issue:** [#481](https://github.com/tgoodyear/usnewsmap.com/issues/481)
- **Evidence:** [evaluation note](../../notes/2026-10-14-buda-evaluation.md) and [remaining spike procedure](../../experiments/2026-10-14-buda/README.md)

## Context

The project has three stock BOINC Docker-wrapper apps: `usnm_ocr`, `usnm_assess` and `usnm_gold`. BUDA could replace their signed app versions with one `buda` app, using science-app variants for the worker files. This would reduce release administration, but changes how science code is authorized, how results are validated and how pending tasks select worker files.

The review used the repository's pinned BOINC commit, `22bbebe08e2eaac5d5e88a6d93b88e800cb6341c`. The published version 22 Mac wrapper was inspected and launched natively on an arm64 Mac. There was no access to the project server, database or signing host. The Mac had no BOINC client or Podman installation. These observations are not an end-to-end BUDA result.

## Proposed decision

Retain the three separate BOINC apps. Run BUDA only as an additional experimental app until the remaining spike establishes validation, immutable worker selection and recovery through the bridge. Do not migrate outstanding cohorts or replace the existing daemons based on this review.

1. **Keep validation selected by the server.** BUDA does not have a validator field in its variant schema at the pinned commit. Its documented setup uses `sample_trivial_validator`, which does not compare scientific output. Our OCR pilot currently uses this validator too; the fuzzy OCR validator in #444 is a separate requirement, not an already deployed control. The assessment and gold apps already use different `script_validator` checks. Migration must preserve those checks and support the future OCR validator.
2. **A custom validator is possible, but requires integration.** Stock `script_validator --app buda` can run a project-owned dispatcher. Passing `result_id` permits that dispatcher to resolve the result's workunit through the server database and select an allowlisted science-app policy. It must reject missing or unknown mappings and compare only results from the same immutable science configuration. An alternative is an upstream change passing workunit/science-app context to validation hooks. Neither has been implemented or run in this spike. Starting several validators with `--app buda` does not give each a science-app filter.
3. **Preserve offline authorization of executable inputs.** BUDA variant Dockerfiles and `job.toml` files are workunit inputs rather than signed app-version files. A digest-pinned `FROM` preserves the selected base image but does not authenticate the Dockerfile or runtime configuration: either could change the command, networking or mounts. Adopting BUDA would require an explicit acceptance of that trust change or a signed launcher/manifest verification design before executing science code.
4. **Freeze science-app names for each worker release.** The scheduler reads current variants and adds their files when sending a task. A mutable variant is not a version pinned into each queued workunit. Replacing it can send pending or replacement results to different code. A migration must journal the science-app identity, image digest, platform/variant policy, batch ID and workunit name, retain old variants until cohorts drain, and prove crash recovery and retries preserve that identity.
5. **Reuse pinned images, with a local build step.** A variant can contain `FROM ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<digest>` and the existing `job.toml`. The wrapper still builds a small derived image on the volunteer host; it is not a registry-pull-only mode. Its default image name is workunit-specific. Cold/warm bandwidth and build time need measuring before claiming a caching improvement. Keep the existing runtime limits and native output checks.
6. **Treat platform support separately from BUDA adoption.** The published Mac wrapper contains both arm64 and x86_64 code, but a native arm64 client needs an app version registered for its primary platform and a matching ARM science variant. The pinned scheduler prevents BUDA's fallback to an Intel app version on an ARM Mac. The worker image also needs a compatible architecture; a universal host wrapper does not make a Linux worker image multi-architecture. The same wrapper can be used by separate apps, so BUDA is not required to add Mac support.

## Alternatives

- **Migrate immediately using the trivial validator.** Rejected: it discards the assessment and gold checks and does not meet the anonymous-volunteer OCR validation requirement.
- **Use one BUDA app with a project-owned validation dispatcher.** Feasible in principle using stock BOINC, subject to a server-side identity mapping and the remaining tests. It does not by itself restore science-code signing or freeze mutable variants.
- **Extend upstream BUDA with per-science-app validation and immutable versions.** A possible later project. Keeping script names in web-editable variant metadata is not sufficient; policy selection must remain administrator-controlled.
- **Share the upstream wrapper binary across the three signed apps.** Keeps the current validator separation and signed Dockerfile/configuration. Improve release tooling independently of a BUDA migration.

## Consequences

The project continues signing worker Dockerfiles and runtime configurations per app version. This costs release work but preserves the current bridge and validation boundaries.

The proposed decision can be accepted after the server spike or revised if a BUDA implementation demonstrates the same validation, code authorization and cohort identity guarantees with acceptable maintenance cost. Runtime observations, including failures, belong in the evaluation note. Issue #481 remains open while server submission, custom validation and full client execution are untested.
