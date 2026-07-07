//! Durable consensus-side storage: the archives backing marshal.
//!
//! Two archives exist per validator, both keyed by height and block digest:
//! - **finalizations**: the finality certificates for each height,
//! - **blocks**: the finalized blocks themselves.
//!
//! Together they let a node serve backfill requests to peers and replay finalized history
//! it has not yet applied. They are consensus-internal bookkeeping — the node's own notion
//! of the chain (its write-ahead log etc.) stays the application's responsibility — and
//! they are *prunable*: with an epoch-retention policy configured, marshal drops whole
//! sections of history below the retention horizon (the node's finality store keeps the
//! permanent certificate trail independently).
//!
//! Storage goes through the runtime's `Storage` abstraction, so the same code writes real
//! files in production and in-memory partitions (which survive simulated restarts) in
//! deterministic tests.

use crate::types::Scheme;
use commonware_consensus::Block;
use commonware_consensus::simplex::types::Finalization;
use commonware_cryptography::Digest;
use commonware_cryptography::certificate::Scheme as _;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_runtime::{BufferPooler, Clock, Metrics, Spawner, Storage};
use commonware_storage::archive::prunable;
use commonware_storage::translator::EightCap;
use commonware_utils::NZUsize;
use std::num::NonZeroU64;

/// Archive of finality certificates, keyed by height and block digest.
pub type FinalizationsArchive<R, D> = prunable::Archive<EightCap, R, D, Finalization<Scheme, D>>;

/// Archive of finalized blocks, keyed by height and block digest.
pub type BlocksArchive<R, B> =
    prunable::Archive<EightCap, R, <B as commonware_cryptography::Digestible>::Digest, B>;

/// Builds the archive config shared by both archives. `items_per_section` is the
/// pruning granularity: only sections entirely below a prune horizon are dropped.
fn archive_config<Cfg>(
    prefix: &str,
    name: &str,
    page_cache: CacheRef,
    codec_config: Cfg,
    items_per_section: NonZeroU64,
) -> prunable::Config<EightCap, Cfg> {
    prunable::Config {
        // Keys are block digests (uniformly distributed), so the eight-byte
        // translation keeps lookups O(1).
        translator: EightCap,
        key_partition: format!("{prefix}-{name}-key"),
        key_page_cache: page_cache,
        value_partition: format!("{prefix}-{name}-value"),
        compression: None,
        codec_config,
        items_per_section,
        key_write_buffer: NZUsize!(1024 * 1024),
        value_write_buffer: NZUsize!(1024 * 1024),
        replay_buffer: NZUsize!(1024 * 1024),
    }
}

/// Opens (or restores after a restart) the finality-certificate archive.
pub async fn init_finalizations_archive<R, D>(
    context: &R,
    partition_prefix: &str,
    page_cache: CacheRef,
    items_per_section: NonZeroU64,
) -> FinalizationsArchive<R, D>
where
    R: Storage + Metrics + Clock + Spawner + BufferPooler,
    D: Digest,
{
    prunable::Archive::init(
        context.child("finalizations_archive"),
        archive_config(
            partition_prefix,
            "finalizations",
            page_cache,
            Scheme::certificate_codec_config_unbounded(),
            items_per_section,
        ),
    )
    .await
    .expect("failed to initialize finalizations archive")
}

/// Opens (or restores after a restart) the finalized-blocks archive.
pub async fn init_blocks_archive<R, B>(
    context: &R,
    partition_prefix: &str,
    page_cache: CacheRef,
    block_codec_config: <B as commonware_codec::Read>::Cfg,
    items_per_section: NonZeroU64,
) -> BlocksArchive<R, B>
where
    R: Storage + Metrics + Clock + Spawner + BufferPooler,
    B: Block,
{
    prunable::Archive::init(
        context.child("blocks_archive"),
        archive_config(
            partition_prefix,
            "blocks",
            page_cache,
            block_codec_config,
            items_per_section,
        ),
    )
    .await
    .expect("failed to initialize blocks archive")
}
