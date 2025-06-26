use std::path::Path;

use anyhow::Context;
use zksync_concurrency::{ctx, time};
use zksync_consensus_engine::{BlockStoreState, Last};
use zksync_consensus_roles::validator;
use crate::block_replay_storage::BlockReplayStorage;
use zksync_storage::{db::NamedColumnFamily, RocksDB};

use crate::consensus::{
    config::{GenesisSpec, GlobalConfig},
    storage::InsertCertificateError,
    types::{BlockCertificate, BlockMetadata, Payload},
};

#[derive(Clone, Copy, Debug)]
pub enum CF {
    GlobalConfig,
    Payload,
    Certificate,
    State,
}

impl CF {
    pub fn global_config_key() -> &'static [u8] {
        b"global_config"
    }

    pub fn state_key() -> &'static [u8] {
        b"state"
    }
}

impl NamedColumnFamily for CF {
    const DB_NAME: &'static str = "consensus";
    const ALL: &'static [Self] = &[CF::GlobalConfig, CF::Payload, CF::Certificate, CF::State];

    fn name(&self) -> &'static str {
        match self {
            CF::GlobalConfig => "global_config",
            CF::Payload => "payload",
            CF::Certificate => "certificate",
            CF::State => "state",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsensusStorage {
    consensus_db: RocksDB<CF>,
    block_replay_storage: BlockReplayStorage,
}

impl ConsensusStorage {
    pub fn new(path: &Path, block_replay_storage: BlockReplayStorage) -> Self {
        let consensus_db = RocksDB::<CF>::new(path)
            .expect("Failed to open RocksDB")
            .with_sync_writes();

        Self {
            consensus_db,
            block_replay_storage,
        }
    }

    pub fn global_config(&self) -> anyhow::Result<Option<GlobalConfig>> {
        let global_config_bytes = self
            .consensus_db
            .get_cf(CF::GlobalConfig, CF::global_config_key())?;
        if let Some(global_config_bytes) = global_config_bytes {
            Ok(Some(zksync_protobuf::decode(&global_config_bytes)?))
        } else {
            Ok(None)
        }
    }

    pub fn try_update_global_config(&self, want: &GlobalConfig) -> anyhow::Result<()> {
        let got = self.global_config()?;
        if let Some(got) = &got {
            // Exit if the global config didn't change.
            if got == want {
                return Ok(());
            }
            // TODO
            // verify_config_transition(got, want)?;

            // If genesis didn't change, just update the config.
            if got.genesis == want.genesis {
                let global_config_bytes = zksync_protobuf::encode(want);

                let mut batch = self.consensus_db.new_write_batch();
                batch.put_cf(
                    CF::GlobalConfig,
                    CF::global_config_key(),
                    &global_config_bytes,
                );
                self.consensus_db.write(batch)?;

                return Ok(());
            }
        }

        // Reset the consensus state.
        let global_config_bytes = zksync_protobuf::encode(want);
        let state_bytes = zksync_protobuf::encode(&validator::ReplicaState::default());
        let mut batch = self.consensus_db.new_write_batch();
        let full_range = (&[0u8] as &[u8])..&[0xffu8; 9]; // contains all u64 keys
        batch.delete_range_cf(CF::Certificate, full_range);
        batch.put_cf(
            CF::GlobalConfig,
            CF::global_config_key(),
            &global_config_bytes,
        );
        batch.put_cf(CF::State, CF::state_key(), &state_bytes);
        self.consensus_db.write(batch)?;

        Ok(())
    }

    pub fn replica_state(&self) -> anyhow::Result<Option<validator::ReplicaState>> {
        let replica_state_bytes = self.consensus_db.get_cf(CF::State, CF::state_key())?;
        if let Some(replica_state_bytes) = replica_state_bytes {
            Ok(Some(zksync_protobuf::decode(&replica_state_bytes)?))
        } else {
            Ok(None)
        }
    }

    pub fn set_replica_state(&self, state: &validator::ReplicaState) -> anyhow::Result<()> {
        // Reset the consensus state.
        let state_bytes = zksync_protobuf::encode(state);
        let mut batch = self.consensus_db.new_write_batch();
        batch.put_cf(CF::State, CF::state_key(), &state_bytes);
        self.consensus_db.write(batch)?;

        Ok(())
    }

    fn next_block(&self) -> anyhow::Result<validator::BlockNumber> {
        let block_number = self.block_replay_storage.latest_block().unwrap_or(0);
        Ok(validator::BlockNumber(block_number + 1))
    }

    pub(crate) fn block_store_state(&self) -> anyhow::Result<BlockStoreState> {
        let first = validator::BlockNumber(1);
        let cfg = self
            .global_config()
            .context("global_config()")?
            .context("global config is missing")?;

        // If there is a cert in storage, then the block range visible to consensus
        // is [first block, block of last cert].
        if let Some(cert) = self.last_block_certificate()? {
            return Ok(BlockStoreState {
                first,
                last: Some(cert.into()),
            });
        }

        // Otherwise it is [first block, min(genesis.first_block-1,last block)].
        let next = self
            .next_block()
            .context("next_block()")?
            .min(cfg.genesis.first_block);

        Ok(BlockStoreState {
            first,
            // unwrap is ok, because `next > first >= 0`.
            last: if next > first {
                Some(Last::PreGenesis(next.prev().unwrap()))
            } else {
                None
            },
        })
    }

    pub fn block_certificate(
        &self,
        number: validator::BlockNumber,
    ) -> anyhow::Result<Option<BlockCertificate>> {
        let key = number.0.to_be_bytes();
        let cert_bytes = self.consensus_db.get_cf(CF::Certificate, &key)?;
        if let Some(cert_bytes) = cert_bytes {
            Ok(Some(zksync_protobuf::decode(&cert_bytes)?))
        } else {
            Ok(None)
        }
    }

    fn last_block_certificate(&self) -> anyhow::Result<Option<BlockCertificate>> {
        let full_range = ..=(&[0xffu8; 8] as &[u8]); // contains all u64 keys
        let mut iter = self
            .consensus_db
            .to_iterator_cf(CF::Certificate, full_range);
        if let Some((_, cert_bytes)) = iter.next() {
            Ok(Some(zksync_protobuf::decode(&cert_bytes)?))
        } else {
            Ok(None)
        }
    }

    pub fn insert_block_certificate(
        &self,
        cert: &BlockCertificate,
    ) -> Result<(), InsertCertificateError> {
        // Extract block number and payload hash based on certificate variant
        let block_number = cert.number();
        let payload_hash = cert.payload_hash();

        let want_payload = self
            .block_payload(block_number)?
            .ok_or(InsertCertificateError::MissingPayload)?;

        if payload_hash != want_payload.encode().hash() {
            return Err(InsertCertificateError::PayloadMismatch(want_payload));
        }

        let key = block_number.0.to_be_bytes();
        let cert_bytes = zksync_protobuf::encode(cert);
        let mut batch = self.consensus_db.new_write_batch();
        batch.put_cf(CF::Certificate, &key, &cert_bytes);
        self.consensus_db
            .write(batch)
            .map_err(Into::<anyhow::Error>::into)?;

        Ok(())
    }

    pub fn block_payload(&self, number: validator::BlockNumber) -> anyhow::Result<Option<Payload>> {
        let record = self.block_replay_storage.get_replay_record(number.0);
        Ok(record.map(Payload))
    }

    pub fn block_metadata(
        &self,
        number: validator::BlockNumber,
    ) -> anyhow::Result<Option<BlockMetadata>> {
        let Some(record) = self.block_replay_storage.get_replay_record(number.0) else {
            return Ok(None);
        };
        let metadata = BlockMetadata {
            payload_hash: Payload(record).encode().hash(),
        };
        Ok(Some(metadata))
    }

    /// (Re)initializes consensus genesis to start at the last L2 block in storage.
    /// Noop if `spec` matches the current genesis.
    pub(in crate::consensus) fn adjust_global_config(&self, spec: &GenesisSpec) -> anyhow::Result<()> {
        let old = self.global_config().context("global_config()")?;
        if let Some(old) = &old {
            if &GenesisSpec::from_global_config(old) == spec {
                // Hard fork is not needed.
                return Ok(());
            }
        }

        tracing::info!("Performing a hard fork of consensus.");
        let new = GlobalConfig {
            genesis: validator::GenesisRaw {
                chain_id: spec.chain_id,
                fork_number: old.as_ref().map_or(validator::ForkNumber(0), |old| {
                    old.genesis.fork_number.next()
                }),
                // first_block: validator::BlockNumber(0), // TODO
                first_block: self.next_block().context("next_block()")?,
                protocol_version: spec.protocol_version,
                validators_schedule: spec.validators.clone(),
            }
            .with_hash(),
            registry_address: spec.registry_address,
            seed_peers: spec.seed_peers.clone(),
        };

        self.try_update_global_config(&new)
            .context("try_update_global_config()")?;
        Ok(())
    }

    /// Fetches a block from storage.
    pub(crate) fn block(
        &self,
        number: validator::BlockNumber,
    ) -> anyhow::Result<Option<validator::Block>> {
        let Some(payload) = self.block_payload(number).context("payload()")? else {
            return Ok(None);
        };

        if let Some(justification) = self
            .block_certificate(number)
            .context("block_certificate()")?
        {
            // Create the appropriate block variant based on the certificate type
            match justification {
                BlockCertificate::V1(commit_qc) => {
                    return Ok(Some(validator::Block::FinalV1(validator::v1::FinalBlock {
                        payload: payload.encode(),
                        justification: commit_qc,
                    })));
                }
                BlockCertificate::V2(commit_qc) => {
                    return Ok(Some(validator::Block::FinalV2(validator::v2::FinalBlock {
                        payload: payload.encode(),
                        justification: commit_qc,
                    })));
                }
            }
        }

        // If no certificate is available, return a PreGenesis block
        Ok(Some(validator::Block::PreGenesis(
            validator::PreGenesisBlock {
                number,
                payload: payload.encode(),
                // We won't use justification until it is possible to verify
                // payload against the L1 batch commitment.
                justification: validator::Justification(vec![]),
            },
        )))
    }

    #[tracing::instrument(skip_all)]
    pub async fn wait_for_payload(
        &self,
        ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::Result<Payload> {
        const POLL_INTERVAL: time::Duration = time::Duration::milliseconds(50);
        loop {
            if let Some(record) = self.block_replay_storage.get_replay_record(number.0) {
                return Ok(Payload(record));
            }
            ctx.sleep(POLL_INTERVAL).await?;
        }
    }
}
