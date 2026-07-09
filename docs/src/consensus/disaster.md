# Disaster recovery

BFT finality and settled finality are two different promises. The committee
promises that a finalized block will never be contradicted; the proving system
promises that only correct state transitions settle to L1. They are kept by
different machinery, and there is exactly one crack between them: every
validator verifies proposals by re-executing the **same** state-transition
implementation, so committee agreement is replication, not diversity — it
catches a dishonest proposer, never a shared defect. The proving stack is the
one independent implementation in the loop, and it renders its verdict at
settlement time, after users were already told "final".

This page is the taxonomy of "finalized but will not settle", with a remedy
per class, ordered by what the remedy costs — ending at the **disaster
hardfork**, the mechanism of last resort. All of it is rare by construction
(the [testing strategy](testing.md) exists to keep it that way); the page
exists so that if one of these ever fires, the response is a runbook rather
than an invention.

## Reading the failure

The presenting symptom is almost always the same — settlement lag (see
[the alarm table](operating.md#the-alarm-table)) with prover-side failures —
so triage starts with one question: **is the chain's state wrong, or is it
merely unprovable as packaged?** Independent re-execution of the affected
range answers it, and the answer selects the class:

| Class | The state is… | Remedy | The remedy costs |
| --- | --- | --- | --- |
| Unprovable batch (resource or prover defect) | correct | re-cut / re-prove; revert-and-recommit on L1 | time; nothing else |
| Verifier rejects a correct outcome | correct | verifier fix via protocol upgrade | a settlement pause, governance-paced |
| State deviates from the spec, tolerably | wrong, but acceptable | ratify the deviation | trust, spent deliberately |
| State deviates from the spec, unacceptably | wrong, unacceptable | the disaster hardfork | abandoning finalized blocks |

The first two never touch history. The judgment between the last two —
whether a deviation can be ratified or must be excised — is a governance
decision, not a computation; what the system provides is a safe mechanical
path for either answer.

## The remedies that never touch history

**An unprovable batch** (resource exhaustion, a prover defect): the chain's
state is fine; a batch just cannot be proven as cut. Batch boundaries are not
consensus-fixed — uncommitted batches can be re-cut freely, and
committed-but-unexecuted batches can be reverted on L1 and re-committed in a
different shape (commit discovery tolerates superseded commitments as a matter
of course). Fix or re-shape, re-prove, move on. No governance, no finality
impact, no operator ceremony beyond the batcher's own tooling.

**A verifier that rejects a correct outcome**: the chain's state stands;
settlement waits for a verifier fix, shipped as an ordinary (if urgent)
protocol upgrade. Sequencing continues throughout — the chain does not stop
because settlement paused — and deposits keep flowing (they ride finalized L1,
not our batches). Withdrawals wait with settlement. The cost is a pause,
paced by governance.

## Ratifying a tolerable deviation

When re-execution shows the chain's actual history deviates from the spec but
the deviation is judged acceptable, the remedy is to **ratify** it: an
emergency protocol upgrade, authorized through the security council, that
allows the affected batch range to settle. History — the thing users hold
receipts against — stands; the spec is amended to acknowledge what the chain
actually did, and the state-transition fix ships alongside so the deviation
stays bounded to its range. This spends trust, deliberately and visibly, which
is exactly why it sits behind governance rather than behind a knob. (A
pre-armed, faster authorization path is a designed-for evolution; what exists
today is the deliberate one.)

The permanent record matters as much as the remedy: a ratified deviation
becomes part of the chain's definition — "the spec, plus the ratified
deviations at their recorded heights" — and the postmortem documents it as
such.

## The disaster hardfork

When the deviation cannot be ratified, the remaining honest exit is to discard
the affected suffix and re-execute: an **operator-coordinated era change**
that abandons finalized blocks above an agreed height N. Three properties are
constitutional, in the sense that the machinery enforces them rather than
trusting anyone to remember:

- **Operators fork; keys cannot.** There is no in-protocol path by which
  validator keys can revert finality — a fork is configuration, deployed out
  of band. The arithmetic enforces the "honest majority agrees" requirement
  on its own: a new era adopted by a quorum produces a live chain, stragglers
  on the dead era disrupt only themselves, and a fork adopted by less than a
  quorum produces no live chain on either side. A mandatory
  `consensus.protocol_version` bump partitions the network at the handshake,
  so forked and un-forked nodes cannot exchange a single interpretable byte.
- **The fork is loud, forever.** The finality store is never touched: the
  abandoned era's certificates remain on every node as the permanent,
  auditable record that finalized blocks were overridden, by whom, and from
  where. The truncation tool additionally exports every discarded block to a
  **tombstone archive** (released wire encoding, plus a manifest naming each
  block, its hash, and its consensus digest) — the raw material for the
  postmortem and for any external consumer that acted on the old tip.
- **The floor is L1-executed state.** N must be at or above the last block of
  the last batch *executed* on L1, and the tool verifies that floor against
  L1 directly — never against local watermarks, which can lag. Irreversibility
  has three tiers, and the fork lives strictly in the first:
  **L2-finalized** (revertible only by this procedure) ⊂ **L1-committed**
  (revertible by the ordinary batch-revert path — the settler's job) ⊂
  **L1-executed** (irreversible for everyone; reversing it would be an L1
  emergency upgrade, a different document).

Jurisdiction follows from the tiers: **L1 first, consensus second.** Reverting
committed batches and guaranteeing nothing is re-committed meanwhile is the
settler side of the runbook, done and verified before anyone touches an era.
A backstop guard enforces the ordering mechanically — a settler restarted
while L1's committed range extends past its local chain refuses to start,
naming the revert step — but the backstop exists to catch a runbook mistake,
not to perform the revert.

### The runbook

1. **Decide.** Governance declares the suffix above N abandoned. N must sit at
   or above the L1-executed floor and at or below every participating node's
   chain tip (compare tips while halted; a node whose chain ends below N
   needs no truncation — it catches up to exactly N after the fork, since
   pre-fork blocks are identical in both eras).
2. **Stop the settler first, and hold it down.** Its recovery machinery would
   faithfully recreate and re-commit the discarded batches — by design.
3. **Halt the rest of the committee**, observers included.
4. **Revert on L1**: all committed-but-unexecuted batches above N, using the
   settler's operator keys. Verify the diamond's counters before proceeding.
5. **Truncate every node** whose chain extends past N:
   `zksync-os-server truncate-to --to-block <N>`. The tool refuses on a
   running node, below the L1 floor, or on a non-`FullDiffs` state backend
   (the compacted backend cannot replay below its compaction start); it
   exports the tombstone before cutting, truncates the write-ahead log, the
   state diffs, and the repositories, and leaves the finality store, the
   merkle tree (it rewinds itself on replay), and preimages alone. Interrupted
   runs re-run safely — every cut is idempotent.
6. **Cross-check the anchor.** Every node's tombstone manifest reports
   `hash_at_truncation_point`; they must all agree (they will, if every
   truncation landed on the same chain — a disagreeing node skipped a step
   and must not proceed).
7. **Clear the consensus engine state** directory on every validator and
   observer — the era guard requires a deliberate clear, exactly as in a
   re-migration.
8. **Deploy the fork configuration**: `consensus.genesis_height = N`, a bumped
   `consensus.protocol_version`, the committee schedule re-anchored at epoch 0
   of the new era, and the acknowledgment —
   `consensus.acknowledge_fork = "<N>:<block hash at N>"`. The era guard
   refuses to override a recorded era without it, refuses an acknowledgment
   whose height or hash does not match the node's own chain at the anchor,
   and refuses a chain that does not end exactly at N. (On a registry-governed
   chain, the fork schedule is a config entry — config takes precedence over
   the registry by design; realign governance's on-chain schedule afterwards.)
9. **Roll the patched state-transition binary** to every node *before* you
   restart it. Re-execution from N replays the discarded window's transactions
   against the STF again — under the *unpatched* binary that can re-include the
   very deviation the fork exists to expel. The fix that motivated the fork is
   a precondition of the restart, not a follow-up. (The tombstoned suffix
   itself is never replayed into the new era; it is only mined for the
   postmortem.)
10. **Restart the validators** and verify: uniform chain fingerprints, blocks
    finalizing from N+1, hash agreement across nodes. **Restart the settler
    last**, once the L1 revert is verified, and watch the re-executed batches
    commit and settle.
11. **The postmortem.** The tombstone manifests name every abandoned block for
    every external consumer of the old tip; the finality store holds both
    eras' certificates; the custody trail records the era transition.

### What survives, on purpose

Chain data at and below N; the finality store in full (certificates are
permanent and digest-keyed across eras; epoch-keyed records — custody,
registry derivations, floors — are era-scoped, so the new era starts its own
trail without colliding with the dead one); preimages; the tombstone. What a
fork costs, it costs in the open.

### Rehearsal

The choreography is executable, not aspirational: the simulation corpus pins
the fork's arithmetic (`lib/consensus/sim/tests/fork_back.rs` — a quorum fork
lives, below-quorum adoption freezes both eras, and a real-execution fork
re-converges to identical committed state), and the integration drill runs the
full runbook against real nodes, real storage, and a real L1, including the
settler backstop (`integration-tests/tests/node/fork.rs`). Run those whenever
you change the tooling.

Rehearse by hand once per operator generation — the difference between a runbook
and a legend — on the [local consensus devnet](../setup/consensus_devnet.md) (a
real multi-validator committee over a local L1; the devnet page has the
bring-up). The steps below are this runbook's *how* on that cluster's compose
tooling. The generated cluster makes one node the settler (`validator-0`, with
`batcher.enabled: true`); the rest sequence only — that is the node this runbook
stops first and restarts last.

**1. Pick the anchor.** Stop any load so the chain goes idle. Choose `N` at or
below the current tip. It must also sit at or above the last L1-executed block —
but you do not have to measure that by hand: the tool reads the executed-batch
floor from L1 directly and refuses (naming the floor) if `N` is below it, so if a
choice is rejected, nudge `N` up. On a drained, idle devnet everything committed
is already executed, which is exactly the motivating case (the poisoned suffix was
never executed).

**2. Stop the committee, settler first.** A live settler would recreate and
re-commit the very batches the fork discards:

```sh
docker stop chaos-validator-0            # the settler, held down
docker stop chaos-validator-1 chaos-validator-2 chaos-validator-3
```

Leave `chaos-anvil` running — the truncation tool reads the executed-batch floor
from it.

**3. Truncate every node to N.** Run the `truncate-to` subcommand as a one-off
against each stopped validator's volume, reusing the same `--config` flags the
service runs with (copy them from the `command:` line in
`./devnet/docker-compose.yaml`; the middle chain path is setup-specific):

```sh
for i in 0 1 2 3; do
  docker compose -f ./devnet/docker-compose.yaml run --rm validator-$i \
    --config /app/local-chains/local_dev.yaml \
    --config /app/<chain>/config.yaml \
    --config /config/validator.yaml \
    truncate-to --to-block <N>
done
```

Each run exports the discarded blocks to `/db/tombstone-<N>/` in that validator's
volume, then cuts the write-ahead log, state, and repositories back to `N`. It
leaves the finality store alone — the old era's certificates are the permanent
record of what was overridden.

**4. Confirm the anchor hash.** Every validator's tombstone manifest records
`hash_at_truncation_point`; they must all agree (a disagreement means a node
skipped a step and must not proceed). That hash is what the fork config
acknowledges:

```sh
docker compose -f ./devnet/docker-compose.yaml run --rm --no-deps --entrypoint sh \
  validator-0 -c 'cat /db/tombstone-<N>/manifest.json' | jq .hash_at_truncation_point
```

**5. Deploy the fork config and clear engine state.** For each validator, edit its
host-side overlay `./devnet/validator-<i>/validator.yaml`, adding under
`consensus:` the new anchor, a bumped protocol version, and the acknowledgment:

```yaml
consensus:
  # ... existing keys ...
  genesis_height: <N>
  protocol_version: 2
  acknowledge_fork: "<N>:<hash at N>"
```

Then clear each validator's consensus **engine** state (the tool deliberately
leaves it; the era guard refuses to start into a new anchor over stale engine
state — and the finality store beside it, with the old era's certificates, stays
put):

```sh
for i in 0 1 2 3; do
  docker compose -f ./devnet/docker-compose.yaml run --rm --no-deps --entrypoint sh \
    validator-$i -c 'rm -rf /db/consensus'
done
```

**6. Restart — settler last.** Bring the sequencing validators back first and
watch `/status`: consensus advancing, and chain fingerprints agreeing across
nodes from `N+1`. Then restart the settler:

```sh
docker start chaos-validator-1 chaos-validator-2 chaos-validator-3
docker start chaos-validator-0
```

If the settler refuses to start, naming committed batches past its chain, then L1
still holds committed-but-unexecuted batches above `N`: revert them first (the
L1-revert step above — the `L1Revert` rebuild mode) and start it again. On a fully
drained devnet there are none, so the backstop stays quiet and this step is the
no-op it should be.

## When the machine itself is suspect

Everything above assumes consensus did its job — the blocks were finalized
correctly by a working protocol, and the *state* was the problem. A defect in
the protocol, the consensus library, or our integration is not an operational
scenario with a runbook; it is a development risk, and its defenses are
structural: the [testing strategy](testing.md) and the
[upgrade discipline](upgrading.md) with its findings register. If such a
defect ever produced an unacceptable chain state anyway, the machinery above
still applies — a fork does not care *why* the suffix is poisoned — but the
fix belongs to development, not operations.
