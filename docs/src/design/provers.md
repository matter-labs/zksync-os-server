# Proving

The server does not prove anything itself. It seals batches, hands out proving
jobs over an HTTP API, checks what comes back, and puts the accepted proofs on
L1. Provers are external processes (GPU fleets) that poll that API.

There are two proof systems. **Airbender** is the primary one and always runs:
a FRI proof per batch, then one SNARK per range of batches. **ZiSK** is the
optional second proof system: a `vadcop_final` STARK per batch, then one
aggregated range proof. When both are on, the range settles as a single
*MultiProof* payload that carries both.

## Pipeline

Proving is two stages of the ordinary batch pipeline. Neither is ZiSK-specific:
each stage drives both proof systems, and with ZiSK off it behaves exactly as
it did before the second system existed.

```
ProverInputGenerator → Batcher → BatchVerification → BatchProvingPipelineStep
   → GaplessCommitter → L1Sender(commitBatches) → RangeProvingPipelineStep
   → GaplessL1ProofSender → L1Sender(proveBatches) → PriorityTree → L1Sender(execute)
```

**`BatchProvingPipelineStep`** — per batch. Opens a FRI job and, when the second
system is on, a ZiSK job. Forwards the batch downstream once its proofs are in.

**`RangeProvingPipelineStep`** — per range. Parks the Airbender range SNARK and
the aggregated ZiSK range proof, composes them into the type-5 MultiProof
payload, and emits one `ProofCommand`.

Each stage is thin. The state lives in **job managers** — `FriJobManager`,
`SnarkJobManager`, `ZiskJobManager`, `ZiskAggregationJobManager` — which the
stage and the HTTP server share through an `Arc`. That pairing (thin step +
shared manager) is the house idiom; the managers are not pipeline components.

The Airbender lane knows nothing about ZiSK: where the two meet, the HTTP
handler does it — a SNARK `pick` also tells the ZiSK aggregation lane which
range bounds were just formed (`note_snark_range`). Inside the second system
the two managers do know each other: `ZiskJobManager` holds the aggregation
manager as the sink it hands accepted per-batch streams to, and forwards
discards so a broken range is dropped with them.

## Where the ZiSK witness comes from

At **seal**. `zksync_os_native_pig::generate_batch_run` builds the Airbender
prover input for the sealed batch, and `zisk_witness::build_batch_witness`
builds the ZiSK one from the same block outputs, replay records and tree data.
Both ride the batch envelope as `ProvingInputs`; the batcher holds no lane
handle and opens no job. The bytes stop at the batch proving stage — nothing
downstream of it carries a witness.

