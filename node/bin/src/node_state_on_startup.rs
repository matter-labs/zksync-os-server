use crate::BLOCK_REPLAY_WAL_DB_NAME;
use crate::config::{Config, ReplayArchiveConfig, ReplayArchiveEncryptionConfig};
use std::fmt::Write;
use std::ops::RangeInclusive;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_types::NodeRole;

#[allow(dead_code)] // some fields are only used for logging (`Debug`)
#[derive(Debug, Clone)]
pub struct NodeStateOnStartup {
    pub node_role: NodeRole,
    pub l1_state: L1State,
    pub state_block_range_available: RangeInclusive<u64>,
    pub block_replay_storage_last_block: u64,
    pub tree_last_block: u64,
    pub repositories_persisted_block: u64,
    pub last_l1_committed_block: u64,
    pub last_l1_proved_block: u64,
    pub last_l1_executed_block: u64,
}

impl NodeStateOnStartup {
    pub fn assert_consistency(&self) {
        assert!(
            self.last_l1_committed_block >= self.last_l1_proved_block,
            "Last committed block ({}) is less than last proved block ({})",
            self.last_l1_committed_block,
            self.last_l1_proved_block,
        );
        assert!(
            self.last_l1_proved_block >= self.last_l1_executed_block,
            "Last proved block ({}) is less than last executed block ({})",
            self.last_l1_proved_block,
            self.last_l1_executed_block,
        );
    }

    /// Main-node-only guard against starting from a stale replay WAL.
    ///
    /// If the local WAL head is behind the last L1-committed block, the WAL is missing records
    /// for blocks that are already committed on L1. Starting the sequencer in this state makes it
    /// produce fresh blocks at WAL head + 1, re-sequencing block numbers L1 already knows —
    /// guaranteed divergence, followed by batcher/commit-watcher crashes. The replay archive is
    /// guaranteed complete up to the last L1-committed block (the archive gate blocks L1 commits
    /// until all blocks of the batch are archived), so the correct fix is offline recovery.
    ///
    /// Panics with recovery instructions unless `general.allow_wal_behind_l1` is set.
    pub fn check_wal_not_behind_l1(&self, config: &Config) {
        if !self.node_role.is_main() {
            return;
        }
        if self.block_replay_storage_last_block >= self.last_l1_committed_block {
            return;
        }

        let gap = self.last_l1_committed_block - self.block_replay_storage_last_block;
        let replay_db_path = config
            .general_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME);

        let mut msg = String::new();
        writeln!(
            msg,
            "local replay WAL is behind L1: \
             WAL head = block {wal}, last L1-committed block = {l1} \
             (last committed batch = {batch}), missing {gap} block(s)",
            wal = self.block_replay_storage_last_block,
            l1 = self.last_l1_committed_block,
            batch = self.l1_state.last_committed_batch,
        )
        .unwrap();
        writeln!(
            msg,
            "The WAL is missing blocks already committed to L1. Producing new blocks from this \
             state would re-sequence committed block numbers and diverge from L1."
        )
        .unwrap();
        if matches!(config.replay_archive_config, ReplayArchiveConfig::Noop) {
            writeln!(
                msg,
                "No replay archive is configured on this node (`replay_archive.type=Noop`), so \
                 recovery commands cannot be suggested. Either restore from a replay archive of \
                 another node in this chain (run `replay_archive_recovery` with that node's \
                 bucket and decryption key against {db}), or copy a complete `{wal_name}` \
                 directory from a healthy node.",
                db = replay_db_path.display(),
                wal_name = BLOCK_REPLAY_WAL_DB_NAME,
            )
            .unwrap();
        } else {
            writeln!(
                msg,
                "Restore the WAL from the replay archive (node must stay stopped):\n\
                 1. replay_archive_recovery download {source} --output-root <dir>\n\
                 2. replay_archive_recovery recover-rocksdb --input-root <dir> \\\n\
                 \x20      --replay-db-path {db} \\\n\
                 \x20      --anchor-block-number {anchor} --anchor-block-hash <hash>{decrypt}\n\
                 \x20  (`--replay-db-path` must point at an empty/absent dir - move the stale \
                 `{wal_name}` away first)",
                source = replay_archive_source_args(&config.replay_archive_config),
                db = replay_db_path.display(),
                anchor = self.last_l1_committed_block,
                decrypt = replay_archive_decrypt_args(&config.replay_archive_config),
                wal_name = BLOCK_REPLAY_WAL_DB_NAME,
            )
            .unwrap();
            writeln!(
                msg,
                "Anchor hash for block {anchor} is not known locally - take it from a healthy \
                 replica (`eth_getBlockByNumber`) or from the archive object listing under prefix \
                 `{anchor}/`.",
                anchor = self.last_l1_committed_block,
            )
            .unwrap();
        }

        if config.general_config.allow_wal_behind_l1 {
            tracing::warn!(
                "`general.allow_wal_behind_l1` is set - starting anyway and DIVERGING from L1.\n{msg}"
            );
        } else {
            panic!(
                "{msg}To bypass this check and start anyway (divergence!), set \
                 `general.allow_wal_behind_l1=true` (env: `ALLOW_WAL_BEHIND_L1=true`)."
            );
        }
    }
}

fn replay_archive_source_args(config: &ReplayArchiveConfig) -> String {
    match config {
        ReplayArchiveConfig::Noop => unreachable!("Noop is handled before command generation"),
        ReplayArchiveConfig::FileSystem { root_path, .. } => {
            format!("--archive-root {}", root_path.display())
        }
        ReplayArchiveConfig::S3WithCredentialFile {
            bucket_base_url,
            s3_credential_file_path,
            endpoint,
            region,
            ..
        } => {
            let mut args = format!(
                "--s3-bucket-base-url {bucket_base_url} \
                 --s3-credential-file-path {}",
                s3_credential_file_path.display()
            );
            if let Some(endpoint) = endpoint {
                write!(args, " --s3-endpoint {endpoint}").unwrap();
            }
            if let Some(region) = region {
                write!(args, " --s3-region {region}").unwrap();
            }
            args
        }
        ReplayArchiveConfig::Gcs {
            bucket_base_url, ..
        } => format!("--gcs-bucket-base-url {bucket_base_url}"),
    }
}

fn replay_archive_decrypt_args(config: &ReplayArchiveConfig) -> String {
    let encryption = match config {
        ReplayArchiveConfig::Noop => return String::new(),
        ReplayArchiveConfig::FileSystem { encryption, .. }
        | ReplayArchiveConfig::S3WithCredentialFile { encryption, .. }
        | ReplayArchiveConfig::Gcs { encryption, .. } => encryption,
    };
    match encryption {
        ReplayArchiveEncryptionConfig::Noop => String::new(),
        ReplayArchiveEncryptionConfig::AgeX25519 { .. } => {
            " \\\n\x20      --identity-file <age identity> (or --age-secret-key / \
             env REPLAY_ARCHIVE_AGE_SECRET_KEY)"
                .to_string()
        }
        ReplayArchiveEncryptionConfig::GcpKms { kms_key_version } => {
            format!(" \\\n\x20      --kms-key-version {kms_key_version}")
        }
    }
}
