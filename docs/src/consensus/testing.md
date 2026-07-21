# How consensus is tested

## Why determinism is the whole strategy

Consensus bugs live in *interleavings*: a crash between a vote and its broadcast, a
partition healing one message too late, two candidates racing at the same height.
Wall-clock integration tests sample interleavings at random — which makes them
flaky when they can fail and silent when they can't reach the interesting case.
This codebase inverts the usual pyramid: the primary test surface is
**deterministic simulation** (DST), and real-process integration tests are a thin
layer for what simulation physically cannot see.

Five principles, each mechanically enforced rather than aspirational:

1. **Determinism is itself asserted.** Every simulated scenario runs *twice per
   seed*, and the runtime's audit fingerprint — a rolling hash over every
   scheduling decision, RNG draw, and network event — must match bit-for-bit. If
   someone introduces wall-clock time or an unordered iteration into a scheduling
   path, the whole suite fails loudly instead of becoming flaky.
2. **Edge cases are modeled, not awaited.** Crashes, partitions, byzantine peers,
   and slow links are injected at exact virtual-time points. Nothing sleeps and
   hopes.
3. **The seam is the harness.** The simulated committee runs the *production*
   consensus stack — engine, marshal, journals, backfill — over the deterministic
   runtime and a simulated network. There are no test forks of consensus logic;
   what is tested is what ships.
4. **Real semantics run in simulation.** The state transition function is
   deterministic and can run in-memory, so simulated committees can carry the
   actual VM executing actual transactions — and assert that every validator's
   *state*, not just its block sequence, is identical. Semantic divergence is a
   simulation-reachable bug, not a staging surprise.
5. **A change ships with its scenario.** Consensus code without a unit test or a
   simulation scenario demonstrating it is not reviewable.

## The layers and their contracts

| Layer | Runs on | Proves | Deliberately cannot see |
| --- | --- | --- | --- |
| Unit | plain | validity rules, codecs and golden fixtures, guard matrices, overlay bookkeeping | anything cross-node |
| Simulation (DST) | deterministic runtime, simulated p2p | whole committees: safety, liveness, recovery, byzantine handling, state equality | real sockets, disks, process boundaries, RPC |
| Integration (L3) | real nodes in-process, RocksDB, real p2p, a local L1 | wiring truths: boot, config, restarts over real storage, RPC surfaces, settlement | byzantine behavior, partitions, adverse timing |
| Chaos rig | containers, seeded fault driver | what only sustained wall-clock randomness finds: teardown races, drift, leaks | — (telescope, not gate: never in CI) |
| Staging | real deployment | latency distributions, operations, runbooks | — |

