# Running consensus: new chains and migrations

## The two modes

`consensus.enabled = false` (the default) is the single-sequencer node, unchanged.
`consensus.enabled = true` puts the node on the consensus network, in one of two
roles (`consensus.role`): a **validator** — block production is driven by
consensus leadership instead of a local loop, every block is verified by
re-execution before this node votes for it, and only finalized blocks reach the
write-ahead log — or an **observer**, which follows the same chain through the
same machinery without ever voting (see "Observers" below). In either mode,
exactly one node per chain runs the batcher (settlement); every consensus node
serves RPC and the external-node replay stream.

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
   (`validators` is shorthand for a single-entry committee *schedule*; a set that
   will change over time uses `consensus.committees` — see
   [changing the committee](#changing-the-committee).)
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
committee-wide fact, not a pairwise one. The step-by-step activation runbook —
including the deliberate halt window and how to track flip progress via the chain
fingerprint — is ["Changing the consensus protocol version (flag
day)"](operating.md#changing-the-consensus-protocol-version-flag-day).

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

## Observers

An observer is a node on the consensus network that never votes: it holds no BLS
key, appears in no committee schedule, and runs no consensus engines — but it
receives gossiped blocks, verifies every finality certificate against the
committee schedule, and applies finalized blocks through the same commit path a
validator uses. The trust improvement over a replay-fetching external node is the
point: an observer accepts a block because a quorum of the committee signed it,
not because one serving node said so. Its operator pins the committee schedule,
not a URL.

Configuration, on top of the committee schedule every consensus node carries:

- `consensus.role = observer`, a network key, and **no** `consensus.bls_key`
  (configuring one is refused — a key that exists but never signs invites the
  wrong conclusions).
- `consensus.observers`: the admission list — `<ed25519_hex>@<host:port>` per
  observer, configured identically on **every** node. The consensus network only
  completes handshakes with explicitly listed identities, so this list is the
  observers' admission perimeter; an observer must find its own identity in it.
  Observers hold no committee power — the worst an admitted one can do is consume
  resources, which the network's rate quotas bound.
- `consensus.tx_forward_rpc_urls`: validator RPC urls. An observer has no leader
  turns, so a transaction submitted to its RPC is kept as a local mirror (pending
  views stay coherent) and forwarded round-robin to a validator, which gossips it
  to whoever leads next.

Two guards keep the roles honest: an observer whose network key is scheduled into
a committee refuses to start (the committee would wait for votes that never
come), and a validator missing its signing key fails loudly instead of silently
downgrading to following. `/status` reports the role.

## Changing the committee

The validator set is fixed within an **epoch** (a fixed number of blocks —
`consensus.epoch_length`, hours-scale by default) and changes only at epoch
boundaries, driven by the **committee schedule**:

```yaml
consensus:
  committees:
    - activation_epoch: 0
      validators: [ <entry>, <entry>, <entry> ]
    - activation_epoch: 120
      validators: [ <entry>, <entry>, <entry>, <entry> ]
```

Each entry holds from its activation epoch until a later entry supersedes it; the
first entry must activate at epoch 0 so every epoch in history resolves to a
committee (backfilled certificates stay verifiable forever). Reconfiguration is an
**append**: every operator deploys a config with the new entry *before* its
activation epoch arrives — pick an activation comfortably in the future, roll the
config out validator by validator (a restart per validator; the committee rides
through each as one tolerated fault), and the chain crosses the boundary into the
new committee with no further coordination. The handoff itself is protocol-level:
the new committee's first act is re-certifying the old committee's final block.

The choreography per direction:

- **Growing**: start the new validator's node *first* (deploy-then-activate). Its
  key is in a future entry, so from day one it is in every member's address book,
  follows the chain as an observer, and simply starts voting when its epoch
  arrives. Startup on the new machine follows the same distribute-state paths as
  a migration (snapshot copy, or sync from genesis for a young chain).
- **Shrinking**: the excluded validator needs nothing at its boundary — it stops
  building consensus engines for epochs it is not scheduled into, but keeps
  following the chain as an observer (it still verifies finality certificates and
  serves RPC from its growing history). For a machine that should keep following
  deliberately, restart it as `consensus.role = observer` (with its entry moved
  to the admission list); otherwise repoint it as an external node at leisure.
  The `acknowledge_non_member` flag covers the tail case of restarting a machine
  whose key has left the schedule entirely.
- **Promoting an observer** (the growing case, starting from a node that already
  runs — the intended path for turning a chain's follower fleet into its
  committee):
  1. *Keys.* The candidate generates its BLS signing keypair (`consensus-keygen`);
     its network identity already exists. Distribute the resulting committee
     entry (`<network_key>:<bls_key>@<host:port>`) to every operator.
  2. *Schedule.* Every sitting validator restarts with the appended entry —
     activation epoch comfortably in the future — and with the candidate removed
     from `consensus.observers` (a key may not be both; the candidate stays
     connectable throughout, because the address book spans every schedule
     entry, future ones included).
  3. *The flip.* The candidate restarts with `consensus.role = validator`, its
     BLS key, and the same appended schedule — over its retained chain **and**
     its retained consensus archives from observing; nothing is resynced.
     Before the boundary, check readiness on its `/status`: role `validator`,
     the finalized round advancing.
  4. *The boundary needs nobody.* When the activation epoch arrives, the
     rotation starts the candidate's first consensus engine because the schedule
     now says "member" — everything special happened in steps 1–3. If the
     candidate is late (still restarting, still catching up), the committee
     runs one member short until it arrives; late first engines are safe.

  Rolling back a promotion that hasn't activated yet is a no-op (ship a schedule
  without the entry); after activation it is the ordinary shrinking path. If the
  candidate ever voted under a wrong schedule, the misconfiguration remedy below
  applies — made cheap by the finality floor.

Every node records a **custody trail** as it observes consensus enter each epoch:
which committee held it, from which block — kept in the node's own finality store
next to the certificates, so the chain's committee history is reconstructible
from durable data alone, independent of any config file's current contents.

Two sharp edges, both loud by design:

- The schedule is a committee-wide constant. A validator whose config is missing
  the newest entry crosses the boundary on the old committee: it cannot verify
  the real committee's certificates, falls behind, and (over real p2p) bans the
  peers it can no longer understand — disrupting nobody but itself. **The remedy
  is a rebuild, not an in-place restart**: consensus vote journals index signers
  by committee position, so votes journaled under the wrong committee do not
  replay under the corrected one (the engine refuses, loudly). Deploy the
  corrected config with a fresh **consensus** data directory and let the node
  re-bootstrap from its peers. The chain itself does not need rebuilding: with
  its chain state retained, the restart resumes from a cached finality floor —
  the node keeps a small cache of recent finalizations exactly for this — and
  backfills only what lies above it, instead of replaying consensus history
  from the era genesis. The floor must be recent (at or after the committee's
  last scheduled change); an older one falls back to the full backfill with a
  warning, unless `consensus.accept_stale_floor` says otherwise. A validator
  that stalled at a committee change may be exactly that case — everything
  *usable* it verified predates the change — so set the flag for the corrected
  restart (harmless when the floor turns out fresh) and drop it afterwards.
- Epoch length is likewise committee-uniform. Hours-scale is the deliberate
  default: a reconfiguration deployed in the morning activates the same day,
  while boundary handoffs (one re-proposal view each) stay rare events.

## Storage retention

Consensus storage grows with the chain: every epoch's engine journals its votes
under its own partition, and marshal archives every finalized block and
certificate. `consensus.epoch_retention` bounds this: once an epoch falls that
many epochs behind the live one, its vote journal is removed and the finalized
archives are pruned below its start. `0` disables pruning; values below 2 are
refused (the window must cover the epoch handoff and the finality-floor cache).

Retention is a **per-node** choice — pruning local storage needs no committee
coordination. Its network-wide consequence does deserve a deliberate decision,
though: once every peer has pruned, chain history below everyone's horizon is
simply not served anymore. A consensus rebuild still converges (the node picks
up live finality and syncs forward, ending with a bounded recent window rather
than the full chain) — but *rejoining the committee* then requires starting
from a finality floor, because the epoch anchor blocks that engines otherwise
start from are gone. The floor comes from the node's own finality store, which
is exactly why that store is never pruned: certificates there are the permanent
proof trail, and the floors for every future rebuild.

> Day-two operations — the alarm table, timing characteristics, and the
> incident playbook — live in [Operating a committee](operating.md).

## The committee as the batch-verification set

Batch verification ("2FA") gates every L1 commit on threshold co-signatures
from independent verifiers — and a consensus committee already *is* that set:
every validator re-executed every block before voting for it, so co-signing a
batch is a recomputation of the commit metadata from its own finalized data,
not a second execution. No separate verifier fleet, and each signature attests
against the signer's independent BFT finality rather than a chain synced from
the node being checked.

To enable it, on **every validator**:

- `network.enabled = true`, with a stable `network.secret_key` and
  `network.boot_nodes` listing the other validators — the committee meshes on
  the zks network, and batch-verification traffic rides those sessions.
  Whoever currently settles collects from its own peers, so the verifier set
  follows a settlement failover with no reconfiguration.
- `batch_verification.client_enabled = true` with a per-validator
  `signing_key`; `server_enabled = true` and the shared `threshold` +
  `accepted_signers` (or the on-chain verifier config, which takes precedence
  when set). Uniform configuration means any promoted standby collects under
  the same policy.

**The threshold rule**: the settler never co-signs its own batches, so the
threshold must be reachable from standbys alone — with `n` validators
tolerating `f` faults, `threshold ≤ n − 1 − f` keeps settlement live through
a failover (n=4, f=1 → threshold 2). A threshold the standbys cannot meet
does not bypass verification; settlement stalls until signatures arrive,
which is the designed failure direction.

## Idle chains

A quiet chain does not fill with empty blocks. With `consensus.idle_heartbeat`
set (the default is 10 minutes), a leader whose mempool is empty passes its
turn — consensus nullifies the view and rotates, and no block is made — until
one of two things happens:

- **Work arrives.** A transaction (or an L1 priority operation picked up by
  the L1 watcher) produces a block within a leader timeout or two. Idle never
  delays real traffic.
- **The heartbeat interval passes.** The leader seals one empty block. This
  pulse bounds everything that anchors to chain progress — consensus journal
  pruning, fee-clamp staleness, the batcher's settlement cadence — and gives
  monitoring a clean rule: on a healthy chain, *no block for longer than the
  heartbeat interval plus a margin is always an alarm*.

The exception is a pending committee change: while a `consensus.committees`
entry has not reached its activation epoch, idle leaders keep producing empty
blocks at full cadence ("sprint") so the change activates without traffic —
epochs are height-driven, and a scheduled rotation must not wait for
transactions. The sprint stops at the boundary. This bounds committee-change
latency on an idle chain by `epoch_length × block time`, which is one of the
inputs to choosing `epoch_length`: on chains expected to idle, a smaller
epoch length (with a correspondingly larger `consensus.epoch_retention`, since
retention windows are epoch-denominated) keeps emergency rotations fast.

Setting `idle_heartbeat: 0s` disables the policy: idle leaders always build,
and a quiet chain seals empty blocks around the clock at the block time.

The policy is leader-local — blocks and passed turns both verify, so
validators with differing settings interoperate — but configure it uniformly
so the chain's cadence is predictable. The heartbeat also sets the floor on
settlement activity: each pulse eventually rides to L1 through the batcher,
which is the deliberate cost of keeping the pipeline warm and observable.

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

Stated here so the page survives its own future: the validator set changes by
**configured schedule** — operators deploy matching schedules out of band, and no
on-chain registry or in-band vote authorizes a change (the custody records make
the history auditable; a registry is the designed evolution); migration and
reconfiguration choreography is **manual with guards** — correctness never
depends on orchestration, which is exactly what makes automating it later a
convenience rather than a safety project; finality certificates are recorded and
durable, while *externally verifiable* finality (light clients checking
certificates without trusting a node) is the designed-for future the certificate
and committee formats already accommodate; and settlement remains a single-node
concern — consensus makes sequencing highly available, not (yet) the batcher.
