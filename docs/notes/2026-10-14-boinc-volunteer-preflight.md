# Rootless BOINC volunteer readiness on an SELinux host

**Status:** measured, contribution proposed · **Date:** October 2026 · **Measured on:** Linux x86-64, RHEL 10.2, SELinux enforcing, BOINC 8.2.13, Podman 5.8.2, cgroup v2 · **Code:** `scripts/boinc/setup_volunteer.py`, `scripts/boinc/volunteer_preflight.py`, [volunteer guide](../boinc-volunteer.md) · **Issues:** [#504](https://github.com/tgoodyear/usnewsmap.com/issues/504), [#469](https://github.com/tgoodyear/usnewsmap.com/issues/469), [#484](https://github.com/tgoodyear/usnewsmap.com/issues/484)

## The question

Can a trusted volunteer attach a stock Linux BOINC client and rely on a container starting successfully to establish slot access, bounded resource use and idle-time pause? A live pilot showed that this was insufficient. The client and the project's pinned wrapper both support Podman directly; Docker-group access was unnecessary.

## What we did

The stock wrapper binds the BOINC slot without a `:z`/`:Z` relabel option. On the enforcing host, container access to the original slot label failed. The worker's job-file existence check made this appear as a missing file. A `container_file_t` label scoped to the slots directory restored input/output access while SELinux remained enabled.

Podman also accepted the configured resource arguments without a usable container cgroup. Inspecting the OCI resource configuration initially overstated enforcement: actual kernel readings were unbounded, and pause failed with “cannot pause the container without a cgroup”. A BOINC user systemd session, Podman's systemd cgroup manager, and writable cgroups in the service restored limits and pause. The container remains rootless; no Docker socket or root-equivalent group is required.

The profile preserves runtime files through a service restart and places Podman's monitors with their containers. The client keeps apps in memory during suspension. The preflight runs in `ExecStartPre` as the service user and exercises the same task runtime policy against a disposable real slot. It adds no mount relabel option, reads the kernel cgroup files, checks read-only/no-network behavior, tests pause/resume, and removes only its own probe. A regression keeps its resource flags aligned with `ocr/boinc/job.toml`.

The installation helper was exercised with a separate stock BOINC service and empty, account-free data on the same host. A deliberate wrong slots label caused `ExecStartPre` to fail with no main-client start timestamp. Restoring the label allowed startup. Unit tests also simulate a runtime accepting flags while reporting unbounded limits; that cannot pass the gate.

## What we measured

| Check                                      | Before                                 | After                                          | Evidence                                             |
| ------------------------------------------ | -------------------------------------- | ---------------------------------------------- | ---------------------------------------------------- |
| Slot input/output access                   | denied with original label             | input readable, host-readable mode-0644 output | real enforcing-host probe, without a `:z`/`:Z` mount |
| CPU limit                                  | unbounded kernel reading               | `cpu.max = 100000 100000`                      | read inside live container and preflight             |
| Memory limit                               | unbounded kernel reading               | `memory.max = 1610612736`                      | read inside live container and preflight             |
| Extra swap                                 | not established                        | `memory.swap.max = 0`                          | preflight kernel reading                             |
| Process limit                              | host-level limit                       | `pids.max = 64`                                | read inside live container and preflight             |
| Pause/resume                               | pause failed                           | paused then running                            | stock BOINC wrapper and preflight                    |
| Service restart                            | monitor/runtime handling needed repair | wrapper resumed the same paused container      | live stock-client task                               |
| Startup with deliberately wrong slot label | client could previously accept work    | main client did not start                      | isolated installed service, `ExecStartPre` failure   |
| Startup after label repair                 | unavailable                            | preflight and client startup passed            | same isolated service                                |

The public gold bundle produced 90 scored crops and 52 skips for unavailable French, Finnish and Swedish models. Those counts establish local execution, not successful ingestion. Result-file uploads were separately rejected by the server for missing signatures, even though the scheduler received completion reports. Upload, validation and collection must be checked independently; they are outside this host-readiness contribution.

## What is left

Only the stated RHEL/BOINC/Podman combination has live coverage. Debian/Ubuntu and other distributions need the same host probe, service-restart test and actual client/server result acceptance before wider enrollment. Keyboard-triggered idle behavior over a full idle interval, laptop sleep and OS reboot have not been verified by the manual pause/service-restart tests. The optional modern idle-detection helper remains a separate dependency to evaluate.

The preflight adds startup latency and depends on a cached or anonymously pullable pinned worker image. Re-run setup with the released image digest after draining tasks when the app image/runtime policy changes. It checks readiness at startup, not continuously while a task runs. Kernel/runtime updates still need a restart and a fresh probe. A future release test (#484) should run this host gate as well as require output upload, validation and assimilation.
