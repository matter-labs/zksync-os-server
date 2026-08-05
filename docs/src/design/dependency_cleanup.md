# Dependency Cleanup

This note tracks temporary dependency refs introduced while integrating the V8 / native PIG path
into `zksync-os-server`. Target end state: supported lanes consume upstream tags / explicit
upstream revs; unsupported lanes are deleted; no bot forks, no long-lived branch refs, no
server-side patches for other repos.

## Remaining temporary state

1. Pre-V8 `zksync-os` lanes (historical / previous / current) point to pinned heads of
   `antoniolocascio-bot/zksync-os` compat branches (`antonio/compat-nightly-2026-02-10-*`),
   which port the corresponding release tags to `nightly-2026-02-10`.
   Replace with upstream refs once the compat changes are merged into `matter-labs/zksync-os`
   (maintenance refs or patched tags per lane); lanes that leave the proving window can instead
   be dropped. The old lanes also pull `antoniolocascio-bot/zksync-airbender` compat revs
   (`v0.4.3` transitively, `v0.5.2` directly via `execution_utils` / `full_statement_verifier`),
   which need the same treatment in `matter-labs/zksync-airbender`.
2. The V8 lane consumes `matter-labs/zksync-os:draft-0.4.0` (branch, not a tagged release).
   Switch to the release tag once it is cut.
3. `[patch."https://github.com/matter-labs/airbender-platform"]` overrides `airbender-crypto`
   to the v0.2.3 release commit (host bigint delegation fix, #76) because `draft-0.4.0` still
   pins pre-fix `80c0541f6` (v0.2.2). The patch goes through `antoniolocascio-bot/airbender-platform`
   only because Cargo forbids `[patch]` entries pointing at the patched source itself; the rev is
   the upstream v0.2.3 release commit. Drop the section once `draft-0.4.0` (or its release) pins
   airbender-platform v0.2.3+.
4. `lib/multivm/build.rs` special-cases the compat branch names / revs when locating
   `forward_system` binary sources, and remaps V6 to `v0.2.5` app binaries. Simplify as the
   corresponding lanes are repinned or leave the support window.

## Resolved

- The V8 airbender / zkos-wrapper lineage is merged and tagged upstream: server pins
  `zksync-airbender` tag `v0.6.0-rc.1` (3f8f8e54, combined recursion layers); the V8 VK is
  generated with it and `zkos-wrapper` `v0.6.0-rc.1`, matching the V8 entry in
  zksync-airbender-prover. Bump to the final v0.6.0 release when cut.
- `airbender-platform` v0.2.3 is released and contains the host bigint delegation fix
  (`2418efaafd96139723b51c6ba51ae48ffce5e06c`, #76).
