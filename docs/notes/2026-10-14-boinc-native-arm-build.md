# Native BOINC worker builds and Mac release tooling

**Status:** measured locally, not released · **Date:** October 2026 ·
**Measured on:** synthetic `pilot_fixture`, repository worker Containerfile SHA-256
`c2290fa7894870ae494d5fdbdaeea4994af23af1c9268ce0404af3b3103790e7`; Rust and model sources unchanged from `df3ddc2` ·
**Code:** [worker workflow](../../.github/workflows/boinc-worker.yml),
[native smoke](../../scripts/boinc/smoke-worker.py),
[Mac wrapper build](../../scripts/boinc/build-mac-wrapper.sh) ·
**Issues:** [#476](https://github.com/tgoodyear/usnewsmap.com/issues/476),
[#481](https://github.com/tgoodyear/usnewsmap.com/issues/481)

## The question

Can Apple Silicon volunteers execute the existing science worker natively in
Podman, and can the repository supply the build and signed-release inputs without
hand-editing the Azure server? The BUDA evaluation remains separate in
[PR #503](https://github.com/tgoodyear/usnewsmap.com/pull/503).

## What we did

Built the repository's worker and fixture-generator build stage in a dedicated
ARM64 Podman VM on an Apple Silicon Mac. The VM used four CPUs and 8 GiB RAM;
`/proc/sys/fs/binfmt_misc/status` was `disabled` during the build and smoke test.
The build used the ARM Rust digest pinned in the workflow and the previously
built native Tesseract engine configuration ID
`sha256:626b866f8148178e65b9f511680f90f64ee90e308cdacd7c75eeec41398cf955`.
Tesseract 5.5.0 and the repository's model lock were unchanged.

The repeatable round trip was:

```sh
python3 scripts/boinc/smoke-worker.py --runtime /opt/podman/bin/podman \
  --image localhost/usnm-boinc-worker:arm64-pr \
  --build-image localhost/usnm-boinc-build:arm64-pr \
  --arch arm64 --dir /tmp/usnm-native-arm-pr/smoke
```

The script checks native image architectures, runs export/work/collect with no
network, a read-only container root and the worker's resource limits, rejects a
forged engine digest, and checks collection replay. The fixture generator runs
natively as well. The workflow runs the same test on native amd64 and arm64
Ubuntu runners before publishing a joint index. Hosted runs and index publication
were not executed locally.

Built both Mac wrappers using the repository script and upstream BOINC commit
`22bbebe08e2eaac5d5e88a6d93b88e800cb6341c`, matching the server source pin. Both
passed architecture and checksum checks. The ARM wrapper SHA-256 was
`8cb7f9f046f4c5355c4ce54b264a85c1c899f089496aec1b77e61e95cc48994c`.
An additional offline probe used that ARM wrapper through BOINC 8.2.15's installed
`Run_Podman` helper with Podman 5.5.2 and the earlier native worker recorded in
PR #503. Its derived image was prebuilt to bypass the standalone cold-build
heartbeat wait; this was not a scheduled BOINC job.

## What we measured

| Probe                                  | Result                                                                                          | Environment and scope                                                            |
| -------------------------------------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| New worker recipe: export/work/collect | `HELLO WORLD`, OCR 86 ms, round trip 0.839 s                                                    | Native ARM VM, one synthetic page; warm image/cache, not a performance benchmark |
| Collector provenance and replay        | Forged image rejected; one ALTO and one Parquet object, unchanged on replay                     | New `smoke-worker.py` on the same native VM                                      |
| Pinned-source ARM wrapper completion   | Exit 0, `HELLO WORLD`, OCR 93 ms; outputs mode 0644                                             | Prebuilt image through the installed BOINC helper                                |
| Wrapper limits                         | aarch64, one CPU, 1.5 GiB RAM, 64 PIDs, read-only root, no network, only DAC_OVERRIDE effective | Separate native probe through the wrapper                                        |
| Cold standalone wrapper                | Waited for client heartbeat; bounded test stopped at 15 s                                       | No client IPC; does not establish failure under a real client                    |
| Pause then restart                     | Pause exited 0 with container paused; restart exited 1 with duplicate-name error                | Upstream helper state-query quoting issue reproduced; still unfixed              |

The Intel wrapper was compiled and checked, not executed. No private signing key,
server registration, scheduled job, upload, validator, assimilator or live bridge
was used. No published settings were changed.

## What is left

The maintainer must merge the build tooling, publish the tested worker index,
commit public digest settings, and use the [offline-signing release procedure](../../contrib-compute/docs/app-release.md#mac-app-versions).
Then verify a reserved native Mac `usnm_ocr` job end to end, fix and retest Podman
pause/restart, test client restart/reboot, and establish native gold-page agreement
before closing #476. Gold and assessment expectations currently hold one
architecture-specific engine base ID; a multi-architecture campaign requires
matching expectations rather than reusing the amd64 ID for arm64. The
server-dependent BUDA evidence in #481 remains outstanding.
