use std::time::Duration;

use super::ProtocolUpgradeBuilder;
use super::default_upgrade::DefaultUpgrade;
use super::interfaces;
use crate::Tester;
use crate::assert_traits::ReceiptAssert;
use crate::config::load_chain_config;
use crate::dyn_wallet_provider::EthDynProvider;
use crate::provider::{ZksyncApi as _, ZksyncTestingProvider as _};
use crate::upgrade::interfaces::ChainAssetHandlerBase::ChainAssetHandlerBaseInstance;
use crate::upgrade::interfaces::ChainTypeManagerV30::ChainTypeManagerV30Instance;
use crate::upgrade::interfaces::FacetCut;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::providers::ext::AnvilApi;
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider};
use alloy::rpc::types::{TransactionInput, TransactionReceipt, TransactionRequest};
use anyhow::Context;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_server::config::Config;
use zksync_os_types::{
    L1PriorityTxType, L1TxType, ProtocolSemanticVersion, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
};

/// Object that helps with preparation and execution of protocol upgrades in integration tests.
///
/// Tester assumes that governance is an EOA account, and uses impersonation
/// to execute the upgrade with it.
#[derive(Debug)]
pub struct UpgradeTester {
    pub tester: Tester,
    // Bridgehub contract on L1
    pub bridgehub_l1: zksync_os_contract_interface::Bridgehub<DynProvider>,
    // Bridgehub contract on SL
    pub bridgehub_sl: interfaces::Bridgehub::BridgehubInstance<EthDynProvider>,
    // Bridgehub owner address on SL
    pub bridgehub_owner_sl: Address,
    // CTM contract on SL
    pub ctm_sl: interfaces::ChainTypeManager::ChainTypeManagerInstance<EthDynProvider>,
    // CTM owner address on SL
    pub ctm_owner_sl: Address,
    // L1 chain admin contract
    pub l1_chain_admin: interfaces::ChainAdmin::ChainAdminInstance<EthDynProvider>,
    // L1 chain admin owner address
    pub l1_chain_admin_owner: Address,
    // L1 chain admin for gateway contract address
    pub l1_chain_admin_gateway: Option<Address>,
    // Diamond proxy on the settlement layer
    pub diamond_proxy_sl: interfaces::ZkChain::ZkChainInstance<EthDynProvider>,
    // Diamond proxy owner address
    pub diamond_proxy_admin_sl: Address,
    // Bytecode supplier contract
    pub bytecode_supplier: interfaces::BytecodesSupplier::BytecodesSupplierInstance<EthDynProvider>,
    // Current protocol version
    pub protocol_version: ProtocolSemanticVersion,
    // If chain settles to gateway
    pub settles_to_gateway: bool,
}

impl UpgradeTester {
    /// Prepares tester for the default upgrade scenario.
    pub async fn for_default_upgrade(tester: Tester) -> anyhow::Result<Self> {
        let upgrade_tester = Self::fetch(tester).await?;
        upgrade_tester.enable_impersonation().await?;
        upgrade_tester.wait_for_genesis_upgrade().await?;
        Ok(upgrade_tester)
    }

