# Consensus-storage fixtures

`consensus-storage-v1.bin` is a frozen dump of a deterministic cluster run's
entire consensus-side storage (engine vote journals, marshal block/finalization
archives, caches, processed-height markers, and each validator's committed
chain). The replay gate in `../tests/replay_gate.rs` reopens it with the
current stack on every test run and requires the chain to resume — the
executable form of the on-disk compatibility claim in
`docs/src/consensus/upgrading.md`.

Do not regenerate casually: a changed fixture asserts a storage-format
migration happened. The policy and the regeneration command are at the top of
`../tests/replay_gate.rs`.
