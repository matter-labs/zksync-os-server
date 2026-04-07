use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Converts a block's L2 unix timestamp into an absolute `tokio::time::Instant` at which
/// the batch containing that block should be sealed.
///
/// The deadline is `first_block_timestamp + batch_timeout`, expressed as an absolute wall-clock
/// instant. This makes the deadline restart-resilient: it is derived from the block timestamp
/// (which is deterministic and part of the chain state), not from `std::time::Instant::now()`
/// at the moment the batch was opened.
pub fn deadline_from_block_timestamp(
    block_timestamp: u64,
    batch_timeout: Duration,
) -> (Instant, u64) {
    let deadline_unix = block_timestamp.saturating_add(batch_timeout.as_secs());

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs();

    if deadline_unix <= now_unix {
        // Deadline already passed (e.g. replaying old blocks after a restart).
        // Seal as soon as possible — once catch-up replay is complete.
        (Instant::now(), now_unix)
    } else {
        (
            Instant::now() + Duration::from_secs(deadline_unix - now_unix),
            deadline_unix,
        )
    }
}
