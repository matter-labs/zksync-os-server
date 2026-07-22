//! Watchdog that kills a target process when the process holding the other end of our
//! stdin pipe dies — however it dies, including SIGKILL and OOM kills.
//!
//! Usage: `leash <pid> <grace_secs> <expected_name>`, with stdin connected to a pipe whose
//! write end is held (and never written to) by the process whose lifetime we track. The
//! kernel closes that pipe on any kind of process death, so reading EOF here is a reliable
//! death signal that needs no polling and no cooperation from the tracked process.
//!
//! On EOF: verify the target is still the process we were asked to kill (guards against
//! PID reuse), send SIGTERM, wait up to `grace_secs`, then SIGKILL.
//!
//! This is intentionally a Rust binary rather than a shell script. Prover test artifacts are
//! built on a non-GPU machine and transferred to a GPU runner, so keeping the watchdog in the
//! Cargo build makes the transferred test archive self-contained.
//!
//! See `src/leash.rs` for the spawning side.

fn main() {
    #[cfg(unix)]
    unix::run();
    #[cfg(not(unix))]
    panic!("leash is only supported on Unix");
}

#[cfg(unix)]
mod unix {
    pub(super) fn run() {
        let args: Vec<String> = std::env::args().collect();
        let (pid, grace_secs, expected_name) = match &args[..] {
            [_, pid, grace, name] => (
                parse_pid(pid).expect("pid must be a positive process ID"),
                grace.parse::<u64>().expect("grace_secs must be an integer"),
                name.clone(),
            ),
            _ => {
                eprintln!("usage: leash <pid> <grace_secs> <expected_name>");
                std::process::exit(2);
            }
        };

        wait_for_parent_death();

        if !name_matches(pid, &expected_name) {
            // Target is gone, or the PID has been reused by an unrelated process.
            return;
        }

        // SAFETY: `kill` only passes these integer values to the OS; it does not dereference
        // memory owned by Rust. A nonzero return value reports an invalid or stale PID.
        unsafe {
            if libc::kill(pid, libc::SIGTERM) != 0 {
                return;
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(grace_secs);
        while std::time::Instant::now() < deadline {
            // SAFETY: Signal 0 only asks the OS whether `pid` can be signalled and does not
            // dereference memory or deliver a signal.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !name_matches(pid, &expected_name) {
            // The target may have exited during the grace period and its PID may now belong
            // to an unrelated process.
            return;
        }
        // SAFETY: As above, `kill` only passes integer values to the OS. Failure is harmless
        // here because SIGKILL is the final best-effort cleanup step.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    /// Rejects values such as `0` and `-1`, which `kill` interprets as process selectors.
    fn parse_pid(raw: &str) -> Option<libc::pid_t> {
        let pid = raw.parse::<u32>().ok()?;
        let pid = libc::pid_t::try_from(pid).ok()?;
        (pid > 0).then_some(pid)
    }

    /// Blocks until every write end of our stdin pipe is closed.
    fn wait_for_parent_death() {
        use std::io::Read;
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin().lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Best-effort check that `pid` still refers to the process we were told to kill.
    fn name_matches(pid: libc::pid_t, expected: &str) -> bool {
        let Some(observed) = observed_name(pid) else {
            return false;
        };
        // Linux truncates a process's `comm` to 15 bytes, so accept that exact-length
        // prefix as well as an untruncated match.
        observed == expected || (observed.len() == 15 && expected.starts_with(&observed))
    }

    #[cfg(target_os = "linux")]
    fn observed_name(pid: libc::pid_t) -> Option<String> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(comm.trim_end().to_string())
    }

    #[cfg(not(target_os = "linux"))]
    fn observed_name(pid: libc::pid_t) -> Option<String> {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8(out.stdout).ok()?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        // macOS `ps` reports the full executable path.
        Some(
            std::path::Path::new(name)
                .file_name()?
                .to_string_lossy()
                .into_owned(),
        )
    }
}
