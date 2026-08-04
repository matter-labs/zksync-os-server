//! Ties the lifetime of helper processes (anvil, the prover service) to the lifetime of the
//! test process itself.
//!
//! `Drop`-based cleanup (`AnvilInstance::drop`, `kill_on_drop`) runs during ordinary teardown
//! or unwinding. It never runs when the test process is SIGKILLed (OOM killer, `kill -9`, IDE
//! stop buttons) or aborts. nextest kills the test's process group on timeouts and Ctrl-C,
//! but children that outlive any other kind of test death are only *detected* as leaky, never
//! killed. A leaked anvil mines a block every 250ms forever, growing without bound.
//!
//! [`attach`] spawns a tiny sidecar (see `src/bin/leash.rs`) that holds the read end of a
//! pipe whose write end stays open in this process for its entire lifetime. The kernel
//! closes that write end on *any* kind of process death, including SIGKILL, at which point
//! the sidecar kills the target (SIGTERM, grace period, SIGKILL) and exits.

use anyhow::Context;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// How long the sidecar waits after SIGTERM before escalating to SIGKILL.
const GRACE_SECS: u64 = 5;

/// Makes sure `pid` dies once the current process dies, no matter how the current process
/// exits — including SIGKILL, where no `Drop` ever runs. The target first gets SIGTERM,
/// then SIGKILL after [`GRACE_SECS`] if it did not shut down.
///
/// `expected_name` is the target's executable name; the sidecar re-checks it before killing
/// so that a PID reused by an unrelated process is left alone.
pub(crate) fn attach(pid: u32, expected_name: &str) -> anyhow::Result<()> {
    let mut child = Command::new(leash_bin()?)
        .args([
            pid.to_string(),
            GRACE_SECS.to_string(),
            expected_name.to_string(),
        ])
        .stdin(Stdio::piped())
        // The sidecar outlives the test process; if it inherited stdout/stderr, nextest
        // would flag every test as leaky via the still-open handles.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn leash sidecar")?;
    // Keep the pipe's write end open until this process dies: its closure — performed by
    // the kernel even on SIGKILL — is what tells the sidecar to fire. The leash is thus a
    // pure backstop; regular shutdown paths stay in charge while the process is alive.
    // (std spawns pipes with CLOEXEC, so no later child can inherit the write end and hold
    // the pipe open past our death, which would stop the sidecar from ever firing.)
    let write_end = child
        .stdin
        .take()
        .context("leash sidecar stdin was not piped")?;
    std::mem::forget(write_end);
    // The sidecar strictly outlives this process, so it can never become our zombie;
    // dropping the handle without waiting is fine.
    drop(child);
    Ok(())
}

/// Path to the `leash` binary built alongside the integration tests.
///
/// Cargo only injects `CARGO_BIN_EXE_leash` into the package's test targets, not this
/// library, so resolve it relative to the running test executable
/// (`target/<profile>/deps/<test>` → `target/<profile>/leash`).
fn leash_bin() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine current executable")?;
    let exe_dir = exe
        .parent()
        .context("current executable has no parent directory")?;
    let mut candidates = vec![exe_dir.join("leash")];
    if let Some(target_dir) = exe_dir.parent() {
        candidates.push(target_dir.join("leash"));
    }
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .with_context(|| format!("leash binary not found; tried {candidates:?}"))
}
