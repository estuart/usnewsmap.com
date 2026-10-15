# Contribute compute to usnewsmap OCR (BOINC)

Volunteers run OCR on historical newspaper pages from Chronicling America (Library of Congress) on their own Linux machines, through a stock BOINC client. Each task reads one page image and returns its text. That text goes into the site's search index after the project's own checks. Two smaller apps measure the OCR engine itself.

Status: a pilot for trusted friends. The project server is up and `usnm_ocr` and `usnm_gold` are published; work goes out only in small hand-picked cohorts. Every step below that has not been tried on a real host is marked UNVERIFIED. Operator docs: [boinc.md](boinc.md).

## 1. Set up and check the host

Use a dedicated BOINC service on Linux x86-64 with enough RAM for a 1536 MiB task (at least 4 GiB is recommended for the default profile). The rootless Podman profile below needs systemd, cgroup v2, Python 3.9 or later, Podman, and a stock BOINC client version 8.2.13 or later. The stock wrapper supports Podman directly; a Docker daemon and Docker-group membership are not required. BOINC still calls the scheduler plan class `docker` and logs `Docker: version ... (podman)`.

A container starting successfully does not establish that its resource limits or pause work. On a real RHEL host, Podman accepted CPU/memory flags without a usable cgroup, and SELinux blocked the wrapper's slot mount. The startup preflight below checks actual kernel limits and slot access and refuses to start BOINC on failure. See the [live measurement](notes/2026-10-14-boinc-volunteer-preflight.md).

### Install prerequisites

