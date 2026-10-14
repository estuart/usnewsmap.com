# Surya 2 on BOINC

Issue: [#516](https://github.com/tgoodyear/usnewsmap.com/issues/516). Round 1 of the engine survey ([report](reports/2026-10-ocr-engine-survey/report.md), #370) ran Surya 2 on one CPU node, on 8 pages. This app, `usnm_surya`, runs it on volunteer hosts, on CPU and on NVIDIA GPUs, to read a larger sample and to measure CPU and GPU time per page. The other apps are described in [boinc.md](boinc.md) and [boinc-assessment.md](boinc-assessment.md).

Status: a probe of a few pages, to find out whether volunteers' container runtimes can give the job a GPU, how long a page takes, and how far two hosts' texts agree. The larger sample comes after it.

## What a unit is

One page per workunit: LoC's full-size IIIF JPEG (`page.jpg`) and the job (`surya.json`). The host reads the page in up to three input modes:

| Mode     | Input                                                                                                                                      |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `tiled`  | 1,280 px tiles overlapping by 256 px, as round 1 read pages, with each overlap split at its midpoint so a block is kept from one tile only |
| `whole`  | the whole page in one request; the model scales it to its own image budget                                                                 |
| `capped` | the whole page, scaled first so its long side is at most 2,048 px                                                                          |

A host with a usable GPU runs all three, each with a 1,800 s deadline. A CPU-only host runs `tiled` only, with a 5,400 s deadline: round 1 took a median 2,411 s per tiled page with about 8 cores busy.

Every result has five files: `text-tiled.txt`, `text-whole.txt`, `text-capped.txt` (empty for a mode that didn't run), `result.json` and `diag.txt`. The types are in `crates/usnm-ocr-contract/src/surya.rs`.

## The image

`ocr/boinc/surya/Containerfile` builds one image for both app versions, on llama.cpp's CUDA server image, pinned by digest. That build loads its CUDA backend when the container can see an NVIDIA GPU and its CPU backends otherwise. On top of it: surya-ocr 0.22.1 in a Python virtual environment, with the CPU build of torch (torch only runs Surya's layout fallback; the OCR model runs in llama-server), and `usnm-boinc`, which runs the unit.

The weights are in the image: Surya 2's GGUF model and vision projector, and the layout model, at the revisions round 1 used. `ocr/boinc/surya/weights.lock` pins each file's URL and sha256, and the build checks them. Datalab's licence (a modified Open RAIL-M) requires a copy with every distribution of the weights, and notice of its use-based restrictions, so the image carries `LICENSE` beside the weights (`/opt/surya/models/LICENSE`) and a `NOTICE`. The image records its llama.cpp base, Surya version and weights lock digest as `USNM_SURYA_LLAMA_IMAGE`, `USNM_SURYA_VERSION` and `USNM_SURYA_WEIGHTS`; a unit carries the same three values and the host refuses a unit staged for another build.

The image is several GB (the weights alone are 1.6 GB), which a volunteer downloads once per image version. `.github/workflows/boinc-surya.yml` builds it, checks that it reads a rendered line on CPU, and, when started by hand on main, pushes it to the public package `ghcr.io/tgoodyear/usnm-boinc-worker` with the tag `surya-<commit>`. The run's summary gives the digest and the values to stage with.

## App versions and plan classes

| Version | Plan class           | Sent to                                                                                                                                        | Container                                                                        |
| ------- | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| 1.0     | `docker_mt`          | hosts with Docker or Podman and at least 4 usable CPUs; 4 to 8 threads                                                                         | `--cpus` from the thread count, 12 GiB, no swap (`ocr/boinc/surya/job-cpu.toml`) |
| 1.1     | `docker_nvidia_cuda` | hosts with Docker or Podman whose client reports an NVIDIA GPU with CUDA 12.0 or later, compute capability 5.0 or later and 3,000 MB of memory | `--gpus all`, 2 CPUs, 8 GiB, no swap (`job-gpu.toml`)                            |

Both classes need client 8.2.13 or later and are in `contrib-compute/boinc/plan_class_spec.xml`. The scheduler sends a host the version it expects to be fastest, so a host with a qualifying GPU gets the GPU version.

### How the GPU reaches the container

The stock `docker_wrapper` adds `--gpus all` to `docker create` (or `podman create`) when the version's `job.toml` sets `use_gpu = true` (`samples/docker_wrapper/docker_wrapper.cpp` at the pinned BOINC commit). The scheduler only checks the GPU the client detected; on Linux it does not check that the container runtime can pass it through. (On Windows it does, from the GPU list the client reports for each WSL distribution, since BOINC's commit 1635c05 of June 2026.) So the host needs, beyond the NVIDIA driver:

