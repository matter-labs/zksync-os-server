pub mod commitment;
pub mod config;

use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use crate::config::L1SenderConfig;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, BlockNumber, TxHash, U256};
use alloy::providers::ext::DebugApi;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{Block, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::TransportResult;
use anyhow::Context;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use smart_config::value::ExposeSecret;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IExecutor::commitBatchesSharedBridgeCall;
use zksync_os_contract_interface::{Bridgehub, IExecutor};

/// Node component responsible for sending transactions to L1.
pub struct L1Sender {
    provider: DynProvider,
    chain_id: u64,
    validator_timelock_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    command_limit: usize,
    command_receiver: mpsc::Receiver<Command>,
}

impl L1Sender {
    /// Initializes a new [`L1Sender`] that will send transaction using supplied provider. Assumes
    /// that zkstack config matches L1 configuration at the other end of provider.
    ///
    /// Resulting [`L1Sender`] is expected to be consumed by calling [`Self::run`]. Additionally,
    /// returns a cloneable handle that can be used to send requests to this instance of [`L1Sender`].
    pub async fn new(config: L1SenderConfig) -> anyhow::Result<(Self, L1SenderHandle, u64)> {
        anyhow::ensure!(config.command_limit > 0, "command limit must be positive");
        let operator_wallet = EthereumWallet::from(
            PrivateKeySigner::from_str(config.operator_private_key.expose_secret())
                .context("failed to parse operator private key")?,
        );
        let provider = DynProvider::new(
            ProviderBuilder::new()
                .wallet(operator_wallet)
                .connect_ws(WsConnect::new(&config.l1_api_url))
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
        let last_committed_batch = bridgehub
            .zk_chain()
            .await?
            .get_total_batches_committed()
            .await?
            .saturating_to::<u64>();
        let validator_timelock_address = bridgehub.validator_timelock_address().await?;
        tracing::info!(?validator_timelock_address, "resolved on L1");

        let (command_sender, command_receiver) = mpsc::channel(50);
        let this = Self {
            provider,
            chain_id: config.chain_id,
            validator_timelock_address,
            max_fee_per_gas: config.max_fee_per_gas(),
            max_priority_fee_per_gas: config.max_priority_fee_per_gas(),
            command_limit: config.command_limit,
            command_receiver,
        };
        let handle = L1SenderHandle { command_sender };
        Ok((this, handle, last_committed_batch))
    }

    /// Runs L1 sender indefinitely thus processing requests received from any of the matching
    /// handles.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut heartbeat = Heartbeat::new(&self.provider).await?;
        let mut cmd_buffer = Vec::with_capacity(self.command_limit);
        // This sleeps until **at least one** command is received from the channel. Additionally,
        // receives up to `self.command_limit` commands from the channel if they are ready (i.e. does
        // not wait for them). Extends `cmd_buffer` with received values and, as `cmd_buffer` is
        // emptied in every iteration, its size never exceeds `self.command_limit`.
        //
        // This method only returns `0` if the channel has been closed and there are no more items
        // in the queue.
        while self
            .command_receiver
            .recv_many(&mut cmd_buffer, self.command_limit)
            .await
            != 0
        {
            // only used for logging
            let batch_number_range =
                cmd_buffer[0].batch_number()..=cmd_buffer.last().unwrap().batch_number();
            tracing::info!(
                ?batch_number_range,
                count = batch_number_range.clone().count(),
                "Committing batches...",
            );
            let pending_tx_hashes = futures::stream::iter(cmd_buffer.drain(..))
                .then(|cmd| async {
                    match cmd {
                        Command::Commit(cmd) => {
                            let batch_number = cmd.batch.batch_number;
                            let pending_tx_hash = self.commit(cmd).await?;
                            anyhow::Ok((pending_tx_hash, batch_number))
                        }
                    }
                })
                // We could buffer the stream here to enable sending transactions in parallel, but
                // this is not necessary for now - they may still be included in one block (as we
                // don't wait for inclusion here)
                .try_collect::<HashMap<TxHash, u64>>()
                .await?;
            tracing::info!(
                ?batch_number_range,
                count = batch_number_range.clone().count(),
                "Waiting for tx inclusion...",
            );
            heartbeat
                .wait_for_pending_txs(&self.provider, pending_tx_hashes)
                .await?;
            tracing::info!(
                ?batch_number_range,
                count = batch_number_range.clone().count(),
                "All batches committed",
            );
        }

        tracing::trace!("channel has been closed; stopping L1 sender");
        Ok(())
    }
}

impl L1Sender {
    async fn commit(&self, cmd: CommitCommand) -> anyhow::Result<TxHash> {
        let call = cmd.to_call(self.chain_id);
        let CommitCommand {
            previous_batch,
            batch,
        } = cmd;
        anyhow::ensure!(
            batch.batch_number == previous_batch.batch_number + 1,
            "Tried to commit non-sequential batch #{} after previous batch #{}",
            batch.batch_number,
            previous_batch.batch_number,
        );

        let eip1559_est = self.provider.estimate_eip1559_fees().await?;
        tracing::debug!(
            eip1559_est.max_priority_fee_per_gas,
            "estimated median priority fee (20% percentile) for the last 10 blocks"
        );
        if eip1559_est.max_fee_per_gas > self.max_fee_per_gas {
            tracing::warn!(
                max_fee_per_gas = self.max_fee_per_gas,
                estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
                "L1 sender's configured maxFeePerGas is lower than the one estimated from network"
            );
        }
        if eip1559_est.max_priority_fee_per_gas > self.max_priority_fee_per_gas {
            tracing::warn!(
                max_priority_fee_per_gas = self.max_priority_fee_per_gas,
                estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
                "L1 sender's configured maxPriorityFeePerGas is lower than the one estimated from network"
            );
        }
        let tx = TransactionRequest::default()
            .with_to(self.validator_timelock_address)
            .with_max_fee_per_gas(self.max_fee_per_gas)
            .with_max_priority_fee_per_gas(self.max_priority_fee_per_gas)
            // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
            .with_gas_limit(15000000)
            .with_call(&call);

        let pending_tx_hash = *self.provider.send_transaction(tx).await?.tx_hash();
        tracing::debug!(
            batch = batch.batch_number,
            ?pending_tx_hash,
            "batch commit transaction sent to L1"
        );
        Ok(pending_tx_hash)
    }
}

/// L1 sender's heartbeat that monitors all L1 blocks and enforces that we receive them sequentially
struct Heartbeat {
    last_l1_block_number: Option<BlockNumber>,
    l1_block_stream: BoxStream<'static, TransportResult<Block>>,
}

impl Heartbeat {
    async fn new(provider: &DynProvider) -> anyhow::Result<Self> {
        Ok(Self {
            last_l1_block_number: None,
            l1_block_stream: Box::pin(
                provider
                    .subscribe_full_blocks()
                    .hashes()
                    .into_stream()
                    .await?,
            ),
        })
    }

    async fn wait_for_pending_txs(
        &mut self,
        provider: &DynProvider,
        mut pending_tx_hashes: HashMap<TxHash, u64>,
    ) -> anyhow::Result<()> {
        while !pending_tx_hashes.is_empty() {
            let Some(block) = self.l1_block_stream.next().await else {
                anyhow::bail!("L1 block stream has been closed unexpectedly");
            };
            let block = block?;

            if let Some(last_l1_block_number) = self.last_l1_block_number {
                if block.header.number != last_l1_block_number + 1 {
                    // This can happen if there is a reorg on L1. As a temporary measure we restart which
                    // forces us to re-initialize L1 sender.
                    anyhow::bail!(
                        "received non-sequential L1 block #{} (expected #{}); restarting",
                        block.header.number,
                        last_l1_block_number + 1
                    );
                }
            }
            self.last_l1_block_number.replace(block.header.number);

            let mut mined_pending_txs = 0;
            for tx_hash in block.transactions.hashes() {
                if let Some(batch_number) = pending_tx_hashes.remove(&tx_hash) {
                    Self::validate_tx_receipt(provider, batch_number, tx_hash).await?;
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

    async fn validate_tx_receipt(
        provider: &DynProvider,
        batch_number: u64,
        tx_hash: TxHash,
    ) -> anyhow::Result<()> {
        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await?
            .context("mined transaction receipt is missing")?;
        if receipt.status() {
            // We could also look at tx receipt's logs for a corresponding `BlockCommit` event but
            // not sure if this is 100% necessary yet.
            tracing::info!(
                batch_number,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "batch committed to L1",
            );

            Ok(())
        } else {
            tracing::error!(
                batch_number,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "commit transaction failed"
            );
            if let Ok(trace) = provider
                .debug_trace_transaction(
                    receipt.transaction_hash,
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await
            {
                let call_frame = trace
                    .try_into_call_frame()
                    .expect("requested call tracer but received a different call frame type");
                // We print top-level call frame's output as it likely contains serialized custom
                // error pointing to the underlying problem (i.e. starts with the error's 4byte
                // signature).
                tracing::error!(
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

impl Command {
    pub fn batch_number(&self) -> u64 {
        match self {
            Command::Commit(cmd) => cmd.batch.batch_number,
        }
    }
}

#[derive(Debug)]
struct CommitCommand {
    previous_batch: StoredBatchInfo,
    batch: CommitBatchInfo,
}

impl CommitCommand {
    /// `commitBatchesSharedBridge` expects the rest of calldata to be of very specific form. This
    /// function makes sure last committed batch and new batch are encoded correctly.
    fn to_calldata_suffix(&self) -> Vec<u8> {
        /// Current commitment encoding version as per protocol.
        const SUPPORTED_ENCODING_VERSION: u8 = 0;

        let stored_batch_info = IExecutor::StoredBatchInfo::from(&self.previous_batch);
        let commit_batch_info = IExecutor::CommitBoojumOSBatchInfo::from(self.batch.clone());
        tracing::debug!(
            last_batch_hash = ?self.previous_batch.hash(),
            last_batch_number = ?self.previous_batch.batch_number,
            new_batch_number = ?commit_batch_info.batchNumber,
            "preparing commit calldata"
        );
        let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

        // Prefixed by current encoding version as expected by protocol
        [[SUPPORTED_ENCODING_VERSION].to_vec(), encoded_data]
            .concat()
            .to_vec()
    }

    /// Prepares a call to `commitBatchesSharedBridge` that should be executed as the result of this
    /// command.
    fn to_call(&self, chain_id: u64) -> commitBatchesSharedBridgeCall {
        IExecutor::commitBatchesSharedBridgeCall::new((
            U256::from(chain_id),
            U256::from(self.previous_batch.batch_number + 1),
            U256::from(self.batch.batch_number),
            self.to_calldata_suffix().into(),
        ))
    }
}
