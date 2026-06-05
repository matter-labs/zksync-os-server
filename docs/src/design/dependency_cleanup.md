# Dependency Cleanup

This note tracks temporary dependency refs introduced while integrating the V8 / native PIG path into `zksync-os-server`.

The goal is simple:

- stop depending on bot forks
- stop depending on long-lived branch refs
- stop carrying server-side patches for other repos
- keep only released tags / explicit upstream revs for supported lanes

## Current temporary state

### `zksync-os-server`

The server currently carries three kinds of temporary dependency state in `Cargo.toml`:

1. Historical / previous / current pre-V8 `zksync-os` lanes point to bot-fork compatibility branches:
   - `antoniolocascio-bot/zksync-os:antonio/compat-nightly-2026-02-10-v0.0.29-interface-v0.1.3`
   - `...-v0.1.2-interface-v0.1.3`
   - `...-v0.2.10-simulation-only-interface-v0.1.3`
   - `...-v0.2.10-interface-v0.1.3`
   - `...-v0.3.1-interface-v0.1.3`
2. Direct old airbender deps point to a bot-fork compatibility branch:
   - `antoniolocascio-bot/zksync-airbender:antonio/compat-nightly-2026-02-10-v0.5.2`
3. The V8 lane points to a bot-fork branch instead of an upstream tag / rev:
   - `antoniolocascio-bot/zksync-os:antonio/use-airbender-platform-2418efa`

### `airbender-platform`

The original server patch existed because the `airbender-crypto` port missed host bigint delegation behavior that existed in the last in-tree `crypto` crate from `zksync-os`.

That fix has now been merged upstream in:

- `matter-labs/airbender-platform@2418efaafd96139723b51c6ba51ae48ffce5e06c`

The server no longer needs a direct `airbender-platform` patch. The remaining temporary state is that the V8 `zksync-os` lane points to a fork branch that already consumes this merged upstream ref.

Baseline for the regression:

- baseline: `matter-labs/zksync-os@852d63156552a6da1968c0998c70f60a1666f397`
- missing behavior:
  - host availability of `bigint_delegation`
  - host availability / export of `raw_delegation_interface`
  - non-RISC-V fallback implementation for bigint delegation
  - use of public `crate::BigInt` / `crate::BigInteger` aliases inside bigint delegation

### `zksync-os`

The V8 / native PIG lane is still consumed from a branch, not a tagged release:

- `antoniolocascio-bot/zksync-os:antonio/use-airbender-platform-2418efa`

That is acceptable while the integration is in flight, but it should not be the steady-state dependency.

### `multivm` app sourcing

The server still carries build-script hacks in `lib/multivm/build.rs`:

- branch-name handling while locating `forward_system` binary sources
- a V6-specific remapping that downloads `v0.2.5` app binaries for the `v0.2.10-interface-v0.1.3` lane

Those are compatibility hacks, not the target design.

## Cleanup targets by repo

### 1. `airbender-platform`

Required cleanup:

- cut a release tag that contains `2418efaafd96139723b51c6ba51ae48ffce5e06c`

After that:

- `zksync-os` should consume the upstream tag / rev
- `zksync-os-server` should move back from the fork branch to an upstream `zksync-os` ref that already includes it

Principle:

- native PIG must not require `proving` or `testing` just to make host bigint delegation compile or run

### 2. `zksync-os`

Required cleanup:

- merge the rebased V8 / native PIG branch upstream
- cut a tagged release for that lane
- make sure the tag points at the fixed `airbender-platform` release, not a temporary fork branch

Nice-to-have cleanup:

- if the release still needs checked-in or downloadable app binaries, publish them under a release tag
- if V8 no longer needs the old app-binary path, document that clearly and remove any dead assumptions downstream

### 3. Old `zksync-os` lanes

The bot-fork compatibility branches exist only because old lanes do not cleanly build on `nightly-2026-02-10`.

There are only two sane end states:

1. Upstream maintenance refs exist for still-supported lanes.
2. The unsupported lanes are deleted from the server as they age out.

What we should avoid:

- keeping permanent bot-fork compatibility branches in the server dependency graph

Decision by lane:

- `v0.0.29`, `v0.1.2`:
  - replay / simulation only
  - either replace with upstream maintenance refs or accept that they are not worth carrying forever
- `v0.2.10`:
  - previous proving lane today
  - when it leaves the proving window, demote it or delete the proving-specific deps
- `v0.3.1`:
  - current proving lane today
  - once V8 becomes current, either upstream a maintenance ref for its remaining support window or let it age out

### 4. `zksync-airbender`

The direct old-airbender server deps are also on bot-fork compatibility refs today:

- `execution_utils`
- `full_statement_verifier`

End state:

- either consume upstream maintenance refs / replacement tags for still-supported lanes
- or drop these dependencies when the corresponding proving lanes are no longer supported

### 5. `zksync-os-server`

After the upstream repos are cleaned up, the server should do the following:

- replace bot-fork `zksync-os` refs with upstream tags / revs for the lanes we still support
- replace bot-fork `zksync-airbender` refs with upstream tags / revs for the lanes we still support
- switch the V8 lane from branch to tag / rev
- simplify `lib/multivm/build.rs` once branch-only handling is no longer needed
- remove the V6 binary remap hack once V6 proving support is gone

## Recommended cleanup sequence

1. Cut an `airbender-platform` release containing `2418efaafd96139723b51c6ba51ae48ffce5e06c`.
2. Update the upstream `zksync-os` V8 / native PIG branch to consume that release.
3. Cut a tagged `zksync-os` release for the V8 lane.
4. Update `zksync-os-server` V8 deps from the fork branch back to the new upstream tag / rev.
5. Decide lane by lane whether pre-V8 compat branches deserve upstream maintenance refs or should just age out.
6. Remove `multivm` binary-source hacks as the old proving lanes leave the support window.

## Concrete deletion checklist

Delete these only after the corresponding upstream replacement exists or the lane is dropped:

- all `antoniolocascio-bot/zksync-os:antonio/compat-nightly-2026-02-10-*` refs in `Cargo.toml`
- `antoniolocascio-bot/zksync-os:antonio/use-airbender-platform-2418efa` in `Cargo.toml`
- `antoniolocascio-bot/zksync-airbender:antonio/compat-nightly-2026-02-10-v0.5.2` refs in `Cargo.toml`
- branch-name special-casing in `lib/multivm/build.rs`
- the V6 `download_tag = "v0.2.5"` remap in `lib/multivm/build.rs`

## Guiding rule

For server integration, prefer one of these outcomes:

1. supported lane -> upstream tag / explicit upstream rev
2. unsupported lane -> delete it

What we should not normalize is a steady-state dependency graph built out of bot forks, ad hoc patches, and long-lived branch refs.
