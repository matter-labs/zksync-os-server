pub mod commitment;
pub mod config;

use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use crate::config::L1SenderConfig;
use alloy::consensus::{SidecarBuilder, SimpleCoder};
use alloy::network::{EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::ext::DebugApi;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{Block, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::TransportResult;
use anyhow::Context;
use futures::{Stream, StreamExt};
use smart_config::value::ExposeSecret;
use std::collections::HashSet;
use std::str::FromStr;
use tokio::sync::mpsc;
use zksync_os_contract_interface::{Bridgehub, IExecutor};

/// Node component responsible for sending transactions to L1.
pub struct L1Sender {
    provider: DynProvider,
    chain_id: u64,
    validator_timelock_address: Address,
    command_receiver: mpsc::Receiver<Command>,
}

impl L1Sender {
    /// Initializes a new [`L1Sender`] that will send transaction using supplied provider. Assumes
    /// that zkstack config matches L1 configuration at the other end of provider.
    ///
    /// Resulting [`L1Sender`] is expected to be consumed by calling [`Self::run`]. Additionally,
    /// returns a cloneable handle that can be used to send requests to this instance of [`L1Sender`].
    pub async fn new(config: L1SenderConfig) -> anyhow::Result<(Self, L1SenderHandle)> {
        let operator_wallet = EthereumWallet::from(
            PrivateKeySigner::from_str(config.operator_private_key.expose_secret())
                .context("failed to parse operator private key")?,
        );
        let provider = DynProvider::new(
            ProviderBuilder::new()
                .wallet(operator_wallet)
                .connect_ws(WsConnect::new(config.l1_api_url))
                .await
                .context("failed to connect to L1 api")?,
        );
        tracing::info!(
            bridgehub_address = ?config.bridgehub_address,
            chain_id = config.chain_id,
            "initializing L1 sender"
        );
        let bridgehub = Bridgehub::new(
            config.bridgehub_address.0.into(),
            provider.clone(),
            config.chain_id,
        );
        let validator_timelock_address = bridgehub.validator_timelock_address().await?;
        tracing::info!(?validator_timelock_address, "resolved on L1");

        let (command_sender, command_receiver) = mpsc::channel(128);
        let this = Self {
            provider,
            chain_id: config.chain_id,
            validator_timelock_address,
            command_receiver,
        };
        let handle = L1SenderHandle { command_sender };
        Ok((this, handle))
    }

    /// Runs L1 sender indefinitely thus processing requests received from any of the matching
    /// handles.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let limit = 16;
        let mut cmd_buffer = Vec::with_capacity(16);
        let mut l1_block_stream = self
            .provider
            .subscribe_full_blocks()
            .hashes()
            .into_stream()
            .await?;
        while self
            .command_receiver
            .recv_many(&mut cmd_buffer, limit)
            .await
            != 0
        {
            let mut pending_tx_hashes = HashSet::new();
            for cmd in cmd_buffer.drain(..) {
                match cmd {
                    Command::Commit(CommitCommand {
                        previous_batch,
                        batch,
                    }) => {
                        if let Some(pending_tx_hash) = self.commit(previous_batch, batch).await? {
                            pending_tx_hashes.insert(pending_tx_hash);
                        }
                    }
                }
            }
            self.wait_for_pending_txs(&mut l1_block_stream, pending_tx_hashes)
                .await?;
        }

        tracing::trace!("channel has been closed; stopping L1 sender");
        Ok(())
    }

    async fn wait_for_pending_txs(
        &mut self,
        l1_block_stream: &mut (dyn Stream<Item = TransportResult<Block>> + Unpin + Send),
        mut pending_tx_hashes: HashSet<TxHash>,
    ) -> anyhow::Result<()> {
        while !pending_tx_hashes.is_empty() {
            let Some(block) = l1_block_stream.next().await else {
                anyhow::bail!("L1 block stream has been closed unexpectedly");
            };
            let block = block?;
            let mut mined_pending_txs = 0;
            for tx_hash in block.transactions.hashes() {
                if pending_tx_hashes.remove(&tx_hash) {
                    self.validate_tx_receipt(tx_hash).await?;
                    mined_pending_txs += 1;
                }
            }
            let remaining_pending_txs = pending_tx_hashes.len();
            let base_fee_per_gas = block.header.base_fee_per_gas.unwrap_or_default();
            tracing::debug!(
                block.header.number,
                ?block.header.hash,
                base_fee_per_gas,
                mined_pending_txs,
                remaining_pending_txs,
                "received new L1 block"
            );
        }
        Ok(())
    }

    async fn validate_tx_receipt(&self, tx_hash: TxHash) -> anyhow::Result<()> {
        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await?
            .context("mined transaction receipt is missing")?;
        if receipt.status() {
            // We could also look at tx receipt's logs for a corresponding `BlockCommit` event but
            // not sure if this is 100% necessary yet.
            tracing::info!(
                // batch = commit_batch_info.batch_number,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "batch committed to L1",
            );

            Ok(())
        } else {
            tracing::error!(
                // batch = commit_batch_info.batch_number,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "commit transaction failed"
            );
            if tracing::enabled!(tracing::Level::DEBUG) {
                let trace = self
                    .provider
                    .debug_trace_transaction(
                        receipt.transaction_hash,
                        GethDebugTracingOptions::call_tracer(CallConfig::default()),
                    )
                    .await?;
                let call_frame = trace
                    .try_into_call_frame()
                    .expect("requested call tracer but received a different call frame type");
                // We print top-level call frame's output as it likely contains serialized custom
                // error pointing to the underlying problem (i.e. starts with the error's 4byte
                // signature).
                tracing::debug!(
                    ?call_frame.output,
                    ?call_frame.error,
                    ?call_frame.revert_reason,
                    "failed transaction's top-level call frame"
                );
            }
            anyhow::bail!(
                "commit transaction failed, see L1 transaction's trace for more details (tx_hash='{:?}')",
                receipt.transaction_hash
            );
        }
    }
}

impl L1Sender {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    fn commit_calldata(previous_batch: &StoredBatchInfo, batch: CommitBatchInfo) -> Vec<u8> {
        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 0;

        let stored_batch_info = IExecutor::StoredBatchInfo::from(previous_batch);
        let commit_batch_info = IExecutor::CommitBoojumOSBatchInfo::from(batch);
        tracing::debug!(
            last_batch_hash = ?previous_batch.hash(),
            last_batch_number = ?previous_batch.batch_number,
            new_batch_number = ?commit_batch_info.batchNumber,
            "preparing commit calldata"
        );
        let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

        // Prefixed by current encoding version as expected by protocol
        [[SUPPORTED_ENCODING_VERSION].to_vec(), encoded_data]
            .concat()
            .to_vec()
    }

    async fn commit(
        &mut self,
        previous_batch: StoredBatchInfo,
        batch: CommitBatchInfo,
    ) -> anyhow::Result<Option<TxHash>> {
        if batch.batch_number <= previous_batch.batch_number {
            tracing::info!(
                batch_number = batch.batch_number,
                "ignoring batch as it has been already committed",
            );
            return Ok(None);
        }
        anyhow::ensure!(
            batch.batch_number == previous_batch.batch_number + 1,
            "Tried to commit non-sequential batch #{} after previous batch #{}",
            batch.batch_number,
            previous_batch.batch_number,
        );

        // Create a blob sidecar with empty data
        let sidecar = SidecarBuilder::<SimpleCoder>::from_slice(&[]).build()?;

        let call = IExecutor::commitBatchesSharedBridgeCall::new((
            U256::from(self.chain_id),
            U256::from(previous_batch.batch_number + 1),
            U256::from(batch.batch_number),
            Self::commit_calldata(&previous_batch, batch.clone()).into(),
        ));

        let gas_price = self.provider.get_gas_price().await?;
        let eip1559_est = self.provider.estimate_eip1559_fees().await?;
        let tx = TransactionRequest::default()
            .with_to(self.validator_timelock_address)
            .with_max_fee_per_blob_gas(gas_price)
            .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
            .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas)
            // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
            .with_gas_limit(15000000)
            .with_call(&call)
            .with_blob_sidecar(sidecar);

        let pending_tx_hash = *self.provider.send_transaction(tx).await?.tx_hash();
        tracing::debug!(
            batch = batch.batch_number,
            ?pending_tx_hash,
            "batch commit transaction sent to L1"
        );
        Ok(Some(pending_tx_hash))
    }
}

/// A cheap cloneable handle to a [`L1Sender`] instance that can send requests.
#[derive(Clone, Debug)]
pub struct L1SenderHandle {
    command_sender: mpsc::Sender<Command>,
}

impl L1SenderHandle {
    /// Request [`L1Sender`] to send batch commitment to L1 asynchronously.
    pub async fn commit(
        &self,
        previous_batch: StoredBatchInfo,
        batch: CommitBatchInfo,
    ) -> anyhow::Result<()> {
        self.command_sender
            .send(Command::Commit(CommitCommand {
                previous_batch,
                batch,
            }))
            .await
            .map_err(|_| anyhow::anyhow!("failed to commit a batch as L1 sender is dropped"))
    }
}

#[derive(Debug)]
enum Command {
    Commit(CommitCommand),
}

#[derive(Debug)]
struct CommitCommand {
    previous_batch: StoredBatchInfo,
    batch: CommitBatchInfo,
}