The division is a rule, not taste: **if a case can be expressed at a lower layer,
it must be.** An integration test that could have been a simulation scenario is a
review finding. The chaos rig inverts the flow — it *finds* rather than *pins*;
it gets [its own section below](#the-chaos-rig).

One honest limitation to keep in mind: the layers above test consensus *given a
healthy node* — but consensus participation also leans on node subsystems that
simulation does not model (the mempool contract, fee sourcing, the L1 watcher, the
persistence pipeline's durability watermark). Those seams are precisely where
integration bugs live, which is what the multi-node integration suite and the
chaos rig exist to catch. Consensus readiness is judged on the **system**, not on
the consensus layer alone.

## The testing primitives commonware provides

The swap that makes all of this work is wholesale: the production stack is generic
over the runtime and network traits ([the integration
chapter](integration.md#the-other-side-of-the-seam) maps the production side), and
the harness instantiates both with commonware's test implementations.

- **`commonware_runtime::deterministic`** — a runner with a seeded task scheduler
  and a virtual clock: timers fire instantly in wall time, in a reproducible order
  decided entirely by the seed. Its in-memory `Storage` keeps partitions alive
  across a simulated crash-and-restart, so journals and archives behave like a
  disk that survived the crash. And its **auditor** maintains a rolling hash over
  every scheduling decision, RNG draw, and I/O event — the fingerprint the
  determinism gate compares between the two runs of every seed.
- **`commonware_p2p::simulated`** — an in-memory network whose `Oracle` the test
  scripts: per-link latency, jitter, and loss; adding and removing links (which is
  how partitions and heals are expressed); and visibility into per-peer blocking,
  so scenarios can assert *who banned whom* — and, just as importantly, that
  nobody banned an honest validator.
- **Byzantine engine mocks** — commonware ships wire-level attackers (an
  equivocator signing conflicting votes, a "nuller" voting to accept and to skip
  the same view, and friends) that slot in below the application layer. A scenario
  hands one validator's real keys to a real attack implementation instead of a
  hand-rolled approximation of one.

Worth stating as the punchline: nothing in the consensus stack knows which runtime
it is on. The deterministic swap is not a mock of consensus — it is the production
consensus stack on different hardware.

## Working with the simulation harness

The harness lives in `lib/consensus/sim`; the scenario corpus is its `tests/`
directory. Every scenario goes through `run_scenario`, which supplies the
guarantees for free:

- **Seed sweeps.** The same scenario logic runs under several scheduler seeds —
  each a genuinely different interleaving of messages, timers, and wakeups.
- **The determinism gate.** Each seed executes twice; fingerprints must match.
- **Perfect reproducers.** A failure prints its scenario name and seed; rerunning
  that seed replays the failure exactly, down to every message on the simulated
  wire. There is no "flaky, retried, green" lane — a red seed is a real bug with a
  deterministic reproducer.
- **Free time.** Timeouts are virtual: a minute of consensus costs milliseconds,
  and generous scenario timeouts cost nothing.

Scenarios drive a `SimCluster` — N validators with control over links (partitions,
loss, latency), crash/restart per validator, and byzantine behaviors — over one of
two execution backends: a **scriptable mock** (content-free blocks; for scenarios
about consensus mechanics) and the **real VM over in-memory state** (real signed
transactions; for scenarios where state itself is the assertion). Byzantine
scenarios come in two flavors matching two attacker models: *wire-level* (the
library's attack mocks — equivocation, double-voting — asserting that evidence
names exactly the culprit and honest safety holds) and *application-level* (an
honest consensus stack whose **proposals** are corrupted — asserting the committee
refuses, the chain rides through, and noting that this attacker produces **no**
fault evidence: "no evidence" never means "no attack").

A pattern worth copying when writing assertions: **match assertion sharpness to
quorum arithmetic.** At n=3 (quorum 3), "the chain advances" proves every single
validator is voting; at n=5 (quorum 4), the same assertion proves nothing about any
particular one. Several scenarios pick their committee size to make the property
they pin provable.

The corpus is organized by capability — steady-state liveness, crash/restart,
partitions and degraded links, byzantine (both flavors), execution edge cases,
node-condition liveness (a stalled pipeline or slow peer must not silence a
validator), late join, migration, and committee-scale sanity. The directory is the
living inventory; each file's header explains what its group pins and why.

For nightly-depth sweeps, the `CONSENSUS_SIM_SEEDS` environment variable widens
every scenario's seed range (a dedicated scheduled workflow runs the corpus this
way); pull-request CI runs each scenario's own small seed set.

## The chaos rig

Deterministic simulation is exhaustive about the failure space you *modeled*. The
chaos rig (`tools/chaos`) exists for the rest: think of it as a fuzzer for
operational reality, hunting what only real processes on a real kernel with real
disks and hours of wall-clock randomness produce — teardown races, resource leaks,
rate and drift interactions, recovery paths that only break once a chain has
actually aged.

The shape: `chaos setup` generates a real committee (the production container
image, production-shaped layered configuration, a local L1), and `chaos drive`
runs a seeded, self-healing fault schedule against it — kill -9 and graceful
stops, SIGSTOP freezes, network partitions, packet loss and latency via tc netem.
The driver is quorum-aware, and it **journals what it broke and what liveness it
therefore expects**; an in-process watcher continuously checks the committee
against those expectations: identical block hashes across validators, finality and
applied-height monotonicity, no progress without quorum, zero byzantine fault
evidence, no unexpected process deaths, and a log oracle for novel errors. The
expectation journal is what makes "the driver took quorum away" distinguishable
from "the chain stalled unexplained". A separate `chaos load` runs concurrently,
funding senders through real L1 deposits and streaming transfers through the
committee — so cross-validator state agreement is checked *under fire*, the
strongest divergence probe the rig has.

Two rules define its place in the pyramid:

- **Telescope, not gate.** The rig never runs in CI. It runs for hours or days on
  a workstation or a dedicated box; seeded schedules make experiments repeatable
  without pretending the system's responses are deterministic.
- **A finding freezes the scene.** On the first violated expectation the rig stops
  healing, captures the journal, the findings, and every container's logs, and
  exits nonzero. Its specialty — the class it exists for and reliably catches — is
  precisely what no other layer can see: restart and recovery paths that depend on
  real elapsed time, and shutdown races that need thousands of genuine teardowns
  to fire. Each finding is then distilled into a deterministic regression at the
  lowest layer that can express it: **the rig discovers, the lower layers pin.**

Bring-up is a few commands and doubles as the
[local consensus devnet](../setup/consensus_devnet.md); the operating manual —
fault classes, watcher semantics, load patterns — is `tools/chaos/README.md`.

## Wire formats: golden fixtures

Everything in `lib/wire` is pinned by **golden tests**: committed fixture files
holding the exact bytes of every released encoding. The tests assert in both
directions — the canonical value still encodes to exactly the committed bytes, and
the committed bytes still decode (and re-encode to themselves) — so a released
encoding can never drift, silently or otherwise. `UPDATE_GOLDENS=1` can only
*create* fixtures for new formats; changing a committed one is not a fix anyone
can make by accident, which is the point: wire changes are new versions, never
edits.

## Integration tests and the harness

The multi-node harness (`integration-tests/src/multi_node.rs`) boots real
committees — full nodes with real RocksDB, real consensus networking, and one
shared local L1 — and supports stopping, restarting (optionally with a modified
configuration), and migrating from a stopped single-sequencer node's data. Tests
here assert only with generous poll-until bounds, never with sleeps tuned to
timing: timing-sensitive integration tests are flaky by construction, and this
suite exists to check wiring, not races. The scenarios that need precise timing
belong in simulation, where timing is a controlled input.
