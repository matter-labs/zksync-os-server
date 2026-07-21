---
name: zksync-os-server-document-code
description: Add or revise documentation comments for an explicit zksync-os-server code scope, using example-driven Rust docs that explain why code exists and how to read it. Use only when the user explicitly invokes $zksync-os-server-document-code inside zksync-os-server or one of its worktrees and provides an explicit scope such as an unstaged diff, file, module, or symbol list; do not infer this skill from ordinary coding or review requests.
---

# ZKsync OS Server Document Code

## Overview

Use this skill to add or alter Rust documentation comments within the exact scope provided by the user. If the scope is not explicit, stop and call that out. Explicit scopes include `unstaged diff`, a particular file, a module, or a list of symbols. Do not assume the scope.

Use only inside `matter-labs/zksync-os-server` or one of its worktrees. Confirm the repository from workspace metadata or root files rather than requiring an exact directory name, because worktree names may have a suffix. If invoked outside such a checkout, stop and ask for clarification.

Documentation should onboard the reader to the context that is coming next in the code. Keep phrasing simple and example-driven where examples help. Do not change code behavior as part of this skill unless the documentation task exposes a clear typo or naming issue and the user explicitly approves the code change.

Comments are pre-reading notes, not reference docs. Optimize for the reader who has not understood the code yet.

## Workflow

1. Confirm the zksync-os-server checkout and the explicit documentation scope.
2. Inspect the surrounding module and nearby naming/documentation style before writing comments. Do not infer architecture from one symbol alone.
3. Decide what actually needs documentation. Avoid comments that only repeat names, signatures, or obvious getters/constructors.
4. Add docs that explain why the code exists, what role it plays, or how to read a non-obvious flow. Keep phrasing simple and direct. Short is good, but clarity beats compression.
5. Review the result for verbosity, stale-prone wording, and accidental behavior changes.

## Voice And Shape

Write comments as pre-reading notes for a tired maintainer, not as compressed API reference text.

Prefer comments that are a little obvious, step-by-step, or example-shaped if that makes the next code easier to enter. It is okay for the prose to be plain, slightly repetitive, or "stupid simple". Do not polish away useful scaffolding.

Do not optimize for the shortest technically-correct sentence. If a reader has to read the function first and then come back to understand the comment, the comment failed.

When proofreading user-written docs, preserve the user's structure and voice. Fix grammar, ambiguity, and small wording issues only. Do not rewrite into a more formal Rustdoc style unless the user explicitly asks for a rewrite.

Short reference-style comments are fine for genuinely simple items. For non-obvious code, prefer
a slightly more explicit pre-reading note over a compressed summary.

### Anti-examples

Bad polished-but-unhelpful function doc:

```rust
/// Propagate direct `impl Fn*(...)` parameter types into closure argument patterns.
```

Better (do not match on numbered sequence -- it works in this example, but it is not why
this example is good; it is good not because it has a list and cross-references, but because
it is obvious and explains the logic in simple words):

```rust
/// Check if this call has:
/// 1. `impl Fn*(...)` params in its target function.
/// 2. Closures passed as args in the corresponding positions.
///
/// If both line up, take the types from (1) and push them into params of the
/// closures from (2).
```

## Module Doc Comments

Module docs should explain the module's general scope: why it exists and what domain it owns. A module doc should be true for the module and its potential submodules. Some leaf modules do not need module docs when the parent doc already covers them.

Good crate-level `lib.rs` example:

```rust
//! Backpressure coordination for the block-processing pipeline.
//!
//! Sequencing cannot outrun state, storage, or downstream batch processing indefinitely. This
//! crate observes subsystem progress and exposes admission signals to both RPC and internal block
//! sources, keeping that policy separate from the components that produce work.
```

Good `mod.rs` example for a module with many children:

```rust
//! L1 event ingestion for transactions, batches, and protocol upgrades.
//!
//! Watchers share polling and finality mechanics here, while processors own the interpretation of
//! each event family. Keeping those roles separate lets startup recovery choose a cursor without
//! duplicating the live polling loop.
```

Good complex leaf-module example:

```rust
//! Admission control for internal block sources.
//!
//! This channel mirrors the monitor's RPC acceptance decision without making command producers
//! depend on RPC-facing state. A closed gate pauses new work while already-produced blocks drain.
```

Good simple module examples:

```rust
//! Ethereum-compatible JSON-RPC method implementations.
```

```rust
//! Serialization types shared by replay archive readers and writers.
```

When a module has a natural workflow shape, a walkthrough module comment can be useful:

```rust
//! Startup recovery for the sequencer pipeline.
//!
//! Recovery rebuilds the in-memory window without exposing partially restored state:
//!
//! 1. Load the compacted state boundary.
//! 2. Replay newer WAL records in block order.
//! 3. Start live command production after every downstream view catches up.
```

Bad module doc example:

```rust
//! Handles batches.
```

This does not identify whether the module builds batches, verifies them, stores proofs, or submits data to L1. Name the owned phase and the boundary with neighboring components instead.

## Struct And Enum Doc Comments

Struct and enum docs should explain why the type exists: its role in the surrounding architecture and the high-level behavior it handles.

Good complex type example:

```rust
/// Signature facts from an already-selected call that can expose trait obligations.
///
/// Example: for `let xs = bar.iter().collect::<Vec<_>>()`, call inference has already selected
/// `Iterator::collect`, instantiated its return as `Vec<?T>`, and bound the function generic
/// `B = Vec<?T>`. The input then carries:
/// - `function`: the selected `Iterator::collect` item;
/// - `owner`: the trait owner `Iterator`;
/// - `generics`: collect's params and `where B: FromIterator<Self::Item>`;
/// - `subst`: inference bindings such as `B = Vec<?T>`;
/// - `signature_subst`: ordinary signature substitutions used to resolve written paths;
/// - `selected_self_ty`: the receiver iterator type, such as `Iter<BarItem>`.
pub(super) struct SelectedCallObligationInput<'input>
```

