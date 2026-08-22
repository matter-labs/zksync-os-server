# Eager L1 Sender Nonce Baseline Design

## Problem

L1 discovery reads `getTotalBatches*` at a specific L1 block and uses those values to seed the
commit, prove, and execute command streams. Recovery must therefore read the operator's confirmed
nonce at the same block. Today that nonce RPC runs only after the sender receives its first
non-passthrough command. A slow prover can delay that point long enough for the provider to discard
the block's state.

Falling back to `latest` is unsafe because it advances only the nonce snapshot. Commands for
transactions mined since discovery remain at the queue head, so recovery can resubmit a completed
command at a new nonce or replace a later pending transaction with it.

## Options

1. Capture the block-pinned nonce eagerly before waiting for commands. This preserves the existing
   snapshot invariant and directly removes the delay that ages the block out.
2. Refresh all `getTotalBatches*` values and rebuild every upstream command source when the nonce
   block is unavailable. This keeps snapshots aligned but requires invasive cross-pipeline restart
   logic.
3. Use `latest` and reconcile queued commands against current contract state. This duplicates
   discovery logic inside the sender and makes range-command reconciliation error-prone.

## Design

Use option 1. At the beginning of `run_l1_sender`, before processing prepending passthrough
commands, query the confirmed operator nonce at `l1_block_number`. Capture it only when the selected
send mode needs the pinned baseline, preserving the stop-and-wait force-resubmission path's existing
behavior. Pass the captured nonce into stop-and-wait or pipelined recovery instead of performing an
RPC there. Remove the `latest` fallback.

If the pinned query fails at this eager point, return the error. L1 discovery has just read contract
state at the same block, so a failure is a genuine startup/provider problem rather than an artifact
of a delayed first command.

## Testing

Add a mocked-provider regression test that starts `run_l1_sender` with an open but empty input
channel. The test asserts that the queued block-pinned nonce response is consumed before any command
arrives. It fails on the lazy implementation and passes only when the baseline is captured eagerly.
Run the full `zksync_os_l1_sender` tests, formatting, and focused Clippy afterward.
