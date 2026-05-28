# Finalized Poll Interval Design

## Summary

Add a new `finalized_poll_interval` setting to the L1 watcher config that controls how often finalized L1 blocks are polled. When this setting is not provided, it must fall back to the existing `poll_interval` so current configs keep the same behavior.

## Goals

- Allow finalized-block polling to use a different cadence than latest-block polling.
- Preserve existing config behavior when `finalized_poll_interval` is omitted.
- Keep downstream watchers unchanged by continuing to publish a single `BlockUpdates` snapshot.

## Non-Goals

- Changing watcher semantics beyond polling cadence.
- Refactoring watcher consumers or altering finality handling.
- Introducing a new config surface for components outside the L1 watcher path.

## Design

### Config

Add `finalized_poll_interval` to:

- `node/bin/src/config/mod.rs` `L1WatcherConfig`
- `lib/l1_watcher/src/config.rs` `L1WatcherConfig`

The node-facing config should deserialize `finalized_poll_interval` with a fallback to `poll_interval`. This preserves current behavior for existing YAML and environment-based configurations.

The conversion from node config into `zksync_os_l1_watcher::L1WatcherConfig` should pass both intervals explicitly.

### Block Updates Polling

Update `lib/l1_watcher/src/block_updates.rs` so `run()` accepts both:

- `poll_interval` for latest block polling
- `finalized_poll_interval` for finalized block polling

The spawned task should maintain one shared `BlockUpdates` state and refresh the relevant field when its timer fires. Watchers should continue receiving updates through the existing `watch::Receiver<BlockUpdates>` interface, with notifications only when the snapshot changes.

### Startup Wiring

Update `node/bin/src/lib.rs` so both L1 and Gateway `block_updates::run()` calls pass the base and finalized polling intervals from `config.l1_watcher_config`.

## Error Handling

Provider failures while fetching latest or finalized blocks should continue to be treated as fatal for the polling task, matching current behavior. The change only affects polling cadence.

If there are no subscribers, the polling task should continue to stop cleanly as it does today.

## Testing

Add focused regression coverage for config behavior:

- `finalized_poll_interval` falls back to `poll_interval` when omitted.
- `finalized_poll_interval` overrides `poll_interval` when explicitly set.

If the watcher implementation can support a small unit test without adding heavy abstractions, add one targeted test proving latest and finalized block updates can advance independently. Otherwise, config coverage plus compile verification is sufficient for this change.

## Implementation Notes

- Prefer the smallest change that keeps the current watcher API stable for consumers.
- Avoid changing unrelated polling or watcher config structures.
