//! OCR engines. Tesseract to start; the engine survey (#296) may add others
//! behind the same trait.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use usnm_core::ocr_engine::{Digest, Fields, OcrEngineTag, TagError};

/// What an engine read on a page.
#[derive(Debug)]
pub struct EngineOutput {
    /// Plain text, as the engine wrote it (not normalized).
    pub text: String,
    /// ALTO XML.
    pub alto: Vec<u8>,
    /// The `ocr_engine` tag (13 §13.8.1): engine, version, models, image
    /// and route, e.g. `tesseract/5.5.0 profile=frak2021 image=sha256:…
    /// models=sha256:… route=minimal@r1 contract=0`.
    pub engine: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// No model for the language on this node: retrying here won't help.
    #[error("no model for language `{0}` on this node")]
    Unsupported(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

#[async_trait]
pub trait Engine: Send + Sync + std::fmt::Debug {
    /// `ocr_source` for this engine's rows, e.g. `usnm-tesseract`.
    fn source(&self) -> &str;
    /// Whether this node can read pages in `language` (ISO 639-3). The
    /// worker asks before it fetches a scan, so a page with no model here
    /// costs no image request (12 Step 7). The engine contract (ADR-0016,
    /// #307) answers it from routing.
    fn can_read(&self, language: &str) -> bool;
    /// Read `image` (a file) for a page in `language` (ISO 639-3), using
    /// `work` for scratch files.
    async fn ocr(
        &self,
        image: &Path,
        language: &str,
        work: &Path,
    ) -> Result<EngineOutput, EngineError>;
}

/// The route in every `ocr_engine` tag the worker writes (13 §13.8.1):
/// [`candidates`], version 1. TODO(#371): `engines.toml` and `routes.toml`
/// replace this hard-coded route, with their gate on which engines and
/// models a production route may name.
pub const ROUTE: &str = "minimal";
pub const ROUTE_VERSION: u32 = 1;
/// The engine contract's version in the tag: 0, the worker runs Tesseract
/// directly (ADR-0016's contract isn't built).
pub const CONTRACT: u32 = 0;

/// The Tesseract image's `models.lock`, built into the worker so the tag's
/// `models` digest comes from the same file the image checked its models
/// against.
pub const MODELS_LOCK: &str = include_str!("../../../ocr/engines/tesseract/models.lock");

/// Tesseract models to try for a language, best first. A `+` joins models
/// read together; a candidate is used only when all its models are installed.
/// German: UB Mannheim's `frak2021` (CC0, the survey's best on German, #370),
/// else `deu_latf` (Fraktur, tessdata 4.1+) with `deu` for pages that mix
/// in roman type, else `deu`. Other languages: their tessdata_best model.
/// A language with no model here isn't read (the worker leaves the page
/// for a node that has one).
pub fn candidates(language: &str) -> Vec<String> {
    let list: &[&str] = match language {
        "deu" => &["frak2021", "deu_latf+deu", "deu"],
        // Serbian papers print Cyrillic and Latin.
        "srp" => &["srp+srp_latn", "srp"],
        "hrv" | "bos" => &["hrv", "srp_latn"],
        "nob" | "nno" | "nor" => &["nor"],
        "zho" => &["chi_tra", "chi_sim"],
        "yid" => &["yid", "heb"],
        "ell" | "grc" => &["ell", "grc"],
        other => return vec![other.to_owned()],
    };
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// The first candidate whose models are all in `installed`.
pub fn route(language: &str, installed: &BTreeSet<String>) -> Option<String> {
    candidates(language)
        .into_iter()
        .find(|c| c.split('+').all(|m| installed.contains(m)))
}

/// Where Tesseract runs: a binary on the host, or a pinned container image
/// run with podman (ADR-0015: EL9 has no Tesseract 5, so the node's comes as
/// an image). The engine contract (ADR-0016, #307) replaces this with a
/// long-lived instance per engine; until then each page is one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    Host(PathBuf),
    Podman {
        /// The podman binary.
        podman: PathBuf,
        /// The engine runtime's Podman API socket, or none for podman run
        /// by the worker's own user (a developer machine).
        remote: Option<Remote>,
        /// `sha256:<id>` or `<name>@sha256:<digest>`: never a mutable tag.
        image: String,
    },
}

/// The engine runtime of ADR-0016 (13 §13.3): podman runs as the engine
/// account, `usnmeng`, behind a Podman API socket that the worker reaches
/// with `podman --remote`, so the worker itself never needs user
/// namespaces and its unit can set `NoNewPrivileges=yes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// `unix:///run/usnm-ocr/podman.sock`.
    pub url: String,
    /// The size-capped tmpfs that pages' scratch directories go in
    /// (`/run/usnm-ocr/scratch`). The worker's own `/tmp` is private to its
    /// unit, so the engine account's containers couldn't mount it.
    pub scratch: PathBuf,
    pub limits: Limits,
}

/// Limits on each engine container (13 §13.6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `--memory` and `--memory-swap`: no swap.
    pub memory_mib: u32,
    pub cpus: u32,
    pub pids: u32,
}

impl Default for Limits {
    /// Tesseract's starting entry in 13 §13.6.2.
    fn default() -> Self {
        Self {
            memory_mib: 1536,
            cpus: 1,
            pids: 64,
        }
    }
}

/// The user engine images run as (`USER 10001` in their Containerfiles).
/// Under the engine account's rootless podman it maps to one of that
/// account's subordinate ids, never to the account itself.
pub const ENGINE_USER: u32 = 10001;

/// The label on every container the worker starts, so stale ones can be
/// found.
const CONTAINER_LABEL: &str = "usnm.engine=tesseract";

/// What the worker learns from the engine runtime at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sandbox {
    podman: PathBuf,
    url: String,
    image: String,
    scratch: PathBuf,
    info: RuntimeInfo,
}

