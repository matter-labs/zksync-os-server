# How consensus is wired into the node

## One seam: `ExecutionEnv`

Consensus and the node meet at exactly one trait, `ExecutionEnv` (defined in
`lib/consensus/core`). Everything consensus ever asks of the node goes through it:

```rust,ignore
trait ExecutionEnv {
    type Block;

    /// The agreed-upon chain root every validator derives identically.
    async fn genesis_block(&mut self) -> Self::Block;
    /// Leader path: produce a fully-executed block on top of `parent`.
    /// `None` means "nothing to propose" — the view times out, a routine event.
    async fn build(&mut self, parent: Self::Block, context: BuildContext) -> Option<Self::Block>;
    /// Follower path: decide whether to vouch for `block`, BEFORE voting.
    async fn verify(&mut self, parent: Self::Block, block: Self::Block) -> bool;
    /// A block is final: apply it durably. Height-ordered, at-least-once,
    /// must be idempotent; consensus waits for the ack before delivering more.
    async fn commit(&mut self, block: Self::Block);
    // ...plus startup and state-availability probes; see the trait docs.
}
```

The consensus core depends on this trait and on commonware — never on the
sequencer, storage, or networking crates. That single fact is what allows entire
committees of the *production* consensus stack to run inside deterministic
simulation tests: swap the environment, keep everything else.

The shape is deliberately the build/verify/commit contract of an Engine API, even
though there is no actual RPC boundary — it earns the same benefit (a swappable,
mockable execution side) without inventing a wire protocol between two halves of
one process.

## The other side of the seam

