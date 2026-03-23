use crate::commands::{L1SenderCommand, commit::CommitCommand};
use alloy::eips::BlockId;
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::Address;
use alloy::primitives::utils::format_ether;
use alloy::providers::ext::DebugApi;
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::Filter;
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::sol_types::SolEvent;
use alloy::sol_types::SolValue;
use anyhow::Context as _;
use async_trait::async_trait;
use std::cmp::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IChainTypeManager::{DiamondCutData, NewUpgradeCutData};
use zksync_os_contract_interface::IValidatorTimelock::{self, IValidatorTimelockInstance};
use zksync_os_contract_interface::ZkChain;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_types::ProtocolSemanticVersion;

const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);
const UPGRADE_DATA_MAX_BLOCKS_TO_PROCESS: u64 = 10_000;

/// Receives Batches with proofs - potentially with incompatible protocol version.
/// Makes sure that batches are only passed to L1 if batch version matches the current protocol version.
#[derive(Debug)]

pub struct UpgradeGatekeeper<P>
where
    P: Provider + WalletProvider<Wallet = EthereumWallet> + Clone,
{
    provider: P,
    chain_address: Address,
    validator_timelock_address: Address,
    operator_signer: SignerConfig,
    max_fee_per_gas_wei: u128,
    max_priority_fee_per_gas_wei: u128,
    poll_interval: Duration,
}

