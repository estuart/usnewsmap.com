# Contribute compute to usnewsmap OCR (BOINC)

Volunteers run OCR on historical newspaper pages from Chronicling America (Library of Congress) on their own Linux machines, through a stock BOINC client. Each task reads one page image and returns its text. That text goes into the site's search index after the project's own checks. Two smaller apps measure the OCR engine itself.

Status: a pilot for trusted friends. The project server is designed but not deployed, and no app version is published yet, so the attach step fails until it opens. Every step below that has not been tried on a real host is marked UNVERIFIED. Operator docs: [boinc.md](boinc.md).

## 1. Check the prerequisites

You need all of these ([boinc.md](boinc.md#boundaries)):

- Linux on x86-64. No other platform has an app version.
- A BOINC client, version 9.0 or later.
- Docker, which the BOINC client's user is allowed to run. Every task runs in a Docker container started by the stock BOINC Docker wrapper. Hosts that can't run Docker (including restricted or unprivileged containers) can't take part.

Check them:

```sh
uname -m                 # expect: x86_64
boinc --version          # expect: 9.0 or later
docker version           # expect: a Client and a Server section, no permission error
```

Install the client from your distribution or from https://boinc.berkeley.edu/download_all.php if `boinc --version` is missing or older than 9.0. Then check that the BOINC service user can run a container. The user is `boinc` on Debian and Ubuntu packages; other distributions may differ (UNVERIFIED):

```sh
sudo -u boinc docker run --rm hello-world   # expect: "Hello from Docker!"
```

If that fails with a permission error, add the user to the `docker` group and restart the client (UNVERIFIED on distributions other than Debian and Ubuntu): `sudo usermod --append --groups docker boinc && sudo systemctl restart boinc-client`.

## 2. Attach to the project

The master URL is `https://boinc.usnewsmap.com/`. It is fixed for the life of the project.

Check that the server is up:

```sh
curl --silent --show-error --fail --output /dev/null --write-out '%{http_code}\n' https://boinc.usnewsmap.com/
# expect: 200. Anything else: the project isn't open yet; stop here.
```

Create an account (the client prints the account key), or get the key of an existing account:

```sh
boinccmd --create_account https://boinc.usnewsmap.com/ YOUR_EMAIL YOUR_PASSWORD YOUR_NAME
boinccmd --lookup_account https://boinc.usnewsmap.com/ YOUR_EMAIL YOUR_PASSWORD
```

The server may ask for an invitation code at sign-up ([design](../contrib-compute/docs/design.md)); UNVERIFIED whether it does. Then attach with the key:

```sh
boinccmd --project_attach https://boinc.usnewsmap.com/ ACCOUNT_KEY
boinccmd --get_project_status   # expect: an entry whose master URL is https://boinc.usnewsmap.com/
```

Set idle-only processing and conservative limits in your BOINC preferences, as the pilot asks.

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

| Step                                                        | Who connects                   | To where                              |
| ----------------------------------------------------------- | ------------------------------ | ------------------------------------- |
| scheduler requests, downloads of inputs, uploads of results | the BOINC client               | `https://boinc.usnewsmap.com/`        |
| worker image                                                | Docker, started by the wrapper | the registry named in the app version |
| gold originals                                              | the BOINC client               | figshare (BLN600) and tile.loc.gov    |
| the task itself                                             | nobody                         | no network inside the container       |

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
- **Tasks fail at once.** Check the event log for a Docker error. Run the `hello-world` check in section 1 as the BOINC user.
- **Tasks fail after a while.** A page may need more than 1536 MiB, or the engine may have crashed. Either is a failed task, which the project sees ([boinc.md](boinc.md#allocation-and-recovery)). If every task fails, detach and report it.
- **Gold downloads fail.** The client fetches from figshare and tile.loc.gov; make sure your network allows both. UNVERIFIED: that the client follows figshare's redirect.

## Detach

```sh
boinccmd --project https://boinc.usnewsmap.com/ detach
```

The client stops its tasks for this project and deletes the project's files, sticky files included. To delete your account too, use the project website.