    /// Executes a "default" flow for `DefaultUpgrade`.
    pub async fn execute_default_upgrade(
        &self,
        protocol_upgrade: &interfaces::ProposedUpgrade,
        deadline: U256,
        upgrade_timestamp: U256,
        patch_only: bool,
        facet_cuts: Vec<FacetCut>,
    ) -> anyhow::Result<()> {
        // Deploy the upgrade contract on SL.
        let upgrade_contract =
            DefaultUpgrade::deploy(self.tester.sl_provider(), protocol_upgrade).await?;
        tracing::info!("DefaultUpgrade contract deployed");

        // Send pause migration to Bridgehub
        self.pause_bridgehub_migrations().await?;
        tracing::info!("Bridgehub migrations are paused");

        // CTM upgrade, `setNewVersionUpgrade` call;
        let upgrade_data = upgrade_contract.diamond_cut_data(facet_cuts);
        self.set_new_version_on_ctm(
            upgrade_data.clone(),
            deadline,
            protocol_upgrade.newProtocolVersion,
        )
        .await?;
        tracing::info!("Upgrade is set on CTM");

        // Set timestamp for upgrade on a specific chain under stm, `setUpgradeTimestamp` call on L1ChainAdmin
        self.set_upgrade_timestamp(protocol_upgrade.newProtocolVersion, upgrade_timestamp)
            .await?;
        tracing::info!("Upgrade scheduled on L1");

        if patch_only {
            // TODO: for patch upgrades, there is no L2 upgrade transaction, so we must be somewhat probabilistic.
            // We will wait until the timestamp + some margin, then fetch the current l2 block and wait until it's finalized.
            let upgrade_timestamp_secs = u64::try_from(upgrade_timestamp).unwrap();
            let wait_duration = Duration::from_secs(upgrade_timestamp_secs).saturating_sub(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?,
            ) + Duration::from_secs(10); // 10 seconds margin
            tracing::info!(
                "Waiting for {:?} until patch upgrade can be executed",
                wait_duration
            );
            tokio::time::sleep(wait_duration).await;
            // We have to wait for the last block before the upgrade to be finalized.
            // If gateway is involved we cannot know for sure in which block is the last one.
            // If there is no gateway we rely on the fact that there is no unprocessed txs on L2 after upgrade was fetched,
            // so no new blocks are processed and so the latest block is the last one before upgrade.
            if self.settles_to_gateway {
                tokio::time::sleep(Duration::from_secs(30)).await;
            } else {
                let current_l2_block = self.tester.l2_zk_provider.get_block_number().await?;
                tracing::info!("Proceeding with patch upgrade");
                self.tester
                    .l2_zk_provider
                    .wait_finalized_with_timeout(
                        current_l2_block,
                        crate::assert_traits::DEFAULT_TIMEOUT,
                    )
                    .await?;
                tracing::info!("Current L2 block is finalized, proceeding with patch upgrade");
            }
        } else {
            // Wait until the block _before_ upgrade tx is finalized on L1.
            self.wait_for_upgrade(upgrade_contract.upgrade_tx_l2_hash())
                .await?;
            tracing::info!("Block before upgrade tx is finalized on L1");
        }

        self.upgrade_chain(upgrade_data).await?;
        tracing::info!("Upgrade tx is executed on SL");

        if patch_only {
            // For patch upgrades, we need to trigger a transaction finalization, since there is no upgrade tx.
            // So we send a bogus tx and wait until it's finalized instead.
            let tx = self
                .tester
                .l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(self.bridgehub_owner_sl) // Random address
                        .with_value(U256::from(1u64)),
                )
                .await?
                .expect_successful_receipt()
                .await?;
            self.tester
                .l2_zk_provider
                .wait_finalized_with_timeout(
                    tx.block_number.unwrap(),
                    crate::assert_traits::DEFAULT_TIMEOUT,
                )
                .await?;
        } else {
            self.wait_for_upgrade_finalization(upgrade_contract.upgrade_tx_l2_hash())
                .await?;
        }
        tracing::info!("Upgrade tx is finalized on L1");

