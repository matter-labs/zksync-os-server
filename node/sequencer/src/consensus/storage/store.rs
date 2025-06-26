use anyhow::Context as _;
use futures::channel::mpsc;
use tracing::Instrument;
use zksync_concurrency::{ctx, error::Wrap as _, scope, sync, time};
use zksync_consensus_engine::{self as engine, BlockStoreState, EngineInterface};
use zksync_consensus_roles::validator;

use crate::consensus::{
    storage::{ConsensusStorage, InsertCertificateError},
    types::{BlockCertificate, Payload, ReducedBlockCommand},
};

/// Wrapper of `ConsensusStorage` implementing `EngineInterface`.
///
/// Contains queues to save Quorum Certificates received over gossip to the store
/// as and when the payload they are over becomes available.
#[derive(Clone, Debug)]
pub(crate) struct Store {
    pub(super) storage: ConsensusStorage,
    /// L2 block QCs received from consensus
    block_certificates: ctx::channel::UnboundedSender<BlockCertificate>,
    /// Range of L2 blocks for which we have a QC persisted.
    blocks_persisted: sync::watch::Receiver<BlockStoreState>,
    /// Sequencer block command sender.
    block_command_sender: mpsc::UnboundedSender<ReducedBlockCommand>,
}

impl Store {
    pub(crate) async fn new(
        storage: ConsensusStorage,
        block_command_sender: mpsc::UnboundedSender<ReducedBlockCommand>,
    ) -> anyhow::Result<(Store, StoreRunner)> {
        // Initial state of persisted blocks
        let blocks_persisted = storage.block_store_state().context("blocks_range()")?;
        let blocks_persisted = sync::watch::channel(blocks_persisted).0;
        let (block_certs_send, block_certs_recv) = ctx::channel::unbounded();

        Ok((
            Store {
                storage: storage.clone(),
                block_certificates: block_certs_send,
                blocks_persisted: blocks_persisted.subscribe(),
                block_command_sender,
            },
            StoreRunner {
                storage,
                blocks_persisted: PersistedBlockState(blocks_persisted),
                block_certificates: block_certs_recv,
            },
        ))
    }
}

#[async_trait::async_trait]
impl EngineInterface for Store {
    async fn genesis(&self, _ctx: &ctx::Ctx) -> ctx::Result<validator::Genesis> {
        Ok(self.storage.global_config()?.context("not found")?.genesis)
    }

    fn persisted(&self) -> sync::watch::Receiver<BlockStoreState> {
        self.blocks_persisted.clone()
    }

    async fn get_validator_schedule(
        &self,
        _ctx: &ctx::Ctx,
        _number: validator::BlockNumber,
    ) -> ctx::Result<(validator::Schedule, validator::BlockNumber)> {
        panic!()
    }

    async fn get_pending_validator_schedule(
        &self,
        _ctx: &ctx::Ctx,
        _number: validator::BlockNumber,
    ) -> ctx::Result<Option<(validator::Schedule, validator::BlockNumber)>> {
        panic!()
    }

