---
name: zksync-os-server-next-slice
description: Choose, propose, and implement the next approved slice from an existing zksync-os-server plan in reference/plans, updating the plan only for factual progress afterward. Use only when the user explicitly invokes $zksync-os-server-next-slice inside zksync-os-server or one of its worktrees and identifies the exact plan file; do not infer this skill from ordinary implementation or review requests.
---

# ZKsync OS Server Next Slice

## Overview

Use this skill to continue an already-approved zksync-os-server plan one coherent slice at a time. The plan must already exist under `reference/plans/`, and the user must identify the exact plan file. If the file is not known or the plan does not exist, stop and ask for clarification. Do not assume.

## Applicability

Use only inside `matter-labs/zksync-os-server` or one of its worktrees. Confirm the repository from workspace metadata or root files rather than requiring an exact directory name, because worktree names may have a suffix. If invoked outside such a checkout, stop and ask for clarification.

Before choosing a slice, read the selected plan file and inspect enough of the current implementation to match planned progress to actual code. Treat the plan as guidance written before implementation, not as perfect current truth.

## Gates

This workflow has three gates:

1. Optional: clarify unclear plan, scope, stale-document, or divergence questions.
2. Get explicit approval for the proposed slice and implementation plan.
3. Implement the slice.

Do not modify code before the user approves the proposed slice and implementation plan.

## Step 1: Choose The Slice

Read the implementation document and determine its apparent completeness state. Then compare it to the actual implementation. The document may be stale.

If the document is stale but the implementation direction is still coherent, call that out. Do not update the document unprompted before the slice. If the user gives no explicit handling instructions, assume the plan can be actualized after the slice is completed as part of the normal document update flow.

If the implementation has significantly diverged from the plan, stop and ask how to proceed. The divergence may mean the user intentionally steered the work in a better direction and the doc needs updating, or it may mean progress tracking was lost and the implementation went the wrong way.

Significant divergence examples:

- The plan says "implement feature A", but the implementation has instead built feature B.
- Phase 5 appears completed before phase 2, and that order changes the intended architecture or scope.

Minor divergences are normal. For example, the code may not strictly match the wording of a checkbox because plans are written before implementation starts and cannot predict exact code shape.

The next slice is usually one of:

- A whole phase, when the phase is self-contained. This is the normal case.
- A subset of a phase, when the phase is too large or some checklist items are optional or much more expensive than the rest.

Avoid mixing work from several phases unless it is clearly the next coherent slice. If proposing a cross-phase slice, explicitly call out that it crosses phases and invite discussion. If the user then approves with a response such as "All good, let's do it", treat that as approval.

Treat checkboxes as guidance, not acceptance criteria to implement by any means. If a checkbox turns out to be much larger than the plan implied, call that out and propose re-discussing. Push back if the user gives no explicit answer in important cases.

Example:

- If a checkbox says "Make `foo.iter()` work" and inspection shows that `foo.iter()` requires a broad iterator model or several missing semantic layers, do not force it through as "just a checkbox". Explain that the plan had incorrect expectations, propose a smaller or different slice, and ask the user to confirm.

After choosing a plausible slice, either ask clarifying questions or proceed to the implementation-plan proposal. Keep clarification separate from implementation planning so the conversation stays focused.

## Step 2: Propose The Implementation Plan

Once there is enough information, propose the slice and a concrete implementation plan. The plan file is usually high-level; this proposal should reflect the current codebase.

Depending on the slice, include:

- new abstractions or integration paths;
- extensions to existing functionality;
- new or updated tests;
- the expected end shape after the slice;
- what user-facing behavior will work, if applicable;
- known caveats, explicit shortcuts, or likely follow-up review points.

Align the implementation plan with the overall document. If the slice assumes functionality is added directly to a module and the document has a later cleanup phase, that may be intentional. Do not split code early just because a cleaner final shape is visible.

If the document does not reserve cleanup for later and functionality would otherwise spread complexity, propose refactoring or module splitting as part of the slice. High-level plans cannot predict exact code shape, so use engineering judgment about how architecture should evolve.

Examples:

- If the plan has a final refactoring phase, and the current phase says to add direct support in an existing module, preserve that direction unless the code quality cost is obvious.
- If every remaining phase only adds functionality and no cleanup phase exists, propose extraction or a new abstraction when continuing inline would make the module harder to reason about.
- If implementation now is reasonable but a small extraction will likely be needed immediately after review, say that in the proposal instead of hiding the expected follow-up.

Ask for explicit approval after presenting the slice and implementation plan. Do not start implementation yet.

## Step 3: Implement The Slice

After approval, implement the slice. Follow the repository instructions and prefer the existing codebase style over new abstractions unless the slice needs them.

Do not assume implementation is one-shot. Slices often go through review iterations, and user feedback is expected.

After implementation but before handoff, do a lightweight sanity check for obvious misses:

- functionality placed in an uncontroversially wrong module or type;
- manual implementation of behavior that likely already exists elsewhere;
- inconsistency with surrounding code style or documentation coverage;
- mostly duplicated tests that should be merged into existing test vectors;
- tests that no longer make sense after implementation, such as low-signal tests useful only while checking that a feature initially worked.

This is not a deep self-review. A deeper review phase may happen afterward, so keep this pass focused on obvious issues.

## Step 4: Update The Plan Document

After implementation and the lightweight sanity check, update the selected plan only for factual progress made in this slice:

- mark completed checkboxes;
- add brief notes for deviations, caveats, shortcuts, or discovered follow-up work;
- do not rewrite future scope or change the milestone without explicit user approval.

If the plan was stale and the user did not provide separate instructions, actualize only the parts needed to reflect this completed slice and any directly observed factual state. Do not silently redesign the milestone.

## Step 5: Hand Off For Review

When handing off the change, include only applicable high-signal context:

- new user-facing functionality that works now;
- architecture changes;
- caveats discovered during implementation;
- shortcuts taken;
- tests or checks run, and any checks not run.

If the functionality is not trivial, include a simple walkthrough that explains the spirit of the implementation rather than enumerating structures and methods. Prefer syntax-based examples where they help.
