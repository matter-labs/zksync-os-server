# Running consensus: new chains and migrations

## The two modes

`consensus.enabled = false` (the default) is the single-sequencer node, unchanged.
`consensus.enabled = true` makes the node one validator of a committee: block
production is driven by consensus leadership instead of a local loop, every block
is verified by re-execution before this node votes for it, and only finalized
blocks reach the write-ahead log. In either mode, exactly one node per chain runs
the batcher (settlement); every validator serves RPC and the external-node replay
stream.

Startup **guards** protect every transition described on this page: each one
refuses to start into a state that could mix chain histories, and each failure
message names its remedy. They are checked before the consensus thread spawns, so
misconfiguration fails the process, not a background task.

## Starting a committee on a new chain

Conceptually, four steps:

1. **Keys.** Each validator generates its two keypairs (network identity +
   consensus signing) with the `consensus-keygen` tool (`tools/consensus-keygen`).
2. **The committee list.** One entry per validator —
   `<network_key>:<bls_key>@<host:port>` — identical on every node
   (`consensus.validators`). The committee *is* this list; there is no discovery.
3. **Committee-uniform configuration.** Verification pins chain-level constants,
   so they must be configured identically everywhere — most notably the **fee
   collector address** (a proposal paying fees anywhere else is invalid; this is
   what stops a leader from redirecting fees to itself), gas and pubdata limits,
   and the consensus protocol version. Per-validator settings — keys, listen
   address, whether this node runs the batcher — naturally differ.
4. **Start everyone.** Validators launch concurrently; the chain begins once a
   quorum has paired up. Stragglers join late and catch up on their own — a
   committee never waits for its slowest member unless quorum demands it.

For a hands-on local version of all of this, the
[local consensus devnet](../setup/consensus_devnet.md) page generates keys,
configs, and a compose file in one command.

What to watch on a running validator: the `consensus` section of `/status`
(committee size, this validator's identity, the latest finalized round, the applied
height, and the certified watermark — the height up to which finality certificates
are durably stored), plus the consensus runtime's own metrics registry served at
`/status/consensus-metrics` as a second scrape target.

## The protocol version: deploy ≠ activate

`consensus.protocol_version` names the version of the consensus protocol the
committee speaks, and it is baked into the network's handshake and vote signing: a
validator configured with a different version **cannot pair** — it fails loudly at
the handshake and freezes where its disk left off, while a committee of `n ≥ 3f+1`
rides through it as one tolerated fault.

The operational consequence is a clean two-step upgrade discipline: *deploying* a
binary that supports a newer protocol version is always safe and can be rolled out
gradually (the binary keeps speaking the configured version); *activating* the new
version — flipping the config — is the coordinated step, done committee-wide.
Consensus messages cannot be negotiated per-connection the way ordinary p2p
protocols do it, for a structural reason: a finality certificate aggregates many
validators' signatures over one message encoding, so the protocol version is a
committee-wide fact, not a pairwise one.

## Migrating an existing chain into a committee

A chain that started life under a single sequencer can be migrated. The model:

- The committee's consensus genesis is **anchored** at an agreed cutover height H:
  a synthetic root block that *stands for* the chain's block H, derived identically
  by every validator from the pair (height, block hash). Its digest is the
  **consensus era** of the chain — a different cutover is a different era, and the
  guards make sure two eras can never mix.
- The first consensus-decided block is H+1. Nothing at or below H is re-agreed or
  re-executed; pre-consensus history is durable input, exactly like a fresh chain's
  genesis state.

One property is inherent rather than an implementation limit: **there must be a
moment when neither the old sequencer nor the committee is extending the chain** —
if both ran concurrently, each would produce its own block H+1 and the chain would
fork. So a migration has a sequencing gap by design. Reads don't stop: nodes keep
serving RPC from their synced state throughout; what pauses is transaction
inclusion.

The procedure:

1. **Prepare** (no downtime): generate validator keys, agree the committee
   configuration, and pre-stage chain-state snapshots on the future validator
   machines — pre-staging is what keeps the eventual gap short.
2. **Drain**: stop the sequencer. Its write-ahead log ends at some height H; that
   H becomes `consensus.genesis_height` in every validator's config. (H is read
   from the stopped node's database — an RPC height taken before the stop is not
   reliable, since the sequencer produces until the moment it winds down.)
3. **Distribute**: every validator needs the chain through H — the validated path
   is copying the drained node's databases (with pre-staged snapshots, only the
   tail moves during the gap, and the node's own startup replay rebuilds
   everything downstream of the write-ahead log). A machine that was running an
   external node already has compatible storage; it can be converted in place
   *provided its log ends exactly at H*, which is worth checking rather than
   assuming — once the sequencer stops, external nodes freeze wherever the replay
   stream left them.
4. **Cut over**: start the validators. The guards verify that this first start of
   the era happens *exactly* at H — a node that is behind is missing history; a
   log past H means someone kept sequencing past the agreed anchor — and record
   the era. Inclusion resumes the moment a quorum pairs up.

The batcher moves to exactly one validator (conventionally the ex-sequencer's
machine); settlement pauses during the gap and reconciles on startup through the
same recovery machinery every restart uses.

## Rolling back

Rollback — returning a committee-run chain to single-sequencer operation — is
always possible, because disk state on every validator is a prefix of the finalized
chain. Stop the validators, pick one node, and restart it with
`consensus.enabled = false` and `consensus.acknowledge_rollback = true`. The
acknowledgment flag exists because a chain that has consensus state refuses to
start single-sequencer without it — silently stranding consensus state invites
accidentally mixing histories later. Acknowledging **deletes nothing**: the
consensus state stays on disk, and the consensus era's blocks are ordinary chain
history to the resumed sequencer (their receipts and state effects serve
unchanged).

One subtlety, worth understanding rather than memorizing: **finality runs slightly
ahead of durability.** A block is finalized the instant a quorum's certificate
exists, but each validator writes it to its own log a moment later — so at the
instant a committee stops, one validator's log can end a block or two before
another's. Roll back from the validator with the **highest** write-ahead-log tip,
and treat the in-flight tail as lost: rollback is a disaster procedure, and a
few hundred milliseconds of blocks is its honest cost. (Settled blocks are never
in that gap — the batcher only settles blocks that reached a log.)

## Migrating again

A rolled-back chain can be migrated a second time at a new cutover. The era guard
makes the procedure deliberate: consensus refuses to start when its recorded era
does not match the configured anchor — *unless* the consensus engine state was
explicitly cleared, which is the documented re-migration step (the finality store
keeps the previous era's certificates either way; provable history is never the
thing you delete). A fresh era start must again land exactly on the new cutover
height.

## Current assumptions

Stated here so the page survives its own future: the validator set is **static**,
configured out of band, and changes arrive as coordinated configuration (per-epoch
committee rotation is the designed evolution and the machinery is shaped for it);
migration choreography is **manual with guards** — correctness never depends on
orchestration, which is exactly what makes automating it later a convenience
rather than a safety project; finality certificates are recorded and durable,
while *externally verifiable* finality (light clients checking certificates
without trusting a node) is the designed-for future the certificate and committee
formats already accommodate; and settlement remains a single-node concern —
consensus makes sequencing highly available, not (yet) the batcher.
