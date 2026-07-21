//! Ties the lifetime of helper processes (anvil, the prover service) to the lifetime of the
//! test process itself.
//!
//! `Drop`-based cleanup (`AnvilInstance::drop`, `kill_on_drop`) only runs while the process
//! unwinds normally. It never runs when the test process is SIGKILLed (OOM killer, `kill -9`,
//! IDE stop buttons) or aborts, and nextest only *detects* leaked grandchildren — it does not
//! kill them. A leaked anvil mines a block every 250ms forever, growing without bound.
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

/// Guarantees that the process `pid` does not outlive the current process.
///
/// `expected_name` is the target's executable name; the sidecar re-checks it before killing
/// to avoid signalling an unrelated process if the PID has been reused by then.
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
    // (std spawns pipes with CLOEXEC, so no later child can inherit the write end and keep
    // the sidecar armed past our death.)
    std::mem::forget(child.stdin.take());
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
pub fn leash_bin() -> anyhow::Result<PathBuf> {
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
