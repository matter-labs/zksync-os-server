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
//! See `crate::leash` for the spawning side.

#[cfg(unix)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (pid, grace_secs, expected_name) = match &args[..] {
        [_, pid, grace, name] => (
            pid.parse::<i32>().expect("pid must be an integer"),
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

    unsafe {
        if libc::kill(pid, libc::SIGTERM) != 0 {
            return;
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(grace_secs);
    while std::time::Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// Blocks until every write end of our stdin pipe is closed.
#[cfg(unix)]
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
#[cfg(unix)]
fn name_matches(pid: i32, expected: &str) -> bool {
    match observed_name(pid) {
        // The kernel truncates a process's `comm` to 15 bytes, so a truncated observed
        // name is matched as a prefix of the expected one.
        Some(observed) => {
            observed == expected || (observed.len() == 15 && expected.starts_with(&observed))
        }
        // Can't tell (no /proc on this platform, or the process is already gone). Signal
        // anyway: `kill` of a dead PID is a no-op, and the caller attached us to this PID
        // moments after spawning it, so a mid-run reuse race is vanishingly unlikely.
        None => true,
    }
}

#[cfg(target_os = "linux")]
fn observed_name(pid: i32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_string())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn observed_name(pid: i32) -> Option<String> {
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

#[cfg(not(unix))]
fn main() {}
