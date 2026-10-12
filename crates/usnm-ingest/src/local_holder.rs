//! Lock holders on one machine, for a lock kept in a local state file
//! (`--state-file`): the on-prem Serial Set fetch's `congress-serialset`.
//!
//! A run killed without releasing its lock (SIGKILL, a crash, a reboot)
//! leaves it held until its lease runs out, 30 minutes. A holder id that
//! names the host, the boot and the process lets the next run on the same
//! host see that the holder is gone and take the lock over at once: a
//! holder from an earlier boot is gone, and so is one from this boot whose
//! process isn't running. Anything else (another host, a live process, an
//! id in another form) waits for the lease as before.

use std::path::Path;

/// This host, this boot and this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHolder {
    host: String,
    boot: String,
    pid: u32,
}

impl LocalHolder {
    /// This process, where `/proc` names the host and the boot (Linux);
    /// `None` elsewhere.
    pub fn current() -> Option<Self> {
        let read = |p: &str| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| Self::part_ok(s))
        };
        Some(Self {
            host: read("/proc/sys/kernel/hostname")?,
            boot: read("/proc/sys/kernel/random/boot_id")?,
            pid: std::process::id(),
        })
    }

    /// Hostnames and boot ids hold no `:` and no `/`; a lock name or owner
    /// goes into a Cosmos id or a path elsewhere, so keep both out.
    fn part_ok(s: &str) -> bool {
        !s.is_empty() && !s.contains([':', '/', '\\', '?', '#']) && !s.contains(char::is_whitespace)
    }

    /// The holder id: `{host}:{boot}:{pid}:{nanos}`, the nanoseconds making
    /// it unique to this run even if the process id comes round again.
    pub fn id(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        format!("{}:{}:{}:{nanos:08x}", self.host, self.boot, self.pid)
    }

    fn parse(id: &str) -> Option<Self> {
        let mut parts = id.split(':');
        let (host, boot, pid, _nanos) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !Self::part_ok(host) || !Self::part_ok(boot) {
            return None;
        }
        Some(Self {
            host: host.to_owned(),
            boot: boot.to_owned(),
            pid: pid.parse().ok()?,
        })
    }

    /// Whether `holder` was a run on this host that is gone: from an earlier
    /// boot, or from this one with its process no longer `running` (or with
    /// this process's id, which it can't be while this process runs).
    pub fn is_gone(&self, holder: &str, running: impl Fn(u32) -> bool) -> bool {
        let Some(h) = Self::parse(holder) else {
            return false;
        };
        h.host == self.host && (h.boot != self.boot || h.pid == self.pid || !running(h.pid))
    }

    /// Whether process `pid` exists on this host (a process id taken again
    /// by another process counts as running: the lease is waited out).
    pub fn running(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> LocalHolder {
        LocalHolder {
            host: "usnm-ocr-node3".into(),
            boot: "d943197f-b3d8-47ce-a29e-7dce543d849b".into(),
            pid: 4242,
        }
    }

    #[test]
    fn an_id_reads_back() {
        let id = me().id();
        assert!(
            id.starts_with("usnm-ocr-node3:d943197f-b3d8-47ce-a29e-7dce543d849b:4242:"),
            "{id}"
        );
        assert_eq!(LocalHolder::parse(&id), Some(me()));
        assert_ne!(
            LocalHolder::parse(&id).map(|h| h.id()),
            Some(id),
            "unique per run"
        );
    }

    #[test]
    fn only_a_gone_run_on_this_host_is_gone() {
        let me = me();
        let alive = |_| true;
        let dead = |_| false;
        let other = |host: &str, boot: &str, pid: u32| format!("{host}:{boot}:{pid}:0000beef");
        let boot = me.boot.clone();
        // This boot: gone when its process is.
        assert!(me.is_gone(&other("usnm-ocr-node3", &boot, 17697), dead));
        assert!(!me.is_gone(&other("usnm-ocr-node3", &boot, 17697), alive));
        // This process's own id, from an earlier run: gone.
        assert!(me.is_gone(&other("usnm-ocr-node3", &boot, 4242), alive));
        // An earlier boot: gone, whatever runs under that id now.
        assert!(me.is_gone(&other("usnm-ocr-node3", "1b0f9a2e-0000", 17697), alive));
        // Another host: never.
        assert!(!me.is_gone(&other("usnm-ocr-node4", &boot, 17697), dead));
        assert!(!me.is_gone(&other("usnm-ocr-node4", "1b0f9a2e-0000", 17697), dead));
        // Ids in another form (owner_id(), an older run's, a released
        // lock's empty owner): never.
        for id in [
            "local-17697-20c807e1",
            "",
            "?",
            "a:b:c:d",
            "a:b:1:d:e",
            "a b:x:1:d",
        ] {
            assert!(!me.is_gone(id, dead), "{id}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_found_on_linux() {
        let me = LocalHolder::current().expect("/proc names the host and boot");
        assert_eq!(LocalHolder::parse(&me.id()), Some(me));
        assert!(LocalHolder::running(std::process::id()));
    }
}
