pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod commitment;
pub mod config;
mod metrics;
mod new_blocks;
pub mod pipeline_component;

use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::commands::L1SenderCommand;
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::new_blocks::NewBlocks;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, BlockNumber, TxHash};
use alloy::providers::ext::DebugApi;
use alloy::providers::{DynProvider, Provider, WalletProvider};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{Block, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::TransportResult;
use anyhow::Context;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::PeekableReceiver;

/// Process responsible for sending transactions to L1.
/// Handles one type of l1 command (e.g. Commit or Prove).
/// Loads up to `command_limit` commands from the channel and sends them to L1 in parallel.
/// Waits for all transactions to be mined, sends them to the output channel
/// and then starts with the next `command_limit` commands.
///
/// Important: the same provider (sender address) must not be used outside this process.
///     Otherwise, there will be a nonce conflict and a failed L1 transaction
///     (recoverable on restart)
///
/// Known issues:
///   * Crashes when there is a gap in incoming L1 blocks (happens periodically with Infura provider)
///   * Does not attempt to detect in-flight L1 transactions on startup - just crashes if they get mined
///
/// Note: we pass `to_address` - L1 contract address to send transactions to.
/// It differs between commit/prove/execute (e.g., timelock vs diamond proxy)
pub async fn run_l1_sender<Input: L1SenderCommand>(
    // == plumbing ==
    mut inbound: PeekableReceiver<Input>,
    outbound: Sender<BatchEnvelope<FriProof>>,

    // == command-specific settings ==
    to_address: Address,

    // == config ==
    mut provider: impl Provider + WalletProvider<Wallet = EthereumWallet> + 'static,
    config: L1SenderConfig<Input>,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);

    let operator_address =
        register_operator::<_, Input>(&mut provider, config.operator_pk.clone()).await?;
    let provider = provider.erased();
    let mut heartbeat = Heartbeat::new(provider.clone(), config.poll_interval).await?;
    let mut cmd_buffer = Vec::with_capacity(config.command_limit);

    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        // This sleeps until **at least one** command is received from the channel. Additionally,
        // receives up to `self.command_limit` commands from the channel if they are ready (i.e. does
        // not wait for them). Extends `cmd_buffer` with received values and, as `cmd_buffer` is
        // emptied in every iteration, its size never exceeds `self.command_limit`.
        let received = inbound
            .recv_many(&mut cmd_buffer, config.command_limit)
            .await;
        // This method only returns `0` if the channel has been closed and there are no more items
        // in the queue.
        if received == 0 {
            anyhow::bail!("inbound channel closed");
        }
        latency_tracker.enter_state(L1SenderState::SendingToL1);
        let range = Input::display_range(&cmd_buffer); // Only for logging
        let command_name = Input::NAME;
        tracing::info!(command_name, range, "Sending l1 transactions...");
        L1_SENDER_METRICS.parallel_transactions[&command_name].set(cmd_buffer.len() as u64);
        // It's important to preserve the order of commands -
        // so that we send them downstream also in order.
        // This holds true because l1 transactions are included in the order of sender nonce.
        // Keep this in mind if changing sending logic (that is, if adding `buffer` we'd need to set nonce manually)
        let pending_tx_hashes: HashMap<TxHash, Input> = futures::stream::iter(cmd_buffer.drain(..))
            .then(|mut cmd| async {
                let tx_request = tx_request_with_gas_fields(
                    &provider,
                    operator_address,
                    config.max_fee_per_gas(),
                    config.max_priority_fee_per_gas(),
                )
                .await?
                .with_to(to_address)
                .with_call(&cmd.solidity_call());
                let pending_tx_hash = *provider.send_transaction(tx_request).await?.tx_hash();
                cmd.as_mut()
                    .iter_mut()
                    .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
                anyhow::Ok((pending_tx_hash, cmd))
            })
            // We could buffer the stream here to enable sending multiple batches of transactions in parallel,
            // but this is not necessary for now - we wait for them to be included in parallel
            .try_collect::<HashMap<TxHash, Input>>()
            .await?;
        tracing::info!(command_name, range, "Sent to L1. Waiting for inclusion...");
        latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
        let mined_envelopes = heartbeat
            .wait_for_pending_txs(&provider, pending_tx_hashes)
            .await?;
        let balance = format_ether(provider.get_balance(operator_address).await?);
        let nonce = provider.get_transaction_count(operator_address).await?;
        tracing::info!(
            command_name,
            range,
            balance,
            nonce,
            "All transactions included. Sending downstream.",
        );
        L1_SENDER_METRICS.balance[&command_name].set(balance.parse()?);
        L1_SENDER_METRICS.nonce[&command_name].set(nonce);
        latency_tracker.enter_state(L1SenderState::WaitingSend);
        for command in mined_envelopes {
            for mut output_envelope in command.into() {
                output_envelope.set_stage(Input::MINED_STAGE);
                outbound.send(output_envelope).await?;
            }
        }
    }
}

async fn tx_request_with_gas_fields(
    provider: &DynProvider,
    operator_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> anyhow::Result<TransactionRequest> {
    let eip1559_est = provider.estimate_eip1559_fees().await?;
    tracing::debug!(
        eip1559_est.max_priority_fee_per_gas,
        "estimated median priority fee (20% percentile) for the last 10 blocks"
    );
    if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(
            max_fee_per_gas = max_fee_per_gas,
            estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
            "L1 sender's configured maxFeePerGas is lower than the one estimated from network"
        );
    }
    if eip1559_est.max_priority_fee_per_gas > max_priority_fee_per_gas {
        tracing::warn!(
            max_priority_fee_per_gas = max_priority_fee_per_gas,
            estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
            "L1 sender's configured maxPriorityFeePerGas is lower than the one estimated from network"
        );
    }

    let tx = TransactionRequest::default()
        .with_from(operator_address)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
        .with_gas_limit(15000000);
    Ok(tx)
}