/// What `podman info` says about the engine runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeInfo {
    /// The host uid that [`ENGINE_USER`] maps to.
    engine_uid: u32,
    /// The host uid the runtime itself runs as (container uid 0: the
    /// engine account), which needs to reach a page's directory to mount
    /// it; none for rootful podman.
    runtime_uid: Option<u32>,
    selinux: bool,
}

/// From `podman info --format json`: the host uids [`ENGINE_USER`] and
/// container root map to, and whether SELinux is on. Refuses a runtime that can't apply the
/// memory, CPU and pid limits (cgroup controllers not delegated to the
/// engine account), so limits never silently don't apply.
fn parse_info(json: &str) -> anyhow::Result<RuntimeInfo> {
    let v: serde_json::Value = serde_json::from_str(json).context("podman info isn't JSON")?;
    let host = &v["host"];
    let controllers: Vec<&str> = host["cgroupControllers"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = ["memory", "pids", "cpu"]
        .into_iter()
        .filter(|c| !controllers.contains(c))
        .collect();
    if !missing.is_empty() {
        bail!(
            "the engine runtime's cgroup has no {} controller (it has {controllers:?}); \
             run scripts/ocr-node/node-setup.sh to delegate them",
            missing.join(", ")
        );
    }
    let maps = host["idMappings"]["uidmap"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let field = |m: &serde_json::Value, k: &str| m[k].as_u64().unwrap_or(0);
    let host_of = |user: u64| {
        maps.iter()
            .find(|m| {
                let c = field(m, "container_id");
                c <= user && user < c + field(m, "size")
            })
            .map(|m| field(m, "host_id") + (user - field(m, "container_id")))
            .and_then(|u| u32::try_from(u).ok())
    };
    let (engine_uid, runtime_uid) = if maps.is_empty() {
        // Rootful podman: no mapping.
        (ENGINE_USER, None)
    } else {
        let engine = host_of(u64::from(ENGINE_USER)).with_context(|| {
            format!("the engine account's subordinate ids don't cover uid {ENGINE_USER}")
        })?;
        (engine, host_of(0))
    };
    let selinux = host["security"]["selinuxEnabled"]
        .as_bool()
        .unwrap_or(false);
    Ok(RuntimeInfo {
        engine_uid,
        runtime_uid,
        selinux,
    })
}

/// Scratch slots, and with them MCS category pairs: slot n is
/// `slot-<n>` in the scratch directory, at level `s0:c<2n>,c<2n+1>`.
const SLOTS: u32 = 512;

/// A page's slot directory, removed with everything in it when dropped.
#[derive(Debug)]
struct PageDir {
    path: PathBuf,
    slot: u32,
}

impl Drop for PageDir {
    /// What [`Sandbox::release`] didn't remove (a page cut short).
    fn drop(&mut self) {
        if !self.path.exists() {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!(dir = %self.path.display(), error = %e, "couldn't remove a page's scratch directory");
        }
    }
}

/// The MCS level of slot `slot` (13 §13.3).
fn slot_level(slot: u32) -> String {
    format!("s0:c{},c{}", 2 * slot, 2 * slot + 1)
}

impl PageDir {
    fn level(&self) -> String {
        slot_level(self.slot)
    }
}

/// Claims a free slot in `scratch`, mode 0700. The claim is the directory
/// itself (`mkdir` fails if it exists), so every process on the node that
/// shares the scratch, a worker and an install's `check-engine` alike,
/// gets a different slot, and so a different MCS pair, for each page in
/// flight.
fn claim_slot(scratch: &Path) -> anyhow::Result<PageDir> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let start = NEXT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(std::process::id());
    for i in 0..SLOTS {
        let slot = start.wrapping_add(i) % SLOTS;
        let path = scratch.join(format!("slot-{slot}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(PageDir { path, slot }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!(
        "all {SLOTS} scratch slots in {} are in use",
        scratch.display()
    )
}

/// Removes slots older than `age`: left by a worker that was killed. No
/// page runs that long. Returns the ones it couldn't remove, with their
/// slot numbers.
fn remove_stale_slots(scratch: &Path, age: Duration) -> Vec<(PathBuf, u32)> {
    let mut left = Vec::new();
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return left;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("slot-") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d > age);
        if old && std::fs::remove_dir_all(e.path()).is_err() {
            if let Some(slot) = name
                .to_string_lossy()
                .strip_prefix("slot-")
                .and_then(|n| n.parse().ok())
            {
                left.push((e.path(), slot));
            }
        }
    }
    left
}

/// The owner and group of `path`.
fn owner(path: &Path) -> anyhow::Result<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok((meta.uid(), meta.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        bail!("the engine runtime needs Unix")
    }
}

/// Runs a short helper (`setfacl`, `chcon`) and fails with its stderr.
async fn checked(cmd: &mut tokio::process::Command) -> anyhow::Result<()> {
    let out = cmd
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {:?}", cmd.as_std().get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} exited with {}: {}",
            cmd.as_std().get_program(),
            out.status,
            tail(&String::from_utf8_lossy(&out.stderr), 400)
        );
    }
    Ok(())
}

