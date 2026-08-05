# Dependency Cleanup

This note tracks temporary dependency refs introduced while integrating the V8 / native PIG path
into `zksync-os-server`. Target end state: supported lanes consume upstream tags / explicit
upstream revs; unsupported lanes are deleted; no bot forks, no long-lived branch refs, no
server-side patches for other repos.

## Remaining temporary state

1. Pre-V8 lanes still reach `antoniolocascio-bot/zksync-airbender` compat branches
   (airbender rebuilt for `nightly-2026-02-10`):
   - directly: `execution_utils` / `full_statement_verifier` pin rev `a69df6c` (head of
     `antonio/compat-nightly-2026-02-10-v0.5.2`);
   - transitively: the zksync-os compat tags' own manifests pin the `...-v0.4.3` and
     `...-v0.5.2` compat branches, so those sources appear in `Cargo.lock` regardless of
     server-side pins.
   Full cleanup needs the airbender compat branches mirrored into `matter-labs/zksync-airbender`
   (branch + tag), the zksync-os compat branches repointed at those upstream refs, and the
   `*-interface-v0.1.4` tags re-cut on the updated heads — or the lanes dropped when V6/V7
   proving support ends.
2. The V8 lane consumes `matter-labs/zksync-os:draft-0.4.0` (branch, not a tagged release).
   Switch to the release tag once it is cut.
3. `[patch."https://github.com/matter-labs/airbender-platform"]` overrides `airbender-crypto`
   to the v0.2.3 release commit (host bigint delegation fix, #76) because `draft-0.4.0` still
   pins pre-fix `80c0541f6` (v0.2.2). The patch goes through `antoniolocascio-bot/airbender-platform`
   only because Cargo forbids `[patch]` entries pointing at the patched source itself; the rev is
   the upstream v0.2.3 release commit. Drop the section once `draft-0.4.0` (or its release) pins
   airbender-platform v0.2.3+.
4. `lib/multivm/build.rs` maps the compat tags to the original releases' app binaries
   (including the V6 remap to `v0.2.5`). Remove entries as the corresponding proving lanes
   leave the support window.

## Resolved

- Pre-V8 `zksync-os` lanes now consume `matter-labs/zksync-os` tags
  (`v0.0.29-interface-v0.1.4`, `v0.1.2-interface-v0.1.4`, `v0.2.10[-simulation-only]-interface-v0.1.4`,
  `v0.3.1-interface-v0.1.4`): nightly-2026-02-10 rebuilds of the corresponding
  `*-interface-v0.1.3` releases, mirrored from the bot-fork compat branches
  (same commits, no content change).
- The V8 airbender / zkos-wrapper lineage is merged and tagged upstream: server pins
  `zksync-airbender` tag `v0.6.0-rc.1` (3f8f8e54, combined recursion layers); the V8 VK is
  generated with it and `zkos-wrapper` `v0.6.0-rc.1`, matching the V8 entry in
  zksync-airbender-prover. Bump to the final v0.6.0 release when cut.
- `airbender-platform` v0.2.3 is released and contains the host bigint delegation fix
  (`2418efaafd96139723b51c6ba51ae48ffce5e06c`, #76).
