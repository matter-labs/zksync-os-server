## Summary

<!-- Briefly explain what this PR does. What problem does it solve? -->

## Checklist
- [ ] I considered if this PR has breaking changes — if it does, included a section below and title starts with "!"
- [ ] I considered if this PR needs rollout instructions — if it needs them, included a section below

<!--
READ BEFORE TICKING THE BOXES ABOVE.

BREAKING CHANGE — either of:
  (a) Not trivially revertible: rolling back the binary alone is NOT enough;
      reverting requires extra steps (DB schema migration, persisted-data /
      state format change, wiring/wire-protocol change, etc.).
  (b) Semver-major per conventional commits (title `feat!:`, `fix!:`, …):
      removed/renamed public API/RPC/CLI flag, changed default behaviour,
      any backwards-incompatible interface change.

ROLLOUT INSTRUCTIONS — anything the person deploying MUST know.
  INCLUDE:
    - config changes (incl. secrets): new config, removed config, or changed
      interpretation of an existing config.
    - operator wallet usage: e.g. a wallet now needs more funds.
    - new/removed services (ports)
  DO NOT INCLUDE:
    - new json-rpc methods, other strictly user-facing behaviour changes
    - metric changes and other diagnostics changes.
-->

<!-- UNCOMMENT if this is a "breaking change"
## Breaking Changes
- Who is affected? (protocol, EN users, main node, …)
- What breaks, and why it isn't trivially revertible?
- Migration steps required for consumers.
- Links to docs / migration guides.
-->

<!-- UNCOMMENT if this needs to be understood by deploying engineer.
## Rollout Instructions
- Config / secret changes (added / removed / reinterpreted).
- Operator wallet / funding changes.
- new / removed ports
- Order of operations (deploy order, migrations).
- Monitoring / alerting to watch.
- Rollback plan. (for braking changes)
-->