impl Sandbox {
    async fn detect(podman: &Path, remote: &Remote, image: &str) -> anyhow::Result<Self> {
        let meta = std::fs::metadata(&remote.scratch)
            .with_context(|| format!("scratch directory {}", remote.scratch.display()))?;
        if !meta.is_dir() {
            bail!("scratch {} isn't a directory", remote.scratch.display());
        }
        let out = podman_command(podman, Some(&remote.url))
            .args(["info", "--format", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("running podman info")?;
        if !out.status.success() {
            bail!(
                "podman info over {} exited with {}: {}",
                remote.url,
                out.status,
                tail(&String::from_utf8_lossy(&out.stderr), 400)
            );
        }
        let info = parse_info(&String::from_utf8_lossy(&out.stdout))?;
        // Containers left by a worker that was killed: the ones that have
        // exited. A running one may belong to another worker process (an
        // install's check-engine), so it's left to finish.
        let pruned = podman_command(podman, Some(&remote.url))
            .args(["container", "prune", "--force", "--filter"])
            .arg(format!("label={CONTAINER_LABEL}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if !matches!(pruned, Ok(s) if s.success()) {
            tracing::warn!("couldn't remove stale engine containers");
        }
        let sandbox = Self {
            podman: podman.to_owned(),
            url: remote.url.clone(),
            image: image.to_owned(),
            scratch: remote.scratch.clone(),
            info,
        };
        for (path, slot) in remove_stale_slots(&remote.scratch, Duration::from_secs(6 * 3600)) {
            sandbox.purge(&path, slot).await;
        }
        Ok(sandbox)
    }

    /// A page's scratch directory, which the engine container can use:
    /// a slot owned by the worker with mode 0700, with ACL entries for the
    /// container user (and defaults, so the files in it are shared both
    /// ways) and search-only for the engine account, whose podman mounts
    /// it, and, under SELinux, labelled `container_file_t` at the slot's
    /// level. The container runs at that level, so no volume is mounted
    /// with `:Z`, which the engine account couldn't apply to a directory it
    /// doesn't own (13 §13.3).
    /// Removes a page's slot. The container user owns what it made there
    /// and can make it unreadable to the worker (a directory with mode
    /// 000), so when the worker can't remove it, a container as that user
    /// at the slot's level empties it first.
    async fn release(&self, dir: PageDir) {
        if std::fs::remove_dir_all(&dir.path).is_ok() {
            return;
        }
        self.purge(&dir.path, dir.slot).await;
    }

    async fn purge(&self, path: &Path, slot: u32) {
        let mut cmd = podman_command(&self.podman, Some(&self.url));
        cmd.args([
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
        ])
        .args(["--cap-drop=all", "--security-opt=no-new-privileges"])
        .args(["--label", CONTAINER_LABEL, "--log-driver=none"])
        .args(["--user", &format!("{ENGINE_USER}:{ENGINE_USER}")])
        .args([
            "--memory",
            "128m",
            "--memory-swap",
            "128m",
            "--pids-limit",
            "16",
        ]);
        if self.info.selinux {
            cmd.args([
                "--security-opt",
                &format!("label=level:{}", slot_level(slot)),
            ]);
        }
        cmd.args(["--volume", &format!("{}:/work:rw", path.display())])
            .args(["--entrypoint", "sh", &self.image, "-c"])
            .arg("chmod -R u+rwX /work; find /work -mindepth 1 -delete")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let ran = tokio::time::timeout(Duration::from_secs(120), cmd.status()).await;
        if let Err(e) = std::fs::remove_dir_all(path) {
            tracing::warn!(dir = %path.display(), error = %e, ran = ?ran.ok(), "couldn't empty a page's scratch slot");
        }
    }

    async fn page_dir(&self) -> anyhow::Result<(PageDir, Option<String>)> {
        let dir = claim_slot(&self.scratch)?;
        let (me, _) = owner(&dir.path)?;
        let e = self.info.engine_uid;
        let mut acl = format!("u:{e}:rwx,d:u:{e}:rwx,d:u:{me}:rwx");
        if let Some(r) = self.info.runtime_uid {
            acl.push_str(&format!(",u:{r}:x"));
        }
        checked(
            tokio::process::Command::new("setfacl")
                .args(["-m", &acl])
                .arg(&dir.path),
        )
        .await?;
        let level = if self.info.selinux {
            let level = dir.level();
            checked(
                tokio::process::Command::new("chcon")
                    .args(["-t", "container_file_t", "-l", &level])
                    .arg(&dir.path),
            )
            .await?;
            Some(level)
        } else {
            None
        };
        Ok((dir, level))
    }
}

/// A podman command, through the Podman API socket at `url` when given.
/// For the socket, the client gets a cleared environment with only what it
/// needs, so the worker's settings never reach it (13 §13.6.1).
fn podman_command(podman: &Path, url: Option<&str>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(podman);
    if let Some(url) = url {
        cmd.env_clear();
        for k in [
            "PATH",
            "LANG",
            "HOME",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
            "CONTAINERS_CONF",
        ] {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        cmd.args(["--remote", "--url", url]);
    }
    cmd
}

/// A container image reference that names its content: an image id
/// (`sha256:` and 64 hex digits) or a name pinned by digest
/// (`<name>@sha256:<digest>`). Tags can move, so they are refused.
pub fn check_image_ref(image: &str) -> anyhow::Result<()> {
    let hex64 =
        |s: &str| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let ok = match image.split_once("@sha256:") {
        Some((name, digest)) => {
            hex64(digest)
                && !name.is_empty()
                && name.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'.' | b'/' | b'-' | b'_' | b':')
                })
        }
        None => image.strip_prefix("sha256:").is_some_and(hex64),
    };
    if !ok {
        bail!("Tesseract image `{image}` must be `sha256:<id>` or `<name>@sha256:<digest>`, not a tag");
    }
    Ok(())
}

/// Removes a page's container. Containers run without `--rm`, so the worker
/// can read `OOMKilled` after one exits ([`ContainerGuard::finish`]); if the
/// page is cancelled or times out first, dropping the guard removes the
/// container: killing the podman client leaves it running.
struct ContainerGuard {
    podman: PathBuf,
    url: Option<String>,
    name: String,
    armed: bool,
}

impl ContainerGuard {
    fn rm(&self) -> tokio::process::Command {
        let mut cmd = podman_command(&self.podman, self.url.as_deref());
        cmd.args(["rm", "--force", "--ignore", "--time", "0", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    /// After the container exits: whether the kernel killed it for memory,
    /// then remove it. A failed removal is left to the drop.
    async fn finish(&mut self) -> bool {
        let oom = podman_command(&self.podman, self.url.as_deref())
            .args([
                "container",
                "inspect",
                "--format",
                "{{.State.OOMKilled}}",
                &self.name,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true"
            });
        if matches!(self.rm().status().await, Ok(s) if s.success()) {
            self.armed = false;
        }
        oom
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut cmd = self.rm();
        let name = self.name.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                // `podman rm` can fail while the container is still
                // stopping; try a few times, and say so if it never goes.
                for attempt in 1..=3u32 {
                    match cmd.status().await {
                        Ok(s) if s.success() => return,
                        Ok(s) => tracing::warn!(container = %name, attempt, status = %s, "podman rm failed"),
                        Err(e) => tracing::warn!(container = %name, attempt, error = %e, "podman rm didn't run"),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(attempt))).await;
                }
                tracing::error!(container = %name, "couldn't remove a stopped page's container; remove it by hand");
            });
        }
    }
}

