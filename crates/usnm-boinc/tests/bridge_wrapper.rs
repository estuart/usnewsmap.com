//! bridge-job.sh's submit-remote branch, in its own test binary: it spawns shells, and a
//! forked child briefly holds copies of every open descriptor, journal locks included.
#![cfg(unix)]
use std::path::Path;

/// bridge-job.sh's submit-remote: pull, then submit only a job that is still reserved.
#[test]
fn bridge_submit_remote_submits_only_a_reserved_job() {
    use std::os::unix::fs::PermissionsExt;
    let id = "b".repeat(64);
    for (state, submits, url) in [
        (
            "reserved",
            true,
            "https://stusnmoexample.blob.core.windows.net/boinc-handoff",
        ),
        (
            "submitted",
            false,
            "https://stusnmoexample.blob.core.windows.net/boinc-handoff",
        ),
        ("reserved", false, "https://example.org/boinc-handoff"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let calls = tmp.path().join("calls");
        let library = tmp.path().join("lib.sh");
        std::fs::write(
            &library,
            "PROJ=/nonexistent/proj\nfetch_secrets() { :; }\nrender_config() { :; }\n\
             log() { echo \"$*\" >&2; }\nfail() { log \"$*\"; exit 1; }\n",
        )
        .unwrap();
        let stub = bin.join("usnm-boinc");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2\" in 'handoff pull') echo {state} ;; esac\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../contrib-compute/images/server/bridge-job.sh");
        let script = std::fs::read_to_string(source)
            .unwrap()
            .replace(
                ". /usr/local/lib/usnm-boinc/lib.sh",
                &format!(". '{}'", library.display()),
            )
            .replace("[ -e \"$PROJ/.usnm-initialised\" ] ||", "true ||");
        let wrapper = tmp.path().join("bridge-job.sh");
        std::fs::write(&wrapper, script).unwrap();
        let out = std::process::Command::new("bash")
            .arg(&wrapper)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("BRIDGE_STEP", "submit-remote")
            .env("JOB_ID", &id)
            .env("HANDOFF_DIR", tmp.path().join("handoff"))
            .env("HANDOFF_URL", url)
            .output()
            .unwrap();
        if !url.contains(".blob.core.windows.net/") {
            // Only an Azure blob container is accepted, and nothing runs before that check.
            assert!(!out.status.success() && !calls.exists());
            continue;
        }
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let calls = std::fs::read_to_string(&calls).unwrap();
        assert!(calls.starts_with("handoff pull --from-url"), "{calls}");
        assert_eq!(calls.contains("\nsubmit --root"), submits, "{calls}");
    }
}
