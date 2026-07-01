use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, TestEnvironment, test_multisetup};
use zksync_os_replay_archive::{
    FileSystemReplayArchiveReader, download_all_replay_archive_objects, read_age_x25519_identity,
    recover_replay_records_to_rocksdb_with_optional_decryption,
};
use zksync_os_server::config::{ReplayArchiveConfig, ReplayArchiveEncryptionConfig};

const REPLAY_ARCHIVE_RECIPIENT: &str =
    "age1km7egrpfclsaf6tu4p3h2d8urcyp9s7cwcfzg2sezl95vmn0zgus8xhpk4";
const REPLAY_ARCHIVE_IDENTITY_FILE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/replay-archive.key");
const TRANSACTIONS_BEFORE_RECOVERY: usize = 3;

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn encrypted_replay_archive_recovers_node_storage_end_to_end(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.sequencer_config.block_time = Duration::from_millis(50);
    config.replay_archive_config = ReplayArchiveConfig::FileSystem {
        root_path: replay_archive_root(&config.general_config.rocks_db_path)?,
        encryption: ReplayArchiveEncryptionConfig::AgeX25519 {
            recipient: REPLAY_ARCHIVE_RECIPIENT.to_owned(),
        },
    };
    let tester = env.launch(config).await?;

    for _ in 0..TRANSACTIONS_BEFORE_RECOVERY {
        tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(1u64)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
    }

    let latest_block_number = tester.l2_provider.get_block_number().await?;
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(latest_block_number, DEFAULT_TIMEOUT)
        .await?;
    let latest_block_hash = tester
        .l2_provider
        .get_block_by_number(latest_block_number.into())
        .await?
        .context("latest block should exist")?
        .header
        .hash;

    let archive_root = match &tester.config().replay_archive_config {
        ReplayArchiveConfig::FileSystem { root_path, .. } => root_path.clone(),
        _ => unreachable!("test enables replay archive"),
    };
    let rocks_db_path = tester.config().general_config.rocks_db_path.clone();
    let stopped = tester.stop().await?;

    tokio::fs::remove_dir_all(&rocks_db_path)
        .await
        .with_context(|| format!("failed to remove node storage {}", rocks_db_path.display()))?;

    recover_replay_storage_from_archive(
        &archive_root,
        &rocks_db_path,
        latest_block_number,
        latest_block_hash,
    )
    .await?;

    let restarted = stopped.start().await?;
    // Wait for finalization before sending tx to make sure the node populated block in repositories up to the anchor.
    restarted
        .l2_zk_provider
        .wait_finalized_with_timeout(latest_block_number, DEFAULT_TIMEOUT)
        .await?;
    let post_recovery_receipt = restarted
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    let post_recovery_block = post_recovery_receipt
        .block_number
        .context("post-recovery transaction receipt should have a block number")?;
    assert!(
        post_recovery_block > latest_block_number,
        "post-recovery block {post_recovery_block} should build on recovered tip {latest_block_number}"
    );
    restarted
        .l2_zk_provider
        .wait_finalized_with_timeout(post_recovery_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}

async fn recover_replay_storage_from_archive(
    archive_root: &Path,
    rocks_db_path: &Path,
    latest_block_number: u64,
    latest_block_hash: B256,
) -> anyhow::Result<()> {
    let recovery_root = archive_root
        .parent()
        .context("archive root should have a parent")?
        .join("replay_archive_recovery");
    let downloaded_root = recovery_root.join("downloaded");
    tokio::fs::create_dir_all(rocks_db_path)
        .await
        .with_context(|| {
            format!(
                "failed to create recovered node storage root {}",
                rocks_db_path.display()
            )
        })?;

    let reader = FileSystemReplayArchiveReader::new(archive_root.to_path_buf());
    let downloaded = download_all_replay_archive_objects(&reader, &downloaded_root).await?;
    assert!(
        downloaded > 0,
        "replay archive should contain encrypted objects"
    );

    let identity = read_age_x25519_identity(Path::new(REPLAY_ARCHIVE_IDENTITY_FILE)).await?;
    let recovered = recover_replay_records_to_rocksdb_with_optional_decryption(
        &downloaded_root,
        &rocks_db_path.join("block_replay_wal"),
        latest_block_number,
        latest_block_hash,
        Some(identity),
    )
    .await?;
    assert_eq!(
        recovered,
        latest_block_number as usize + 1,
        "recovery should restore all canonical replay records from genesis through the anchor"
    );

    Ok(())
}

fn replay_archive_root(rocks_db_path: &Path) -> anyhow::Result<PathBuf> {
    Ok(rocks_db_path
        .parent()
        .context("rocks DB path should have a parent")?
        .join("replay_archive"))
}