Get a recent client from the [BOINC project's packages](https://boinc.berkeley.edu/linux_install.php), then check `boinc --version`. Some distribution clients are older than the required 8.2.13. Upstream packages may put `boinc` and `boinccmd` in `/usr/local/bin`; use their full paths if needed.

On Debian/Ubuntu, install the runtime and user-session prerequisites:

```sh
sudo apt-get install podman uidmap dbus-user-session python3 ca-certificates
```

On RHEL, install the runtime and SELinux policy-management tools:

```sh
sudo dnf install podman shadow-utils python3 ca-certificates policycoreutils-python-utils
```

The official BOINC package must give its unprivileged service user a non-overlapping subordinate UID/GID allocation. The helper verifies an allocation exists; it does not invent ranges that could overlap another user. It also refuses a root BOINC service, cgroup v1, nonstandard/symlinked configuration files, or queued/running work. For an existing client, select **No new tasks** and finish/report its tasks before reconfiguration.

### Preview, install, then attach

From a current checkout of this repository, preview the setup and then install it:

```sh
python3 scripts/boinc/setup_volunteer.py

# After reviewing the preview:
sudo python3 scripts/boinc/setup_volunteer.py --apply
```

The helper detects the service's existing data directory and uses the published worker digest in this checkout's `contrib-compute/infra/boinc/env/prod.json` (`images.worker`). It does not move data. The default service is `boinc-client.service`; use `--service` if the installed package names it differently. `--data-dir` and `--image` can override the detected directory and released image, but the directory must still be the service's existing working directory and the image must use its full immutable digest.

The helper preserves account data and unrelated settings, backs up the changed preference/configuration XML, and sets a conservative pilot profile: one BOINC CPU, no GPUs, idle-only processing after five minutes, no battery use, 50% memory allowance, and no extra work buffer. Apps stay in memory while suspended so the wrapper can pause their container. CPU-load suspension is disabled because the client can count whole-machine load against its simulated single CPU; the idle policy and actual per-container CPU limit remain active.

Installation enables the BOINC user's systemd session at boot and configures Podman to use that session for cgroups. It makes the service's cgroups writable (`ProtectControlGroups=false`); the unprivileged UID still limits which cgroups it can modify. It allows read-only home-directory traversal while keeping the data directory writable and retaining the service's other restrictions. Podman's container monitors run with the container, and runtime files survive service restarts. Only the slots directory receives a persistent `container_file_t` label when SELinux is enabled; SELinux remains enabled and the account/config files outside slots retain their labels. The helper does not grant Docker access or change other users' Podman settings. If this dedicated account already belongs to `docker`, an administrator can remove that unused access.

The preflight runs in `ExecStartPre` as the service user, with the same service restrictions the client will have. It uses a disposable slot, the pinned worker image, and the task's runtime options to check:

- a mode-0600 input can be read through the same mount the wrapper uses, and a mode-0644 output is readable by the host;
- the **kernel** reports one CPU, 1536 MiB of memory, no extra swap, and 64 processes;
- the root filesystem is read-only and the container has no network routes;
- Podman actually enters paused and running states, and its probe can be removed.

It does not add `:z` or `:Z` to hide a broken slots label. The installer also rejects conflicting service overrides. Each restart repeats the checks; a failure prevents BOINC startup. The initial run may pull the worker image and the upstream hello-world image used by stock BOINC runtime detection. No account key is required by setup or preflight.

Inspect the result:

```sh
sudo journalctl -u boinc-client.service --no-pager -n 30
# expect: BOINC host preflight passed ...
```

This is host readiness, not server acceptance. After attaching, still verify one task's output upload, validation, and collection. A scheduler completion report or exit code 0 alone is insufficient.

The measured profile is RHEL 10.2 x86-64, SELinux enforcing, BOINC 8.2.13, and Podman 5.8.2. Debian/Ubuntu installation and other distributions remain UNVERIFIED until the same live checks are recorded. Windows/macOS support is separate. Docker remains an existing pilot option; this helper and its rootless guarantees apply to Podman only.

## 2. Attach to the project

The master URL is `https://boinc.usnewsmap.com/`. It is fixed for the life of the project.

Check that the server is up:

```sh
curl --silent --show-error --fail --output /dev/null --write-out '%{http_code}\n' https://boinc.usnewsmap.com/
# expect: 200. Anything else: the project isn't open yet; stop here.
```

Create an account (the client prints the account key), or get the key of an existing account. Account creation is open; the server asks for no invitation code (checked 2026-10-14):

```sh
boinccmd --create_account https://boinc.usnewsmap.com/ YOUR_EMAIL YOUR_PASSWORD YOUR_NAME
boinccmd --lookup_account https://boinc.usnewsmap.com/ YOUR_EMAIL YOUR_PASSWORD
```

BOINC Manager can add the project using its URL and your account login. For a command-line client, attach with the key:

```sh
boinccmd --project_attach https://boinc.usnewsmap.com/ ACCOUNT_KEY
boinccmd --get_project_status   # expect: an entry whose master URL is https://boinc.usnewsmap.com/
```

`boinccmd --get_messages 0` then shows `Master file download succeeded` and `Scheduler request completed`. The client also logs `Docker: version ... (podman)` at start-up for this profile; if that line is missing, the scheduler sends this host no work.

Set idle-only processing and conservative limits in your BOINC preferences, as the pilot asks. On a headless host or a container, the client may log `Suspending computation - CPU is busy` because it sees the whole machine's load; set `<suspend_cpu_usage>0</suspend_cpu_usage>` in `/var/lib/boinc/global_prefs_override.xml` and run `boinccmd --read_global_prefs_override`.

## 3. What you will run

Work goes out in explicit cohorts of 1 to 100 pages that the project selects by hand ([boinc.md](boinc.md#boundaries)), so it's normal to see no tasks for a while. All three apps use the same worker image, which the client pulls from a registry the project names in the app version.

| App           | What a task does                                                                                                                           | Downloads                                                             | Returns                                                                                                         |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `usnm_ocr`    | OCR of one newspaper page with Tesseract 5.5.0 and the project's model lock                                                                | the page image and a job file, from the project server                | the page's text (`text.txt`), its layout as ALTO XML (`layout.xml`) and `result.json` (checksums, engine, time) |
| `usnm_assess` | reads one page twice, as JP2 and as JPEG, to compare the two formats ([boinc-assessment.md](boinc-assessment.md)); two hosts get each page | the JP2, the JPEG (OCR units) and a job file, from the project server | both texts and `result.json` (outcome, exit code, time, memory per format)                                      |
| `usnm_gold`   | scores your engine on public hand-transcribed text ([gold check](boinc.md#gold-check))                                                     | see below                                                             | `gold-result.json`: counts only                                                                                 |

The OCR text from `usnm_ocr` is what the project uses. It is published as ALTO and Parquet and goes through the same import and release as the project's own OCR nodes.

**The gold check, `usnm_gold`.** Bundle version 1 ([ocr-gold-set.md](ocr-gold-set.md#volunteer-bundle)):

- `bundle.tar`, 9.6 MB, from the project server: crops and transcriptions the licences allow us to share, with their licence files.
- The BLN600 zip, 68 MB, from figshare, and 18 page images from tile.loc.gov. Your BOINC client fetches these from the publishers, not from us, and checks each one's size and md5.

The input template marks all of these `<sticky/>` (written by `usnm-boinc gold submit`), so your client downloads each once per host and reuses it for later gold tasks (UNVERIFIED on a live server, #469). By taking gold tasks you use these datasets under their licences: BLN600 under CC BY-NC-ND 4.0; Overproof transcriptions under CC BY-NC-SA 4.0, with Library of Congress page images; NewsEye under CC BY 4.0; NZZ under the licence in `licenses/nzz/` in the bundle. Each source's licence and attribution files are in `licenses/<source>/` in the bundle. The NC licences allow non-commercial use only. Nothing derived from BLN600 is shared: your host cuts the crops itself, and nothing it cuts leaves the machine.

`gold-result.json` holds, per crop: reference characters, character edits, reference words, word edits, the model profile and the time; then a summary per source, the crops it skipped and why, the engine version, the bundle version and manifest checksum, the job id and your BOINC host id. It holds no page text and no transcription. Counts are enough to compare engines across hosts, and the licences (BLN600's No-Derivatives terms in particular) rule out sending anything else. Crops in a language the worker image has no model for are listed as skipped.

**How gold results will be used.** The OCR app's validator accepts any well-formed result. It doesn't compare text between hosts yet ([boinc.md](boinc.md#connect-a-stock-boinc-project)). A gold result shows how a host's engine does on known text, and the plan is to use it to decide whether to trust that host's OCR. That is a tracked follow-up and is not built yet.

## 4. Resources, network and privacy

Per task, from `ocr/boinc/job.toml`: one CPU, 1536 MiB of memory with no extra swap, at most 64 processes, a read-only root filesystem, a 512 MiB `/tmp`, all capabilities dropped except `DAC_OVERRIDE`, and `--network=none`. The container sees only the task's slot directory.

Network, step by step:

| Step                                                        | Who connects                               | To where                              |
| ----------------------------------------------------------- | ------------------------------------------ | ------------------------------------- |
| scheduler requests, downloads of inputs, uploads of results | the BOINC client                           | `https://boinc.usnewsmap.com/`        |
| worker image                                                | Podman (or Docker), started by the wrapper | the registry named in the app version |
| gold originals                                              | the BOINC client                           | figshare (BLN600) and tile.loc.gov    |
| the task itself                                             | nobody                                     | no network inside the container       |

What leaves your machine: the result files in the table in section 3, and what any stock BOINC client sends its project (host description, account details). The project stores your email address, name, IP address and account key. Privacy is on by default, and you can delete your account ([design](../contrib-compute/docs/design.md), Privacy). Web server and scheduler logs keep IP addresses for 30 days.

Disk: the gold check keeps about 78 MB of sticky files plus the 18 page images for as long as you stay attached, and each running gold task copies them into its slot.

## 5. Check that it works

```sh
boinccmd --get_tasks              # tasks with project URL https://boinc.usnewsmap.com/ and their state
boinccmd --get_messages 0 | grep --ignore-case usnewsmap   # the event log for this project
```

Success: a task moves from downloading to running to uploaded, and the event log shows no error for it. The project's website lists your host and tasks under your account (stock BOINC pages).

## 6. Troubleshooting

- **No tasks.** Work goes out in small cohorts; there may be none queued. Run `boinccmd --project https://boinc.usnewsmap.com/ update` and check the event log.
- **Client will not start.** Read `journalctl -u boinc-client.service`. A slot/SELinux error needs the slots-only label; missing limits need a working user systemd session, cgroup v2 and writable delegated cgroups. Re-run the setup preview and fix its diagnostic. Do not disable the preflight to accept work.
- **Tasks fail at once.** Check the event log for a runtime or slot error. A hello-world check alone is insufficient; the startup preflight exercises the real task limits and mount.
- **Upload fails.** An upload error is not a completed acceptance test even if the scheduler receives a completion report. Preserve a local result when diagnosing, stop requesting new tasks, and report the upload-server message to the project operator.
- **Tasks fail after a while.** A page may need more than 1536 MiB, or the engine may have crashed. Either is a failed task, which the project sees ([boinc.md](boinc.md#allocation-and-recovery)). If every task fails, detach and report it.
- **Gold downloads fail.** The client fetches from figshare and tile.loc.gov; make sure your network allows both. UNVERIFIED: that the client follows figshare's redirect.

## Detach

```sh
boinccmd --project https://boinc.usnewsmap.com/ detach
```

The client stops its tasks for this project and deletes the project's files, sticky files included. To delete your account too, use the project website.

## Remove the managed profile

First select **No new tasks** and let BOINC finish and report all queued and running work before stopping the service. Stopping BOINC with unfinished work can leave task containers paused in the profile's managed Podman storage; removing the drop-in also removes `CONTAINERS_STORAGE_CONF` and `XDG_RUNTIME_DIR`, so restarting cannot find those containers and may recreate tasks while leaving the originals stranded. Once all work is reported, stop BOINC, remove `/etc/systemd/system/boinc-client.service.d/zz-usnm-volunteer.conf`, and restore the XML state recorded on first installation in `/etc/usnm-boinc-volunteer/boinc-client.service/backups/` (substitute the service name if different): copy back each backed-up XML file, or if `<file>.absent` is present because the file did not exist before installation, delete that XML file (or remove only this profile's settings if you added others afterward). Run `systemctl daemon-reload` before starting the client. Account data and container images are retained. Only disable the user's lingering session or remove its slots-label rule when that user/slots tree no longer needs them. Do not remove the preflight while continuing to rely on this profile's resource/isolation guarantees.
