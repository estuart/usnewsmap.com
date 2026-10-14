# Native ARM64 BOINC worker on the Mac

**Status:** built and tested locally; no registry publication or server changes · **Date:** 2026-10-14 · **Measured on:** macOS 26.7.1 arm64, Podman 5.5.2, BOINC 8.2.15, docker_wrapper 22, repository code at `96dff87` · **Issue:** [#481](https://github.com/tgoodyear/usnewsmap.com/issues/481)

## The question

Can the Mac run the actual OCR worker natively, rather than emulate the committed amd64 worker? This supersedes the emulated OCR path in the [earlier Mac tests](2026-10-14-buda-mac-offline-tests.md). The native engine and worker were built successfully, loaded into BOINC's Podman VM and tested with the same synthetic fixture and runtime restrictions.

## Native environment and build

A dedicated Podman machine, `usnm-buda-native-481`, uses 4 CPUs, 8192 MiB RAM and a 60 GiB disk. Its configuration and data live under `~/Library/Application Support/usnm-buda-native-481/`, in a separate Podman namespace. This permits it to run alongside the existing Desktop and BOINC machines without changing their default connection. Cross-architecture execution was disabled in this build VM by writing `0` to `/proc/sys/fs/binfmt_misc/status`; its observed status was `disabled` during the build and executable checks. A VM reboot requires checking that setting again before a native-only rerun.

The engine used the repository's existing `ocr/engines/tesseract/Containerfile`, with `BASE` overridden to a pinned ARM64 counterpart of the Debian trixie-slim base. The same Debian snapshot, Tesseract 5.5.0 package pins, floating-point shim and model checksums were retained. The worker used `ocr/boinc/Containerfile` with its Rust build-stage `FROM` pinned to an ARM64 Rust image, `BUILD_JOBS=2`, and the native engine supplied through `TESSERACT_FROM` and `TESSERACT_IMAGE`. These were local experiment overrides; production Containerfiles, image settings and release workflows were not changed.

| Artifact                             | Immutable identity                                                                                 |
| ------------------------------------ | -------------------------------------------------------------------------------------------------- |
| Native Debian base                   | `docker.io/library/debian@sha256:a2ca65377c251208de42e1a2722cb949211236c161b634f6f3663754e2d06850` |
| Native Rust build base               | `docker.io/library/rust@sha256:e393e07b3c3c39f238de676b0eb60c03dc7aefe5d6e37be6d29c686bf4d7e256`   |
| Native engine configuration/image ID | `sha256:626b866f8148178e65b9f511680f90f64ee90e308cdacd7c75eeec41398cf955`                          |
| Native worker configuration/image ID | `sha256:59aacffd3c9ea675dcd6bc6ef1943a8ea7b9a8eb27c3f7b87b16c71e8f760c3c`                          |
| Native worker OCI archive manifest   | `sha256:fa691571530372480d20565891afd8213f1793c2b4941743cab4508905907198`                          |

The local tags are `localhost/usnm-ocr-tesseract:arm64-spike` and `localhost/usnm-boinc-worker:arm64-spike`. The worker was exported as an OCI archive and loaded through the installed `Run_Podman` helper into BOINC's separate image store. Inspection there confirmed the same configuration ID, OCI manifest digest and `linux/arm64` architecture. This is a local test artifact, not a published GHCR release or a signed BOINC app version.

The new synthetic cohort used the OCI manifest digest in its request's `engine.image`. The standalone wrapper's Dockerfile selected the already-loaded image by its immutable configuration ID. These are distinct identities: the request records the exported artifact manifest, while the cached local build references its verified configuration/image ID. The tests checked that both referred to the same loaded ARM worker; they did not reuse an amd64 cohort or disguise the new image as the old digest.

## What we measured

| Test                                      | Result                       | Evidence and limit                                                                                                                                                                                                                                                                                     |
| ----------------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Native engine build                       | Pass                         | All model checksums passed; engine inspection reported `linux/arm64`; Tesseract 5.5.0 executed in the build VM with cross-architecture handlers disabled.                                                                                                                                              |
| Native worker build and engine binding    | Pass                         | Worker inspection reported `linux/arm64`; engine layers were the prefix of its layer list, and `USNM_TESSERACT_IMAGE` matched the native engine ID.                                                                                                                                                    |
| Native executables                        | Pass                         | ELF headers of `/usr/local/bin/usnm-boinc` and `/usr/bin/tesseract` both had machine 183 (`EM_AARCH64`). The checks ran with cross-architecture execution disabled.                                                                                                                                    |
| Direct native OCR through BOINC's helper  | Pass                         | `HELLO WORLD`, matching job/scan/engine metadata and text/ALTO checksums; all three outputs `0644`. One warm fixture measured 79 ms worker elapsed and 0.407 s command wall time. This is not a throughput or accuracy benchmark.                                                                      |
| Native Mac wrapper and native OCR payload | Pass                         | `/usr/bin/arch -arm64 ./docker_wrapper` exited 0 with a prebuilt native derived image. OCR elapsed was 93 ms; returned files were readable and passed provenance/checksum checks.                                                                                                                      |
| Runtime limits                            | Pass                         | Running OCR-container inspection confirmed one CPU, 1536 MiB memory with no extra swap, 64 PIDs, no network, read-only root, `no-new-privileges` and the 512 MiB tmpfs. The separate native shell probe confirmed the actual cgroup limits and only `CAP_DAC_OVERRIDE`, and failed to write to `/etc`. |
| Native output collection and replay       | Pass                         | The native collector rejected a forged image digest and altered text before publication. Valid Mac ARM output then produced one ALTO/Parquet pair in local stores; replay left both objects unchanged.                                                                                                 |
| Cold standalone build                     | Still incomplete             | Wrapper 22 waited for a client heartbeat and was stopped after 15 s. Prebuilding the immutable local image enabled standalone completion. A client-controlled workunit remains necessary to test the wrapper's own cold-build path.                                                                    |
| Pause and restart                         | Pause passed; restart failed | SIGTERM paused the native container and the wrapper exited 0. Restart exited 1 on a duplicate-container-name error, reproducing the installed helper's quoting/state-query problem from the earlier note. This failure is independent of the worker's CPU architecture.                                |

No amd64 payload was used in this native rerun. The same result-contract checks were applied to the new native image and cohort.

## Using the environment

On the Mac, select the native build namespace in the shell where its images are needed:

```sh
export XDG_CONFIG_HOME="$HOME/Library/Application Support/usnm-buda-native-481/config"
export XDG_DATA_HOME="$HOME/Library/Application Support/usnm-buda-native-481/data"
/opt/podman/bin/podman machine list
/opt/podman/bin/podman image inspect localhost/usnm-boinc-worker:arm64-spike
/opt/podman/bin/podman run --rm --network=none --entrypoint /usr/bin/uname \
  sha256:59aacffd3c9ea675dcd6bc6ef1943a8ea7b9a8eb27c3f7b87b16c71e8f760c3c -m
```

The namespace directory retains `source/`, `source.tar.gz`, `base-pins.json`, `build-report.json`, `native-elf-report.json`, build logs and the preparation/build/test scripts under `tools/`. Its native build script reuses the saved ARM base pins. The BOINC-managed VM also retains the loaded native worker tag for wrapper follow-up. Temporary standalone `boinc` containers/derived tags were removed after testing.

## What is left

Resolve and retest the wrapper/helper pause-restart failure. Complete the [server spike](../experiments/2026-10-14-buda/README.md) with a registered native Mac app version, signed wrapper, native science variant, task distribution, upload, custom validation and recovery. A public native worker release must receive its own immutable registry digest and fresh cohort metadata; this locally built image does not update the existing production worker setting.

The native build establishes that the OCR worker can run on ARM. It does not remove ADR-0020's validation, code-authorization or mutable-variant concerns, and does not establish the minimum Mac client version from testing one development release.