Shadow execution (`zisk_shadow_execution`) re-executes the sealed batch's ZiSK
input in-process and records the guest's own commitment. That value is the local
arbiter when a submitted proof disagrees with the batch metadata (see
[Disagreement](#disagreement)).

## Modes

| `second_proof_system` | `multi_proof_verifier` | Mode | Behaviour |
|---|---|---|---|
| `false` | `false` | **Disabled** | Airbender only. No ZiSK jobs, no witness, no gate. |
| `true` | `false` | **Shadow** | ZiSK proves every batch and every range; the proofs are verified, measured and logged, but the range settles on the Airbender proof alone. |
| `true` | `true` | **Required** | L1 runs the MultiProofVerifier. A batch's data may not be committed until both systems have proved the batch, and a range settles only as a composed MultiProof. |

Modes change by restart, not at runtime.

The difference between Shadow and Required is not a knob inside the lane — it is
what losing a proof costs. In Shadow, ZiSK coverage is *sheddable*: a range that
fails deterministically is dropped, an over-full buffer evicts its oldest entry,
and settlement never notices. In Required none of that is allowed: the batch is
held at the commit gate and only its proof can release it, so dropping the work
would stall commits with nothing left to retry. Every bound below therefore has
a Required carve-out, and what bounds memory there is the admission window, not
eviction.

## The commit gate

Armed only in Required. A batch leaves `BatchProvingPipelineStep` when both its
FRI proof and its ZiSK proof have arrived; whichever comes first waits for the
other.

To stop the wait from growing without limit, the stage admits a batch only while

```
batch_number − oldest_batch_still_waiting < commit_gate_admission_window
```

Reaching the window stops admission and, through the batcher's backpressure,
block production. That is the accepted cost: a stalled sequencer is recoverable,
a committed batch that can never settle is not. The window must not exceed the
second lane's active queue (`MAX_TOTAL_JOBS = 50`), which startup validates.

## Restart

Nothing about proving survives a restart, in either lane.

L1 supplies the committed/proved/executed frontiers, the block-replay WAL
supplies block content, and everything else is recomputed: the batcher re-seals
the batches that still need proving, and both lanes open fresh jobs for them.
Proofs in flight when the process died are simply re-proved. There is no durable
job queue, proof cache or completion journal, and there must not be one — a
node-local artifact that outlives the process would compete with replay for
authority, and failover to a fresh host would stop working.

The FRI proof store on disk (`prover_api.proof_storage`) is a diagnostic archive
served back over an HTTP peek endpoint. Nothing reads it at startup.

## HTTP API

Served on `prover_api.address` when `prover_api.enabled` and the node is a main
node with the batcher on. Routes are under `/prover-jobs/v1`:

| Route | Purpose |
|---|---|
| `POST /FRI/pick`, `/FRI/submit` | Airbender per-batch FRI |
| `POST /FRI/{id}/failed`, `GET /FRI/{id}/peek` | report a failed proof; read one back |
| `POST /SNARK/pick`, `/SNARK/submit` | Airbender range SNARK |
| `GET /SNARK/{from}/{to}/peek` | read a range's FRI proofs |
| `POST /ZiSK/pick`, `/ZiSK/submit` | ZiSK per-batch `vadcop_final` |
| `GET /ZiSK/status`, `/ZiSK/{batch_number}/peek` | lane status; read a batch's ZiSK input |
| `POST /ZiSK-AGG/pick`, `/ZiSK-AGG/submit` | ZiSK range aggregation |
| `GET /status/` | all lanes |

Assignment is lease-based: `pick` assigns a job to a prover id, and an
unfinished job returns to the queue after its timeout (`fri_job_timeout`,
`snark_job_timeout`, `zisk_aggregation.job_timeout`). Native aggregation
verification has its own shorter recovery timeout. A rejected submission
never consumes the job — it stays assigned and times out to another prover.

The API has no authentication. Treat the port as internal.

## What the server checks before accepting a proof

In order, and every failure below leaves the job retryable:

1. **Wire shape** — proof and public-values lengths, and for ZiSK the
   `vadcop_final` stream parses.
2. **Key drift** — the program VK and inner vadcop VK the prover reports must
   match the versioned ZiSK release manifest compiled for *that batch's
   protocol version*. A version with no compiled manifest is rejected in both
   Shadow and Required; the check remains per batch so an upgrade that arrives
   after startup also fails closed.
3. **Cryptographic verification** (`zisk_proof_verification_enabled`) — the
   native verifier runs on the blocking pool, bounded to four at a time, with
   verifier panics contained. Note the pinned `zisk-verifier` checks the STARK
   layer and the aggregate's wire form and public signals; the final BN254
   pairing of the range proof is verified **on L1**, not off-chain.
4. **Batch commitment** — the proof's public input must equal the commitment
   derived from the batch metadata captured at seal.

Only then is the job consumed and the proof handed on: per-batch to the
aggregation lane, and aggregated ranges to the range stage for composition.

## Disagreement

A commitment mismatch means one of the two proof systems is wrong about the same
batch. Which one is a separate question, and the server answers it with three
values: `E` (expected, from the batch metadata), `S` (submitted, from the
proof), and `L` (local, from shadow execution).

- `S ≠ E` and `L = S` — the two independent ZiSK executions agree against the
  Airbender-derived expectation. That is a corroborated divergence. With
  `halt_on_zisk_commitment_mismatch` armed, the node halts.
- Anything else — a faulty or hostile prover, or an inconclusive result. Counted,
  logged, and the job is retried.

A halt only ever fires for a proof that passed cryptographic verification *and*
was corroborated locally. An unverified submission is just bytes a caller sent;
it must never be able to stop the node.

After repeated deterministic mismatches the lane raises `zisk_lane_unprovable`.
In Shadow it then drops the job. In Required it keeps it queued and re-raises
the alarm: commits stall until one of the two systems is fixed, and a repaired
prover can still land the proof without a restart.

## Protocol upgrades

An upgrade raises the batches' semantic protocol version, and both systems have
to keep proving across the boundary. Both proving registries are compiled into
the server binary: `ProvingVersion` for Airbender and `ZiskProvingVersion` for
the second lane.

### What the server already handles

**Range formation cuts at the boundary.** Airbender needs one *proving* version
per range, and two protocol versions can share one — 0.31.0 and 0.31.1 are both
V7. ZiSK likewise maps both patches to its V1 manifest, but ranges still cut on
the semantic protocol change so future releases cannot silently aggregate
across incompatible identities. The cut is off when the second lane is off,
leaving the Airbender-only shape unchanged.

**A complete release is pinned per proving version.** The vendored manifest
binds the inner guest ELF and program VK, aggregator ELF and program VK,
recursive `rootCVadcopFinal`, host prover binary, release archives, toolchain,
Git commit, and the combined verification-key hash enforced by L1. CI downloads
artifacts from that manifest and verifies every SHA-256 digest before GPU tests.

**Unknown versions never lease or submit.** Both ZiSK pick endpoints filter on
the prover's declared combined hash before assigning work. Even an older daemon
that omits the capability query can only lease a protocol version known to the
compiled registry. Submission repeats the per-batch check so a live protocol
upgrade cannot bypass startup validation.

### Where the two lanes differ

| Aspect | Airbender | ZiSK |
|---|---|---|
| Key source | compiled in (`ProvingVersion`) | compiled versioned release manifest (`ZiskProvingVersion`) |
| Adding a version | binary release | manifest plus binary release |
| `pick` filters by prover-declared version | yes (`supported_vk_hashes`) | yes, using the combined ZiSK L1 identity |
| Per-batch lease timeout | `fri_job_timeout` | `snark_job_timeout` |

At main-node startup, an enabled second lane checks the current L1 protocol
against the compiled ZiSK registry. Required mode additionally resolves
`getVerifier()` through the production or testnet multiproof wrapper and
compares `ZiskVerifier.verificationKeyHash()` with the manifest's combined hash.

### Settling across an upgrade (unresolved)

Airbender settles across an upgrade because L1 holds *several* verifiers and the
proof payload selects one by execution version (the `verifier_version` byte in
the type-2 payload). A batch committed before an upgrade and proved after it
still reaches the verifier it was proved for.

The type-5 MultiProof has no equivalent. The `ZiskVerifier` deployed for the
multiprover fixture pins `innerProgramVK`, `rootCVadcopFinal` and
`aggregatorProgramVK` as compile-time constants (see
`local-chains/v31.0-multiprover/README.md`), and the type-5 payload carries no
ZiSK public values — the contract reconstructs the binding digest from its own
pins. A range proved under one guest build therefore cannot verify against a
contract pinned to another. The same fixture wires a fixed Airbender verifier
address into `MultiProofVerifier`, so neither half of a type-5 payload is
version-routed.

**Before Required is enabled on a chain that will ever upgrade**, the rotation
procedure has to be agreed with era-contracts: specifically, what happens to
batches of the outgoing version that are committed but not yet proved when a new
`ZiskVerifier` is deployed. What follows from the constraint above is that the
second lane must be drained to zero unproved batches before the upgrade
transaction lands, unless the contracts gain a way to address the outgoing
verifier — either way an operational requirement the Airbender-only path never
had.

Until type-5 verification is version-routed on L1, a new ZiSK manifest must not
be activated merely by adding its protocol mapping. The outgoing lane must be
drained before the deployed verifier and server binary rotate together.

## Configuration

### Turning the second system on

```yaml
prover_input_generator:
  second_proof_system: true       # build the ZiSK witness, open ZiSK jobs
  zisk_shadow_execution: true     # local arbiter for a mismatch
  halt_on_zisk_commitment_mismatch: false
  multi_proof_verifier: false     # true = Required (see below)
prover_api:
  zisk_proof_verification_enabled: true
  zisk_aggregation:
    job_timeout: 600s
    verification_timeout: 60s      # native server-side verification
```

The proving identities are not operator configuration. They live in the
versioned manifests under `lib/zisk_prover_lane/manifests/` and changing them
requires a reviewed server binary.

### Required mode is validated fail-closed

`multi_proof_verifier: true` is rejected at startup unless all of the following
hold, because each of them would otherwise produce a node that stalls at the
commit gate on its first batch rather than settling half-proved:

- `second_proof_system: true` and `zisk_shadow_execution: true`;
- `enable_input_generation: true` — a fake prover input builds no ZiSK witness;
- `prover_api.enabled: true` — the API is the only route a ZiSK proof arrives by;
- `zisk_proof_verification_enabled: true`;
- the current L1 protocol maps to a compiled ZiSK manifest and the deployed
  `ZiskVerifier.verificationKeyHash()` matches it;
- both fake pools off — a fake Airbender proof cannot compose into a MultiProof;
- `1 ≤ commit_gate_admission_window ≤ 50`.

### Sizing

| Setting | Meaning |
|---|---|
| `prover_api.max_fris_per_snark` | Batches per SNARK range. The ZiSK aggregation range takes the same bounds. |
| `prover_api.max_assigned_batch_range` | How far ahead of the oldest unproved batch a prover may be assigned work. |
| `prover_api.commit_gate_admission_window` | Required only: how far the batch stage may run ahead of its oldest batch that is not yet proved by both systems. |
| `prover_api.fri_job_timeout`, `snark_job_timeout` | Lease timeouts before a job is re-offered. |
| `prover_api.zisk_aggregation.job_timeout` | Lease timeout for work performed by an external aggregation prover. |
| `prover_api.zisk_aggregation.verification_timeout` | Recovery timeout for native server-side verification after capacity is reserved. Semaphore queueing remains under `job_timeout`; expired verification attempts are re-offered and late results are ignored by generation. |
| `prover_api.proof_storage.*` | Disk caps for the FRI diagnostic archive. |

### Fake proving

`prover_api.fake_fri_provers` and `fake_snark_provers` run in-process pools that
produce dummy proofs, for local development and testnets without a GPU fleet.
A fake Airbender proof cannot be composed, so the range stage discards the
range's ZiSK state and settles Airbender-only — which is why Required refuses
the fake pools outright. There is no fake ZiSK prover; GPU-free tests exercise
the seal and batch-boundary halves of the ZiSK path over the fake Airbender
route.

## Bounds and what happens when they are hit

| Bound | Value | On overflow (Shadow) | Required |
|---|---|---|---|
| Active ZiSK jobs | `MAX_TOTAL_JOBS = 50` | new inputs park in a backlog | same |
| Parked backlog | `MAX_BACKLOG_ENTRIES`, `MAX_BACKLOG_AGE` | oldest/expired evicted, `coverage_lost` | never evicted |
| Buffered aggregation inputs | `MAX_BUFFERED_INPUTS = 64` | submission refused, input re-parked | same |
| Ranges awaiting inputs | `MAX_TRACKED_RANGES = 128` | lowest retired | never retired |
| Aggregates awaiting their SNARK | `MAX_COMPLETED = 16` | oldest dropped | never dropped |
| Deterministic failures | 3 attempts | job/range abandoned | kept, alarm re-raised |

In Required the admission window is what keeps all of these bounded: it caps how
many batches can be in the lane at once, so eviction is not needed to stay
within them.

## Observability

Lane metrics use the `zisk_lane` prefix. The ones worth alerting on:

- `zisk_lane_unprovable` — deterministic disagreement; in Required, commits are
  stalling.
- `zisk_lane_coverage_lost` — a sealed batch lost its ZiSK path. Shadow may shed
  coverage under its documented bounds; any increment in Required is an
  invariant violation and should page.
- `zisk_lane_vk_drift`, `zisk_lane_vadcop_vk_drift`,
  `zisk_lane_aggregated_vk_drift` — a prover is running a different guest build.
- `zisk_lane_commitment_mismatches`, `zisk_lane_wrong_result_submissions`.
- `zisk_lane_proof_verification_failures`,
  `zisk_lane_aggregated_proof_verification_failures`.
- `zisk_lane_aggregation_verification_timeouts` — native verification exceeded
  its recovery lease; sustained growth usually means the timeout is too short
  or the verifier is stuck.
- `zisk_lane_superseded_submissions` — a submission crossed a lease
  reassignment and its stale result was rejected.
- `zisk_lane_jobs_pending` / `_assigned` / `_backlog_entries`,
  `zisk_lane_oldest_job_age_seconds` — queue health.
- `zisk_lane_time_to_submit`, `zisk_lane_shadow_execution_time`.

`GET /prover-jobs/v1/status/` reports every lane's queue for operators.

## Known limitations

- The final aggregated PLONK proof is verified for wire form and public signals
  off-chain; its BN254 pairing is checked on L1. A well-shaped but
  pairing-invalid aggregate is therefore only caught at settlement.
- A ZiSK `pick` reports the Airbender verification-key hash, not a ZiSK key-set
  identity, and ignores the prover's declared versions, so a fleet running a
  specific ZiSK guest build cannot filter on either. Range formation does cut at
  protocol-version boundaries when the second lane runs, so a range never mixes
  ZiSK key sets. See [Protocol upgrades](#protocol-upgrades).
- A type-5 MultiProof is not version-routed on L1, so a chain in Required mode
  cannot settle two guest builds at once. See
  [Settling across an upgrade](#settling-across-an-upgrade-unresolved).
