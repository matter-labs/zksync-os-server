# zksync_os_command_source

Pipeline command sources for sequencer block work.

This crate owns the decision of **when a block command may enter execution**.
It converts local replay state, consensus events, external-node replay input,
and leadership status into `BlockCommand`s consumed by `BlockExecutor`.

---

## Responsibilities

The crate provides two pipeline sources:

- `ConsensusNodeCommandSource` for main nodes.
  - Replays local WAL records on startup.
  - Optionally emits rebuild commands for rollback/rebuild flows.
  - Forwards canonized replay records returned by consensus.
  - Emits fresh `Produce` commands only while the node is leader.
- `ExternalNodeCommandSource` for external nodes.
  - Forwards replay records received from the main node into local execution.

It intentionally does **not** execute blocks, apply state, canonize consensus
results, manage mempool contents, or decide RPC transaction acceptance.

---

## Pacing model

Command-source pacing is explicit and does not rely on Tokio channel capacity
for correctness.

`CommandWindow` bounds how many commands may be outstanding until
`BlockExecutor` sends `CommandAck::Executed`:

| Command | Window behavior |
|---|---|
| `Produce` | At most one pending produce command. |
| `Replay` | May fill the configured command window. |
| `Rebuild` | May fill the configured command window. |

The default command window size is `DEFAULT_COMMAND_WINDOW_CAPACITY` (currently
`2`). This is a shared window, not a per-type quota. With the default:

- `Replay + Replay` is allowed.
- `Replay + Rebuild` is allowed.
- `Produce + Replay` is allowed.
- `Produce + Produce` is not allowed.

`ReplayCommandForwarder` also checks the pipeline admission gate before replay
work is forwarded. When downstream backpressure closes the gate, both main-node
and external-node replay forwarding pause until the gate opens again.

The pipeline's mpsc buffers remain useful as transport buffers, but they are not
the source of the sequencing guarantee.

---

## Acknowledgements

The command source records every emitted command in FIFO order inside
`CommandWindow`. `BlockExecutor` sends `CommandAck::Executed(command_type)` after
the command has executed and the resulting `BlockPayload` has been handed to the
next pipeline stage.

An acknowledgement frees one command-window slot. Its command type must match
the oldest pending command; a mismatch is treated as an error because it means
the source and executor no longer agree on command ordering.

This ACK is an execution-boundary signal only. It does **not** mean the block was
applied to storage, canonized by consensus, batched, or finalized on L1. Those
are tracked by later pipeline stages.

### Why `CommandAck` and not a `Semaphore`

A `tokio::sync::Semaphore` would handle the concurrency-bounding concern with
less code, but it carries no information — releasing a permit tells the source
only that a slot is free, nothing about what happened during execution.

`CommandAck` is a typed feedback channel. The current `Executed` variant is
minimal, but the enum is the natural place to add execution metadata when
throttling based on observed runtime behaviour becomes necessary:

```rust
// hypothetical future extension
CommandAck::ExecutedWithStats {
    cmd_type: BlockCommandType,
    tx_count: u64,
    execution_ms: u64,
    memory_bytes: u64,
}
```

The source already receives acks inside its `select!` loop, so a token-bucket
or leaky-bucket rate limiter driven by real execution data can be added there
without changing the channel topology or the executor's interface. A semaphore
would require a separate stats channel alongside it, splitting what `CommandAck`
keeps in one place.

---

## Main-node flow

```text
local WAL / consensus / leadership
        │
        ▼
ConsensusNodeCommandSource
        │ BlockCommand::{Replay, Rebuild, Produce}
        ▼
BlockExecutor
        │ CommandAck::Executed(command_type)
        └───────────────────────────────► CommandWindow
```

The main-node source gives priority to role changes and executor ACKs, then
canonized replay records, and emits fresh `Produce` commands as lowest-priority
leader work.

---

## External-node flow

```text
main-node replay stream
        │
        ▼
ExternalNodeCommandSource
        │ BlockCommand::Replay
        ▼
BlockExecutor
        │ CommandAck::Executed(Replay)
        └───────────────────────────────► CommandWindow
```

The external-node source uses the same command window and admission-gate model
as the main-node replay path, so replay throughput can be bounded independently
from raw network buffering.
