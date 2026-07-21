//! End-to-end tests for the `leash` sidecar (see `src/bin/leash.rs` and `src/leash.rs`):
//! the mechanism that guarantees anvil/prover children die with the test process even when
//! it is SIGKILLed.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zksync_os_integration_tests::leash::leash_bin;

/// `CARGO_BIN_EXE_leash` is only available here, in the package's test targets; the library
/// resolves the binary from `current_exe()` instead. Pin the two to each other so the
/// resolution logic can't silently rot.
#[test]
fn leash_bin_resolution_matches_cargo() {
    let resolved = leash_bin().expect("failed to resolve leash binary");
    let expected = std::path::Path::new(env!("CARGO_BIN_EXE_leash"));
    assert_eq!(
        resolved.canonicalize().unwrap(),
        expected.canonicalize().unwrap(),
    );
}

/// Emulates the real deployment: a "holder" process owns the write end of the leash's stdin
/// pipe (like the test process does via the leaked `ChildStdin`), and gets SIGKILLed. The
/// kernel closes the pipe, and the leash must take the victim down.
#[test]
fn kills_victim_when_holder_is_sigkilled() {
    let mut victim = spawn_sleep();

    let mut leash = leash_command(victim.id(), "sleep")
        .spawn()
        .expect("failed to spawn leash");
    let pipe_write_end = leash.stdin.take().unwrap();

    // Hand the pipe's write end off to the holder as its stdout; our own copy is closed
    // when `spawn` returns, so the holder is then the sole owner.
    let mut holder = Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::from(pipe_write_end))
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn holder");

    // Give the chain a moment to settle, then hard-kill the holder — the case Drop-based
    // cleanup can never handle.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        victim.try_wait().unwrap().is_none(),
        "victim died too early"
    );
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_dies_within(&mut victim, Duration::from_secs(10));
    let _ = leash.wait();
}

/// If the PID was reused by an unrelated process by the time the leash fires, it must not
/// kill it. Emulated by giving the leash a wrong expected name from the start.
#[test]
fn spares_victim_on_name_mismatch() {
    let mut victim = spawn_sleep();

    let mut leash = leash_command(victim.id(), "definitely-not-sleep")
        .spawn()
        .expect("failed to spawn leash");
    // Dropping the write end delivers EOF immediately.
    drop(leash.stdin.take());
    leash.wait().unwrap();

    assert!(
        victim.try_wait().unwrap().is_none(),
        "leash killed a process whose name did not match"
    );
    victim.kill().unwrap();
    victim.wait().unwrap();
}

/// The kill path triggers on pipe EOF itself, not on holder death specifically: dropping
/// the write end without any process dying must have the same effect. (Production never
/// drops the write end early — `attach` leaks it until process death — so this pins the
/// mechanism, not a real deployment scenario.)
#[test]
fn kills_victim_on_plain_eof() {
    let mut victim = spawn_sleep();

    let mut leash = leash_command(victim.id(), "sleep")
        .spawn()
        .expect("failed to spawn leash");
    drop(leash.stdin.take());

    assert_dies_within(&mut victim, Duration::from_secs(10));
    let _ = leash.wait();
}

fn spawn_sleep() -> Child {
    Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn victim")
}

fn leash_command(victim_pid: u32, expected_name: &str) -> Command {
    let mut cmd = Command::new(leash_bin().expect("failed to resolve leash binary"));
    cmd.arg(victim_pid.to_string())
        .arg("2")
        .arg(expected_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn assert_dies_within(victim: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if victim.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    victim.kill().unwrap();
    victim.wait().unwrap();
    panic!("victim was not killed by the leash within {timeout:?}");
}
