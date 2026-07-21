---
name: rust-glancer-plan-feature
description: Plan a rust-glancer feature as an explicitly agreed milestone, then write a phased plan under reference/plans only after two confirmation gates. Use only when the user explicitly invokes $rust-glancer-plan-feature for the rust-glancer repository or one of its worktrees; do not infer this skill from ordinary planning, Rust, or LSP requests.
---

# Rust Glancer Plan Feature

## Overview

Use this skill to turn a rust-glancer feature request into an agreed milestone plan. Do not implement code during this workflow. Do not write a plan file until the user has approved both the milestone end shape and the phased plan plus filename.

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

If the feature already exists and the user has not stated what is supported, clarify whether they want to increase breadth or depth. Prefer common, basic use cases first. For example, unsupported closure input/output inference usually matters more than const generics because it moves project readiness forward for more code.

Examples:

- If the user asks to implement a type inference engine and no type inference exists, do not propose a full engine with trait solving and broad Rust coverage. Propose a milestone that creates the type inference foundation, integrates it in the relevant query or semantic path, and proves it with one basic case such as instantiating direct return generic arguments.
- If a type inference engine already exists and the user asks to "improve inference", clarify whether they want breadth, such as supporting more functional entrypoints or basic trait solving, or depth, such as supporting more cases inside already-covered scenarios.
- If common closure input/output return type inference is missing, prefer that kind of basic use case over const generics unless the user explicitly wants a narrower advanced milestone. The plan should move project readiness forward, not optimize for novelty.

## Step 2: Validate Scope

Inspect enough of the repository to understand whether required foundations exist and whether they are extendable. The supported use case is the end goal, but the milestone must leave a solid base for continuation.

Propose architecture or refactoring as the milestone when it is the correct next step and it will simplify future milestones in the same scope. Avoid building feature layers on missing or weak foundations.

Avoid the opposite extreme too. Do not turn a feature request into "build every foundation first" if useful progress can be encapsulated without damaging future work. Explicit, reversible shortcuts are acceptable when they produce useful progress and the tradeoff is documented.

If the requested visible result is only reachable through major tech debt, call that out directly. Explain the missing foundations, identify which ones are likely separate milestones, and propose a smaller goal.

Examples:

- If the codebase only has item tree lowering and lacks def maps, semantic analysis, and body IR, a request for IDE hover hints is not a reasonable immediate milestone. Explain that hover depends on several foundational pieces, each likely requiring its own milestone, and propose a smaller foundational goal.
- If there is no type inference yet, do not require the first milestone to build type inference, trait solving, all missing model support, and all missing lowering support before any feature works. Prefer a bounded foundation that leaves room for later milestones.
- If architecture or refactoring is the best milestone, make the payoff concrete: name the future milestones it will simplify, and avoid selling refactoring as an end in itself.

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

`I would scope this milestone as: create the initial type inference foundation, wire it into the semantic path where later LSP features can consume it, and support one visible proof case: direct generic return type instantiation. This does not include trait solving or broad expression inference yet. The value is that future milestones can add more inference cases without changing the core ownership model again. Please confirm this end shape before I split it into phases.`

## Step 4: Propose Phased Plan And Filename

After the end shape is agreed, create a high-level phased plan for that scope and propose a filename.

The first phase may be refactoring or missing infrastructure and may have no direct user-visible outcome. Subsequent phases should move gradually toward the milestone. Include a final architecture-alignment phase only when the milestone is meant to prepare a foundation for future scopes.

Filename guidance:

- Use a simple uppercase name such as `SUPPORT_LSP_RENAME` or `TYPE_INFERENCE_V3`.
- Keep it memorable and not exhaustive.
- Use the `.md` extension when writing the file.

Filename examples:

- Use `SUPPORT_LSP_RENAME.md` for a rename-support milestone, not `SUPPORT_LSP_RENAME_WITH_WORKSPACE_EDIT_AND_SYMBOL_GRAPH_AND_EDGE_CASES.md`.
- Use `TYPE_INFERENCE_V3.md` when the user names the scope that way or when it is the next clear type-inference milestone.

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

Plan for a robust, high-quality LSP. Code quality and architecture matter more than rushing visible output, but development should still move forward.

Assume the user knows the current codebase but may not know missing domain foundations. Explain missing prerequisites plainly, especially around LSP, semantic analysis, type inference, trait solving, body IR, def maps, and IDE feature integration.

The user may use imprecise terms or underestimate scope. Correct assumptions respectfully and make complexity visible. Prefer plans that help the user learn the domain while the feature is built.

Examples of assumptions to correct:

- The user may think trait solving is a fully separate procedure from trait inference, similar to a concrete indexing pass. Explain only as much as needed to shape the milestone correctly.
- The user may ask for a visible LSP feature without realizing that semantic analysis, body IR, def maps, or type information are missing prerequisites.

Do not be a code purist. Prefer high-quality architecture by default, but propose explicit, reversible shortcuts when they produce useful progress and can be unwound as the codebase matures.

Shortcut example:

- It can be acceptable in an early milestone to treat `&T` and `T` as equivalent to unlock basic autocomplete scenarios, as long as the shortcut is explicit, reversible, and the plan leaves room for later reference types, reference peeling, and autoderef.
