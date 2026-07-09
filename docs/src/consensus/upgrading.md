# Upgrading commonware

The consensus stack is built on [commonware](https://commonware.xyz), which ships
**monthly CalVer releases with breaking changes** — the library is young and moving,
and upstream does not promise API or on-disk stability between releases. The
workspace therefore pins every commonware crate to an exact version (see the comment
in the root `Cargo.toml`), and an upgrade is a deliberate, self-contained chore with
a fixed procedure — never a routine dependency bump.

This chapter is that procedure. It exists because the risk profile of a consensus
upgrade is unusual: most dependency bumps can at worst break the build or a feature,
while a consensus library swap can silently change *what bytes mean* — and our block
digests, finality certificates, and vote journals are all downstream of those
meanings. The procedure is organized around the things that must provably survive.

## What must survive an upgrade

**Wire sovereignty.** Every released encoding — the consensus block envelope,
finality certificates, replay records — is defined in `lib/wire` under our own
version tags, precisely so that no upstream change can move released bytes. The
golden tests under `lib/wire/tests/` enforce this byte-for-byte. The rule during an
upgrade: the goldens must pass **unmodified**. Regenerating a golden fixture to make
an upgrade compile is never a fix; it is the discovery of a released-format break,
and the response is a new format version or a different integration choice, decided
deliberately.

**Digest and height semantics.** A block's consensus identity (its digest) and the
chain's height bookkeeping must mean the same thing before and after. Upstream is
free to change what *its* traits expect — 2026.5.0, for example, began requiring the
consensus-era genesis to sit at height zero — but such changes must be absorbed at
the integration boundary (in that case by era-relative height translation) rather
than by letting new semantics leak into stored or gossiped data. The goldens catch
encoding drift; the simulation corpus and its migration scenarios catch semantic
drift.

**The finality store's own schema** sits between the two worlds above: it is our
format (not upstream's), but it is node storage rather than a released wire
encoding, and it is the one store the runbooks never wipe. Its epoch-keyed
entries — custody transitions, registry derivations, the floor cache, the
observed-round floor — are scoped by the consensus era (epoch numbering restarts
per era, so a fork or re-migration would otherwise collide with a dead era's
records); digest-keyed certificates are global and permanent. Pre-launch, schema
changes here follow the same policy as the journals below (the 2026-07-09
era-scoping change was one: stores written before it read as empty trails under
the current era, and a pre-launch chain that cares re-syncs). Post-launch, a
schema change needs an in-place migration, exactly because this store is never
rebuilt.

**The journal-compatibility policy.** The engine's vote journals and marshal's
archives are commonware's own on-disk formats, and upstream may change them between
releases. Our policy, pre-launch: **cross-version journal compatibility is not
required.** A validator can always be rebuilt — fresh consensus storage, state
restored from the chain itself — using the recovery runbook in
[Running it](enabling.md); a committee of `n ≥ 3f+1` tolerates each member being
rebuilt in turn. What the procedure does require is *knowing*, and knowing is
executable: the **consensus-storage replay gate**
(`lib/consensus/sim/tests/replay_gate.rs`) reopens a committed fixture of a full
cluster's consensus storage — vote journals, block and finalization archives,
marshal's caches and processed-height markers — under the current stack and
requires the chain to resume. An upgrade that breaks on-disk compatibility fails
this gate on the PR; the response is either upstream compatibility for that version
pair or a rollout plan that says "rebuild per validator" explicitly (and then a
deliberate, reviewed fixture regeneration — the policy is at the top of that test
file). Once real value depends on the network, the rebuild-is-acceptable stance
gets revisited — that is a launch checklist item, not a footnote here.

**The committee's version discipline.** A commonware upgrade rides inside a node
release, and inside a committee it is governed by the protocol-version rule from
[Running it](enabling.md): deploying a binary is safe and gradual; anything that
changes what consensus messages *mean* must ship as a new
`consensus.protocol_version`, activated committee-wide as a coordinated step, since
validators on different versions refuse to pair. An upgrade that leaves all message
encodings and signing namespaces untouched (the goldens and the cross-version
pairing test prove this) needs no version bump; rolling restarts suffice.

## Registered findings are the acceptance tests

Between upgrades, we accumulate *registered findings* against the pinned version:
upstream warts we document and work around instead of fixing — a panic on an
teardown path, a missing API the tests want, a determinism gap. The register lives
in the planning notes, and each entry is kept **executable**: an ignored reproducer
test, a chaos-rig log allowance, or an L3 that exercises exactly the wart's path.

At upgrade time this register becomes the version's acceptance suite. For each
finding, run its reproducer against the new version and record one of two verdicts:

- **Fixed upstream** — delete the workaround *in the same change*: remove the log
  allowance, un-ignore the test, drop the defensive branch. A workaround that
  outlives its wart is a trap for the next reader.
- **Still present** — the workaround stays, and the finding's upstream issue draft
  gets refreshed with "reproduced on <new version>". If we care enough, this is the
  moment to file or bump the issue upstream, while the reproduction is fresh.

This is the part that makes upgrades cumulative rather than Sisyphean: every soak
finding from the previous cycle either gets closed by upstream or gets a sharper
reproduction, and nothing is re-discovered from scratch.

## The procedure

Each step gates the next; a failure means stop and decide, not push through.

1. **Survey before touching code.** Read the release notes and changelogs for every
   pinned crate between the current and target versions (all commonware crates move
   in lockstep — mixed versions are unsupported upstream, so the family upgrades as
   one). Write a **migration map** into the planning notes: every API or semantic
   change that touches us, and for each one the intended mapping into our code. The
   map forces semantic decisions to be made deliberately and reviewed once, instead
   of emerging one compile error at a time. Pay special attention to anything
   touching wire encodings, digests, heights, journals, or shutdown semantics.

2. **Bump all pins at once.** Update every `=X.Y.Z` pin in the root `Cargo.toml` in
   a single step, including any newly-split or newly-merged crates.

3. **Migrate bottom-up, by crate.** Fix compilation in dependency order — wire →
   consensus core → simulation → execution → node → tests and tools — consulting the
   migration map so each fix is the *decided* mapping, not the minimum edit that
   compiles. Where upstream changed a contract (a sync callback that used to be
   async, a config field that moved), the code comment at the integration point
   explains the contract, so the next upgrade can re-derive why the code looks the
   way it does.

4. **Climb the regression ladder.** In order, because each rung is faster and more
   precise than the next: workspace unit tests including the full simulation corpus
   (the DSTs are the primary consensus surface — see
   [How it is tested](testing.md)) and the consensus-storage replay gate, which
   must pass **against the old fixture, unregenerated** (that is the on-disk
   compatibility claim — see the policy above); the `lib/wire` goldens,
   byte-identical and unregenerated; clippy and fmt clean; the consensus,
   migration, and reconfiguration integration-test groups (real nodes, real
   RocksDB, real L1); and finally a chaos-rig soak on an image built from the
   upgraded tree — faults, load, and epoch rotation under wall-clock time, watched
   by the rig's log scanner. The soak is the only rung that catches timing- and
   teardown-shaped regressions, which is exactly the shape of change runtime
   releases tend to carry.

5. **Re-judge the findings register** (previous section), removing workarounds for
   what upstream fixed and refreshing issue drafts for what remains.

6. **Record the upgrade.** The migration map plus verdicts becomes the upgrade's
   record in the planning notes, and this chapter absorbs any *durable* lesson —
   a new invariant worth listing above, a new rung worth adding to the ladder.

## A worked example: 2026.4.0 → 2026.5.0

The first upgrade executed under this procedure, in July 2026. What its migration
map contained, abbreviated to the shape of things a survey should catch:

- **A height-zero requirement with wire implications.** Marshal began asserting
  that the consensus genesis sits at height zero, while a migrated chain anchors
  consensus at its cutover height H. Resolved by making consensus-facing heights
  **era-relative** (the anchor is height zero; the anchor's chain height is carried
  as local, non-serialized knowledge injected through the block codec's
  configuration) — wire bytes, digests, and goldens all unchanged, node-side
  bookkeeping still chain-absolute. This is the archetype of a change that would
  have leaked into released formats if absorbed naively.
- **Callback contracts flipped from async to sync.** Reporters, relays, and p2p
  sends became synchronous, returning a `Feedback`/delivery result instead of a
  future. Each call site became a deliberate decision: block-and-apply stays on the
  committer's ordered channel; gossip becomes fire-and-forget; shutdown paths stop
  awaiting sends.
- **Engine and marshal start-up state moved into configuration.** The application
  no longer answers "what is genesis"; the engine takes a `floor` (its epoch's
  anchor digest) and marshal takes a `start` (the era genesis block). Rotation now
  resolves each epoch's anchor from marshal before spawning the engine — safe
  because rotation is driven by committed height, so the boundary block is always
  locally available.
- **Supervision relabeling.** Dynamic metric labels (`with_label(format!(...))`)
  were removed upstream; per-entity dimensions became metric attributes. Mechanical,
  but it touches every spawn site.

Verdicts from the findings register, same upgrade: the simulated network still has
no unblock API (issue stands); the teardown determinism gap still reproduces (issue
stands, reproducer stays ignored); journals remain committee-relative (rebuild
remedy unchanged). Two warts turned out **fixed in practice** — the empty-archive
destroy panic and the runtime-drop teardown error did not fire once across the
upgrade soak's epoch rotations and graceful stops, where on 2026.4.0 both were
routine — so their log allowances came out of the chaos rig's scanner, per the
"a workaround must not outlive its wart" rule. And the soak demonstrated the last
rung's value directly: it caught a non-monotone status-surface update (stale
finalizations re-heard through the tip scout moved `/status.finalized` backward on
healthy validators) that no faster rung could see, because the trigger is a
reconnect-after-lagging-across-an-epoch-boundary — pure wall-clock choreography.
