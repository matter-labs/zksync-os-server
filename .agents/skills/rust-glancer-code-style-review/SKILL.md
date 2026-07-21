---
name: rust-glancer-code-style-review
description: Review rust-glancer project changes for uncontroversial code-style and structure smells, then propose a refactoring plan without applying changes. Use only when the user explicitly invokes $rust-glancer-code-style-review for the rust-glancer repository or one of its worktrees; do not infer this skill from ordinary review or Rust requests.
---

# Rust Glancer Code Style Review

## Overview

Use this skill as a focused reviewer for rust-glancer changes. Identify likely smells, explain why they matter in this codebase, and propose a concrete refactoring plan. Do not apply changes unless the user explicitly approves the plan.

## Workflow

1. Confirm that the current repository is `rust-glancer` or a rust-glancer worktree. If it is not, say the skill does not apply.
2. Determine the review scope. Prefer the user-specified files or diff; otherwise inspect the working tree with `git status` and `git diff`.
3. Read the surrounding modules before judging a smell. Use `rg` first for related types, helpers, existing APIs, and repeated functionality.
4. Report only uncontroversial or high-confidence findings. If a heuristic is plausible but not clear, mark it as a question or omit it.
5. Propose a refactoring plan with the exact files, APIs, or modules to change. Keep the plan review-only until the user explicitly asks to implement it.

## Review Checks

- Prefer `rg_std::UniqueVec` for ordered sets instead of manually maintaining unique `Vec` semantics.
- Prefer `rg_std::ExpectedUnique` for values where any number may technically exist, but the normal and expected case is exactly one.
- Avoid single-use helpers that do not improve readability. Keep a single-use method only when it encapsulates meaningful cognitive complexity or makes the call site much cleaner.
- Treat a single-use helper calling another single-use helper as almost always suspicious. Prefer inlining the sequential logic with a short comment.
- Place methods on the entity that owns the concept when possible. For example, prefer a path-oriented API such as `Path::single_name(&self)` over `single_name_from_path(&Path)`.
- If a helper is not good general API but clearly exists for one consumer, prefer a private or `pub(crate)` static method in that consumer's `impl` block over a free function.
- Merge equivalent `impl` blocks when moving code during refactors. Do not leave duplicate `impl Type` blocks in the same destination without a concrete readability reason.
- Enforce the project module convention: multi-file modules use `mod.rs`, not both `foo.rs` and `foo/`.
- Watch for modules that grow beyond their logical responsibility. If new code does not belong to the module's domain, propose moving it to a more relevant location; if related abstractions crowd one module, propose splitting it into files.
- Watch for structs becoming god objects. If one struct starts owning unrelated responsibilities such as import resolution, scope resolution, and macro expansion, propose an orchestrator plus focused sub-structs.
- Prefer obvious `if let` chains over consecutive `let Some(...) = ... else { return ... };` bindings when the result is cleaner. Do not force this when sequential `let` bindings read better.
- Return iterators instead of `Vec` when a method is added only to support immediate iteration, unless collection is explicitly needed or lifetimes/ergonomics make the iterator version worse.
- Check new functionality against likely repetitions. Search for existing equivalent helpers or domain APIs before accepting a new helper.
- When reuse is appropriate, prefer a natural shared API or shared module shape. Avoid awkward dependencies where a general thing reaches through one implementation to serve another implementation.

## Output

Lead with findings ordered by confidence and impact. Include file and line references when available. For each finding, state the smell, the reason it is likely uncontroversial, and the preferred refactoring direction.

After findings, provide a short refactoring plan. If no findings are found, say so clearly and mention the review scope. Do not run broad tests or apply changes unless the user asks for implementation.