        Ok(())
    }

    // Fetch the contracts configuration from the tester.
    async fn fetch(tester: Tester) -> anyhow::Result<Self> {
        let chain_config: Config = load_chain_config(tester.chain_layout);
        let chain_id = chain_config
            .genesis_config
            .chain_id
            .expect("Chain id is missing in the config");

        let bridgehub_address_l1 = tester.l2_zk_provider.get_bridgehub_contract().await?;
        let l1_state = L1State::fetch(
            tester.l1_provider().clone().erased(),
            tester.sl_provider().clone().erased(),
            bridgehub_address_l1,
            chain_id,
        )
        .await?;
        let bridgehub_sl = interfaces::Bridgehub::new(
            *l1_state.bridgehub_sl.address(),
            tester.sl_provider().clone(),
        );
        let bridgehub_owner_sl = bridgehub_sl.owner().call().await?;

        let ctm_sl_address = l1_state.bridgehub_sl.chain_type_manager_address().await?;
        let ctm_sl =
            interfaces::ChainTypeManager::new(ctm_sl_address, tester.sl_provider().clone());
        let raw_protocol_version = ctm_sl
            .getProtocolVersion(U256::from(chain_id))
            .call()
            .await?;
        let protocol_version = ProtocolSemanticVersion::try_from(raw_protocol_version)
            .expect("invalid protocol version stored in CTM");
        let ctm_owner_sl = ctm_sl.owner().call().await?;

        let diamond_proxy_sl = l1_state.bridgehub_sl.zk_chain().await?;
        let diamond_proxy_sl =
            interfaces::ZkChain::new(*diamond_proxy_sl.address(), tester.sl_provider().clone());
        let diamond_proxy_admin_sl = diamond_proxy_sl.getAdmin().call().await?;

        let diamond_proxy_l1 = l1_state.bridgehub_l1.zk_chain().await?;
        let l1_chain_admin =
            interfaces::ZkChain::new(*diamond_proxy_l1.address(), tester.l1_provider().clone())
                .getAdmin()
                .call()
                .await?;
        let l1_chain_admin =
            interfaces::ChainAdmin::new(l1_chain_admin, tester.l1_provider().clone());

        let l1_chain_admin_owner = l1_chain_admin.owner().call().await?;

        let settles_to_gateway = l1_state.l1_chain_id != l1_state.sl_chain_id;
        let l1_chain_admin_gateway = if settles_to_gateway {
            let bridgehub_l1_for_gw = zksync_os_contract_interface::Bridgehub::new(
                *l1_state.bridgehub_l1.address(),
                tester.l1_provider().clone(),
                l1_state.sl_chain_id,
            );
            Some(
                interfaces::ZkChain::new(
                    *bridgehub_l1_for_gw.zk_chain().await?.address(),
                    tester.l1_provider().clone(),
                )
                .getAdmin()
                .call()
                .await?,
            )
        } else {
            None
        };

        // Bytecode supplier is a bit special: right now it's not discoverable
        // yet, so fetch the expected address from the active chain config.
        let bytecode_supplier_address = chain_config
            .genesis_config
            .bytecode_supplier_address
            .expect("Bytecode supplier address is missing in the config");
        anyhow::ensure!(
            !tester
                .l1_provider()
                .get_code_at(bytecode_supplier_address)
                .await?
                .is_empty(),
            "Bytecode supplier contract is not deployed at expected address {bytecode_supplier_address:?}; if zkos-l1-state.json.gz was updated, update the address in the test code"
        );
        let bytecode_supplier = interfaces::BytecodesSupplier::new(
            bytecode_supplier_address,
            tester.l1_provider().clone(),
        );

        Ok(Self {
            tester,
            bridgehub_l1: l1_state.bridgehub_l1,
            bridgehub_sl,
            bridgehub_owner_sl,
            ctm_sl,
            ctm_owner_sl,
            diamond_proxy_sl,
            diamond_proxy_admin_sl,
            l1_chain_admin,
            l1_chain_admin_owner,
            l1_chain_admin_gateway,
            bytecode_supplier,
            protocol_version,
            settles_to_gateway,
        })
    }

    /// Enables impersonation and adds funds to all the wallets participating in the upgrade.
    async fn enable_impersonation(&self) -> anyhow::Result<()> {
        // Enable impersonation and fund all governance accounts
        for addr in [
            Some(self.bridgehub_owner_sl),
            Some(self.ctm_owner_sl),
            Some(self.diamond_proxy_admin_sl),
            Some(self.l1_chain_admin_owner),
            self.l1_chain_admin_gateway,
        ]
        .into_iter()
        .flatten()
        {
            self.tester
                .l1_provider()
                .anvil_impersonate_account(addr)
                .await?;
            self.tester
                .l1_provider()
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(addr)
                        .with_value(U256::from(10).pow(U256::from(18u64))), // 1 ETH
                )
                .await?
                .expect_successful_receipt()
                .await?;
        }
        Ok(())
    }

    async fn wait_for_genesis_upgrade(&self) -> anyhow::Result<()> {
        // The genesis transaction has to be in the first block, so we wait for block 1 to be finalized.
        self.tester
            .l2_zk_provider
            .wait_finalized_with_timeout(1, crate::assert_traits::DEFAULT_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn wait_for_upgrade(&self, upgrade_tx_l2_hash: B256) -> anyhow::Result<()> {
        let pending_tx = PendingTransactionBuilder::new(
            self.tester.l2_zk_provider.root().clone(),
            upgrade_tx_l2_hash,
        )
        .expect_successful_receipt()
        .await?;
        let upgrade_block_number = pending_tx.block_number.expect("Upgrade tx must be mined");
        let block_before_upgrade = upgrade_block_number
            .checked_sub(1)
            .expect("Upgrade tx can't be in the first block");
        self.tester
            .l2_zk_provider
            .wait_finalized_with_timeout(
                block_before_upgrade,
                crate::assert_traits::DEFAULT_TIMEOUT,
            )
            .await
            .context("Block before upgrade transaction was not finalized")?;
        Ok(())
    }

    async fn wait_for_upgrade_finalization(&self, upgrade_tx_l2_hash: B256) -> anyhow::Result<()> {
        let pending_tx = PendingTransactionBuilder::new(
            self.tester.l2_zk_provider.root().clone(),
            upgrade_tx_l2_hash,
        )
        .expect_successful_receipt()
        .await?;
        let upgrade_block_number = pending_tx.block_number.expect("Upgrade tx must be mined");
        self.tester
            .l2_zk_provider
            .wait_finalized_with_timeout(
                upgrade_block_number,
                crate::assert_traits::DEFAULT_TIMEOUT,
            )
            .await
            .context("Block before upgrade transaction was not finalized")?;
        Ok(())
    }

    pub async fn pause_bridgehub_migrations(&self) -> anyhow::Result<()> {
        if self.settles_to_gateway {
            let chain_asset_handler = self.bridgehub_sl.chainAssetHandler().call().await?;
            let chain_asset_handler = ChainAssetHandlerBaseInstance::new(
                chain_asset_handler,
                self.bridgehub_sl.provider().clone(),
            );
            let chain_asset_handler_owner = chain_asset_handler.owner().call().await?;
            let calldata = chain_asset_handler.pauseMigration().calldata().clone();
            self.send_l1_to_gateway(
                chain_asset_handler_owner,
                *chain_asset_handler.address(),
                calldata,
            )
            .await?;
        } else {
            let pause_migration_tx = if self.tester.chain_layout.protocol_version().contains("v30")
            {
                self.bridgehub_sl
                    .pauseMigration()
                    .into_transaction_request()
                    .with_from(self.bridgehub_owner_sl)
            } else {
                let chain_asset_handler = self.bridgehub_sl.chainAssetHandler().call().await?;
                let chain_asset_handler = ChainAssetHandlerBaseInstance::new(
                    chain_asset_handler,
                    self.bridgehub_sl.provider().clone(),
                );
                chain_asset_handler
                    .pauseMigration()
                    .into_transaction_request()
                    .with_from(self.bridgehub_owner_sl)
            };
            self.send_impersonated_transaction(pause_migration_tx)
                .await?;
        }
        Ok(())
    }

    pub async fn generic_l2_upgrade_target(&self) -> anyhow::Result<(Address, Bytes)> {
        // HACK: right now we need to call an account with bytecode to make the upgrade work.
        // So we deploy a event emitter contract and use it as a delegate.
        let event_emitter =
            crate::contracts::EventEmitter::deploy(self.tester.l2_provider.clone()).await?;
        let event_emitter_calldata = event_emitter
            .emitEvent(U256::from(42u64))
            .calldata()
            .clone();
        Ok((*event_emitter.address(), event_emitter_calldata))
    }

    pub async fn protocol_upgrade_builder(&self) -> anyhow::Result<ProtocolUpgradeBuilder> {
        let delegate_to = self.generic_l2_upgrade_target().await?;
        Ok(ProtocolUpgradeBuilder::new(
            self.protocol_version.clone(),
            delegate_to,
        ))
    }

    pub async fn publish_bytecodes<I: IntoIterator<Item = Bytes>>(
        &self,
        bytecodes: I,
    ) -> anyhow::Result<()> {
        // TODO: right now, using bytecode publisher doesn't work.
        // so instead, we just deploy each contract once on L2 to make sure that preimages are known.
        for bytecode in bytecodes {
            self.tester
                .l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_kind(TxKind::Create)
                        .with_input(bytecode),
                )
                .await?
                .expect_successful_receipt()
                .await?;
        }

        // self.bytecode_supplier
        //     .publishBytecodes(bytecodes.into_iter().collect())
        //     .send()
        //     .await?
        //     .expect_successful_receipt()
        //     .await?;
        Ok(())
    }

    pub async fn set_new_version_on_ctm(
        &self,
        upgrade_data: interfaces::DiamondCutData,
        deadline: U256,
        new_version: U256,
    ) -> anyhow::Result<()> {
        if self.settles_to_gateway {
            let verifier = self.diamond_proxy_sl.getVerifier().call().await?;
            let calldata = self
                .ctm_sl
                .setNewVersionUpgrade(
                    upgrade_data,
                    self.protocol_version
                        .packed()
                        .expect("incorrect protocol version"),
                    deadline,
                    new_version,
                    verifier,
                )
                .calldata()
                .clone();
            self.send_l1_to_gateway(self.ctm_owner_sl, *self.ctm_sl.address(), calldata)
                .await?;
        } else {
            let tx = if self.tester.chain_layout.protocol_version().contains("v30") {
                let ctm = ChainTypeManagerV30Instance::new(
                    *self.ctm_sl.address(),
                    self.ctm_sl.provider().clone(),
                );
                ctm.setNewVersionUpgrade(
                    upgrade_data,
                    self.protocol_version
                        .packed()
                        .expect("incorrect protocol version"),
                    deadline,
                    new_version,
                )
                .into_transaction_request()
                .with_from(self.ctm_owner_sl)
            } else {
                let verifier = self.diamond_proxy_sl.getVerifier().call().await?;
                self.ctm_sl
                    .setNewVersionUpgrade(
                        upgrade_data,
                        self.protocol_version
                            .packed()
                            .expect("incorrect protocol version"),
                        deadline,
                        new_version,
                        verifier,
                    )
                    .into_transaction_request()
                    .with_from(self.ctm_owner_sl)
            };
            self.send_impersonated_transaction(tx).await?;
        }
        Ok(())
    }

    pub async fn set_upgrade_timestamp(
        &self,
        protocol_version: U256,
        timestamp: U256,
    ) -> anyhow::Result<()> {
        let tx = self
            .l1_chain_admin
            .setUpgradeTimestamp(protocol_version, timestamp)
            .into_transaction_request()
            .with_from(self.l1_chain_admin_owner);
        self.send_impersonated_transaction(tx).await?;
        Ok(())
    }

    pub async fn upgrade_chain(
        &self,
        upgrade_data: interfaces::DiamondCutData,
    ) -> anyhow::Result<()> {
        if self.settles_to_gateway {
            let calldata = self
                .diamond_proxy_sl
                .upgradeChainFromVersion(
                    self.protocol_version
                        .packed()
                        .expect("Incorrect protocol version"),
                    upgrade_data,
                )
                .calldata()
                .clone();
            self.send_l1_to_gateway(
                self.diamond_proxy_admin_sl,
                *self.diamond_proxy_sl.address(),
                calldata,
            )
            .await?;
        } else {
            let tx = self
                .diamond_proxy_sl
                .upgradeChainFromVersion(
                    self.protocol_version
                        .packed()
                        .expect("Incorrect protocol version"),
                    upgrade_data,
                )
                .into_transaction_request()
                .with_from(self.diamond_proxy_admin_sl);
            self.send_impersonated_transaction(tx).await?;
        }
        Ok(())
    }

    /// Sends a transaction without a signature, expecting the account to be impersonated.
    /// Expects the transaction to succeed.
    async fn send_impersonated_transaction(
        &self,
        tx: TransactionRequest,
    ) -> anyhow::Result<TransactionReceipt> {
        // `anvil_send_impersonated_transaction` always returns "Insufficient funds" error for some reason.
        // We call `estimate_gas` first to get proper error message.
        let _ = self.tester.l1_provider().estimate_gas(tx.clone()).await?;
        // `anvil_send_impersonated_transaction` allows sending transaction receipt without signatures, unlike
        // `send_transaction`, and doesn't require setting bogus signature and encoding unlike `send_raw_transaction`.
        let hash = self
            .tester
            .l1_provider()
            .anvil_send_impersonated_transaction(tx)
            .await?;
        let receipt =
            PendingTransactionBuilder::new(self.tester.l1_provider().root().clone(), hash)
                .expect_successful_receipt()
                .await?;
        Ok(receipt)
    }

    async fn send_l1_to_gateway(
        &self,
        from: Address,
        to: Address,
        tx_input: impl Into<TransactionInput>,
    ) -> anyhow::Result<()> {
        let tx_input = tx_input.into();
        let bridgehub_l1_for_gw = zksync_os_contract_interface::Bridgehub::new(
            *self.bridgehub_l1.address(),
            self.bridgehub_l1.provider().clone(),
            self.tester.sl_provider.get_chain_id().await?,
        );
        let gw_zk_chain_on_l1 = interfaces::ZkChain::new(
            *bridgehub_l1_for_gw.zk_chain().await?.address(),
            self.bridgehub_l1.provider().clone(),
        );
        let gw_filterer = gw_zk_chain_on_l1.getTransactionFilterer().call().await?;
        let gw_filterer = interfaces::GatewayTransactionFilterer::new(
            gw_filterer,
            self.bridgehub_l1.provider().clone(),
        );

        // Whitelist sender if it's not already whitelisted.
        if !gw_filterer.whitelistedSenders(from).call().await? {
            let owner = gw_filterer.owner().call().await?;
            tracing::info!(
                "Sender {from} is not whitelisted on the gateway, whitelisting it with owner {owner}"
            );
            let whitelist_tx = gw_filterer
                .grantWhitelist(from)
                .from(owner)
                .into_transaction_request();
            self.send_impersonated_transaction(whitelist_tx).await?;
        }

        let l1_provider = self.tester.l1_provider();
        let max_priority_fee_per_gas = l1_provider.get_max_priority_fee_per_gas().await?;
        let base_l1_fees = l1_provider
            .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
                alloy::eips::eip1559::Eip1559Estimation {
                    max_fee_per_gas: base_fee_per_gas * 3 / 2,
                    max_priority_fee_per_gas: 0,
                }
            }))
            .await?;
        let max_fee_per_gas = base_l1_fees.max_fee_per_gas + max_priority_fee_per_gas;
        let gas_limit = self
            .tester
            .sl_provider()
            .estimate_gas(
                TransactionRequest::default()
                    .transaction_type(L1PriorityTxType::TX_TYPE)
                    .from(from)
                    .to(to)
                    .input(tx_input.clone()),
            )
            .await?
            * 2;
        let tx_base_cost = bridgehub_l1_for_gw
            .l2_transaction_base_cost(
                max_fee_per_gas + max_priority_fee_per_gas,
                gas_limit,
                REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            )
            .await?;

        let request = bridgehub_l1_for_gw
            .request_l2_transaction_direct(
                tx_base_cost,
                to,
                U256::ZERO,
                tx_input.into_input().unwrap().to_vec(),
                gas_limit,
                REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                from,
            )
            .value(tx_base_cost)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(max_priority_fee_per_gas)
            .from(from)
            .into_transaction_request();
        let l1_receipt = self.send_impersonated_transaction(request).await?;
        let l1_to_l2_tx_log = l1_receipt
            .logs()
            .iter()
            .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
            .next()
            .expect("no L1->L2 logs produced by funding tx");
        let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

        PendingTransactionBuilder::new(
            self.tester
                .gateway_provider()
                .await?
                .unwrap()
                .root()
                .clone(),
            l2_tx_hash,
        )
        .expect_successful_receipt()
        .await?;
        Ok(())
    }
}
