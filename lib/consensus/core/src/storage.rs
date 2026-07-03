//! Durable consensus-side storage: the archives backing marshal.
//!
//! Two archives exist per validator, both keyed by height and block digest:
//! - **finalizations**: the finality certificates for each height,
//! - **blocks**: the finalized blocks themselves.
//!
//! Together they let a node serve backfill requests to peers and replay finalized history
//! it has not yet applied. They are consensus-internal bookkeeping — the node's own notion
//! of the chain (its write-ahead log etc.) stays the application's responsibility.
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
use commonware_storage::archive::immutable;
use commonware_utils::{NZU64, NZUsize};

/// Archive of finality certificates, keyed by height and block digest.
pub type FinalizationsArchive<R, D> = immutable::Archive<R, D, Finalization<Scheme, D>>;

/// Archive of finalized blocks, keyed by height and block digest.
pub type BlocksArchive<R, B> =
    immutable::Archive<R, <B as commonware_cryptography::Digestible>::Digest, B>;

/// Builds the archive config shared by both archives. The sizing constants are modest
/// defaults; they become configuration once production profiles exist.
fn archive_config<Cfg>(
    prefix: &str,
    name: &str,
    page_cache: CacheRef,
    codec_config: Cfg,
) -> immutable::Config<Cfg> {
    immutable::Config {
        metadata_partition: format!("{prefix}-{name}-metadata"),
        freezer_table_partition: format!("{prefix}-{name}-freezer-table"),
        freezer_table_initial_size: 64,
        freezer_table_resize_frequency: 10,
        freezer_table_resize_chunk_size: 1024,
        freezer_key_partition: format!("{prefix}-{name}-freezer-key"),
        freezer_key_page_cache: page_cache,
        freezer_value_partition: format!("{prefix}-{name}-freezer-value"),
        freezer_value_target_size: 1024 * 1024,
        freezer_value_compression: None,
        ordinal_partition: format!("{prefix}-{name}-ordinal"),
        items_per_section: NZU64!(1024),
        codec_config,
        replay_buffer: NZUsize!(1024 * 1024),
        freezer_key_write_buffer: NZUsize!(1024 * 1024),
        freezer_value_write_buffer: NZUsize!(1024 * 1024),
        ordinal_write_buffer: NZUsize!(1024 * 1024),
    }
}

/// Opens (or restores after a restart) the finality-certificate archive.
pub async fn init_finalizations_archive<R, D>(
    context: &R,
    partition_prefix: &str,
    page_cache: CacheRef,
) -> FinalizationsArchive<R, D>
where
    R: Storage + Metrics + Clock + Spawner + BufferPooler + Clone,
    D: Digest,
{
    immutable::Archive::init(
        context.with_label("finalizations_archive"),
        archive_config(
            partition_prefix,
            "finalizations",
            page_cache,
            Scheme::certificate_codec_config_unbounded(),
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
) -> BlocksArchive<R, B>
where
    R: Storage + Metrics + Clock + Spawner + BufferPooler + Clone,
    B: Block,
{
    immutable::Archive::init(
        context.with_label("blocks_archive"),
        archive_config(partition_prefix, "blocks", page_cache, block_codec_config),
    )
    .await
    .expect("failed to initialize blocks archive")
}