impl<P> UpgradeGatekeeper<P>
where
    P: Provider + WalletProvider<Wallet = EthereumWallet> + Clone,
{
    pub fn new(
        provider: P,
        chain_address: Address,
        validator_timelock_address: Address,
        operator_signer: SignerConfig,
        max_fee_per_gas_wei: u128,
        max_priority_fee_per_gas_wei: u128,
        poll_interval: Duration,
    ) -> Self {
        Self {
            provider,
            chain_address,
            validator_timelock_address,
            operator_signer,
            max_fee_per_gas_wei,
            max_priority_fee_per_gas_wei,
            poll_interval,
        }
    }

    async fn current_protocol_version(
        &self,
        zk_chain: &ZkChain<P>,
    ) -> anyhow::Result<ProtocolSemanticVersion> {
        let current_protocol_version =
            zk_chain
                .get_raw_protocol_version(BlockId::latest())
                .await
                .context("Failed to fetch current protocol version from L1")?;
        let current_protocol_version =
            ProtocolSemanticVersion::try_from(current_protocol_version).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid protocol version fetched from L1: {e}; protocol_version: {current_protocol_version}"
                )
            })?;
        Ok(current_protocol_version)
    }

    async fn wait_until_protocol_version(
        &self,
        zk_chain: &ZkChain<P>,
        target_protocol_version: &ProtocolSemanticVersion,
    ) -> anyhow::Result<()> {
        let mut current_protocol_version = self.current_protocol_version(zk_chain).await?;
        tracing::info!(
            %current_protocol_version,
            %target_protocol_version,
            "Waiting for L1 protocol version {current_protocol_version} to reach target version {target_protocol_version}",
        );
        loop {
            match current_protocol_version.cmp(target_protocol_version) {
                Ordering::Greater => {
                    // We don't expect protocol version on L1 to be greater than the version of non-committed
                    // batch, it's an unexpected hard error.
                    anyhow::bail!(
                        "Protocol version on the contract {current_protocol_version} is greater than protocol version for the next uncommitted batch: {target_protocol_version}"
                    );
                }
                Ordering::Equal => {
                    tracing::info!(
                        %current_protocol_version,
                        "Protocol version on the contract matches batch protocol version"
                    );
                    return Ok(());
                }
                Ordering::Less => {
                    tracing::debug!(
                        %current_protocol_version,
                        %target_protocol_version,
                        "Protocol version on L1 is still less than target version, waiting"
                    );
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
            current_protocol_version = self.current_protocol_version(zk_chain).await?;
        }
    }

    async fn wait_until_batches_executed(
        &self,
        zk_chain: &ZkChain<P>,
        last_old_batch_number: u64,
    ) -> anyhow::Result<()> {
        if last_old_batch_number == 0 {
            return Ok(());
        }
        loop {
            let executed_batches = zk_chain
                .get_total_batches_executed(BlockId::latest())
                .await
                .context("Failed to fetch total batches executed from L1")?;
            if executed_batches >= last_old_batch_number {
                return Ok(());
            }
            tracing::debug!(
                executed_batches,
                last_old_batch_number,
                "Waiting for old protocol batches to be executed on L1"
            );
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Applies the protocol upgrade by calling `upgradeChainFromVersion` on the validator timelock.
    ///
    /// Returns `Ok(true)` if the upgrade was applied (modern v31+ timelock).
    /// Returns `Ok(false)` if the timelock is legacy (v30.2 and earlier): the caller should
    /// fall back to passively waiting for the protocol version to change on L1 (the upgrade
    /// will be applied externally, e.g. by governance or a test harness).
    async fn finalize_upgrade(
        &self,
        upgrade_target: &IValidatorTimelockInstance<P, Ethereum>,
        operator_address: Address,
        old_protocol_version: &ProtocolSemanticVersion,
        diamond_cut_data: DiamondCutData,
    ) -> anyhow::Result<bool> {
        // Check UPGRADER_ROLE if the validator timelock supports role-based access control.
        // Older validator timelocks (v30.2 and earlier) don't expose UPGRADER_ROLE/hasRole —
        // on those chains the protocol upgrade must be applied externally (e.g. by governance
        // or a test harness). Return `Ok(false)` to signal the caller to wait passively.
        match upgrade_target.UPGRADER_ROLE().call().await {
            Ok(upgrader_role) => {
                let has_upgrader_role = upgrade_target
                    .hasRole(self.chain_address, upgrader_role, operator_address)
                    .call()
                    .await
                    .context("Failed to check UPGRADER_ROLE for finalize upgrade operator")?;
                if !has_upgrader_role {
                    anyhow::bail!(
                        "Finalize upgrade operator {operator_address:?} lacks UPGRADER_ROLE for chain {chain_address:?}",
                        operator_address = operator_address,
                        chain_address = self.chain_address
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "Validator timelock does not support UPGRADER_ROLE (legacy v30 timelock); \
                     waiting for upgrade to be applied externally"
                );
                return Ok(false);
            }
        };

        let packed_old_protocol_version = old_protocol_version
            .packed()
            .context("Failed to pack old protocol version for finalize upgrade")?;

        let eip1559_est = upgrade_target
            .provider()
            .estimate_eip1559_fees()
            .await
            .context("Failed to estimate EIP-1559 fees for finalize upgrade")?;
        if eip1559_est.max_fee_per_gas > self.max_fee_per_gas_wei {
            tracing::warn!(
                max_fee_per_gas = self.max_fee_per_gas_wei,
                estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
                "Finalize upgrade maxFeePerGas is lower than the one estimated from network"
            );
        }
        if eip1559_est.max_priority_fee_per_gas > self.max_priority_fee_per_gas_wei {
            tracing::warn!(
                max_priority_fee_per_gas = self.max_priority_fee_per_gas_wei,
                estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
                "Finalize upgrade maxPriorityFeePerGas is lower than the one estimated from network"
            );
        }
        let diamond_cut_bytes = diamond_cut_data.abi_encode();
        tracing::info!(
            diamond_cut_bytes_len = diamond_cut_bytes.len(),
            "Diamond cut data prepared"
        );

        let tx_request = upgrade_target
            .upgradeChainFromVersion(
                self.chain_address,
                packed_old_protocol_version,
                diamond_cut_data,
            )
            .into_transaction_request()
            .with_from(operator_address)
            .with_max_fee_per_gas(self.max_fee_per_gas_wei)
            .with_max_priority_fee_per_gas(self.max_priority_fee_per_gas_wei);

        let tx_handle = match upgrade_target
            .provider()
            .send_transaction(tx_request.clone())
            .await
        {
            Ok(handle) => handle,
            Err(err) => {
                let call_err = upgrade_target
                    .provider()
                    .call(tx_request.clone())
                    .await
                    .err();
                tracing::error!(
                    ?err,
                    ?call_err,
                    chain_address = ?self.chain_address,
                    upgrade_target = ?upgrade_target.address(),
                    old_protocol_version = ?packed_old_protocol_version,
                    "Finalize upgrade transaction was rejected by the L1 provider"
                );
                return Err(err).context("Failed to send finalize upgrade transaction");
            }
        };

        let receipt = tx_handle
            .with_required_confirmations(1)
            .with_timeout(Some(TRANSACTION_TIMEOUT))
            .get_receipt()
            .await
            .context("Finalize upgrade transaction was not included on L1 in time")?;
        validate_tx_receipt(upgrade_target.provider(), receipt).await?;
        Ok(true)
    }

    async fn fetch_diamond_cut_data(
        &self,
        zk_chain: &ZkChain<P>,
        new_protocol_version: &ProtocolSemanticVersion,
    ) -> anyhow::Result<DiamondCutData> {
        let chain_type_manager = zk_chain
            .get_chain_type_manager()
            .await
            .context("Failed to fetch chain type manager for finalize upgrade")?;
        let raw_protocol_version = new_protocol_version
            .packed()
            .context("Failed to pack new protocol version for finalize upgrade")?;
        let mut current_block = zk_chain
            .provider()
            .get_block_number()
            .await
            .context("Failed to fetch current L1 block number for finalize upgrade")?;
        let start_block = current_block.saturating_sub(100_000).max(1u64);
        // Scan backwards in chunks to avoid requesting too many blocks at once.
        let mut upgrade_cut_data_logs = Vec::new();
        while current_block >= start_block && upgrade_cut_data_logs.is_empty() {
            let from_block = current_block
                .saturating_sub(UPGRADE_DATA_MAX_BLOCKS_TO_PROCESS.saturating_sub(1))
                .max(start_block);
            let filter = Filter::new()
                .from_block(from_block)
                .to_block(current_block)
                .address(chain_type_manager)
                .event_signature(NewUpgradeCutData::SIGNATURE_HASH)
                .topic1(raw_protocol_version);
            upgrade_cut_data_logs = zk_chain
                .provider()
                .get_logs(&filter)
                .await
                .context("Failed to fetch upgrade cut data logs for finalize upgrade")?;
            current_block = from_block.saturating_sub(1);
        }
        if upgrade_cut_data_logs.is_empty() {
            anyhow::bail!(
                "No upgrade cut found for protocol version {}",
                new_protocol_version
            );
        }
        if upgrade_cut_data_logs.len() > 1 {
            tracing::warn!(
                %new_protocol_version,
                "Multiple upgrade cuts found for protocol version; using the most recent one"
            );
        }
        let upgrade_cut_data = upgrade_cut_data_logs.last().unwrap();
        let raw_diamond_cut: alloy::rpc::types::Log<NewUpgradeCutData> = upgrade_cut_data
            .log_decode()
            .context("Failed to decode upgrade cut data log")?;
        Ok(raw_diamond_cut.inner.data.diamondCutData)
    }
}

#[async_trait]
impl<P> PipelineComponent for UpgradeGatekeeper<P>
where
    P: Provider + WalletProvider<Wallet = EthereumWallet> + Clone + Send + Sync + 'static,
{
    type Input = L1SenderCommand<CommitCommand>;
    type Output = L1SenderCommand<CommitCommand>;

    const NAME: &'static str = "upgrade_gatekeeper";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("upgrade_gatekeeper", GenericComponentState::WaitingRecv);

        let mut provider = self.provider.clone();
        let operator_address = match register_operator(&mut provider, &self.operator_signer).await {
            Ok(address) => address,
            Err(err) => {
                tracing::error!(?err, "Failed to initialize upgrade gatekeeper operator");
                return Ok(());
            }
        };
        let zk_chain = ZkChain::new(self.chain_address, provider.clone());
        let upgrade_target = IValidatorTimelock::new(self.validator_timelock_address, provider);
        let mut current_protocol_version = match self.current_protocol_version(&zk_chain).await {
            Ok(version) => version,
            Err(err) => {
                tracing::error!(?err, "Failed to fetch current protocol version at startup");
                return Ok(());
            }
        };

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let Some(command) = input.recv().await else {
                tracing::error!("UpgradeGatekeeper input stream ended unexpectedly");
                return Ok(());
            };

            // When the batch carries a newer protocol version than what is currently on L1,
            // we need to trigger the upgrade. There are two upgrade paths:
            //
            // * Modern (v31+): call `upgradeChainFromVersion` on the ValidatorTimelock,
            //   then wait for the L1 protocol version to change before committing the batch.
            //
            // * Legacy (v30.2 and earlier): the ValidatorTimelock does not expose
            //   `upgradeChainFromVersion`. We wait passively for the protocol version to
            //   change on L1 (the upgrade must be applied externally, e.g. by governance).
            if let L1SenderCommand::SendToL1(command) = &command {
                latency_tracker.enter_state(GenericComponentState::Processing);

                let batch_protocol_version = command.input().batch.protocol_version.clone();
                let last_old_batch_number = command.input().batch_number().saturating_sub(1);

                tracing::info!(
                    %current_protocol_version,
                    %batch_protocol_version,
                    last_old_batch_number,
                    "Current protocol version: {current_protocol_version}, batch protocol version: {batch_protocol_version}, last old batch number: {last_old_batch_number}"
                );
                match current_protocol_version.cmp(&batch_protocol_version) {
                    Ordering::Greater => {
                        tracing::error!(
                            %current_protocol_version,
                            %batch_protocol_version,
                            last_old_batch_number,
                            "Protocol version on the contract is greater than next batch version; skipping finalize attempt"
                        );
                    }
                    Ordering::Equal => {}
                    Ordering::Less => {
                        tracing::info!(
                            %current_protocol_version,
                            %batch_protocol_version,
                            last_old_batch_number,
                            "Detected protocol upgrade, waiting for batches to be executed and finalizing upgrade"
                        );
                        if let Err(err) = self
                            .wait_until_batches_executed(&zk_chain, last_old_batch_number)
                            .await
                        {
                            tracing::error!(
                                ?err,
                                "Failed while waiting for old batches to execute"
                            );
                        } else {
                            let diamond_cut_data = match self
                                .fetch_diamond_cut_data(&zk_chain, &batch_protocol_version)
                                .await
                            {
                                Ok(data) => data,
                                Err(err) => {
                                    tracing::error!(?err, "Failed to fetch diamond cut data");
                                    continue;
                                }
                            };
                            let upgrade_applied = match self
                                .finalize_upgrade(
                                    &upgrade_target,
                                    operator_address,
                                    &current_protocol_version,
                                    diamond_cut_data,
                                )
                                .await
                            {
                                Ok(applied) => applied,
                                Err(err) => {
                                    tracing::error!(?err, "Failed to finalize upgrade");
                                    continue;
                                }
                            };

                            if upgrade_applied {
                                tracing::info!(
                                    %batch_protocol_version,
                                    "Upgrade applied via ValidatorTimelock; waiting for L1 version update"
                                );
                            } else {
                                tracing::info!(
                                    %batch_protocol_version,
                                    "Legacy timelock detected; waiting for external upgrade"
                                );
                            }
                            // In both cases, wait for the L1 protocol version to match
                            // the batch version before sending the batch downstream.
                            if let Err(err) = self
                                .wait_until_protocol_version(&zk_chain, &batch_protocol_version)
                                .await
                            {
                                tracing::error!(
                                    ?err,
                                    "Failed while waiting for protocol version update"
                                );
                                continue;
                            }
                            current_protocol_version = batch_protocol_version;
                        }
                    }
                }
            }

            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            output.send(command).await?;
        }
    }
}

async fn register_operator<P: Provider + WalletProvider<Wallet = EthereumWallet>>(
    provider: &mut P,
    signer_config: &SignerConfig,
) -> anyhow::Result<Address> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        anyhow::bail!("Finalize upgrade operator address {address} has zero balance");
    }
    tracing::info!(
        balance_eth = format_ether(balance),
        %address,
        "initialized finalize upgrade operator"
    );
    Ok(address)
}

async fn validate_tx_receipt(
    provider: &impl Provider,
    receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    if receipt.status() {
        tracing::info!(
            tx_hash = ?receipt.transaction_hash,
            l1_block_number = receipt.block_number.unwrap_or_default(),
            "Finalize upgrade transaction succeeded"
        );
        Ok(())
    } else {
        if let Ok(trace) = provider
            .debug_trace_transaction(
                receipt.transaction_hash,
                GethDebugTracingOptions::call_tracer(CallConfig::default()),
            )
            .await
            && let Ok(call_frame) = trace.try_into_call_frame()
        {
            tracing::error!(
                ?call_frame.output,
                ?call_frame.error,
                ?call_frame.revert_reason,
                "Finalize upgrade transaction failed call frame"
            );
        }
        anyhow::bail!(
            "Finalize upgrade transaction failed on L1 (tx_hash='{:?}')",
            receipt.transaction_hash
        );
    }
}
