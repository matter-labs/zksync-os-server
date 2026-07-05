# Consensus: what and why

## The problem

A single sequencer is a single point of trust. Validity proofs already prevent it
from *forging* state — but nothing prevents it from stopping, censoring, or
reordering. Every user and every partner chain depends on one operator's uptime and
good behavior.

A **shared network** replaces that one operator with a committee: several
validators, run by parties who do not trust each other, jointly sequencing one
chain. The goal is that no single participant — not even a fully malicious one —
can break the chain's two core promises:

- **Safety**: no two conflicting blocks are ever finalized at the same height.
  Finality is forever; there are no reorgs of finalized blocks.
- **Liveness**: the chain keeps growing as long as enough of the committee is
  honest and reachable.

"Byzantine fault tolerant" means exactly this: the guarantees hold even when some
members are *byzantine* — not just crashed or unreachable, but actively lying,
sending different messages to different peers, or colluding. Crashed, partitioned,
and malicious validators all draw from the same fault budget.

## The arithmetic

BFT protocols in this family tolerate `f` faulty members out of `n = 3f + 1`, and
decisions require a **quorum** of `n − f` votes:

| Committee size `n` | Tolerated faults `f` | Quorum |
| --- | --- | --- |
| 3 | 0 | 3 |
| 4 | 1 | 3 |
| 5 | 1 | 4 |
| 40 | 13 | 27 |

Two things are worth internalizing from this table. First, **three validators
tolerate nothing**: every vote is needed, so any single stopped node halts the
chain. Four is the smallest committee that survives losing a member. Second, the
reason quorums work is *overlap*: any two quorums of size `n − f` share at least
`f + 1` members, so at least one **honest** validator sits in every overlap — and an
honest validator never votes for two conflicting blocks. That intersection argument
is the entire safety proof in miniature, and it is why the quorum size cannot be
bargained down.

## Views and leaders

Consensus time is divided into **views** (also called rounds). Each view has exactly
one **leader** — here chosen round-robin over the committee — who gets to propose
one block. Everyone else votes on the proposal. If the leader is slow, offline,
partitioned, or proposes something the others refuse, the view times out and the
next view begins with the next leader.

The mindset shift from single-sequencer thinking: **losing your proposal is
routine.** A view that produces no block is not an incident; it is the protocol
working as designed, and the cost is one view's worth of latency. Everything in the
integration is built to treat proposal abandonment as a normal path — exercised
constantly in tests — rather than an error to panic on.

## Simplex in five minutes

The committee runs [Simplex](https://eprint.iacr.org/2023/463), a deliberately
minimal BFT protocol (the name is the thesis). Validators exchange three kinds of
votes; the names below are the ones you will meet in the code and metrics:

- **notarize** — "this proposal is well-formed and I vouch for it." A quorum of
  notarize votes forms a *notarization*: the block is a serious candidate.
- **nullify** — "this view is going nowhere; let's move on." Sent when the view
  times out (or the proposal is unacceptable). A quorum forms a *nullification*:
  the view is skipped and the next leader builds on the latest notarized block.
- **finalize** — "I notarized this block and saw no reason to skip the view."
  A quorum of finalize votes forms a *finalization*: the block — and its whole
  ancestry — is final.

On the happy path a block goes propose → notarization → finalization: two vote
rounds. On a bad view, nullify votes let the committee skip forward without
waiting out cascading timeouts — a dead leader costs one view, not an escalation.

Simplex belongs to the same family as Tendermint and HotStuff — rotating leaders,
quorum votes, a couple of phases to finality — and a fair protocol comparison is a
research topic of its own (the paper is the reference). What matters for working on
this codebase is the three operational consequences:

1. **Notarized is not finalized.** A block can be notarized and then abandoned (its
   view nullified after the fact, a competing branch finalized). Short-lived forks
   among *candidates* are normal; that is why the node keeps speculative state for
   blocks under consideration and throws most of it away. Finalized blocks never
   revert.
2. **Re-proposal is normal.** The same block content can be proposed again in a
   later view (most commonly by a leader recovering from a crash). A block's
   identity is the hash of its content — not the view it appeared in, not who
   proposed it.
3. **Timeouts are the failure unit.** Whatever a faulty or malicious leader does,
   the designed worst case for the chain is "its leader turns are wasted." Any
   situation that could cost more than that (a halt, a fork) is a bug in the
   integration, not a tuning problem.

## What commonware provides

The consensus stack is built on [commonware](https://commonware.xyz), a library of
composable blockchain primitives (the same stack used by other production chains).
The pieces you will meet, top to bottom:

- **The runtime abstraction.** All consensus code is written against a runtime
  trait — clocks, task spawning, storage, networking. In production that runtime is
  tokio; in tests it is a *deterministic* runtime with a seeded scheduler and
  virtual time. The same production code runs under both, which is the foundation
  of the whole [testing strategy](testing.md).
- **Authenticated p2p.** Validators connect over a dedicated port, authenticate
  each other by ed25519 identity against the configured committee, and multiplex
  traffic over numbered channels (votes, certificates, block broadcast, backfill,
  transaction gossip). The domain-separation namespace of this network carries the
  committee's protocol version: validators on different versions fail to pair at
  the handshake — loudly — instead of misinterpreting each other's bytes.
- **The simplex engine.** Runs the vote state machine. It is generic over a signing
  *scheme* and a leader *elector*, and it deals exclusively in 32-byte digests —
  it neither knows nor cares what a block contains. Crucially, the engine writes
  every vote to a disk **journal before sending it**, so a validator that crashes
  and restarts replays its journal and can never contradict its earlier votes
  (equivocation by accident is impossible).
- **The marshal.** The block librarian sitting between the engine and the
  application. It caches block bodies arriving via broadcast, archives finalized
  blocks and their certificates, **backfills** anything missing from peers (for
  validators that were offline, partitioned, or just joined), and delivers
  finalized blocks to the application strictly in height order, waiting for an
  acknowledgement per block. The application side of the node plugs into consensus
  through the marshal's application traits (build a block, verify a block) and a
  *reporter* that receives the activity stream — every vote, certificate, and
  piece of misbehavior evidence observed.
- **Digests only.** Consensus messages carry 32-byte hashes; full blocks travel on
  a separate broadcast channel and through backfill. Votes stay small and
  fixed-size no matter how big blocks get.

## Keys, certificates, and evidence

Each validator holds two keys: an **ed25519** key that is its network identity
(connections are authenticated with it) and a **BLS12-381** key that signs
consensus votes. The committee configuration binds the two together per validator.

Vote quorums are aggregated into **BLS multi-signature certificates**: one compact
aggregated signature plus a bitmap of which committee positions signed. Two
properties were chosen deliberately:

- **No ceremonies.** Multi-signatures need no distributed key generation — a
  committee is just a list of public keys, which keeps validator on-boarding and
  (eventually) rotation operationally simple. Threshold signatures — one fixed
  group public key, verifiable by light clients without knowing the member list —
  are the designed-for evolution; certificate formats already carry the scheme and
  committee identifiers that transition needs.
- **Attributable votes.** Every signature in a certificate names its signer. If a
  validator signs two conflicting votes, the pair of signatures is transferable
  cryptographic **proof of misbehavior** naming the culprit — surfaced today in
  logs, metrics, and status (and preserved; acting on evidence automatically, e.g.
  slashing, is a policy question deliberately left open).

Every finalization certificate is also converted into the node's own storage format
the moment it is observed and kept in the node's **finality store** — the chain's
durable, library-independent proof trail that block N was finalized by quorum. Why
that matters, and how the store works, is covered in
[the integration chapter](integration.md).