impl Runner {
    /// The command for `tesseract <args>`, with `work` (when given) shared
    /// as `/work`, and the paths in `args` already relative to it. `level`
    /// is the MCS level `work` is labelled with, under the engine runtime.
    /// The guard removes the container.
    fn command(
        &self,
        threads: u32,
        work: Option<&Path>,
        level: Option<&str>,
    ) -> (tokio::process::Command, Option<ContainerGuard>) {
        match self {
            Runner::Host(bin) => {
                let mut cmd = tokio::process::Command::new(bin);
                cmd.env("OMP_THREAD_LIMIT", threads.to_string());
                if let Some(w) = work {
                    cmd.current_dir(w);
                }
                (cmd, None)
            }
            Runner::Podman {
                podman,
                remote,
                image,
            } => {
                let name = format!("usnm-tesseract-{}", crate::queue::random_token());
                let url = remote.as_ref().map(|r| r.url.clone());
                let mut cmd = podman_command(podman, url.as_deref());
                // No network, a read-only root, no capabilities and no
                // privilege gain: the engine only reads the scan in /work
                // and writes its output there.
                cmd.args(["run", "--pull=never", "--network=none", "--read-only"])
                    .args(["--cap-drop=all", "--security-opt=no-new-privileges"])
                    .args(["--name", &name, "--label", CONTAINER_LABEL])
                    .args(["--env", &format!("OMP_THREAD_LIMIT={threads}")])
                    .args(["--entrypoint", "tesseract"]);
                match remote {
                    Some(r) => {
                        // 13 §13.6.1: the image's user, which maps to a
                        // subordinate id of the engine account; memory
                        // equal to memory plus swap, so no swap; CPU, pid,
                        // open-file and core limits; nothing from the host.
                        let mem = format!("{}m", r.limits.memory_mib);
                        cmd.args(["--http-proxy=false", "--no-hosts"])
                            .args(["--tmpfs", "/tmp:rw,nosuid,nodev,size=64m,mode=1777"])
                            .args(["--user", &format!("{ENGINE_USER}:{ENGINE_USER}")])
                            .args(["--memory", &mem, "--memory-swap", &mem])
                            .args(["--cpus", &r.limits.cpus.to_string()])
                            .args(["--pids-limit", &r.limits.pids.to_string()])
                            .args(["--ulimit", "nofile=1024:1024", "--ulimit", "core=0"])
                            .args(["--log-driver=none", "--env", "HOME=/tmp"]);
                        if let Some(level) = level {
                            cmd.args(["--security-opt", &format!("label=level:{level}")]);
                        }
                        if let Some(w) = work {
                            cmd.args(["--volume", &format!("{}:/work:rw", w.display())])
                                .args(["--workdir", "/work"]);
                        }
                    }
                    None => {
                        cmd.arg("--userns=keep-id");
                        if let Some(w) = work {
                            // :Z gives the page's private scratch directory
                            // the container's SELinux label. The engine runs
                            // as the directory's owner (the worker's user,
                            // mapped to itself by keep-id), not the image's
                            // user, so it can write its output there.
                            cmd.args(["--volume", &format!("{}:/work:Z", w.display())])
                                .args(["--workdir", "/work"]);
                            if let Ok((uid, gid)) = owner(w) {
                                cmd.args(["--user", &format!("{uid}:{gid}")]);
                            }
                        }
                    }
                }
                cmd.arg(image);
                let guard = ContainerGuard {
                    podman: podman.clone(),
                    url,
                    name,
                    armed: true,
                };
                (cmd, Some(guard))
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Runner::Host(bin) => bin.display().to_string(),
            Runner::Podman {
                image,
                remote: Some(r),
                ..
            } => format!("podman image {image} through {}", r.url),
            Runner::Podman { image, .. } => format!("podman image {image}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tesseract {
    runner: Runner,
    /// Under the engine runtime.
    sandbox: Option<Sandbox>,
    version: String,
    installed: BTreeSet<String>,
    psm: u8,
    dpi: u32,
    threads: u32,
    timeout: Duration,
}

/// `tesseract 5.5.1` → `5.5.1`, from `tesseract --version`'s first line
/// (stdout in 5.x, stderr in older releases).
fn parse_version(out: &str) -> Option<String> {
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("tesseract "))?;
    let v = line.trim().strip_prefix("tesseract ")?.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    (!v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')))
    .then(|| v.to_owned())
}

/// The model names in `tesseract --list-langs` output.
fn parse_langs(out: &str) -> BTreeSet<String> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of available languages"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' '))
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone)]
pub struct TesseractOptions {
    pub runner: Runner,
    /// Page segmentation mode (3: automatic, no orientation detection).
    pub psm: u8,
    /// Resolution to assume: IIIF JPEGs carry none, and Tesseract guesses 70.
    pub dpi: u32,
    /// OpenMP threads per page (`OMP_THREAD_LIMIT`): 1 when pages run in parallel.
    pub threads: u32,
    pub timeout: Duration,
}

impl Default for TesseractOptions {
    fn default() -> Self {
        Self {
            runner: Runner::Host("tesseract".into()),
            psm: 3,
            dpi: 300,
            threads: 1,
            timeout: Duration::from_secs(600),
        }
    }
}

async fn run(runner: &Runner, args: &[&str]) -> anyhow::Result<String> {
    let (mut cmd, mut guard) = runner.command(1, None, None);
    let out = cmd
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running tesseract ({})", runner.describe()))?;
    let oom = match guard.as_mut() {
        Some(g) => g.finish().await,
        None => false,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "tesseract {:?} ({}) exited with {}{}: {}",
            args,
            runner.describe(),
            out.status,
            if oom { ", killed for memory" } else { "" },
            tail(&err, 400)
        );
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// The last `n` characters of `s`.
fn tail(s: &str, n: usize) -> String {
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

impl Tesseract {
    /// Find Tesseract's version and installed models (once, at startup).
    pub async fn detect(opts: TesseractOptions) -> anyhow::Result<Self> {
        let mut sandbox = None;
        if let Runner::Podman {
            image,
            podman,
            remote,
        } = &opts.runner
        {
            check_image_ref(image)?;
            if let Some(r) = remote {
                sandbox = Some(Sandbox::detect(podman, r, image).await?);
            }
        }
        let version = parse_version(&run(&opts.runner, &["--version"]).await?)
            .context("can't read `tesseract --version`")?;
        let installed = parse_langs(&run(&opts.runner, &["--list-langs"]).await?);
        Ok(Self {
            runner: opts.runner,
            sandbox,
            version,
            installed,
            psm: opts.psm,
            dpi: opts.dpi,
            threads: opts.threads.max(1),
            timeout: opts.timeout,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn installed(&self) -> &BTreeSet<String> {
        &self.installed
    }

    pub fn runner(&self) -> &Runner {
        &self.runner
    }

    /// The `ocr_engine` value for a language, if it has a model here.
    pub fn engine_for(&self, language: &str) -> Option<String> {
        let profile = route(language, &self.installed)?;
        self.tag(&profile).ok().map(|t| t.to_string())
    }

    /// The `ocr_engine` tag for a read with `profile` (13 §13.8.1); an
    /// error when Tesseract's version or a model name doesn't fit the
    /// tag's character sets.
    pub fn tag(&self, profile: &str) -> Result<OcrEngineTag, TagError> {
        let (image, models) = match &self.runner {
            Runner::Podman { image, .. } => (
                image
                    .rsplit_once("sha256:")
                    .and_then(|(_, h)| Digest::short(h).ok())
                    .unwrap_or(Digest::Local),
                models_digest(MODELS_LOCK, profile).unwrap_or(Digest::Local),
            ),
            // A host binary reads whatever models the host has.
            Runner::Host(_) => (Digest::Local, Digest::Local),
        };
        OcrEngineTag::new(Fields {
            family: "tesseract",
            version: &self.version,
            profile,
            image,
            models,
            route: ROUTE,
            route_version: ROUTE_VERSION,
            contract: CONTRACT,
        })
    }
}

/// The sha256 of a profile's model files (`deu_latf+deu`), as sorted lines
/// `<name> <sha256>` from `models.lock` (13 §13.8.1); none when a model
/// isn't in the lock.
pub fn models_digest(lock: &str, profile: &str) -> Option<Digest> {
    let entries: std::collections::BTreeMap<&str, &str> = lock
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (name, _url, sum) = (f.next()?, f.next()?, f.next()?);
            Some((name, sum))
        })
        .collect();
    let mut lines = Vec::new();
    for m in profile.split('+') {
        let name = format!("tessdata/{m}.traineddata");
        let sum = entries.get(name.as_str())?;
        lines.push(format!("{name} {sum}\n"));
    }
    lines.sort();
    let hex = crate::prefetch::sha256(lines.concat().as_bytes());
    Digest::short(&hex).ok()
}

/// Size caps on an engine's files (13 §13.4.7).
const MAX_TEXT: u64 = 8 << 20;
const MAX_ALTO: u64 = 64 << 20;

/// Reads a file an engine wrote: it must be a regular file, not a symlink
/// (checked before opening, and refused by `O_NOFOLLOW` when opening), and
/// at most `max` bytes. The container has exited by then, so nothing
/// changes the file meanwhile.
fn read_output(path: &Path, max: u64) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        bail!("{} isn't a regular file", path.display());
    }
    if meta.len() > max {
        bail!("{} is {} bytes, over {max}", path.display(), meta.len());
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        bail!("{} isn't a regular file", path.display());
    }
    let mut buf = Vec::new();
    file.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!("{} is over {max} bytes", path.display());
    }
    Ok(buf)
}

/// Point ALTO's `sourceImageInformation/fileName` at the scan's URL rather
/// than the scratch file it was read from.
pub fn set_alto_file_name(alto: &str, name: &str) -> String {
    let escaped = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    match (alto.find("<fileName>"), alto.find("</fileName>")) {
        (Some(a), Some(b)) if a < b => {
            format!("{}<fileName>{escaped}{}", &alto[..a], &alto[b..])
        }
        _ => alto.to_owned(),
    }
}

/// Point ALTO's `sourceImageInformation/fileName` at the scan's URL, as
/// [`set_alto_file_name`], and record the sha256 of the bytes read in a
/// `fileIdentifier` after it (ADR-0015 decision 9), so a result can be
/// traced to the exact scan, fetched or cached. A JP2 from its batch also
/// gets a `fileIdentifier` with the md5 LoC's bag manifest lists for it,
/// which the bytes matched.
pub fn set_alto_source(alto: &str, name: &str, sha256: &str, md5: Option<&str>) -> String {
    let named = set_alto_file_name(alto, name);
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex(sha256) {
        return named;
    }
    let mut ids =
        format!("<fileIdentifier fileIdentifierLocation=\"sha256\">{sha256}</fileIdentifier>");
    if let Some(m) = md5.filter(|m| hex(m)) {
        ids.push_str(&format!(
            "<fileIdentifier fileIdentifierLocation=\"md5\">{m}</fileIdentifier>"
        ));
    }
    match named.find("</fileName>") {
        Some(b) if named.contains("<fileName>") => {
            let end = b + "</fileName>".len();
            format!("{}{ids}{}", &named[..end], &named[end..])
        }
        _ => named,
    }
}

#[async_trait]
impl Engine for Tesseract {
    fn source(&self) -> &str {
        "usnm-tesseract"
    }

    fn can_read(&self, language: &str) -> bool {
        route(language, &self.installed).is_some()
    }

    async fn ocr(
        &self,
        image: &Path,
        language: &str,
        work: &Path,
    ) -> Result<EngineOutput, EngineError> {
        let models = route(language, &self.installed)
            .ok_or_else(|| EngineError::Unsupported(language.to_owned()))?;
        // Under the engine runtime, the page gets a slot on the shared
        // scratch tmpfs instead of `work`, which the engine account's
        // containers can't reach; the scan is copied in.
        match &self.sandbox {
            Some(sb) => {
                let (dir, level) = sb.page_dir().await?;
                let out = self
                    .read_page(image, &models, &dir.path, level.as_deref())
                    .await;
                sb.release(dir).await;
                out
            }
            None => self.read_page(image, &models, work, None).await,
        }
    }
}

impl Tesseract {
    /// Reads `image` with `models` in `work`, labelled `level` under the
    /// engine runtime.
    async fn read_page(
        &self,
        image: &Path,
        models: &str,
        work: &Path,
        level: Option<&str>,
    ) -> Result<EngineOutput, EngineError> {
        // Tesseract runs in `work`, on paths relative to it, so the same
        // arguments serve the host binary and a container with `work` as
        // /work. A scan from elsewhere is copied in first.
        let input = match image.parent() {
            Some(p) if p == work => image.to_owned(),
            _ => {
                let ext = image.extension().and_then(|e| e.to_str()).unwrap_or("img");
                let copy = work.join(format!("input.{ext}"));
                tokio::fs::copy(image, &copy)
                    .await
                    .context("copying the scan into the scratch directory")?;
                copy
            }
        };
        let input_name = input
            .file_name()
            .and_then(|n| n.to_str())
            .context("scan file name isn't UTF-8")?
            .to_owned();
        let base = work.join("page");
        let (mut cmd, mut guard) = self.runner.command(self.threads, Some(work), level);
        // `txt` and `alto` are Tesseract's config files for the two outputs
        // (page.txt, page.xml), written in one pass.
        let child = cmd
            .args([input_name.as_str(), "page"])
            .args(["-l", models, "--psm", &self.psm.to_string()])
            .args(["--dpi", &self.dpi.to_string()])
            .args(["txt", "alto"])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| anyhow::anyhow!("tesseract took over {:?}", self.timeout))?
            .context("running tesseract")?;
        let oom = match guard.as_mut() {
            Some(g) => g.finish().await,
            None => false,
        };
        if oom {
            let limit = match &self.runner {
                Runner::Podman {
                    remote: Some(r), ..
                } => format!(" ({} MiB)", r.limits.memory_mib),
                _ => String::new(),
            };
            return Err(anyhow::anyhow!("tesseract ran out of memory{limit}").into());
        }
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow::anyhow!(
                "tesseract exited with {}: {}",
                out.status,
                tail(&err, 400)
            )
            .into());
        }
        // What Tesseract said, for a run that exits 0 but writes nothing.
        let said = || {
            let files: Vec<String> = std::fs::read_dir(work)
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "scratch holds {files:?}; tesseract said: {}",
                tail(&String::from_utf8_lossy(&out.stderr), 400)
            )
        };
        // The engine wrote these, so they're read as untrusted (13 §13.4.7):
        // regular files only, no symlinks, size-capped.
        let text = read_output(&base.with_extension("txt"), MAX_TEXT)
            .with_context(|| format!("tesseract wrote no usable text ({})", said()))?;
        let text = String::from_utf8(text).context("tesseract's text isn't UTF-8")?;
        let alto = read_output(&base.with_extension("xml"), MAX_ALTO)
            .with_context(|| format!("tesseract wrote no usable ALTO ({})", said()))?;
        Ok(EngineOutput {
            text,
            alto,
            engine: self
                .tag(models)
                .with_context(|| {
                    format!(
                        "no ocr_engine tag for tesseract {} with {models}",
                        self.version
                    )
                })?
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_german_to_fraktur_when_installed() {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
        assert_eq!(
            route("deu", &set(&["deu", "deu_latf", "eng"])).as_deref(),
            Some("deu_latf+deu")
        );
        assert_eq!(
            route("deu", &set(&["deu", "deu_latf", "frak2021"])).as_deref(),
            Some("frak2021")
        );
        assert_eq!(route("deu", &set(&["deu"])).as_deref(), Some("deu"));
        assert_eq!(route("pol", &set(&["deu", "eng"])), None);
        assert_eq!(route("deu", &set(&["eng"])), None);
        assert_eq!(route("srp", &set(&["srp"])).as_deref(), Some("srp"));
        assert_eq!(route("ces", &set(&["ces"])).as_deref(), Some("ces"));
        assert_eq!(route("eng", &set(&["eng", "osd"])).as_deref(), Some("eng"));
    }

    #[test]
    fn the_lock_pins_every_model_a_route_names() {
        let lock: BTreeSet<String> = MODELS_LOCK
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|n| n.strip_prefix("tessdata/")?.strip_suffix(".traineddata"))
            .map(str::to_owned)
            .collect();
        for l in ["ces", "deu", "eng", "pol", "rus", "srp"] {
            let profile = route(l, &lock).unwrap_or_else(|| panic!("no route for {l}"));
            assert!(models_digest(MODELS_LOCK, &profile).is_some(), "{profile}");
        }
        assert_eq!(route("deu", &lock).as_deref(), Some("frak2021"));
        assert_eq!(models_digest(MODELS_LOCK, "frk"), None);
    }

    #[test]
    fn tags_name_the_image_and_models() {
        let t = Tesseract {
            runner: Runner::Podman {
                podman: "podman".into(),
                remote: None,
                image: format!("sha256:{}", "ab".repeat(32)),
            },
            sandbox: None,
            version: "5.5.0".into(),
            installed: BTreeSet::new(),
            psm: 3,
            dpi: 300,
            threads: 1,
            timeout: Duration::from_secs(1),
        };
        let tag = t.tag("frak2021").unwrap().to_string();
        let want = models_digest(MODELS_LOCK, "frak2021").unwrap();
        assert_eq!(
            tag,
            format!("tesseract/5.5.0 profile=frak2021 image=sha256:abababababab models={want} route=minimal@r1 contract=0")
        );
        // The order of a profile's models doesn't change the digest.
        assert_eq!(
            models_digest(MODELS_LOCK, "deu_latf+deu"),
            models_digest(MODELS_LOCK, "deu+deu_latf")
        );
        let host = Tesseract {
            runner: Runner::Host("tesseract".into()),
            ..t
        };
        assert!(!host.tag("eng").unwrap().pinned());
    }

    #[test]
    fn reads_version_and_languages() {
        assert_eq!(
            parse_version("tesseract 5.5.1\n leptonica-1.87.0\n").as_deref(),
            Some("5.5.1")
        );
        assert_eq!(
            parse_version("tesseract v5.3.0.20221214\n").as_deref(),
            Some("5.3.0.20221214")
        );
        assert_eq!(parse_version("nope"), None);
        let langs = parse_langs(
            "List of available languages in \"/usr/share/tesseract/tessdata/\" (3):\neng\nosd\ndeu_latf\n",
        );
        assert_eq!(
            langs.into_iter().collect::<Vec<_>>(),
            ["deu_latf", "eng", "osd"]
        );
    }

    #[test]
    fn image_must_be_pinned() {
        let id = "a".repeat(64);
        assert!(check_image_ref(&format!("sha256:{id}")).is_ok());
        assert!(check_image_ref(&format!("ghcr.io/x/tesseract@sha256:{id}")).is_ok());
        assert!(check_image_ref(&format!("localhost/usnm-ocr/tesseract:5.5@sha256:{id}")).is_ok());
        assert!(check_image_ref("usnm-ocr/tesseract:latest").is_err());
        assert!(check_image_ref("sha256:abc").is_err());
        assert!(check_image_ref(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(check_image_ref(&format!("@sha256:{id}")).is_err());
        assert!(check_image_ref(&format!("x y@sha256:{id}")).is_err());
    }

    fn args_of(cmd: &tokio::process::Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn podman_runs_the_image_without_network_or_privileges() {
        let image = format!("sha256:{}", "b".repeat(64));
        let runner = Runner::Podman {
            podman: "podman".into(),
            remote: Some(Remote {
                url: "unix:///run/usnm-ocr/podman.sock".into(),
                scratch: "/run/usnm-ocr/scratch".into(),
                limits: Limits::default(),
            }),
            image: image.clone(),
        };
        let (cmd, guard) = runner.command(
            2,
            Some(Path::new("/run/usnm-ocr/scratch/page-x")),
            Some("s0:c4,c5"),
        );
        let mut guard = guard.expect("a container has a guard");
        guard.armed = false;
        let args = args_of(&cmd);
        let has = |a: &str| args.iter().any(|x| x == a);
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert_eq!(
            args[..3],
            ["--remote", "--url", "unix:///run/usnm-ocr/podman.sock"]
        );
        for a in [
            "run",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-hosts",
            "--log-driver=none",
            "OMP_THREAD_LIMIT=2",
            "HOME=/tmp",
            "/run/usnm-ocr/scratch/page-x:/work:rw",
        ] {
            assert!(has(a), "{a} missing from {args:?}");
        }
        // No --rm: OOMKilled is read before the container is removed.
        assert!(!has("--rm"), "{args:?}");
        assert!(pair("--user", "10001:10001"), "{args:?}");
        assert!(pair("--memory", "1536m") && pair("--memory-swap", "1536m"));
        assert!(pair("--cpus", "1") && pair("--pids-limit", "64"));
        assert!(pair("--security-opt", "label=level:s0:c4,c5"));
        assert!(!args
            .iter()
            .any(|a| a.ends_with(":Z") || a == "--userns=keep-id"));
        assert_eq!(args.last(), Some(&image));
        // The client gets none of the worker's environment.
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(envs.iter().all(|(k, _)| [
            "PATH",
            "LANG",
            "HOME",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
            "CONTAINERS_CONF"
        ]
        .contains(&k.to_str().unwrap())));

        // Podman run by the worker's own user (a developer machine): the
        // engine runs as the scratch directory's owner, which keep-id maps.
        let local = Runner::Podman {
            podman: "podman".into(),
            remote: None,
            image: image.clone(),
        };
        let dir = tempfile::tempdir().unwrap();
        let (cmd, guard) = local.command(1, Some(dir.path()), None);
        guard.unwrap().armed = false;
        let args = args_of(&cmd);
        assert_eq!(args[0], "run");
        assert!(args.iter().any(|a| a == "--userns=keep-id"));
        assert!(args.iter().any(|a| a.ends_with("/work:Z")));
        #[cfg(unix)]
        {
            let (uid, gid) = owner(dir.path()).unwrap();
            let want = format!("{uid}:{gid}");
            assert!(args.contains(&want));
        }
        let (cmd, guard) = Runner::Host("tesseract".into()).command(1, None, None);
        assert!(guard.is_none());
        assert_eq!(cmd.as_std().get_program(), "tesseract");
    }

    #[test]
    fn reads_the_engine_runtime_from_podman_info() {
        let info = |controllers: &str, uidmap: &str| {
            format!(
                r#"{{"host": {{"cgroupControllers": {controllers},
                  "idMappings": {{"uidmap": {uidmap}}},
                  "security": {{"selinuxEnabled": true}}}}}}"#
            )
        };
        let rootless = r#"[{"container_id": 0, "host_id": 991, "size": 1},
                           {"container_id": 1, "host_id": 231072, "size": 65536}]"#;
        assert_eq!(
            parse_info(&info(r#"["cpu", "io", "memory", "pids"]"#, rootless)).unwrap(),
            RuntimeInfo {
                engine_uid: 231072 + 10000,
                runtime_uid: Some(991),
                selinux: true
            }
        );
        // Rootful: the user is itself on the host.
        assert_eq!(
            parse_info(&info(r#"["cpu", "memory", "pids"]"#, "[]")).unwrap(),
            RuntimeInfo {
                engine_uid: 10001,
                runtime_uid: None,
                selinux: true
            }
        );
        // EL9's default delegation (memory and pids only) can't apply --cpus.
        let err = parse_info(&info(r#"["memory", "pids"]"#, rootless)).unwrap_err();
        assert!(err.to_string().contains("no cpu controller"), "{err}");
        // Subordinate ids too few for the image's user.
        let small = r#"[{"container_id": 0, "host_id": 991, "size": 1},
                        {"container_id": 1, "host_id": 231072, "size": 1000}]"#;
        assert!(parse_info(&info(r#"["cpu", "memory", "pids"]"#, small)).is_err());
        assert!(parse_info("nope").is_err());
    }

    #[test]
    fn engine_files_are_read_as_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("page.txt");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(read_output(&f, 5).unwrap(), b"hello");
        assert!(read_output(&f, 4).is_err());
        assert!(read_output(dir.path(), 100).is_err());
        assert!(read_output(&dir.path().join("none"), 100).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("page.xml");
            std::os::unix::fs::symlink(&f, &link).unwrap();
            assert!(read_output(&link, 100).is_err());
        }
    }

    #[test]
    fn slots_are_claimed_once_and_freed() {
        let scratch = tempfile::tempdir().unwrap();
        let a = claim_slot(scratch.path()).unwrap();
        let b = claim_slot(scratch.path()).unwrap();
        assert_ne!(a.slot, b.slot);
        assert_ne!(a.level(), b.level());
        assert!(a.level().starts_with("s0:c"), "{}", a.level());
        // Another process's slot is skipped.
        let taken = (a.slot + 1) % SLOTS;
        std::fs::create_dir_all(scratch.path().join(format!("slot-{taken}"))).unwrap();
        for _ in 0..4 {
            let c = claim_slot(scratch.path()).unwrap();
            assert!(c.slot != taken && c.slot != a.slot && c.slot != b.slot);
        }
        let path = a.path.clone();
        drop(a);
        assert!(!path.exists());
        // Old slots go; new ones stay.
        remove_stale_slots(scratch.path(), Duration::from_secs(3600));
        assert!(b.path.exists());
        remove_stale_slots(scratch.path(), Duration::ZERO);
        assert!(!scratch.path().join(format!("slot-{taken}")).exists());
    }

    #[test]
    fn alto_names_the_scan() {
        let alto =
            "<sourceImageInformation><fileName>/tmp/x/page.jpg</fileName></sourceImageInformation>";
        assert_eq!(
            set_alto_file_name(alto, "https://tile.loc.gov/a?b&c"),
            "<sourceImageInformation><fileName>https://tile.loc.gov/a?b&amp;c</fileName></sourceImageInformation>"
        );
        assert_eq!(set_alto_file_name("<alto/>", "x"), "<alto/>");
        assert_eq!(
            set_alto_source(alto, "https://tile.loc.gov/a", "ab12", None),
            "<sourceImageInformation><fileName>https://tile.loc.gov/a</fileName><fileIdentifier fileIdentifierLocation=\"sha256\">ab12</fileIdentifier></sourceImageInformation>"
        );
        assert_eq!(set_alto_source("<alto/>", "x", "ab", None), "<alto/>");
        assert!(!set_alto_source(alto, "x", "<x>", None).contains("fileIdentifier"));
        // A JP2 from its batch: the storage URL, the sha256 and the bag's md5.
        assert_eq!(
            set_alto_source(alto, "https://tile.loc.gov/storage-services/service/ndnp/x/0812.jp2", "ab12", Some("cd34")),
            "<sourceImageInformation><fileName>https://tile.loc.gov/storage-services/service/ndnp/x/0812.jp2</fileName><fileIdentifier fileIdentifierLocation=\"sha256\">ab12</fileIdentifier><fileIdentifier fileIdentifierLocation=\"md5\">cd34</fileIdentifier></sourceImageInformation>"
        );
        assert!(!set_alto_source(alto, "x", "ab", Some("<x>")).contains("md5"));
    }

    /// Reads a rendered line with the real Tesseract, when it is installed
    /// with English (skipped otherwise).
    #[tokio::test]
    async fn real_tesseract_reads_text_and_alto() {
        let Ok(t) = Tesseract::detect(TesseractOptions::default()).await else {
            eprintln!("tesseract not installed; skipping");
            return;
        };
        if t.engine_for("eng").is_none() {
            eprintln!("tesseract has no eng model; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("line.pgm");
        std::fs::write(&img, crate::testimg::text_image("HELLO WORLD")).unwrap();
        let out = t.ocr(&img, "eng", dir.path()).await.unwrap();
        assert!(out.engine.starts_with("tesseract/"), "{}", out.engine);
        assert!(out.engine.contains(" profile=eng "), "{}", out.engine);
        assert!(out.text.contains("HELLO"), "read {:?}", out.text);
        assert!(String::from_utf8_lossy(&out.alto).contains("<alto"));
        assert!(matches!(
            t.ocr(&img, "zzz", dir.path()).await,
            Err(EngineError::Unsupported(_))
        ));
    }
}
