---
name: zksync-os-server-plan-feature
description: Plan a zksync-os-server feature as an explicitly agreed milestone, then write a phased plan under reference/plans only after two confirmation gates. Use only when the user explicitly invokes $zksync-os-server-plan-feature inside zksync-os-server or one of its worktrees; do not infer this skill from ordinary planning, Rust, or LSP requests.
---

# ZKsync OS Server Plan Feature

## Overview

Use this skill to turn a zksync-os-server feature request into an agreed milestone plan. Do not implement code during this workflow. Do not write a plan file until the user has approved both the milestone end shape and the phased plan plus filename.

## Gates

This workflow has three gates:

1. Agree on the milestone scope and end shape.
2. Agree on the phased high-level plan and plan filename.
3. Write the plan file under `reference/plans/`.

Stop at each gate and wait for explicit user confirmation. Treat user corrections as scope input to reason about, not as automatic approval.

## Step 1: Gather Requirements

Clarify the boundaries of the feature implementation. Many LSP features are too large to implement as one milestone, so define a sensible milestone unless the request is obviously contained, such as a bug fix or a terminal config feature.

A good milestone must enable real use cases and usually contains intermediate phases or checkpoints. It does not need to encompass everything.

For large feature families, propose a foundation plus one useful demonstration case. For example, if the user asks for a type inference engine and none exists, a reasonable milestone could create the required abstractions, add the engine skeleton and basic integration, then support one concrete use case such as direct return generic argument instantiation.

If related functionality already exists and support is unclear, determine whether the user wants breadth or depth:

- Breadth adds another RPC method, transaction class, provider, or component path using established semantics.
- Depth strengthens recovery, persistence, finality, compatibility, observability, or failure handling for an existing path.

## Step 2: Validate Scope

Inspect enough of the repository to understand whether required foundations exist and whether they are extendable. The supported use case is the end goal, but the milestone must leave a solid base for continuation.

Propose architecture or refactoring as the milestone when it is the correct next step and it will simplify future milestones in the same scope. Avoid building feature layers on missing or weak foundations.

Avoid the opposite extreme too. Do not turn a feature request into "build every foundation first" if useful progress can be encapsulated without damaging future work. Explicit, reversible shortcuts are acceptable when they produce useful progress and the tradeoff is documented.

If the requested visible result is only reachable through major tech debt, call that out directly. Explain the missing foundations, identify which ones are likely separate milestones, and propose a smaller goal.

Examples:

- A new RPC field may look local to `rpc`, but if its source is not retained by state or repositories across replay, define the data ownership and recovery path before planning the endpoint.
- Removing a gateway client may also move finality choice, retries, caching, and error semantics. Plan the new owner for those responsibilities rather than replacing calls one-for-one.
- Persisting a new value may require schema compatibility, compaction behavior, and restart tests. Do not plan only the write path.
- A wire payload change must add a new versioned replay file and compatibility coverage; never plan an edit to an existing `lib/network/src/wire/replays/v*.rs` file.

## Step 3: Propose End Shape

After gathering requirements and validating scope, propose the high-level milestone end shape only. Do not break it into phases yet.

Explain:

- what will be supported;
- why this is the right milestone size;
- what missing infrastructure will be created, if any;
- what refactoring will be done and why, if any;
- what new use cases will work;
- what future extensions this milestone unblocks;
- any explicit shortcuts and why they are reversible.

Ask the user to confirm or correct the end shape. If the user asks to add something that meaningfully expands scope, explain the prerequisites and recommend postponing it when appropriate.

Example end-shape response:

`I would scope this milestone as: introduce a provider-owned replacement for the gateway reads used by one complete L1 watcher path, preserve its confirmed/finalized and retry semantics, and prove main-node restart behavior with an integration test. This does not remove every gateway caller yet. The value is that later migrations reuse an established ownership boundary instead of duplicating provider logic. Please confirm this end shape before I split it into phases.`

## Step 4: Propose Phased Plan And Filename

After the end shape is agreed, create a high-level phased plan for that scope and propose a filename.

The first phase may be refactoring or missing infrastructure and may have no direct user-visible outcome. Subsequent phases should move gradually toward the milestone. Include a final architecture-alignment phase only when the milestone is meant to prepare a foundation for future scopes.

Filename guidance:

- Use a simple uppercase name such as `REMOVE_GATEWAY` or `EXTERNAL_NODE_FAILOVER_V3`.
- Keep it memorable and not exhaustive.
- Use the `.md` extension when writing the file.

Filename examples:

- Use `REMOVE_GATEWAY.md` for a remove gateway support milestone, not `REMOVE_GATEWAY_SUPPORT_IN_L1_SENDER.md`.
- Use `EXTERNAL_NODE_FAILOVER_V3.md` when the user names the scope that way or when it is the next clear external node failover milestone.

Present the phases and the filename, then ask for explicit confirmation. Do not write the file yet.

## Step 5: Write The Plan File

After the user confirms the phased plan and filename, create `reference/plans/` if needed and write the plan file there. The `reference` directory is intentionally local developer material; do not check whether it is tracked before using it.

The plan must include:

- a short title and milestone end shape;
- the approved filename context if useful;
- the agreed scope and non-goals;
- the relevant current-codebase context;
- explicit assumptions and documented shortcuts;
- phased sections with checklists;
- notes about what future work the milestone unblocks.

Include this implementation guidance in the plan:

- The phases and checkboxes are guidance written before implementation starts. They are not set in stone. If the situation changes, bring it up with the user instead of rewriting scope silently.
- The phases and checkboxes track progress; they are not goals by themselves. Do not tick a checkbox by lowering code quality or skipping required base functionality unless that shortcut was explicitly agreed.
- If a checkbox turns out to require a scope of its own, or no longer makes sense after discoveries, discuss it with the user.
- Architecture and code quality are baseline requirements even when not repeated in every checkbox.

After writing the file, report the path and summarize the agreed milestone in one short paragraph.

## Project Context

Plan for a high-throughput ZKsync OS sequencer whose components must remain restartable and replayable. Code quality and architecture matter more than rushing visible output, but development should still move forward.

Assume the user knows the current codebase but may not know missing domain foundations. Explain missing prerequisites plainly, especially around p2p networking, consensus, protocol, zksync-os VM and proving.

The user may use imprecise terms or underestimate scope. Correct assumptions respectfully and make complexity visible. Prefer plans that help the user learn the domain while the feature is built.

Examples of assumptions to correct:

- adding persistence without planning recovery, compaction, and schema compatibility;
- adding RPC output without identifying a durable source of truth;
- changing a payload without accounting for replay and wire-version compatibility;
- testing only main-node behavior when external nodes consume the same data differently.

Do not be a code purist. Prefer high-quality architecture by default, but propose explicit, reversible shortcuts when they produce useful progress and can be unwound as the codebase matures.

Shortcut example:

- It can be acceptable to adapt one subsystem to a new provider trait before migrating every caller, as long as the adapter preserves old semantics, the dependency direction is correct, and removal of the compatibility layer is tracked as future work.