Another good complex example:

```rust
/// Instantiates function type params as variables inside a projected call return.
///
/// ```text
/// fn id<T>(value: T) -> T
/// id(missing())       // resolved return: <unknown>, declared return: T
///                     // inference return: ?T
///
/// fn make_vec<T>() -> Vec<T>
/// make_vec()          // resolved return: Vec<unknown>, declared return: Vec<T>
///                     // inference return: Vec<?T>
/// ```
pub struct GenericReturnInstantiationBuilder<'table>
```

Good simple type example:

```rust
/// Projects member expressions while preserving inference variables from the base.
pub(crate) struct BodyMemberInference<'query, D, I> {
```

Bad type-doc example:

```rust
/// Scans module declarations, item names, and import path segments owned by DefMap.
struct NamespaceCursorScanner<'txn, 'db> {
```

This says what it scans, but it is unclear that declarations, item names, and import path segments are the output candidates, not transient scanning scope. A better version:

```rust
/// Scans the DefMap within the scope specified by arguments to find matching
/// cursor candidates: declarations, item names, and import path segments.
struct NamespaceCursorScanner<'txn, 'db> {
```

## Method And Function Doc Comments

Method and function docs usually need less context than module or type docs. Do not repeat what the surrounding type or module already says. Focus on why the function matters or what non-obvious transformation it performs.

Good one-line examples:

```rust
/// Bind written generic args, such as turbofish args, to declaration params.
pub(crate) fn subst_for_explicit_args(
```

```rust
/// Instantiate written `_` slots in explicit args such as `make::<Vec<_>>()`.
fn replace_written_ty(&mut self, written_ty: &TypeRef) -> Option<InferTy> {
```

Good example with a small explicit example:

```rust
/// Returns the oldest block that must remain in the replay WAL.
///
/// For `latest = 120` and `blocks_to_retain = 20`, block 100 may be compacted into state while
/// blocks 101 through 120 remain replayable from the WAL.
```

Good rare complex function example:

```rust
/// Solve obligations exposed by one already-selected generic call.
///
/// Continuing `bar.iter().collect::<Vec<_>>()`, this lowers collect's where-clause into the
/// goal `Vec<?T>: FromIterator<IterItem>` and commits the resulting `?T = IterItem` only when
/// exactly one visible impl proves the goal.
pub(super) fn solve_selected_call(
```

## Walkthrough Comments Inside Functions

Even if a function is simple to describe, it can be complex inside.
For cases, where the function is complex inside, consider adding short walkthrough comments before logical steps. These comments explain phases, not "call this method" mechanics.

Good walkthrough example:

```rust
pub(super) fn run(mut self) -> Result<(), PackageStoreError> {
    // 1. Mark `T` as `?T` in contexts where local evidence may infer it later.
    // Without this step, those positions stay as plain `Ty::Unknown`.
    self.instantiate_inference_facts()?;

    // 2. Propagate `?` markers through expressions that depend on instantiated children.
    self.refresh_inference_dependent_expr_facts()?;

    // 3. Run inference: observe available evidence and solve `?T` where possible.
    self.constrain_expected_types()?;

    // 4. Write inferred facts back into Body IR as ordinary `Ty` values.
    self.finalize_facts();
    Ok(())
}
```

These comments would still be useful if the steps were not inlined, because they describe the logical flow rather than the exact code shape.

## Choosing What To Document

Not everything needs documentation. Skip comments that would only restate obvious constructors, getters, or simple helper shorthands.

Usually too trivial:

```rust
pub fn new(config: Config) -> Self
```

```rust
pub fn block_number(&self) -> BlockNumber {
    self.block_number
}
```

If the only reasonable comment repeats the signature, type name, or a widely known Rust pattern, omit it. If there is meaningful local context, even one line is often better than no comment.

Private methods can deserve doc comments when they carry non-obvious context, because short `///` comments appear in LSP hovers. Still avoid documenting trivial private constructors, getters, and obvious helpers.

## Proofreading

When proofreading an existing comment, do not replace its shape unless the shape is the problem.
Prefer a small patch over a rewrite. If the original uses a list, example, or deliberately plain
wording, keep that structure and only remove real confusion.

## Planning Order

When documenting a file, plan in this order:

1. Does the file have a main entrypoint, such as a struct? It probably deserves the main doc comment of the file.
2. Does the file represent a scope of work that logically extends beyond the current item? Add a module-level doc comment.
3. Which methods or functions require complex reasoning? Give these more detailed doc comments.
4. Which methods, fields, or enum variants are not complex but benefit from context? Use one-line comments.
5. Which methods or functions are complex inside? Add walkthrough comments before logical phases.

## Style Principles

- Write comments to make the module easier to read, not to maximize documentation coverage.
- Explain the why, role, or reader-facing context. Avoid comments that merely narrate obvious code.
- Keep prose simple and informal. Avoid walls of text, heavy phrasing, and excessive adjectives.
- Do not turn comments into formal specifications or a second version of the code.
- Prefer examples when they clarify a non-obvious transformation.
- Avoid stale-prone words such as "currently" unless the temporary state is exactly the point.
- Match surrounding documentation density and style when it is healthy; improve it gently when the scope asks for documentation cleanup.
