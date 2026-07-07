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
| `consensus_verify_verdicts{verdict="withhold"}` | increments only around restarts / L1 lag | persistent growth means this validator cannot vouch for proposals — usually its L1 view lags (priority-op authenticity checks) or commits are backlogged | check the L1 provider and the `speculative state at capacity` log line; the condition clears itself when the cause does |
| `consensus_activity{...conflicting_notarize / conflicting_finalize / nullify_finalize}` | zero, always | protocol fault evidence: a committee member equivocated | treat as compromise of that validator's key; schedule it out of the committee (["Changing the committee"](enabling.md#changing-the-committee)) |
| block staleness (`eth_getBlockByNumber("latest")` age, or `/status` `finalized.observed_unix`) | never older than `consensus.idle_heartbeat` plus a couple of leader timeouts | the chain stopped: quorum loss, or every leader declines. The heartbeat makes this rule sharp — an idle chain still pulses | count reachable validators vs quorum; a below-quorum committee needs nodes restored (["Rolling back"](enabling.md#rolling-back) is the last resort) |
| `/status` `finality_certified_height` | tracks `applied_height` within a small tail | the finality trail stopped being provable: certificates are not being stored. After a restart it briefly trails, then one live certificate covers the gap — a *persistent* stall means the certificate stream itself is broken | check the activity-observer logs ("failed to persist a finality certificate") and disk space |
| `repositories_persistence_lag` | ~0 | the block-persist loop is falling behind memory; a crash while behind loses the unpersisted window | check disk throughput; falling behind persistently is a capacity problem |
| `jemalloc_allocated_bytes` | grows with write load, bounded by the RocksDB write-buffer plateau | unbounded growth is a leak; the known grower is RocksDB memtables (node-team item) | compare against the documented baseline before suspecting consensus |
| RPC admission (`"pipeline backpressure"` rejections) | brief bursts under heavy load | sustained refusal means proof generation cannot keep up with the block rate | a capacity signal, not a fault: reduce load or scale provers |

Two structural notes. First, all `consensus_*` counters reset on process
restart — alert on increases, not absolute values. Second, a *stopped*
validator alarms on nothing by itself: it is the surviving committee's view
(quorum arithmetic, staleness) that tells you whether the loss matters.

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

Chains expected to idle should size `epoch_length` small enough that the
sprint bound is acceptable for incident response, and raise `epoch_retention`
to keep the loaded catch-up window sane; the journals are small either way.

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