- **Podman** (including rootless): a CDI specification for the GPUs, made with the NVIDIA Container Toolkit (`nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`). Podman turns `--gpus all` into the CDI device `nvidia.com/gpu=all`. With SELinux enforcing, the container may also need `container_use_devices` (`setsebool -P container_use_devices on`).
- **Docker**: the NVIDIA Container Toolkit installed and configured as a Docker runtime (`nvidia-ctk runtime configure --runtime=docker`).

Without these, the container either fails to start (the wrapper's stderr, which BOINC returns with the result, holds the runtime's error) or starts without a GPU. In the second case the unit returns `no_gpu` for each mode, quickly, with the diagnostics.

The wrapper passes every GPU, not only the one BOINC assigned (`gpu_device_num`). The worker runs llama-server on the assigned one (`CUDA_VISIBLE_DEVICES`, taking BOINC's number as nvidia-smi's index, which may differ on a host with mixed GPUs) and measures that GPU's memory; the diagnostics record which. llama.cpp's CUDA build needs a driver that supports CUDA 12.

## Diagnostics

`usnm-boinc work` always writes all five outputs, so a host that can't run Surya still returns why, as a valid result rather than a lost one. Before reading, it records in `diag.txt` (at most 64 KiB) and `result.json`:

- from BOINC's `init_data.xml`, only these fields: client version, app version, plan class, GPU type and device number, CPU count and memory bound (the file also holds the account key and host name, which are never read out);
- the container runtime and whether it is rootless (Podman's `/run/.containerenv`; Docker's `/.dockerenv`), the cgroup limits the container got, and its memory;
- the NVIDIA device nodes, the CUDA and NVIDIA variables, the driver version and whether `libcuda` is there;
- `nvidia-smi`'s GPU name, driver, memory, compute capability and full output, or its error;
- Python's probe: torch's version and CUDA check, surya-ocr's version and its inference settings.

Each mode then runs in its own process group with its deadline. The worker samples the group's memory and CPU time every 0.5 s and, on a GPU, the GPU's memory every 2 s. A mode in which Surya reports a block it could not read counts as `failed`, whatever text the rest gave. It records the outcome (`ok`, `timeout`, `failed`, `no_gpu`), exit code or signal, time, CPU seconds, peak memory, peak GPU memory above what was in use before, and the device llama-server actually used: `cuda` when its log says it offloaded layers to the GPU, else `cpu`. `diag.txt` also gets llama-server's device lines and last lines, and the end of Surya's stderr.

A unit that can't run at all (an input whose checksum doesn't match, an engine that differs from the job's) returns `unit_error` with the reason and no runs.

## Redundancy and validation

`create_work` makes each unit with `min_quorum` 2 and `target_nresults` 2, and at most 4 results. `script_validator` runs:

- `usnm_surya_check` (`usnm-boinc surya check`) on each result: shape, sizes, UTF-8 and checksums. It also copies the result to `<project>/surya/seen/<result>/`, so every host's timings and diagnostics are kept, not only the canonical result's.
- `usnm_surya_compare` (`usnm-boinc surya compare`) on each pair: the same job and engine; in each mode both ran, both read the page or neither did; and where both read it, a word-bag Dice (the measure in [boinc-assessment.md](boinc-assessment.md)) of at least the threshold. A mode only one of them ran is not compared, and two results with no mode in common don't agree; two results that both could not run the unit (`unit_error`) do. Every pair is appended to `<project>/surya/pairs.jsonl` with its Dice per mode and both hosts' devices.

Vision-language output can differ between GPUs, and between CPU and GPU, so the threshold is measured, not assumed. Until the probe has measured it, `compare` accepts any Dice of 0.5 or more (`PROBE_MIN_DICE`), which only rejects unrelated text, and the logged pairs give the distribution to set it from.

`rsc_fpops_est` is 1e14 (round 1's median 18,753 CPU-seconds per tiled page at a few GFLOPS per core) and `rsc_memory_bound` 8 GiB. Round 1 peaked at 11.8 GB with 8 threads on a whole node, but BOINC compares the bound with the share of memory the volunteer allows, so 12 GiB would keep most 16 GB GPU hosts out. Both get revised once the probe has measured peaks and times.

## Running a campaign

The server must have the `usnm_surya` app (`register-apps`), the two plan classes and daemons (server and bridge images built from a commit that includes `usnm_surya`, `scripts/build-boinc-images.sh` then `scripts/provision-boinc.sh`), and both app versions:

```sh
gh workflow run boinc-surya.yml --ref main -f correlation="surya-$(date -u +%Y%m%dT%H%M%S)"
# From the run's summary: the image, its digest, SURYA_ENGINE_LLAMA and SURYA_ENGINE_WEIGHTS.
scripts/boinc-app-release.sh --env prod --app usnm_surya --version 1.0 --plan-class docker_mt \
  --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:... --signing-host HOST
scripts/boinc-app-release.sh --env prod --app usnm_surya --version 1.1 --plan-class docker_nvidia_cuda \
  --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:... --signing-host HOST
```

Then, under `<share>/surya/<campaign>/`:

```sh
# Fetches each page's JPEG from LoC: two requests per page, one at a time, at least 5 s apart; a
# 403, 429 or 5xx writes HOLD and stops (as for the assessment).
scripts/start-boinc-bridge.sh --env prod --step surya-stage --var SURYA_CAMPAIGN=probe1 \
  --var SURYA_PAGES=serbian:srp:sn85055101_1917-04-03_ed-1_seq-3,... \
  --var SURYA_ENGINE_IMAGE=sha256:... --var SURYA_ENGINE_LLAMA=sha256:... --var SURYA_ENGINE_WEIGHTS=sha256:... \
  --var SURYA_MAX=8 --wait-mins 15
scripts/start-boinc-bridge.sh --env prod --step surya-submit --var SURYA_CAMPAIGN=probe1 --var SURYA_MAX=8 --wait-mins 15
# As results come in:
scripts/start-boinc-bridge.sh --env prod --step surya-summary --var SURYA_CAMPAIGN=probe1 --wait-mins 15
```

`surya-summary` prints a table of every kept result (page, client, plan class, runtime, GPU, and per mode the outcome, device, time, memory) and every compared pair, and writes it to `<campaign>/summary.md`. A unit left at `submitting` is resolved as for the assessment.

To check a host or the image without LoC or BOINC, make a unit from any image and run it:

```sh
docker run --rm -v "$PWD/u:/work" IMAGE usnm-boinc surya local-unit --image /work/in.jpg --dir /work --modes whole
docker run --rm --network=none --read-only --tmpfs=/tmp:rw,size=1g --gpus all -v "$PWD/u:/work" IMAGE
```

## Not verified

- That rootless Podman with a CDI specification gives the container the GPU through the stock wrapper (the probe's question).
- That llama.cpp's CUDA build falls back to its CPU backends on a host without a GPU outside the CI runner.
- Peak memory and time on volunteers' hosts, and so the plan classes' limits and the workunit bounds.