[The intro](intro.md#what-commonware-provides) described commonware's pieces
conceptually; here is the same machinery by the names you will navigate in code.
The layering, bottom to top:

- **`commonware_runtime`** — every component receives a *context* implementing the
  runtime traits (`Spawner` for tasks, `Clock` for time, `Storage` for durable
  partitions, `Metrics` for labeled registries). Consensus code never calls tokio
  directly; it spawns, sleeps, and persists through the context. This indirection
  is not ceremony — it is the exact property that lets the identical stack run
  under the deterministic test runtime, and it has one operational rule attached:
  any consensus task holding node resources must watch the context's `stopped()`
  signal, or it will outlive shutdown.
- **`commonware_p2p::authenticated::lookup`** — the committee network. "Lookup"
  means peers are known upfront (the configured address book — there is no
  discovery). Connections authenticate by ed25519 identity, traffic is multiplexed
  over numbered, rate-limited channels, and a `Blocker` lets higher layers ban
  peers caught misbehaving on the wire.
- **The simplex `Engine`** — the vote state machine, generic over a signing
  `Scheme` (ours: BLS12-381 multi-signatures) and an `Elector` (ours: round-robin).
  It speaks digests, signs votes, assembles certificates, and journals every vote
  before sending it. It drives whatever implements the **`Automaton`** trait:
  "give me a proposal for this view", "is this digest acceptable?".
- **The marshal (`Standard`)** — the block-level middle layer, composed of a
  `broadcast::buffered` engine (payload dissemination), a `resolver` (fetch
  protocol for backfill), and two **`Archive`s** (finalized blocks; finalization
  certificates). Marshal is what turns "consensus over digests" into "a chain of
  blocks delivered in order".
- **Our application** — implements commonware's **`Application`** and
  **`VerifyingApplication`** traits (build a block on a parent; verify a block
  given its ancestry) as a thin adapter that forwards to `ExecutionEnv`. One
  deliberate choice worth knowing when you go looking for "our `Automaton`":
  there isn't one. Marshal ships an `Inline` wrapper that implements the
  digest-level `Automaton` on top of a block-level application — handling payload
  fetching, ancestry resolution, and structural checks — and we use it as-is.
  Less hand-written consensus-critical code; the upstream wrapper is the tested
  path.
- **`Reporter`** — commonware's generic "stream of things that happened" trait,
  which our stack implements twice: the *committer* receives marshal's ordered
  finalized-block updates and acknowledges each after `commit` returns durably,
  and the *activity observer* receives the engine's full activity stream — every
  vote, certificate, and piece of fault evidence — feeding metrics, `/status`,
  and the finality store.

The composition root that assembles all of this — engine, marshal, channels,
reporters, epoch scoping — is `lib/consensus/core/src/stack.rs`, whose module
diagram is kept current with the code. The deterministic-runtime and simulated-p2p
halves of commonware belong to the [testing chapter](testing.md#the-testing-primitives-commonware-provides).

## Execute-then-order, verify-before-vote

Everything downstream in this node consumes *executed* blocks: the write-ahead log
stores replay records, external nodes sync by re-executing them, the batcher settles
them. Consensus keeps that model rather than inventing another:

- The **leader executes while building**. A proposal is a fully-executed block whose
  record commits to the execution outcome (`block_output_hash`).
- Every **follower re-executes the proposal before voting** and refuses unless its
  own outcome matches the declared one, bit for bit.

This is the load-bearing security decision of the integration. A proposer that lies
about execution results — the one attack that vote-counting alone can never catch —
is caught by every honest validator *before* finality, and its block simply never
gets votes. The alternative (vote on structure, execute after finality) would turn
the same attack into a finalized-but-invalid block: a chain halt and a manual
recovery, discovered at settlement after users were already told "final".

The cost is that execution sits on the vote path twice per view — once building,
once verifying. At current block sizes that is noise compared to networking;
overlapping execution with consensus (pipelining) is the known lever if a
latency-critical deployment ever needs it, and nothing in the design forecloses it.

## Speculative state

Verification means executing block H+1 before H+1 is final — sometimes before its
parent H is final either, and sometimes for two *competing* candidates at the same
height (see [notarized is not finalized](intro.md#simplex-in-five-minutes)). None
of that may touch the node's durable stores, which are strictly linear and hold
only finalized history.

The answer is a small in-memory **overlay tree** above the committed state
(`lib/consensus/execution`): each pending block's execution writes become one
overlay layer keyed by the block's digest and linked to its parent's layer. Reads
during build/verify traverse the branch down to the committed base. When a block
finalizes, its branch's outputs flow into the persistence pipeline and competing
layers are pruned; when a candidate is abandoned, its layer simply gets dropped.

Two invariants carry the design:

- **Nothing persists before finality.** Disk state is always a prefix of the
  finalized chain — which is also why rollback to single-sequencer operation is
  always possible from any validator's data.
- **Speculation is bounded.** If the persistence pipeline stalls, consensus keeps
  voting (a slow disk must not silence a validator) and overlays accumulate — up to
  a cap, past which the validator withholds votes until commits drain. Memory
  stays bounded; the committee rides through one member's pause.

## Validity rules: bounding the inputs

Re-execution proves a block's *outcome* matches its *inputs* — but it re-executes
whatever inputs the leader chose. A second layer of checks
(`lib/consensus/execution/src/rules.rs`) bounds the inputs themselves, so a leader
cannot smuggle content that executes fine and is still wrong for the chain.

Verdicts are three-way, and the distinction matters operationally:

- **Valid** — vote.
- **Invalid** — no future knowledge could make this block acceptable (wrong chain
  constants, a forged L1 transaction, a timestamp regression). Withhold the vote.
- **Withhold** — "I cannot vouch for this *yet*": typically an L1 input this
  validator's own watcher hasn't observed. Withhold the vote — but this is lag,
  not attack, and the two are labeled differently for operators.

Both negative verdicts have the same protocol effect: this round's vote is
withheld, the view times out, and a later proposal is judged fresh. That ceiling is
the point — **a leader whose inputs the committee won't accept costs the chain a
view timeout, never a halt.**

What the rules pin down, in families: committee-wide chain constants (chain id, gas
and pubdata limits, and — economically important — the fee collector address, so a
leader cannot redirect fees to itself); timestamps (never behind the parent, never
beyond the verifier's clock plus a configured skew); block size caps; fee inputs
within protocol bounds of the parent's; and the **authenticity of every L1-derived
input** — deposits and protocol upgrades must match, byte for byte, what this
validator's *own* L1 watcher saw, with strictly contiguous consumption cursors. A
committee never takes the leader's word for anything that originated outside the
chain. The rules file is the authoritative, tested list.

## The life of a block

The component-level version, start to finish. (The same journey told
chronologically — with the vote orderings, the failure scenarios, and epoch
turnover — is [the lifecycle chapter](lifecycle.md).)

1. A transaction reaches **any** validator's RPC and lands in its mempool. The
   committee gossips pending transactions to each other, so it does not matter
   which validator will lead next.
2. A view begins; its leader asks the execution side to **build**: transactions are
   selected from the local mempool and executed against the parent branch's
   speculative view, producing a fully-executed block. The block body is handed to
   the marshal for broadcast; its 32-byte digest goes into the engine.
3. Every other validator receives the proposal digest, fetches the body, and
   **verifies**: structural linkage, the validity rules, then full re-execution
   with outcome comparison. Only then does it vote.
4. Notarization, then finalization (or a nullified view and a fresh start — see
   [the protocol chapter](intro.md)).
5. The marshal delivers the finalized block — strictly in order, waiting for an
   ack — and **commit** hands the block's already-computed outputs to the node's
   persistence pipeline: write-ahead log first, then state, repositories, tree.
   The ack is durability-gated, so consensus never runs unboundedly ahead of disk.
6. Downstream is untouched: the batcher (running on exactly one validator) settles
   finalized blocks to L1 exactly as a single sequencer's would; external nodes
   pull the replay stream from whichever validator they like.

Alongside this flow, the activity reporter converts every finalization certificate
into the node's own format and stores it in the **finality store** (RocksDB, owned
by the node): certificates by block digest, a height index, and a *certified
watermark* — the highest height up to which every block's certificate is present,
surfaced in `/status`. The consensus library keeps its own archives too, but those
are treated as a rebuildable cache; the finality store is the durable proof trail,
deliberately independent of any library's encoding so that a dependency upgrade can
never strand the chain's evidence of finality.

## Crashes, restarts, late joiners

The recovery story is a composition of guarantees, each pinned by tests:

- **A validator cannot contradict itself.** Votes are journaled before they are
  sent; restart replays the journal.
- **Finalized delivery is at-least-once.** After a restart the marshal re-delivers
  from the durable height; `commit` is idempotent and treats re-delivery of an
  already-committed block as a no-op (and asserts it is *the same* block — a
  mismatch there would mean the one thing BFT exists to prevent).
- **The node's own pipeline recovers by replay.** On startup, the write-ahead-log
  range every downstream component still needs is re-executed through the pipeline
  before live consensus commits resume — each component picks up from its own
  watermark, the batcher strictest among them.
- **Lost speculation is rebuilt, not waited for.** A restarted validator has its
  durable chain but none of its speculative overlays. Verification walks a
  proposal's ancestry back to the first block whose state it holds and re-executes
  forward — so it can vote again promptly even about branches it never saw.
- **A brand-new validator backfills everything.** The marshal fetches missing
  blocks and certificates from peers; commits re-execute from the validator's own
  base. Catching up ends in *participation*, not just observation.

## Two worlds, four touchpoints

Consensus runs on its own OS thread with its own async runtime and its own
networking stack, deliberately isolated from the node's main runtime: consensus
must keep making progress — or failing loudly — independent of RPC load or pipeline
stalls. The two worlds touch in exactly four places: the execution environment,
the committed-payload channel into the persistence pipeline, the mempool (for
transaction gossip), and a death signal — if consensus dies, the node goes down
with it rather than serving a chain that stopped. The module documentation in
`node/bin/src/consensus.rs` is the wiring's source of truth.

## Durable bytes live in one place

Every encoding that outlives a process or crosses a trust boundary — the versioned
replay-record formats (shared with external-node sync), the consensus block
envelope (whose hash *is* a block's identity, and is therefore frozen), the
finality certificate format — lives in `lib/wire`, under one policy: **released
encodings are immutable; changes add a new version.** Golden tests pin every
released encoding byte-for-byte. Transient peer-to-peer messages, by contrast,
belong to their protocols and may evolve with them; what they need is version
*coordination* (the protocol-versioned handshake), not immortal encodings.

## What consensus does not change

The reassurance list, load-bearing enough to state explicitly: the write-ahead-log
format; external-node sync (any validator serves the same replay stream a single
sequencer would); the batcher and settlement pipeline (one batcher-enabled node,
reading finalized blocks); the RPC surface (plus a `consensus` section in
`/status`); and single-sequencer mode itself, which remains the default and the
[rollback path](enabling.md#rolling-back).

## Where the code lives

- `lib/consensus/core` — the stack composition: engine + marshal wiring, the
  application adapter, the `ExecutionEnv` trait, the committer.
- `lib/consensus/execution` — the node-side environment: speculative state,
  validity rules, the block builder, the finality store.
- `lib/consensus/sim` — the deterministic simulation harness and scenario corpus
  ([next chapter](testing.md)).
- `lib/wire` — the durable encodings and their golden corpus.
- `node/bin/src/consensus.rs` and the composition root in `node/bin/src/lib.rs` —
  configuration, keys, startup guards, thread spawn, observability surfaces.

Each crate's module documentation carries the detail this page deliberately
doesn't.