    async fn get_block(
        &self,
        _ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::Result<validator::Block> {
        Ok(self.storage.block(number)?.context("not found")?)
    }

    /// If actions queue is set (and the block has not been stored yet),
    /// the block will be translated into a sequence of actions.
    /// The received actions should be fed
    /// to `ExternalIO`, so that `StateKeeper` will store the corresponding L2 block in the db.
    ///
    /// `store_next_block()` call will wait synchronously for the L2 block.
    /// Once the L2 block is observed in storage, `store_next_block()` will store a cert for this
    /// L2 block.
    async fn queue_next_block(&self, ctx: &ctx::Ctx, block: validator::Block) -> ctx::Result<()> {
        let (p, j) = match &block {
            validator::Block::FinalV1(block) => (
                &block.payload,
                Some(BlockCertificate::V1(block.justification.clone())),
            ),
            validator::Block::FinalV2(block) => (
                &block.payload,
                Some(BlockCertificate::V2(block.justification.clone())),
            ),
            validator::Block::PreGenesis(block) => (&block.payload, None),
        };

        tracing::info!("queue_next_block: block_number={:?}", block.number());

        let persisted_payload = self.storage.block_payload(block.number())?;
        if persisted_payload.is_none() {
            let payload = Payload::decode(p).context("Payload::decode()")?;
            self.block_command_sender
                .unbounded_send(ReducedBlockCommand::Replay(payload.0.clone()))
                .context("send replay")?;
            self.storage
                .wait_for_payload(ctx, block.number())
                .await
                .wrap("wait_for_payload")?;
        };
        if let Some(certificate) = j {
            self.block_certificates.send(certificate);
        }
        Ok(())
    }

    async fn verify_pregenesis_block(
        &self,
        _ctx: &ctx::Ctx,
        block: &validator::PreGenesisBlock,
    ) -> ctx::Result<()> {
        let meta = self
            .storage
            .block_metadata(block.number)?
            .context("metadata not in storage")?;
        if meta.payload_hash != block.payload.hash() {
            return Err(anyhow::format_err!("payload hash mismatch").into());
        }
        Ok(())
    }

    /// Verify that `payload` is a correct proposal for the block `block_number`.
    /// * for the main node it checks whether the same block is already present in storage.
    /// * for the EN validator
    ///   * if the block with this number was already applied, it checks that it was the
    ///     same block. It should always be true, because main node is the only proposer and
    ///     to propose a different block a hard fork is needed.
    ///   * otherwise, EN attempts to apply the received block. If the block was incorrect
    ///     the statekeeper is expected to crash the whole EN. Otherwise OK is returned.
    async fn verify_payload(
        &self,
        ctx: &ctx::Ctx,
        block_number: validator::BlockNumber,
        payload: &validator::Payload,
    ) -> ctx::Result<()> {
        let received = Payload::decode(payload).context("Payload::decode(got)")?;
        tracing::info!("verify_payload: block_number={:?}", received.0.context.block_number);
        let persisted_payload = self.storage.block_payload(block_number)?;
        let persisted_payload = if let Some(persisted_payload) = persisted_payload {
            persisted_payload
        } else {
            self.block_command_sender
                .unbounded_send(ReducedBlockCommand::Replay(received.0.clone()))
                .context("send replay")?;
            let payload = self
                .storage
                .wait_for_payload(ctx, block_number)
                .await
                .wrap("wait_for_payload")?;
            payload
        };
        if received != persisted_payload {
            return Err(anyhow::format_err!(
                "unexpected payload: got {received:?} persisted {persisted_payload:?}"
            )
            .into());
        }
        Ok(())
    }

    /// Currently (for the main node) proposing is implemented as just converting an L2 block from db (without a cert) into a payload.
    async fn propose_payload(
        &self,
        _ctx: &ctx::Ctx,
        block_number: validator::BlockNumber,
    ) -> ctx::Result<validator::Payload> {
        tracing::info!("propose_payload: block_number={block_number:?}");

        const LARGE_PAYLOAD_SIZE: usize = 1 << 20;

        let persisted_payload = self.storage.block_payload(block_number)?;
        let payload = if let Some(persisted_payload) = persisted_payload {
            persisted_payload
        } else {
            self.block_command_sender
                .unbounded_send(ReducedBlockCommand::Produce(block_number.0))
                .context("send produce")?;
            // Kludge: uses ctx::root() which is not getting cancelled,
            // so that execution gets blocked here and we do not create many proposal.
            let payload = self
                .storage
                .wait_for_payload(&ctx::root(), block_number)
                .await
                .wrap("wait_for_payload")?;
            payload
        };

        let encoded_payload = payload.encode();
        if encoded_payload.0.len() > LARGE_PAYLOAD_SIZE {
            tracing::warn!(
                "large payload ({}B) with {} transactions",
                encoded_payload.0.len(),
                payload.0.transactions.len()
            );
        }
        Ok(encoded_payload)
    }

    async fn get_state(&self, _ctx: &ctx::Ctx) -> ctx::Result<validator::ReplicaState> {
        self.storage
            .replica_state()?
            .context("replica_state()")
            .map_err(Into::into)
    }

    async fn set_state(&self, _ctx: &ctx::Ctx, state: &validator::ReplicaState) -> ctx::Result<()> {
        self.storage
            .set_replica_state(state)
            .context("set_replica_state()")
            .map_err(Into::into)
    }
}

/// Background task of the `Store`.
pub struct StoreRunner {
    storage: ConsensusStorage,
    blocks_persisted: PersistedBlockState,
    block_certificates: ctx::channel::UnboundedReceiver<BlockCertificate>,
}

impl StoreRunner {
    pub async fn run(self, ctx: &ctx::Ctx) -> anyhow::Result<()> {
        let StoreRunner {
            storage,
            blocks_persisted,
            mut block_certificates,
        } = self;

        let res = scope::run!(ctx, |ctx, s| async {
            #[tracing::instrument(skip_all)]
            async fn update_blocks_persisted_iteration(
                ctx: &ctx::Ctx,
                storage: &ConsensusStorage,
                blocks_persisted: &PersistedBlockState,
            ) -> ctx::Result<()> {
                const POLL_INTERVAL: time::Duration = time::Duration::seconds(1);

                let state = storage.block_store_state().context("block_store_state()")?;
                blocks_persisted.update(state);
                ctx.sleep(POLL_INTERVAL).await?;

                Ok(())
            }

            s.spawn::<()>(async {
                // Loop updating `blocks_persisted` whenever blocks get pruned.
                loop {
                    update_blocks_persisted_iteration(ctx, &storage, &blocks_persisted).await?;
                }
            });

            #[tracing::instrument(skip_all)]
            async fn insert_block_certificates_iteration(
                ctx: &ctx::Ctx,
                storage: &ConsensusStorage,
                block_certificates: &mut ctx::channel::UnboundedReceiver<BlockCertificate>,
                blocks_persisted: &PersistedBlockState,
            ) -> ctx::Result<()> {
                const POLL_INTERVAL: time::Duration = time::Duration::milliseconds(50);

                let cert = block_certificates
                    .recv(ctx)
                    .instrument(tracing::info_span!("wait_for_block_certificate"))
                    .await?;
                // Wait for the block to be persisted, so that we can attach a cert to it.
                // We may exit this loop without persisting the certificate in case the
                // corresponding block has been pruned in the meantime.
                while blocks_persisted.should_be_persisted(&cert) {
                    // Try to insert the cert.
                    let res = storage.insert_block_certificate(&cert);
                    match res {
                        Ok(()) => {
                            // Insertion succeeded: update persisted state
                            // and wait for the next cert.
                            blocks_persisted.advance(cert);
                            break;
                        }
                        Err(InsertCertificateError::MissingPayload) => {
                            // the payload is not in storage, it's either not yet persisted
                            // or already pruned. We will retry after a delay.
                            ctx.sleep(POLL_INTERVAL)
                                .instrument(tracing::info_span!("wait_for_block"))
                                .await?;
                        }
                        Err(err) => Err(err).context("insert_block_certificate()")?,
                    }
                }

                Ok(())
            }

            // Loop inserting block certs to storage.
            loop {
                insert_block_certificates_iteration(
                    ctx,
                    &storage,
                    &mut block_certificates,
                    &blocks_persisted,
                )
                .await?;
            }
        })
        .await;

        match res {
            Err(ctx::Error::Canceled(_)) | Ok(()) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }
}

struct PersistedBlockState(sync::watch::Sender<BlockStoreState>);

impl PersistedBlockState {
    /// Updates `persisted` to new.
    /// Ends of the range can only be moved forward.
    /// If `persisted.first` is moved forward, it means that blocks have been pruned.
    /// If `persisted.last` is moved forward, it means that new blocks with certificates have been
    /// persisted.
    #[tracing::instrument(skip_all, fields(first = %new.first, next = ?new.next()))]
    fn update(&self, new: BlockStoreState) {
        self.0.send_if_modified(|p| {
            if &new == p {
                return false;
            }
            p.first = p.first.max(new.first);
            if p.next() < new.next() {
                p.last = new.last;
            }
            true
        });
    }

    /// Checks if the given certificate should be eventually persisted.
    /// Only certificates block store state is a range of blocks for which we already have
    /// certificates and we need certs only for the later ones.
    fn should_be_persisted(&self, cert: &BlockCertificate) -> bool {
        self.0.borrow().next() <= cert.number()
    }

    /// Appends the `cert` to `persisted` range.
    #[tracing::instrument(skip_all, fields(batch_number = %cert.number()))]
    fn advance(&self, cert: BlockCertificate) {
        self.0.send_if_modified(|p| {
            if p.next() != cert.number() {
                return false;
            }
            p.last = Some(match cert {
                BlockCertificate::V1(qc) => engine::Last::FinalV1(qc),
                BlockCertificate::V2(qc) => engine::Last::FinalV2(qc),
            });
            true
        });
    }
}
