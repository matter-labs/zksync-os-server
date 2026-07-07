# Operating a committee

What to watch, what it means, and where the remedy is. Everything here uses
signals the node already exports (`/status` and the prometheus endpoint) and
points into ["Running it"](enabling.md) for the procedures.

## The alarm table

On a healthy committee, every row's condition holds on every validator. Each
alarm names the first place to look.

| Signal | Healthy | Alarm means | First response |
| --- | --- | --- | --- |
| `consensus_verify_verdicts{verdict="invalid"}` | never increments | a peer proposed a block this validator *permanently* rejects — failed linkage, validity rules, or re-execution mismatch. On an honest committee this is the operational signature of a state-transition divergence (e.g. an upgrade that changed execution) or a byzantine leader | correlate the proposer via logs ("block failed re-execution; rejecting"); halt any rollout in progress; a diverging validator must be rebuilt from a floor after the cause is fixed |
| `consensus_verify_verdicts{verdict="withhold"}` | increments only around restarts / L1 lag / clock skew | persistent growth means this validator cannot vouch for proposals — usually its L1 view lags (priority-op authenticity checks), commits are backlogged, or a proposer's timestamps outrun this validator's clock (NTP drift on either side) | check the L1 provider, the `speculative state at capacity` log line, and clock sync; the condition clears itself when the cause does |
| `consensus_activity{...conflicting_notarize / conflicting_finalize / nullify_finalize}` | zero, always | protocol fault evidence: a committee member equivocated | treat as compromise of that validator's key; schedule it out of the committee (["Changing the committee"](enabling.md#changing-the-committee)) |
| block staleness (`eth_getBlockByNumber("latest")` age, or `/status` `finalized.observed_unix`) | never older than `consensus.idle_heartbeat` plus a couple of leader timeouts | the chain stopped: quorum loss, or every leader declines. The heartbeat makes this rule sharp — an idle chain still pulses | count reachable validators vs quorum; a below-quorum committee needs nodes restored (["Rolling back"](enabling.md#rolling-back) is the last resort) |
| `/status` `finality_certified_height` | tracks `applied_height` within a small tail | the finality trail stopped being provable: certificates are not being stored. After a restart it briefly trails, then one live certificate covers the gap — a *persistent* stall means the certificate stream itself is broken | check the activity-observer logs ("failed to persist a finality certificate") and disk space |
| `repositories_persistence_lag` | ~0 | the block-persist loop is falling behind memory; a crash while behind loses the unpersisted window | check disk throughput; falling behind persistently is a capacity problem |
| `jemalloc_allocated_bytes` | grows with write load, bounded by the RocksDB write-buffer plateau | unbounded growth is a leak; the known grower is RocksDB memtables (node-team item) | compare against the documented baseline before suspecting consensus |
| RPC admission (`"pipeline backpressure"` rejections) | brief bursts under heavy load | sustained refusal means proof generation cannot keep up with the block rate | a capacity signal, not a fault: reduce load or scale provers |
| `/status` `consensus.chain_fingerprint` | identical on every node (also logged once at startup: "committee-uniform configuration fingerprint") | two nodes disagree on a committee-uniform fact — schedule, epoch geometry, consensus timing, or a verification-pinned chain constant. Symptoms otherwise surface later and expensively: a stall at the next epoch boundary, or false byzantine alarms | diff the drifted node's config against a healthy one *before* the next epoch boundary; the fingerprinted surface is enumerated in `node/bin/src/chain_fingerprint.rs` |

Three structural notes. First, all `consensus_*` counters reset on process
restart — alert on increases, not absolute values. Second, a *stopped*
validator alarms on nothing by itself: it is the surviving committee's view
(quorum arithmetic, staleness) that tells you whether the loss matters.
Third, **an idle chain is not a quiet chain**: between heartbeats, leaders
decline their turns and consensus does what it does with a silent leader —
views time out, nullifications assemble, leaders rotate, about once per
leader timeout, forever. Dashboards will show steady view/nullification
counters ticking on a chain producing no blocks; that is the designed idle
behavior, not distress. The signal that matters stays block staleness
against the heartbeat, not view churn.

## Timing characteristics worth knowing by heart

Logged once at consensus startup, and derivable from config:

- **Epoch duration under load** ≈ `epoch_length × block time`. This is the
  granularity of committee changes and of consensus-storage retention.
- **Emergency rotation on an idle chain** ≤ epoch duration: a deployed
  schedule entry makes idle leaders sprint to the boundary at full cadence
  (["Idle chains"](enabling.md#idle-chains)).
- **Catch-up window under load** ≈ `epoch_retention × epoch duration`: a
  validator down longer than this cannot catch up from peers' retained
  consensus storage and needs a floor restart (["Storage
  retention"](enabling.md#storage-retention)). Idle chains stretch this
  window enormously — retention is epoch-denominated and idle epochs take
  wall-clock ages.
- **Deposit latency** ≈ L1 finality (~13 minutes on Ethereum, two epochs):
  under consensus, deposits and protocol upgrades are ingested only from
  *finalized* L1 blocks. This is deliberate, not a knob: every validator
  verifies included L1 content against its own L1 view before voting, and a
  BFT-finalized block is irrevocable — the deep-reorg remedy a single
  sequencer had (roll back and re-sequence) no longer exists. The decision
  note lives where the mempool's watchers are wired in `node/bin/src/lib.rs`.

Chains expected to idle should size `epoch_length` small enough that the
sprint bound is acceptable for incident response, and raise `epoch_retention`
to keep the loaded catch-up window sane; the journals are small either way.

## Changing the consensus protocol version (flag day)

`consensus.protocol_version` names the domain-separation namespace everything
on the consensus network signs and speaks (`zksync-os-consensus/{version}`).
It cannot be negotiated per connection — a finality certificate aggregates
signatures over one message encoding, so the whole committee speaks exactly
one version per round. Two versions do not interoperate *by design*: a
mismatched validator fails at the p2p handshake, loudly, instead of
exchanging messages it might misinterpret. That makes bumping it a **flag
day**: a coordinated, briefly chain-halting activation. The choreography:

1. **Deploy first, flip later.** Roll the new binary across the committee at
   normal operational pace, with `consensus.protocol_version` unchanged.
   Deploying a binary that *supports* a new version and *activating* that
   version are separate steps; rolling restarts are routine (each validator
   catches up on restart).
2. **Verify the committee is uniform** before the flip: every node serves the
   same `/status` `consensus.chain_fingerprint` (the protocol version is part
   of the fingerprinted surface, so a half-flipped committee shows two
   fingerprints — the drift alarm doubles as flag-day progress tracking).
3. **Flip together.** At the agreed time, restart every validator with the
   bumped `consensus.protocol_version`. Between the moment the old-version
   side drops below quorum and the moment the new-version side reaches it,
   the chain is deliberately halted — nothing is wrong, and no operator
   action is needed beyond completing the restarts. Sequence the flip like
   any quorum-sensitive operation: one node at a time is fine, the halt just
   lasts until 2f+1 nodes are on the new version.
4. **Confirm** all fingerprints match again and blocks finalize. Finalized
   history is untouched — certificates already stored remain valid, and the
   node's own persisted encodings are version-tagged independently of the
   network namespace.
5. **Rolling back**: safe by symmetry (flip the value back the same way) as
   long as it happens before the new-version committee finalizes blocks;
   after that, roll forward only — stragglers join by upgrading, never by
   the committee returning to the old version.

An idle chain flag-days the same way — the halt window is invisible between
heartbeats, and the idle policy needs no special handling.

## Incident playbook pointers

- Validator down briefly → nothing to do; it catches up on restart, and its
  certified watermark covers the gap on the first live certificate.
- Validator down past the catch-up window, or with damaged consensus storage
  → floor restart: wipe the consensus directory and start; the floor cache
  and the freshness policy are described under ["Running
  it"](enabling.md#storage-retention).
- Compromised or misbehaving validator → schedule it out; on an idle chain
  the sprint activates the change without traffic.
- Committee below quorum → restore nodes; the chain resumes on its own. If
  nodes are unrecoverable, the rollback section applies.
- Suspected divergence (invalid verdicts firing) → freeze rollouts first,
  investigate second; the rejecting validator's logs name the height and
  reason, and every validator's RPC serves the blocks for comparison.
