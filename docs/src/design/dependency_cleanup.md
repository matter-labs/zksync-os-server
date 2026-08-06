# Dependency Cleanup

This note tracks temporary dependency refs introduced while integrating the V8 / native PIG path
into `zksync-os-server`. Target end state: supported lanes consume upstream tags / explicit
upstream revs; unsupported lanes are deleted; no bot forks, no long-lived branch refs, no
server-side patches for other repos.

## Remaining temporary state

1. Pre-V8 lanes ride the `matter-labs/zksync-airbender` compat branches
   (`compat-nightly-2026-02-10-v0.4.3` / `-v0.5.2` — airbender rebuilt for
   `nightly-2026-02-10`): directly via `execution_utils` / `full_statement_verifier`, and
   transitively via the zksync-os compat tags' own manifests. These are frozen branch refs,
   not releases; they get deleted together with V6/V7 proving support (airbender 0.6.0
   removed `risc_v_simulator`, so the old lanes cannot be ported forward).
2. The V8 lane consumes `matter-labs/zksync-os:draft-0.4.0` (branch, not a tagged release).
   Switch to the release tag once it is cut.
3. `[patch."https://github.com/matter-labs/airbender-platform"]` overrides `airbender-crypto`
   to the v0.2.3 release commit (host bigint delegation fix, #76) because `draft-0.4.0` still
   pins pre-fix `80c0541f6` (v0.2.2). The patch goes through `antoniolocascio-bot/airbender-platform`
   only because Cargo forbids `[patch]` entries pointing at the patched source itself; the rev is
   the upstream v0.2.3 release commit. Drop the section once `draft-0.4.0` (or its release) pins
   airbender-platform v0.2.3+.
4. `lib/multivm/build.rs` maps the v0.3.1 compat tag to the original release's app binaries.
   Remove when V7 proving leaves the support window.

## Resolved

- No `antoniolocascio-bot` sources remain in `Cargo.lock` except the `airbender-crypto`
  patch above: the airbender compat branches are mirrored into `matter-labs/zksync-airbender`,
  the zksync-os compat branches consume them, and the server's direct pre-V8 airbender pins
  use the same `compat-nightly-2026-02-10-v0.5.2` branch spec as the v0.3.1 lane so cargo
  unifies the sources.
- Pre-V8 `zksync-os` lanes now consume `matter-labs/zksync-os` tags
  (`v0.0.29`/`v0.1.2`/`v0.2.10[-simulation-only]`/`v0.3.1` `-interface-v0.1.3-2026-02-10`):
  nightly-2026-02-10 rebuilds of the corresponding `*-interface-v0.1.3` releases, mirrored
  from the bot-fork compat branches (same commits, no content change).
- The V8 airbender / zkos-wrapper lineage is merged and tagged upstream: server pins
  `zksync-airbender` tag `v0.6.0-rc.1` (3f8f8e54, combined recursion layers); the V8 VK is
  generated with it and `zkos-wrapper` `v0.6.0-rc.1`, matching the V8 entry in
  zksync-airbender-prover. Bump to the final v0.6.0 release when cut.
- `airbender-platform` v0.2.3 is released and contains the host bigint delegation fix
  (`2418efaafd96139723b51c6ba51ae48ffce5e06c`, #76).
