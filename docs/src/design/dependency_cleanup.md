# Dependency Cleanup

This note tracks temporary dependency refs introduced while integrating the V8 / native PIG path into `zksync-os-server`.

The goal is simple:

- stop depending on bot forks
- stop depending on long-lived branch refs
- stop carrying server-side patches for other repos
- keep only released tags / explicit upstream revs for supported lanes

## Current temporary state

### `zksync-os-server`

The server currently carries four kinds of temporary dependency state in `Cargo.toml`:

1. Historical / previous / current pre-V8 `zksync-os` lanes point to bot-fork compatibility branches:
   - `antoniolocascio-bot/zksync-os:antonio/compat-nightly-2026-02-10-v0.0.29-interface-v0.1.3`
   - `...-v0.1.2-interface-v0.1.3`
   - `...-v0.2.10-simulation-only-interface-v0.1.3`
   - `...-v0.2.10-interface-v0.1.3`
   - `...-v0.3.1-interface-v0.1.3`
2. Direct old airbender deps point to a bot-fork compatibility branch:
   - `antoniolocascio-bot/zksync-airbender:antonio/compat-nightly-2026-02-10-v0.5.2`
3. The V8 lane points to an upstream branch instead of a tagged release:
   - `matter-labs/zksync-os:draft-0.4.0`
4. A `[patch."https://github.com/matter-labs/airbender-platform"]` section overrides `airbender-crypto` to the v0.2.3 commit via a bot fork:
   - `antoniolocascio-bot/airbender-platform@db2767d9ba871bb556ad8a2c8b4d39717f36cd47`

### `airbender-platform`

The server patch exists because the `airbender-crypto` port missed host bigint delegation behavior that existed in the last in-tree `crypto` crate from `zksync-os`.

That fix is merged and released upstream:

- merged in `matter-labs/airbender-platform@2418efaafd96139723b51c6ba51ae48ffce5e06c` (#76)
- released in `airbender-platform` v0.2.3 (`db2767d9ba871bb556ad8a2c8b4d39717f36cd47`, 2026-06-11)

The remaining problem is upstream: `zksync-os:draft-0.4.0` pins `airbender-platform` at `80c0541f6` (v0.2.2, pre-fix). Until that pin moves, the server overrides `airbender-crypto` to the v0.2.3 commit via `[patch]`. The patch goes through `antoniolocascio-bot/airbender-platform` only because Cargo forbids `[patch]` entries pointing at the patched source itself; the rev is the upstream v0.2.3 release commit.

Baseline for the regression:

- baseline: `matter-labs/zksync-os@852d63156552a6da1968c0998c70f60a1666f397`
- missing behavior:
  - host availability of `bigint_delegation`
  - host availability / export of `raw_delegation_interface`
  - non-RISC-V fallback implementation for bigint delegation
  - use of public `crate::BigInt` / `crate::BigInteger` aliases inside bigint delegation

### `zksync-os`

The V8 / native PIG lane is now consumed from the upstream `draft-0.4.0` branch, but not yet a tagged release:

- `matter-labs/zksync-os:draft-0.4.0`

That is acceptable while the integration is in flight, but it should not be the steady-state dependency. The branch also pins `airbender-platform` at the pre-fix `80c0541f6`, which is what forces the server-side `airbender-crypto` patch above.

### V8 proving artifacts

The V8 VK hash in `lib/types/src/protocol/proving_version.rs` is real (no longer a placeholder), but its provenance is raw revs, not releases:

- zksync-os `draft-0.4.0` + zksync-airbender rev `73d69b5` + zkos-wrapper rev `a9eec62` (security_80)

The V8 `multiblock_batch` app binary used for proving / VK generation is built locally from `draft-0.4.0` (`dump_bin.sh`); no release-tagged V8 app binary is published yet. Once tagged releases of zksync-os / zksync-airbender / zkos-wrapper exist for this lane, the VK provenance should reference them.

### `multivm` app sourcing

The server still carries build-script hacks in `lib/multivm/build.rs`:

- branch-name handling while locating `forward_system` binary sources
- a V6-specific remapping that downloads `v0.2.5` app binaries for the `v0.2.10-interface-v0.1.3` lane

Those are compatibility hacks, not the target design.

The V8 lane has no `build.rs` entry at all: native PIG does not consume downloaded app binaries on the server side.

## Cleanup targets by repo

### 1. `airbender-platform`

Done:

- v0.2.3 is released and contains `2418efaafd96139723b51c6ba51ae48ffce5e06c` (#76)

Required cleanup:

- `zksync-os:draft-0.4.0` should bump its `airbender-platform` pin from `80c0541f6` to v0.2.3 or later
- after that, `zksync-os-server` drops its `[patch."https://github.com/matter-labs/airbender-platform"]` section

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

The old `zksync-os` compat lanes additionally pull `antoniolocascio-bot/zksync-airbender:antonio/compat-nightly-2026-02-10-v0.4.3` transitively; it disappears together with those lanes.

End state:

- either consume upstream maintenance refs / replacement tags for still-supported lanes
- or drop these dependencies when the corresponding proving lanes are no longer supported

### 5. `zksync-os-server`

After the upstream repos are cleaned up, the server should do the following:

- replace bot-fork `zksync-os` refs with upstream tags / revs for the lanes we still support
- replace bot-fork `zksync-airbender` refs with upstream tags / revs for the lanes we still support
- switch the V8 lane from branch to tag / rev
- drop the `airbender-crypto` patch section once `draft-0.4.0` (or its release) pins `airbender-platform` v0.2.3+
- simplify `lib/multivm/build.rs` once branch-only handling is no longer needed
- remove the V6 binary remap hack once V6 proving support is gone

## Recommended cleanup sequence

1. Done: `airbender-platform` v0.2.3 contains `2418efaafd96139723b51c6ba51ae48ffce5e06c`.
2. Update the upstream `zksync-os` V8 / native PIG branch to consume that release, then drop the server's `airbender-crypto` patch section.
3. Cut a tagged `zksync-os` release for the V8 lane.
4. Update `zksync-os-server` V8 deps from the `draft-0.4.0` branch to the new upstream tag / rev.
5. Decide lane by lane whether pre-V8 compat branches deserve upstream maintenance refs or should just age out.
6. Remove `multivm` binary-source hacks as the old proving lanes leave the support window.

## Concrete deletion checklist

Delete these only after the corresponding upstream replacement exists or the lane is dropped:

- all `antoniolocascio-bot/zksync-os:antonio/compat-nightly-2026-02-10-*` refs in `Cargo.toml`
- the V8 lane `matter-labs/zksync-os:draft-0.4.0` branch ref in `Cargo.toml` (move to a tagged release once cut; the bot-fork `antonio/use-airbender-platform-2418efa` ref it replaced is already gone)
- `antoniolocascio-bot/zksync-airbender:antonio/compat-nightly-2026-02-10-v0.5.2` refs in `Cargo.toml`
- the `[patch."https://github.com/matter-labs/airbender-platform"]` section in `Cargo.toml` (delete once `draft-0.4.0` pins `airbender-platform` v0.2.3 or later)
- branch-name special-casing in `lib/multivm/build.rs`
- the V6 `download_tag = "v0.2.5"` remap in `lib/multivm/build.rs`

## Guiding rule

For server integration, prefer one of these outcomes:

1. supported lane -> upstream tag / explicit upstream rev
2. unsupported lane -> delete it

What we should not normalize is a steady-state dependency graph built out of bot forks, ad hoc patches, and long-lived branch refs.