async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: L1SenderCommand,
>(
    provider: &mut P,
    private_key: SecretString,
) -> anyhow::Result<Address> {
    let signer = PrivateKeySigner::from_str(private_key.expose_secret())
        .context("failed to parse operator private key")?;
    let address = signer.address();
    provider.wallet_mut().register_signer(signer);

    let balance = provider.get_balance(address).await?;
    L1_SENDER_METRICS.balance[&Input::NAME].set(format_ether(balance).parse()?);
    let address_string: &'static str = address.to_string().leak();
    L1_SENDER_METRICS.l1_operator_address[&(Input::NAME, address_string)].set(1);

    if balance.is_zero() {
        anyhow::bail!("L1 sender's address {address} has zero balance");
    }

    tracing::info!(
        balance_eth = format_ether(balance),
        %address,
        "{} Initialized L1 sender",
        Input::NAME
    );
    Ok(address)
}

/// L1 sender's heartbeat that monitors all L1 blocks and enforces that we receive them sequentially
struct Heartbeat {
    last_l1_block_number: Option<BlockNumber>,
    l1_block_stream: BoxStream<'static, TransportResult<Block>>,
}

impl Heartbeat {
    async fn new(provider: DynProvider, poll_interval: Duration) -> anyhow::Result<Self> {
        let current_block = provider.get_block_number().await?;
        let new_blocks = NewBlocks::new(provider, current_block + 1, poll_interval);
        let l1_block_stream = new_blocks.into_block_stream().boxed();
        Ok(Self {
            last_l1_block_number: None,
            l1_block_stream,
        })
    }

    async fn wait_for_pending_txs<Input: L1SenderCommand>(
        &mut self,
        provider: &DynProvider,
        mut pending_tx_hashes: HashMap<TxHash, Input>,
    ) -> anyhow::Result<Vec<Input>> {
        let mut complete: Vec<Input> = Vec::new();
        while !pending_tx_hashes.is_empty() {
            let Some(block) = self.l1_block_stream.next().await else {
                anyhow::bail!("L1 block stream has been closed unexpectedly");
            };
            let block = block?;

            if let Some(last_l1_block_number) = self.last_l1_block_number
                && block.header.number != last_l1_block_number + 1
            {
                // This can happen if there is a reorg on L1. As a temporary measure we restart which
                // forces us to re-initialize L1 sender.
                anyhow::bail!(
                    "received non-sequential L1 block #{} (expected #{}); restarting",
                    block.header.number,
                    last_l1_block_number + 1
                );
            }
            self.last_l1_block_number.replace(block.header.number);

            let mut mined_pending_txs = 0;
            for tx_hash in block.transactions.hashes() {
                if let Some(command) = pending_tx_hashes.remove(&tx_hash) {
                    Self::validate_tx_receipt(provider, &command, tx_hash).await?;
                    complete.push(command);
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
                "{}: received new L1 block",
                Input::NAME
            );
        }
        Ok(complete)
    }

    async fn validate_tx_receipt<Input: L1SenderCommand>(
        provider: &DynProvider,
        command: &Input,
        tx_hash: TxHash,
    ) -> anyhow::Result<()> {
        let receipt = loop {
            let maybe_res = provider.get_transaction_receipt(tx_hash).await?;
            if let Some(res) = maybe_res {
                break res;
            }
            tracing::info!(
                "L1 transaction receipt for a mined block is not available yet. Retrying..."
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        if receipt.status() {
            // Transaction succeeded - log output and return OK(())

            // We could also look at tx receipt's logs for a corresponding
            // `BlockCommit` / `BlockProve`/ etc event but
            // not sure if this is 100% necessary yet.

            let l2_txs_count: usize = command
                .as_ref()
                .iter()
                .map(|envelope| envelope.batch.tx_count)
                .sum();
            let l1_transaction_fee = receipt.gas_used as u128 * receipt.effective_gas_price;

            tracing::info!(
                %command,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                gas_used = receipt.gas_used,
                gas_used_per_l2_tx = receipt.gas_used / l2_txs_count as u64,
                l1_transaction_fee_ether = format_ether(l1_transaction_fee),
                l1_transaction_fee_ether_per_l2_tx = format_ether(l1_transaction_fee / l2_txs_count as u128),
                "succeeded on L1",
            );
            L1_SENDER_METRICS.gas_used[&Input::NAME].observe(receipt.gas_used);
            L1_SENDER_METRICS.gas_used_per_l2_tx[&Input::NAME]
                .observe(receipt.gas_used / l2_txs_count as u64);
            L1_SENDER_METRICS.l1_transaction_fee_ether[&Input::NAME]
                .observe(format_ether(l1_transaction_fee).parse()?);
            L1_SENDER_METRICS.l1_transaction_fee_per_l2_tx_ether[&Input::NAME]
                .observe(format_ether(l1_transaction_fee / l2_txs_count as u128).parse()?);

            Ok(())
        } else {
            tracing::error!(
                %command,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "failed on L1",
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
                "{} L1 command transaction failed, see L1 transaction's trace for more details (tx_hash='{:?}')",
                command,
                receipt.transaction_hash
            );
        }
    }
}
