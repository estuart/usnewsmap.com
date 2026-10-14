# BUDA Mac tests without a project server

**Status:** measured; no server changes · **Date:** 2026-10-14 · **Measured on:** arm64 Mac, macOS 26.7.1, BOINC 8.2.15, Podman 5.5.2, docker_wrapper 22 · **Issues:** [#481](https://github.com/tgoodyear/usnewsmap.com/issues/481), [draft PR #503](https://github.com/tgoodyear/usnewsmap.com/pull/503)

## The question

Follow-up: [Native ARM64 worker build and tests](2026-10-14-buda-native-arm64.md) supersedes this note's emulated OCR path. It records a native-only build environment, ARM worker executables and a new native fixture cohort.

How far can we test the Mac worker without a BOINC project server? This follows the [initial evaluation](2026-10-14-buda-evaluation.md) and supplies additional evidence for [ADR-0020](../design/adr/0020-boinc-buda-evaluation.md). It covers installed-client runtime detection, standalone wrapper execution, container restrictions, pause/restart and native output collection. It does not cover BUDA scheduling, code-signature distribution, upload or server validation.

## Setup and correction to the initial note

Podman was already installed by Podman Desktop at `/opt/podman/bin/podman`. The initial check missed this location; its conclusion that Podman was absent was wrong. The earlier wrapper could not use BOINC's Mac Podman integration because BOINC and its `Run_Podman` helper were not yet installed.

The existing Podman Desktop machine was running and reachable, with an ARM Linux guest. It was left in place. The official BOINC 8.2.15 installer was downloaded from [BOINC's Mac downloads](https://boinc.berkeley.edu/download_all.php?platform=mac); `pkgutil --check-signature` verified an Apple distribution signature and trusted notarization, and `codesign --verify` passed. The Mac user completed the normal administrator installation. The GUI/client and setuid `Run_Podman` helper were installed by that installer, not replaced with test shims.

Version 8.2.15 is the upstream development/test build. The recommended Mac release listed at the time was 8.2.11, below this project's 8.2.13 plan-class floor. The installed client reported `8.2.15 arm64-apple-darwin`, detected Podman 5.5.2 and initialized its own ARM Linux VM through `Run_Podman`. This BOINC-owned VM is separate from the existing Desktop VM. The client had zero attached projects when the standalone tests began; project configuration was left to the Mac user, and no test command attached an account.

| Artifact                                     | Pin                                                                                                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Official Mac GUI installer archive           | `boinc_8.2.15_macOSX_universal.zip`; downloaded SHA-256 `321ad72dd2712522202bf974f84b17cbc3b5282dad8859a585ae40fcf88ed34b`                              |
| Published universal Mac wrapper              | `docker_wrapper-22-apple-darwin`; SHA-256 `56032135e2d0fcdfaf48aca47bdc2defd07e816a8a7abd8bfff5f24e2f0d38b3`, matching the release asset digest         |
| OCR worker, from the committed prod settings | `ghcr.io/tgoodyear/usnm-boinc-worker@sha256:9c8eac0485d52e6f8f060f665371d60bf84356bc4b177f32f4aa3f5ccf9307b9`; inspected as `linux/amd64`               |
| Separate native ARM probe                    | Alpine 3.22, resolved as `docker.io/library/alpine@sha256:2e1a7aa4cbc4e9e5222bb4c24a839aa1a6170ea5492d644777ce7b178824e44f`; inspected as `linux/arm64` |
| Local fixture/export/collector               | Built from `96dff87`, the draft PR's source-review commit, with the locked Rust dependencies                                                            |

The worker digest was not changed to make the OCR task native. Both Podman VMs could execute its x86_64 programs through the Mac's emulation support; these OCR results are emulated, not native ARM measurements.

## What we did

The repository's `pilot_fixture` generated a synthetic `HELLO WORLD` page. `usnm-boinc export` reserved it against directory-backed native stores with the committed worker digest. Only that synthetic page and its request went to the Mac. Tests used fresh directories under the BOINC data tree, with group `boinc_project` and mode `0770`, and the repository's existing `ocr/boinc/job.toml` restrictions. An initial SSH-user-owned `0755` test directory failed to accept output through the helper; making the scratch directory group-writable fixed that test setup. This did not establish that `DAC_OVERRIDE` alone bypasses Mac host filesystem permissions.

Each wrapper launch used `/usr/bin/arch -arm64 ./docker_wrapper` without `app_init_data.xml`, so it explicitly entered standalone mode. Standalone uses image/container name `boinc`; the tests checked that name was unused first. A cold launch was bounded to 15 seconds. For completion/control tests, the derived image was then prebuilt through the installed helper, and a test-only `image_name = "boinc"` setting kept it available between launches. The OCR build explicitly selected `linux/amd64`. A delayed command in a separate derived OCR image allowed time to inspect the running container and send SIGTERM; the base worker digest and science command remained the same.

The native ARM probe used an ARM Alpine image and a small shell payload that recorded its architecture, cgroup limits and effective capabilities, and attempted a write to `/etc`. It was a container/wrapper probe, not an ARM build of the OCR worker.

## What we measured

The BOINC-managed VM had 2 GiB RAM. All tasks requested one CPU, 1536 MiB memory with no additional swap, 64 PIDs, no network, a read-only root filesystem, `no-new-privileges`, a 512 MiB `/tmp` tmpfs and the existing capability policy.

| Test                                       | Result                                     | Evidence and limit                                                                                                                                                                                                                                                                                                                       |
| ------------------------------------------ | ------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Installed native client                    | Pass                                       | `boinc --version` reported `8.2.15 arm64-apple-darwin`; client event output recorded Podman 5.5.2 detection and successful VM initialization. No scheduler request was sent to the project.                                                                                                                                              |
| Anonymous pinned-worker pull and execution | Pass, emulated                             | Both existing Desktop and BOINC VMs pulled the exact GHCR digest and executed x86_64 programs. This does not provide a native ARM worker release.                                                                                                                                                                                        |
| Direct OCR through BOINC's helper          | Pass, emulated                             | `HELLO WORLD`, matching job/image/scan metadata and text/ALTO hashes; three output files mode `0644`. Final repetition measured 208 ms worker elapsed and 1.312 s container-command wall time. One tiny warm synthetic page is not an OCR throughput benchmark.                                                                          |
| Standalone wrapper, prebuilt OCR image     | Pass, emulated payload/native host wrapper | Wrapper exited 0 and returned `HELLO WORLD`; worker elapsed 195 ms, all output files `0644`, metadata and checksums matched.                                                                                                                                                                                                             |
| Native ARM wrapper/payload                 | Pass                                       | Wrapper exited 0; container recorded `aarch64`. `/etc` write failed with exit 1, consistent with the read-only root filesystem.                                                                                                                                                                                                          |
| Resource/capability enforcement            | Pass in the native probe                   | Guest `cpu.max` was `100000 100000`, `memory.max` was `1610612736`, and `pids.max` was `64`. `CapEff` was `0000000000000002` (only `CAP_DAC_OVERRIDE`). Running OCR-container inspection also confirmed network `none`, read-only root, 1.5 GiB memory/swap limits, one CPU, PID limit, `no-new-privileges` and the tmpfs configuration. |
| Cold standalone wrapper build              | Incomplete: bounded timeout                | With no cached derived `boinc` image, wrapper 22 repeatedly logged `waiting for heartbeat`; it was stopped after 15 s. Standalone mode has no client heartbeat. This does not prove failure under a real BOINC task.                                                                                                                     |
| SIGTERM pause                              | Pass                                       | Wrapper exited 0 and Podman inspection showed the container was paused. This exercised the wrapper's signal handler, not a client RPC suspend message.                                                                                                                                                                                   |
| Restart of paused standalone task          | Fail                                       | The restarted wrapper treated the existing paused container as absent, tried to create another `boinc`, and exited 1 on a duplicate-name error. The test harness removed the paused test container afterward.                                                                                                                            |
| Native collector integrity checks          | Pass                                       | On Linux, the current `usnm-boinc collect` rejected a forged image digest and then altered OCR text before publication. Restoring the Mac wrapper's valid files published one ALTO/Parquet pair; replay left the same two objects with unchanged bytes. All stores were local directories.                                               |

## Isolating the restart failure

The wrapper's state query formats output as `{{.Names}}|{{.Status}}`. On Mac it invokes `Run_Podman`, which concatenates its arguments into another shell command. The outer shell consumes the format's quotes, so the helper reparses the unquoted `|` as shell syntax. With the actual installed helper and an isolated created test container, the same query produced:

| Invocation                                                        | Output                                                | Process exit      |
| ----------------------------------------------------------------- | ----------------------------------------------------- | ----------------- |
| Raw format argument, as received from the wrapper's shell command | Empty stdout; `zsh:1: parse error near '}'` on stderr | 0 from the helper |
| Format argument explicitly quoted for the helper's second shell   | `usnm_buda_quote_probe_481\|Created`                  | 0                 |

The published wrapper's restart failure is observed. The helper's reparsing is a supported explanation: the reproduction changes only quoting and restores the expected state output. The helper's zero exit code also hides the query failure. The reviewed source shows both mechanisms in [Run_Podman.cpp](https://github.com/BOINC/boinc/blob/22bbebe08e2eaac5d5e88a6d93b88e800cb6341c/client/Run_Podman.cpp) and [get_container_state](https://github.com/BOINC/boinc/blob/22bbebe08e2eaac5d5e88a6d93b88e800cb6341c/samples/docker_wrapper/docker_wrapper.cpp#L548). No installed helper or published wrapper binary was patched in this test.

## What is left

A native ARM science variant still needs an ARM worker image. The existing amd64 digest can execute under emulation here, but that should be an explicit platform policy and needs more than a one-page fixture for performance/result comparisons.

Resolve the Mac helper/state-query failure before claiming pause/restart support. Retest cold image building and suspend/resume with a real client-controlled workunit; client heartbeat and IPC were absent from these standalone launches. The [server spike](../experiments/2026-10-14-buda/README.md) must still establish native-platform app registration, signature checks, scheduling, download/upload, custom validation and bridge reconciliation. These Mac observations do not settle those server questions or justify a BUDA migration.

Temporary `boinc` containers/images and the quoting-probe container were removed. The installed BOINC client/helper, BOINC-owned VM, original Desktop VM, pinned source images and synthetic test files were retained for follow-up. No project account was created or attached.
