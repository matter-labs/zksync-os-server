# PR Review Patterns & Common Mistakes

Compiled from review comments across **all 579 merged PRs** in `matter-labs/zksync-os-server` (PRs #2–#925). These are recurring themes that reviewers flag consistently.

**Key reviewers:** `itegulov`, `perekopskiy`, `slowli`, `popzxc`, `RomanBrodetski`, `EmilLuta`, `Deniallugo`, `mm-zk`, `cytadela8`, `vladbochok`, `antonbaliasnikov`

---

## 1. Avoid Unnecessary Cloning

Reviewers consistently flag unnecessary `.clone()` calls. Prefer moves, references, or `into_iter()`.

**Common mistakes:**
- Using `.iter().cloned()` or `.clone()` when `.into_iter()` would suffice
- Cloning values that could be moved further down the control flow
- Cloning inside hot loops when a reference works

**Examples from reviews:**
- PR #848: *"no need to clone here if you do `into_iter()`"* — itegulov
- PR #849: *"Please avoid unnecessary cloning, I am pretty sure you can move this line further down the flow so that you won't need to clone"* — itegulov
- PR #806: *"should be possible to do without clone"* — perekopskiy
- PR #779: *"Arithmetic operations are defined on `Ratio` references, too, so it's possible to use expressions like `&new_ratio - &old_ratio`"* — slowli
- PR #849: *"Remove `clone()` and make `tx_type()` return `&SystemTxType`. This would be conventional to 'the Rust way' so that the caller can choose to clone if need be"* — itegulov
- PR #122: *"No need to clone here I think"* — itegulov

**Rule:** Return references from getters. Let callers decide whether to clone. Use `into_iter()` when consuming a collection. Move values to their final use site instead of cloning early.

---

## 2. Error Handling — Be Idiomatic and Informative

Reviewers want proper Rust error handling patterns, not panics or swallowed errors.

**Common mistakes:**
- Using `todo!()` panics in reachable runtime code paths
- Swallowing the first error when trying multiple decode strategies
- Generic error messages that don't explain what went wrong
- Using `.expect()` / `.unwrap()` on untrusted or external input
- Returning `None` silently instead of an error

**Examples from reviews:**
- PR #806: *"Let's just do `return Poll::Ready(None)` here. These todo panics are pretty annoying in runtime, no idea why we keep using them"* — itegulov
- PR #849: *"This method should be rewritten to provide better debugging info. Right now the first error is always swallowed even if that's the error with proper context. Check first 4 bytes to match one of the ABI selectors and then try decode for that specific method."* — itegulov
- PR #867: *"This should be at least an `.expect("good message on what happened here")`"* — EmilLuta
- PR #779: *"Can be replaced with `.with_context` to be more idiomatic"* — slowli
- PR #779: *"Can be more idiomatically expressed with `anyhow::ensure!(..)`"* — slowli
- PR #631: *"Replace `.expect()` with proper error handling for `proving_version()` conversion... Since this is a remote API endpoint handling untrusted input from provers, panicking on invalid input is inappropriate."* — coderabbitai
- PR #631: *"Mapping `SubmitError::ProvingVersionMismatch` to a `panic!` in an HTTP handler means a malformed / unexpected interaction can crash the process. Even if you believe it's unreachable, it's safer to return a 4xx/5xx here"* — coderabbitai
- PR #203: *"hmm not critical, but surprised we just return `None` in such cases — wouldn't it make sense to return an error?"* — RomanBrodetski
- PR #332: *"I went with if and anyhow::bail to return the Error instead of panicking"* — dimazhornyk; RomanBrodetski responds: *"there is `anyhow::ensure`"*
- PR #470: *"we log error, but client doesn't know anything about it, and we just return incomplete results. Instead, we should return a proper error and corresponding StatusCode."* — RomanBrodetski
- PR #547: *"it'll result in a panic/crashloop if we misconfigure sentry. I suggest just logging a warning/error and returning something like 'Unexpected Level'"* — RomanBrodetski

**Rule:** Use `?` with `.context()` / `.with_context()`. Use `anyhow::ensure!()` for preconditions. Never use `todo!()` in production paths. Never `.unwrap()` / `.expect()` on untrusted input — return proper HTTP errors. When trying multiple decode strategies, check a discriminant first rather than swallowing errors.

---

## 3. Naming Must Be Precise and Unambiguous

Reviewers care deeply about naming. Names should be precise, non-colliding, and follow Rust/project conventions.

**Common mistakes:**
- Using the same name for different concepts (e.g., two different `tx_type()` methods returning different types)
- Naming something that returns a number as if it returns the entity (e.g., `first_block()` vs `first_block_number()`)
- Overly verbose names when the context is already clear
- Ambiguous prefixes (e.g., `l2_` when `sl_` or `gateway_` is meant)
- Using `_pk` for private keys (it's commonly read as "public key")
- Implementation-specific names leaking to user-facing output (e.g., "Fake" in production logs)

**Examples from reviews:**
- PR #849: *"IMHO we should rename this to something else as it collides with the other `tx_type()` that returns `ZkTxType`. This is more of a `system_subtype`"* — itegulov
- PR #886: *"consider naming it as `sl_pubdata_price_statistics` (or even `gateway_`?) as by `l2_` we usually refer to the chain running on this node"* — itegulov
- PR #827: *"I'd still go for `first_block_number` because it returns the number, not block"* — RomanBrodetski
- PR #779: *"`_pk` suffix is commonly used for public keys; private keys commonly have `_sk` suffix"* — slowli
- PR #662: *"we never use term `block_height` we always say `block_number`. Please change for consistency"* — RomanBrodetski
- PR #548: *"we use the term `process` for two different meanings... a little confusing. I'd just rename `process_l1_blocks` to `handle_l1_blocks`"* — RomanBrodetski
- PR #659: *"I don't like the idea of using Fake here. This will be run on mainnet and we'll be seeing 'fake proofs' in logs, which is misleading and scary. Let's create a new enum variant — something like `AlreadySubmittedToL1`"* — RomanBrodetski
- PR #332: *"nit: this is a good name when used in the component itself, but may be a little confusing when used in this code... Maybe something like `batcher_subsystem_first_block_to_process`"* — RomanBrodetski
- PR #459: *"IMHO `verification_` prefix is unnecessary here due to this crate already being called `batch_verification`. I'd rename to `client.rs`, `request.rs` etc"* — itegulov
- PR #172: *"I know I used the term serial_id here first but we should really stick to one or the other (priority id is IMHO the better of the two)"* — itegulov
- PR #60: *"Time to rename this :) now it enriches commands with transactions — but also with the whole BatchContext. Maybe CommandBlockContextProvider?"* — RomanBrodetski
- PR #306: *"nit: filename — let's make it something like `monitoring_middleware.rs`"* — RomanBrodetski

**Rule:** Getters returning a number should include the type in the name (`_number`, `_count`). Avoid name collisions across traits/types. Use `sl_` for settlement layer, `l1_` for Ethereum L1, `l2_` for this chain. Use `_private_key` or `_sk` not `_pk`. Don't use internal/testing terminology (like "Fake") in production-visible enum variants or log messages. Be consistent with existing codebase terms (`block_number` not `block_height`, `priority_id` not `serial_id`).

---

## 4. Visibility — Keep Things Private by Default

Fields and methods should be `pub` only when necessary. Code should live in the right crate.

**Examples from reviews:**
- PR #849: *"Any reason to have this as `pub`? I think the intended usage should only go through `tx_type()` method"* — itegulov
- PR #849: *"I would also make both `inner` and `hash` private. Everything should be available through methods"* — itegulov
- PR #848: *"all `InteropTxPoolInner` methods can be non-`pub`"* — itegulov
- PR #608: *"Consider making InternalConfigManager fields private. Exposing `file_path` and `file_lock` as `pub` makes it easy for callers to bypass the intended API"* — coderabbitai
- PR #171: *"is this only used by the persist loop? Can we make the whole struct `pub(crate)`?"* — RomanBrodetski

**Rule:** Default to private. Expose through methods. If a field must be accessed externally, prefer a getter over `pub` fields, especially when there's computed/cached state.

---

## 5. Config Validation and Sentinel Defaults

Invalid configs should fail loudly at startup, not produce silent wrong behavior.

**Common mistakes:**
- Defaulting secret keys to known values without warnings
- Using `Option` for config fields that are always required under certain modes
- Not validating cross-field invariants
- Not using `SecretString` / `#[config(secret)]` for sensitive values
- Adding configs that aren't needed (increasing cognitive load)

**Examples from reviews:**
- PR #779: *"I feel like this should be a tagged enum instead, otherwise we do not express required fields accurately. You can choose `source: CoinMarketCap` but then omit `api_key`"* — itegulov
- PR #773: *"Is having a default value for this field temporary? If we need zero-configuration, maybe create a secret key randomly if not set"* — slowli
- PR #779: *"Please use `SecretString` instead of `String`"* — slowli
- PR #773: *"specify `#[config(secret)]` so that the value is zeroized on drop, not printed in logs"* — slowli
- PR #608: *"if `revm_consistency_checker_revert_on_divergence` = true but leaves `config_override_db_enabled` = false... you'll likely get stuck in a loop. I think these two variables should be merged into one"* — vladbochok
- PR #669: *"The `estimate_gas_pubdata_price_factor` field is an unvalidated f64... will panic when receiving negative, infinite, or NaN values"* — coderabbitai
- PR #407: *"lately I'm not of opinion that we should add everything imaginable to the configs as there is a price — even if it stays default (larger config files, more time to find what you need)."* — RomanBrodetski
- PR #165: *"do we need this as a config? What are the cases where we DON'T want this? Every config line increases overall config complexity"* — RomanBrodetski
- PR #387: *"it's poor experience for EN operator to set `bridgehub_address=none` in config manually"* — perekopskiy
- PR #603: *"let's use U128, alloy types expect fees to be in u128 range so large U256 shouldn't be allowed"* — perekopskiy

**Rule:** Use tagged enums for config variants with different required fields. Mark secrets with `SecretString` and `#[config(secret)]`. Validate invariants at startup with clear messages. Don't default sensitive fields to well-known values. Question whether a new config field is truly needed — only add it when there's a real use case for varying the value. Validate numeric ranges at parse time.

---

## 6. Concurrency — Watch for Race Conditions and Waker Bugs

Async/concurrent code gets thorough review. Stream/Future implementations are especially scrutinized.

**Common mistakes:**
- TOCTOU (time-of-check-time-of-use) race conditions
- Iterator-based polling that doesn't register wakers with `cx`
- Processing the same event twice due to partial batch processing
- Using broadcast channels when unbounded channels are needed (lagged = data loss)
- Missing lock coordination between readers and writers
- One slow consumer blocking all other consumers

**Examples from reviews:**
- PR #848: *"I think you have a race condition here... There is still a TOCTOU race condition. Just `send` and ignore error without checking for receiver count."* — itegulov
- PR #848: *"Your iterator is not setting a waker for `cx`. So the stream might not get polled again even when delay has already passed."* — itegulov
- PR #886: *"I think your approach is buggy because it can process the same event twice if there is a block with multiple events and one of them is not processable."* — itegulov
- PR #631: *"I believe this is a potential race condition. We should create the `Notified` struct inside the lock... otherwise, we might miss the wake up call altogether"* — EmilLuta
- PR #459: *"Shouldn't it be `select`? `join_all` will not exit if any of the futures exits, and I'm not sure if that's what we want."* — popzxc
- PR #459: *"hmm so this is a broadcast — doesn't this mean that one slow/unresponsive EN will block the whole process?"* — RomanBrodetski
- PR #608: *"`write_config_and_panic` uses `file_lock`, but `read_config` doesn't. If `read_config` is ever called concurrently with write, you can hit transient parse failures."* — coderabbitai

**Rule:** In `Stream::poll_next()`, always register wakers for all pending sources. Avoid TOCTOU by combining check-and-act atomically. Consider whether `Lagged` from broadcast channels is acceptable — if not, use unbounded channels. If processing events in batches, ensure partial failure doesn't cause re-processing. Use `select!` not `join_all` when any task failure should halt everything.

---

## 7. Struct Design and Type Reuse

Reviewers prefer leveraging existing types and clean abstractions over ad-hoc structs.

**Common mistakes:**
- Creating new wrapper structs when existing types (like `alloy::primitives::Sealed`) already serve the purpose
- Using `Option` on fields that are always present
- Not separating concerns between persisted types and in-memory types
- Returning tuples when a named struct would be clearer
- Putting trait bounds on struct definitions instead of impl blocks

**Examples from reviews:**
- PR #867: *"You can use `alloy::primitives::Sealed` and avoid creating a separate struct."* — perekopskiy
- PR #886: *"why `Option` if it seems to be always present?"* — itegulov
- PR #806: *"I think it will be a bit cleaner to have `enum PeekedBlockType { Upgrade(..), Interop(..), Regular }`"* — itegulov
- PR #759: *"Maybe it's time to introduce `ExecuteBatchData` struct? These tuples are getting out of hand"* — itegulov
- PR #162: *"would it maybe be better if we put block_output_hash inside PreparedBlockCommand?... I think we can make it non-optional in ReplayRecord"* — RomanBrodetski
- PR #162: *"so, the only place when it's None is genesis? Maybe put `B256::zero()` instead?"* — RomanBrodetski
- PR #409: *"for future — I wish these were enums instead of u32 (makes it easier to see in the tools, and not forgetting to update somewhere)"* — mm-zk
- PR #664: *"Trait bounds on struct definitions must be repeated everywhere the struct is used and are generally considered an anti-pattern in Rust. The bounds are already enforced on the impl block where they matter."* — coderabbitai
- PR #447: *"You might be able to get rid of `Source`/`Sink` if you make `Input` and `Output` a bit more generic. Instead of forcing them to be wrapped in `PeekableReceiver`/`mpsc::Sender` you implement `PipelineInput`/`PipelineOutput` (new traits)"* — itegulov

**Rule:** Check if alloy/reth already has a type for what you need. Use enums for mutually exclusive states. Don't use `Option` for always-present fields — use zero/sentinel values for edge cases. Name tuples when they're passed around. Put trait bounds on impl blocks, not struct definitions.

---

## 8. Serialization and Wire Format

Changes to serialized formats have outsized impact and require careful handling.

**Common mistakes:**
- Accidentally making breaking wire format changes without rollout planning
- Keeping `Deserialize` derives on types where deserialized defaults would be wrong
- Using `bincode` for types that may need migration (prefer `serde_json` for easier migration)
- Forgetting that ENs must upgrade before main node for wire format changes
- Not testing that old format still deserializes correctly

**Examples from reviews:**
- PR #803: *"I think the PR is breaking, basically this change means that all ENs must upgrade before the main node does"* — perekopskiy
- PR #803: *"I'd like to remove `Deserialize` derive, to avoid someone using it later and silently getting the default `hash`"* — perekopskiy
- PR #827: *"Perhaps it's better to use serde_json? It will be easier to do migrations"* — perekopskiy
- PR #459: *"Shouldn't we have some kind of handshake to ensure that both parties use the same version of the protocol?"* — popzxc
- PR #459: *"Also please modify workflow `check-EN-backwards-compatibility` that asserts wire format doesn't get changed"* — itegulov
- PR #414: *"Did you test this? Would make sense to add a unit test here that old format still works."* — itegulov
- PR #458: *"this will break prover <-> sequencer compatibility — the field name must stay the same. The struct name can change."* — RomanBrodetski
- PR #369: *"I still believe reusing 'protocol version' here is a bad thing (if only short-term). We now have two protocol versions in the system"* — itegulov

**Rule:** Any change to types stored in RocksDB or sent over the network is potentially breaking. Consider rollout order (ENs before main node). Remove `Deserialize` from types with non-trivial default fields. Prefer `serde_json` over `bincode` for DB storage. Add unit tests with known-good serialized bytes. Include protocol version handshakes in network protocols.

---

## 9. Comments — Add Them Where Needed, Remove Where Not

Comments should explain *why*, not *what*. Non-obvious invariants MUST be documented.

**Common mistakes:**
- Missing comments on non-obvious state machines or invariants
- Misleading comments that describe different behavior than the code
- Stale comments that refer to old behavior
- Missing `TODO` tracking (issues should be created)

**Examples from reviews:**
- PR #869: *"It took me a while to understand the purpose of StreamState. Please add a comment"* — perekopskiy
- PR #849: *"the comment is misleading as you can read it as if the chain's own ID got updated. Please mention SL here"* — itegulov
- PR #800: *"please leave a comment here why we decided to return `[0; N]`"* — itegulov
- PR #873: *"Not sure if you currently create issues for TODOs, but if you do — don't forget to do it."* — popzxc
- PR #284: *"nit: explain where '1' comes from"* — mm-zk (on magic number)
- PR #247: *"please add a comment here — why do we have this as a no-op?"* — RomanBrodetski
- PR #121: *"please add a comment about this — 'The first priority transaction to be retrieved here is the earliest one that wasn't executed on-chain yet...'"* — RomanBrodetski
- PR #692: *"Let's maybe add a comment that function is not monotonic if there is any block revert event."* — perekopskiy
- PR #565: *"please add detailed comments for node operator. Even for me it's not immediately clear why we have both `from_block` and `blocks_to_empty`"* — RomanBrodetski
- PR #223: *"nit: there is a comment 'only fake proof is supported for now' which is not true anymore"* — perekopskiy
- PR #631: *"Adding docstrings to all metrics would help with following up how they're used"* — EmilLuta

**Rule:** Comment on: non-obvious invariants, intentional design decisions (especially "why not X"), state machine semantics, biased `tokio::select!` ordering, magic numbers, semantic contracts (inclusive vs exclusive ranges, startup vs runtime meaning). Remove stale comments. Create issues for TODOs and reference the issue number.

---

## 10. Tests — Preserve Existing, Use Proper Tools

Test quality matters. Use type-safe tools and don't remove existing coverage.

**Common mistakes:**
- Replacing existing tests instead of adding new ones alongside
- Manually constructing ABI calls instead of using `sol!` macro
- Hardcoding addresses as hex strings instead of using `address!` macro
- Not testing wire format backwards compatibility
- Unreasonable timeouts in tests

**Examples from reviews:**
- PR #849: *"Please keep the original test and introduce a new one for `set_sl_chain_id`"* — itegulov
- PR #774: *"please use `sol!` and build the call with safety instead of reinventing it"* — Deniallugo
- PR #774: *"please use `address!`"* — Deniallugo
- PR #459: *"We should have a test with example encoding to make sure format doesn't change instead, similar to what is done for blockreplay"* — cytadela8
- PR #631: *"It would be fantastic to add some UT to this module."* — EmilLuta
- PR #640: *"Is 300s still needed? If not let's still leave something more sensible (30-60s)"* — itegulov
- PR #565: *"I am somewhat concerned by the lack of testing. Given the urgency, I'm not going to nitpick — but it will be great if they will be added in a follow-up"* — popzxc

**Rule:** Add new tests alongside existing ones. Use `sol!` macro for ABI encoding. Use `address!` macro for addresses. Use `?` for error handling in tests. Add snapshot tests for wire format stability. Use sensible test timeouts.

---

## 11. Logging and Observability

Use appropriate log levels and include useful context.

**Common mistakes:**
- Using `warn`/`error` where `info` is appropriate (they go to Sentry/alerting)
- Conversely, using `info` when `warn` is justified
- Using string interpolation instead of structured log fields
- Missing context (tx type, batch number, etc.) in log/panic messages
- Silently dropping data without any log

**Examples from reviews:**
- PR #459: *"I'd lessen this to `info!`, note that all `warn`/`error` logs go to sentry and hence to alerting"* — itegulov
- PR #459: *"please use `debug!(request_id, batch_number, "message")` syntax instead — we use very little string interpolation in logs and put all relevant data to params instead"* — RomanBrodetski
- PR #827: *"let's do warn?"* — RomanBrodetski (for unexpected but recoverable state)
- PR #849: *"for debugging purposes I'd provide system tx (sub)type here as well"* — itegulov
- PR #659: *"Silently dropping batches without logging makes debugging difficult"* — coderabbitai
- PR #612: *"Please replace it with `tracing::info` and remove newlines"* — popzxc
- PR #311: *"Ideally we'd even log what exact transactions are in-flight (e.g. 'commit batch #342 is in-flight')"* — RomanBrodetski
- PR #597: *"We want to `warn` only about unexpected stuff as it goes into sentry."* — perekopskiy; then vladbochok's rebuttal: *"Realistic scenario: We forget to update the dependency and skip consistency checks but think it is live. One warning is exactly what we want."* — perekopskiy concedes.
- PR #559: *"is it possible to do DEBUG level for 'our' components and INFO for everything else? This is what we want to run by default"* — RomanBrodetski

**Rule:** Use `warn`/`error` only for unexpected situations (they trigger Sentry alerts). Use structured logging: `tracing::info!(batch_number, block_number, "message")` — not format strings. Include entity identifiers. Log data being dropped or skipped. Use DEBUG for verbose/internal state.

---

## 12. Breaking Changes Awareness

Be conscious of deployment ordering and backwards compatibility.

**Common mistakes:**
- Changing replay wire format without version bump or migration
- Not considering that ENs and main nodes upgrade at different times
- Removing support for protocol versions that are still live
- Renaming fields in serialized structs (which breaks deserialization)

**Examples from reviews:**
- PR #803: *"I think the PR is breaking... all ENs must upgrade before the main node does"* — perekopskiy
- PR #822: *"we have this `is_live()` function for ProtocolSemanticVersion. Right now 29 returns true, but you've purged most of v29. Should we change that code?"* — EmilLuta
- PR #458: *"this will break prover <-> sequencer compatibility — the field name must stay the same"* — RomanBrodetski
- PR #631: *"Introducing `fri_job_timeout` with `alias = "job_timeout"` while leaving `snark_job_timeout` without an alias means legacy configs silently fall back to default"* — coderabbitai
- PR #158: *"we should formally define what 'breaking' means — it's very much non-trivial, but IMO vital long-term"* — RomanBrodetski

**Rule:** When changing wire formats, bump the version. Document upgrade ordering. Check `is_live()` when removing protocol version support. Test crash-restart scenarios. Field names in serialized structs are part of the API — renaming is breaking. When adding config aliases for backwards compat, provide aliases for ALL old names, not just some.

---

## 13. Avoid Unrelated Changes

Keep PRs focused. Unrelated changes should go in separate PRs.

**Examples from reviews:**
- PR #857: *"Looks like unrelated change?"* — itegulov
- PR #803: *"I only see `last_interop_event_index` added"* — perekopskiy (questioning stale/leftover changes)
- PR #459: *"Please add PR description and a link to ticket"* — itegulov

**Rule:** Each PR should have a single purpose. Remove leftover debugging or experimental code. Add PR descriptions with ticket links.

---

## 14. Rust Idioms Checklist

Recurring micro-level feedback from reviewers:

| Pattern | Preferred | Avoid |
|---------|-----------|-------|
| Paths | `PathBuf` / `Path` | `String` for file paths |
| String params | `&str` | `&String` |
| Unwrap errors in tests | `.unwrap_err()` | `match` on `Err` |
| Min of multiple values | `[a, b, c].into_iter().min()` | Nested `if` chains |
| Enum display | `impl Display` or `as_str()` | Inline match in format strings |
| Derive traits | Include `Copy` for small types, `Debug` always | Missing derives |
| Static strings | `impl Display` for types | `String::leak()` for metrics labels |
| Field ordering in `mod.rs` | `mod` declarations, blank line, re-exports | Interleaved mods and re-exports |
| `ready!` macro | `ready!(future.poll(cx))` | Manual match on `Poll` |
| Match guards | `if let` chains | Deep nesting with `match` inside `if` |
| Self-documenting expressions | `1u64 << 32` | magic number + comment |
| Immutable bindings | `let x = if { ... } else { ... };` | `let mut x; if { x = ... }` |
| Unnecessary async wrappers | `.boxed()` directly | `async move { x.await }` |
| Trait bounds | On `impl` blocks | On struct definitions |
| Byte types for hashes/IDs | `Bytes` | Hex-encoded `String` |
| Checked arithmetic | `checked_div` / `checked_sub` | Raw `-` on `u64` that could underflow |
| Import re-exports | Use crate's public API | Internal/private module paths like `alloy::consensus::private::serde_json` |

---

## 15. Pipeline and Recovery Safety

When adding new behavior to the batcher pipeline:
- Trace every `Passthrough` code path — not just `SendToL1`
- Consider mode transitions (what happens when operator switches config on a running chain?)
- Test restart scenarios: "what if server crashes after step X but before step Y?"
- Compare stored state vs discovered state when recovering
- Ensure gapless ordering invariants hold

**Examples from reviews:**
- PR #827: *"shall we maybe compare what is stored vs what is discovered?"* — RomanBrodetski
- PR #918: *"We should check as many fields as we can, `block_output_hash` doesn't guarantee that other fields are equal"* — perekopskiy
- PR #582: *"`L1SenderCommand` can be a `Passthrough` — this happens when we are restarting server and rescheduling batches... So I guess we should add a match and don't block the PassThroughs."* — RomanBrodetski
- PR #631: *"If upstream ever emits commands starting from a different batch... this component could buffer indefinitely"* — coderabbitai
- PR #332: *"can we make the condition stronger and assert that replay_record.block_context.block_number == self.first_block_to_process? In fact, we could have such assert on each incoming block"* — RomanBrodetski
- PR #477: *"I will revisit all buffer sizes to be more deliberate. Just putting 5 everywhere is not a good approach"* — RomanBrodetski

---

## 16. Crate/Module Placement — Code in the Right Place

Reviewers enforce the layered architecture strictly. Code should live in the appropriate crate.

**Common mistakes:**
- Putting business logic in `node/bin` (the binary entrypoint)
- Putting domain-specific types in the low-level `types` crate
- Putting high-level types in low-level crates like `storage_api` or `merkle_tree`
- Not extracting shared types into dedicated crates (e.g., `batcher-types`)

**Examples from reviews:**
- PR #459: *"Please don't put this in `node/bin`. Most things that are currently there shouldn't actually be there, we just haven't had time to refactor them out"* — itegulov
- PR #459: *"This definitely does not belong inside `storage-api`"* — itegulov
- PR #459: *"This does not belong to `merkle_tree` either. It's way more high-level than what `merkle_tree` operates on."* — itegulov
- PR #459: *"`types` crate should only contain basic types and this is a lot more domain-specific. I propose you move this and maybe some other batcher-related types into a separate crate..."* — itegulov
- PR #631: *"Does this component really belong to prover_api?"* — EmilLuta
- PR #631: *"I think we're making it harder to do it in the future, rather than just doing it now. Moving this file once is low effort, but the more we accumulate, the worse it gets."* — EmilLuta
- PR #409: *"We are bringing proving-related stuff into multivm crate — this is a worrying sign... This is a module providing unified interface for running blocks. From my POV it was not meant to understand the nuances of proving."* — itegulov

**Rule:** `types` = basic types only. `storage_api` = storage interfaces. `node/bin` = wiring/composition only, no business logic. Domain-specific types go in domain crates (e.g., `batch_types`). Don't let crate boundaries erode — move code now, not later.

---

## 17. Unbounded Buffers and Resource Exhaustion

In-memory collections that can grow without bound are a serious concern.

**Examples from reviews:**
- PR #459: *"It seems like a bad idea to grow this unbounded."* — itegulov
- PR #631: *"Should we apply some sort of backpressure here? What happens if we get all proofs but only the second is missing... I'm concerned the server will OOM."* — EmilLuta
- PR #631: *"if a batch is missing, all subsequent batches will accumulate in the `BTreeMap` buffer indefinitely, potentially causing OOM."* — coderabbitai
- PR #459: *"I think ENs should support the mode where they are ready to verify batches — but don't actually get any verification requests. Currently they would aggregate blocks indefinitely."* — RomanBrodetski
- PR #631: *"we have a similar situation in `gapless_committer`. We need a way — not based on backpressure — to limit block/batch production when there are delays."* — RomanBrodetski

**Rule:** Every in-memory buffer must have a bounded size or a documented reason for being unbounded. Add metrics gauges for in-memory collection sizes to detect leaks. Consider what happens when one pipeline stage stalls — will upstream OOM?

---

## 18. Memory Leaks — Beware `Box::leak`

Using `Box::leak` to satisfy `&'static str` lifetime requirements creates memory leaks when called on every request.

**Examples from reviews:**
- PR #631: *"Fix memory leak: `Box::leak` on every call. Line 82 leaks the `prover_id` string on every invocation. With frequent polling, this accumulates unbounded memory."* — coderabbitai
- PR #631: *"Line 315 requires `prover_id: &'static str`, which forces callers to either use string literals or leak memory. This is inconsistent with `submit_proof` which accepts `&str`."* — coderabbitai

**Rule:** Never use `Box::leak` on per-request data. Use `String` / `&str` in APIs. For metrics labels, use `impl Display` or pre-compute the static string once.

---

## 19. Metrics Design

Metrics require careful design to be useful and not harmful.

**Common mistakes:**
- Unbounded label cardinality from dynamic/untrusted values
- Stale gauge values when queues empty
- Breaking changes to metric names/labels without dashboard updates
- Missing metric documentation

**Examples from reviews:**
- PR #631: *"If we want to use multiple provers with pseudo-random string names, the metrics cardinality will explode"* — EmilLuta
- PR #631: *"`prover_id` originates from untrusted HTTP query parameters and is used as-is in Prometheus label keys... unbounded cardinality risk."* — coderabbitai
- PR #631: *"`block_number` moved from a labeled family to a plain `Gauge<u64>`. Any consumers expecting `execution_block_number{stage="execute"}` will now see `execution_block_number` without labels. Make sure dashboards/alerts are updated"* — coderabbitai
- PR #631: *"let's set the `latency_tracker` to `WaitingSend` here — otherwise on grafana this component will show 'Processing' even though it's throttled by downstream."* — RomanBrodetski
- PR #137: *"let's have a Gauge that shows the number of transactions in memory. Nice to have for each in-memory repo... I want to make sure we don't 'forget' to remove transactions and it doesn't grow unbounded."* — RomanBrodetski
- PR #98: *"oh I like this `_observer` suffix. Maybe something like `_timer`. Ideally we'd update it everywhere to be consistent"* — RomanBrodetski

**Rule:** Never use untrusted/dynamic strings as metric labels. Document what each metric measures. When renaming metrics, update dashboards. Add gauges for in-memory data structure sizes. Use consistent naming suffixes (`_timer`, `_observer`).

---

## 20. Code Duplication — Extract When It Hits 3+

Reviewers push hard against duplicated logic, especially when patterns appear 3+ times.

**Examples from reviews:**
- PR #201: *"ok, this is the **fourth** time we have this in our code."* — RomanBrodetski
- PR #459: *"I noticed a lot of copy-pasted code from relay transport. I think it makes sense to refactor common logic into a generic TCP listener component"* — itegulov
- PR #610: *"Nit: looks like we now have a bunch of places with binary search implementation, would be nice to provide a generic interface"* — popzxc
- PR #610: *"this file becomes more and more messy, I'd love us to do some refactoring and split the logic across multiple files"* — popzxc

**Rule:** If a pattern appears 3+ times, extract it. Don't copy-paste between similar protocol implementations — factor out the common base. Large files (like `node/bin/src/lib.rs`) should be split.

---

## 21. Graceful Shutdown and Lifecycle

How components start, stop, and handle failure matters.

**Common mistakes:**
- Using `join_all` when `select!`/`try_join_all` is needed (one failed task continues silently)
- Not propagating `stop_receiver` signals
- Crash-loop potential from config mismatches
- Not syncing to disk before panic

**Examples from reviews:**
- PR #185: *"Using `join_all()` will wait for all tasks to complete, but if any task fails, the others continue running. Consider using `try_join_all()` or implementing proper error handling"* — Copilot
- PR #608: *"Sync file to disk before panicking to avoid truncated/corrupt config"* — coderabbitai
- PR #608: *"if revert_on_divergence = true but config_override_db_enabled = false... you'll likely get stuck in a loop"* — vladbochok

**Rule:** Use `select!` or `try_join_all` for task groups where any failure should halt everything. Sync durably before intentional panics. Test that config combinations don't create crash loops.

---

## 22. Arithmetic Safety

Watch for integer underflow/overflow, especially with unsigned types.

**Examples from reviews:**
- PR #662: *"When the cache is empty, `block_number - 1` would compute as `u64::MAX` due to wrapping subtraction"* — coderabbitai
- PR #610: *"you can do `checked_div` and then `Option::inspect`"* — popzxc
- PR #787: *"Why `max(1)` here?"* — itegulov (answer: to avoid division by zero / EIP-1559 price stuck at 0)

**Rule:** Use `checked_sub`, `checked_div`, `saturating_sub` for arithmetic that could underflow/overflow. Document why `max(1)` or similar guards exist.

---

## 23. Hardcoded Constants and Magic Numbers

Hardcoded values that will need to change with protocol upgrades should be centralized.

**Examples from reviews:**
- PR #284: *"side comment — I wish we didn't have to change it on each re-genesis."* — mm-zk
- PR #762: *"could we have one constant with current protocol version and change only this constant? Now I know at least 3 strings where I have to change `v30` to `v31`"* — Deniallugo
- PR #411: *"No need to hardcode this — you can get bridgehub address by calling `l2_zk_provider.get_bridgehub_contract()`"* — itegulov
- PR #639: *"The block range (29544..=29644) and the offset (100) lack documentation"* — coderabbitai

**Rule:** Centralize version-dependent constants. Don't hardcode addresses that can be queried from contracts. Document magic numbers. Environment-specific workarounds need explicit safeguards and TODOs for removal.

---

## 24. Dependency Management

Cargo dependency hygiene is actively reviewed.

**Common mistakes:**
- Depending on branch refs instead of tagged releases
- Coupling to internal/private re-exports of transitive dependencies
- Stale dependencies in Cargo.toml after import removal
- Inconsistent dependency ordering in Cargo.toml

**Examples from reviews:**
- PR #201: *"is there a reason why you depend on a branch rather than a new tag? (we should really avoid it when possible)"* — mm-zk
- PR #608: *"Avoid relying on `alloy::consensus::private::serde_json` re-export. Using an internal/private module is more likely to break on dependency upgrades."* — coderabbitai
- PR #762: *"We need to remove `zksync_os_server` from dependencies now then."* — antonbaliasnikov (stale dep after import removal)
- PR #459: *"Nit: we usually follow this convention: `[dependencies] <local crates> <zksync-os crates> <everything else>`"* — itegulov

**Rule:** Use tagged releases, not branch refs. Don't import from internal/private modules of dependencies. Clean up Cargo.toml when removing imports. Follow the dependency ordering convention: local crates first, zksync-os crates second, external crates last.

---

## 25. Operation Ordering and State Consistency

When a function performs multiple side effects, the order matters critically.

**Examples from reviews:**
- PR #624: *"Shouldn't we forward transaction before we add it to the mempool? If rejected by the main node, we don't want it to remain in the local pool."* — popzxc
- PR #624: *"If forwarding fails, the transaction remains in the local mempool while the RPC returns an error. This creates 'zombie' transactions."* — coderabbitai

**Rule:** When performing multiple side effects, consider: "what if step 2 fails after step 1 succeeds?" Put the fallible/validating step first. Ensure cleanup on partial failure.

---

## 26. Invariant Documentation / Cross-Component Contracts

Document assumptions about values, ordering, and ranges so future developers understand cross-component contracts.

**Examples from reviews:**
- PR #659: *"Documenting that this is an inclusive L1 cutoff ('highest batch already committed on L1 at startup') and is not updated during runtime."* — coderabbitai
- PR #137: *"please add a comment with the semantics of this Sender. Is it updated before or after db? Does it show a number that is AT MOST db or AT LEAST db?"* — RomanBrodetski
- PR #121: *"so we will start from the first l1 transaction that is not executed yet... please add a comment about this"* — RomanBrodetski
- PR #213: *"It looks weird to use `>` for blocks and `>=` for everything else, is it intended?"* — perekopskiy

**Rule:** Document inclusive vs exclusive semantics for ranges. Document whether a value is updated at startup only or continuously. Document ordering guarantees (before/after DB write). Use consistent comparison operators (`>` vs `>=`) and document when inconsistency is intentional.

---

## 27. Function Size and Abstraction Level

Large functions that mix business logic with implementation details should be decomposed.

**Examples from reviews:**
- PR #370: *"this method is now over 200 LoC. Others are over 100. Not necessarily in this PR, but maybe makes sense to refactor?"* — RomanBrodetski
- PR #370: *"Ideally we should have the high-level function that is easy to follow for business logic — and all the implementation details are encapsulated elsewhere"* — RomanBrodetski
- PR #191: *"Why do you need a new method instead of returning `max(in_memory_block, db_block)` in `get_latest_block`?"* — perekopskiy

**Rule:** Keep functions focused. Extract helper functions when a method exceeds ~100 lines. Consider whether a new method is needed vs adjusting an existing one.

---

## Key Reviewers and Their Focus Areas

| Reviewer | Primary Focus |
|----------|--------------|
| **itegulov** | API correctness, naming, unnecessary cloning, concurrency bugs, convenience methods, crate boundaries, test quality |
| **perekopskiy** | Protocol correctness, wire format safety, naming precision, config validation, fee model, comparison semantics |
| **slowli** | Rust idioms, error handling, config design, type safety, metric patterns, architecture |
| **popzxc** | Documentation, code organization, encapsulation, TODO tracking, network protocol design, shutdown safety |
| **RomanBrodetski** | Logging/observability, comments, recovery safety, struct naming, invariant documentation, metrics design, operational safety |
| **EmilLuta** | Error handling clarity, migration safety, unnecessary code, test coverage, crate placement, resource exhaustion |
| **Deniallugo** | Test quality, type-safe macros, version management, constant centralization |
| **mm-zk** | Magic numbers, enum vs integers, protocol upgrades, deployment safety |
| **cytadela8** | Wire format testing, code duplication, error handling in verification flows |
| **vladbochok** | Config safety, crash-loop prevention, warn-vs-info for safety-critical checks |
