use crate::Tester;
use crate::assert_traits::ReceiptAssert;
use crate::dyn_wallet_provider::EthDynProvider;
use crate::provider::ZksyncApi as _;
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;

use super::ProtocolUpgradeBuilder;
use super::default_upgrade::DefaultUpgrade;
use super::interfaces;

// We assume that chain has this ID.
const CHAIN_ID: u64 = 270;

/// Object that helps with preparation and execution of protocol upgrades in integration tests.
///
/// Tester assumes that governance is an EOA account, and uses impersonation
/// to execute the upgrade with it.
/// It also assumes that we don't have a gateway.
#[derive(Debug)]
pub struct UpgradeTester {
    pub tester: Tester,
    // Bridgehub contract
    pub bridgehub: interfaces::Bridgehub::BridgehubInstance<EthDynProvider>,
    // Bridgehub owner address
    pub bridgehub_owner: Address,
    // CTM contract
    pub ctm: interfaces::ChainTypeManager::ChainTypeManagerInstance<EthDynProvider>,
    // CTM owner address
    pub ctm_owner: Address,
    // L1 chain admin contract
    pub l1_chain_admin: interfaces::ChainAdmin::ChainAdminInstance<EthDynProvider>,
    // L1 chain admin owner address
    pub l1_chain_admin_owner: Address,
    // Diamond proxy on the settlement layer
    pub diamond_proxy: interfaces::ZkChain::ZkChainInstance<EthDynProvider>,
    // Diamond proxy owner address
    pub diamond_proxy_admin: Address,
    // Bytecode supplier contract
    pub bytecode_supplier: interfaces::BytecodesSupplier::BytecodesSupplierInstance<EthDynProvider>,
    // Current protocol version
    pub protocol_version: U256,
    // TODO (not discoverable without config right now): // Address of bytecode supplier contract
    // l1_bytecode_supplier: Address,
}

impl UpgradeTester {
    /// Prepares tester for the default upgrade scenario.
    pub async fn for_default_upgrade(tester: Tester) -> anyhow::Result<Self> {
        let upgrade_tester = Self::fetch(tester).await?;
        upgrade_tester.enable_impersonation().await?;
        upgrade_tester.fund_wallets().await?;
        upgrade_tester.wait_for_genesis_upgrade().await?;
        Ok(upgrade_tester)
    }

    /// Executes a "default" flow for `DefaultUpgrade`.
    pub async fn execute_default_upgrade(
        &self,
        protocol_upgrade: &interfaces::ProposedUpgrade,
        deadline: U256,
        upgrade_timestamp: U256,
    ) -> anyhow::Result<()> {
        // Deploy the upgrade contract on L1.
        let upgrade_contract =
            DefaultUpgrade::deploy(&self.tester.l1_provider, protocol_upgrade).await?;

        // STM upgrade, `setNewVersionUpgrade` call;
        let upgrade_data = upgrade_contract.diamond_cut_data();

        // Send pause migration to Bridgehub
        self.pause_bridgehub_migrations().await?;
        self.set_new_version_on_ctm(
            upgrade_data.clone(),
            deadline,
            protocol_upgrade.newProtocolVersion,
        )
        .await?;

        // Set timestamp for upgrade on a specific chain under stm, `setUpgradeTimestamp` call on L1ChainAdmin
        self.set_upgrade_timestamp(protocol_upgrade.newProtocolVersion, upgrade_timestamp)
            .await?;

        // Wait until the block _before_ upgrade tx is finalized on L1.
        let pending_tx = PendingTransactionBuilder::new(
            self.tester.l2_zk_provider.root().clone(),
            upgrade_contract.upgrade_tx_l2_hash(),
        )
        .expect_successful_receipt()
        .await?;
        let upgrade_block_number = pending_tx.block_number.expect("Upgrade tx must be mined");
        let block_before_upgrade = upgrade_block_number
            .checked_sub(1)
            .expect("Upgrade tx can't be in the first block");
        wait_finalized(&self.tester.l2_provider, block_before_upgrade).await?;
        tracing::info!("Block before upgrade tx is finalized on L1");

        self.upgrade_chain(upgrade_data).await?;

        wait_finalized(&self.tester.l2_provider, upgrade_block_number).await?;
        tracing::info!("Upgrade tx is finalized on L1");

        Ok(())
    }

    // Fetch the contracts configuration from the tester.
    async fn fetch(tester: Tester) -> anyhow::Result<Self> {
        let bridgehub = tester.l2_zk_provider.get_bridgehub_contract().await?;
        let bridgehub = interfaces::Bridgehub::new(bridgehub, tester.l1_provider.clone());
        let ctm = bridgehub
            .chainTypeManager(U256::from(CHAIN_ID))
            .call()
            .await?;
        let ctm = interfaces::ChainTypeManager::new(ctm, tester.l1_provider.clone());
        let protocol_version = ctm.getProtocolVersion(U256::from(CHAIN_ID)).call().await?;

        let diamond_proxy = bridgehub.getZKChain(U256::from(CHAIN_ID)).call().await?;
        let diamond_proxy = interfaces::ZkChain::new(diamond_proxy, tester.l1_provider.clone());

        let l1_chain_admin = diamond_proxy.getAdmin().call().await?;
        let l1_chain_admin =
            interfaces::ChainAdmin::new(l1_chain_admin, tester.l1_provider.clone());

        let bridgehub_owner = bridgehub.owner().call().await?;
        let ctm_owner = ctm.owner().call().await?;
        let diamond_proxy_admin = diamond_proxy.getAdmin().call().await?;
        let l1_chain_admin_owner = l1_chain_admin.owner().call().await?;

        // Bytecode supplier is a bit special: right now it's not discoverable
        // The value is hardcoded, keep it aligned with `node/bin/src/config.rs`, it must correspond
        // to the value stored in `zkos-l1-state.json`.
        let bytecode_supplier_address = "0x8e7f59534b29d4de9f6979e349392a3dc351592f".parse()?;
        anyhow::ensure!(
            !tester
                .l1_provider
                .get_code_at(bytecode_supplier_address)
                .await?
                .is_empty(),
            "Bytecode supplier contract is not deployed at expected address {bytecode_supplier_address:?}; if zkos-l1-state.json was updated, update the address in the test code"
        );
        let bytecode_supplier = interfaces::BytecodesSupplier::new(
            bytecode_supplier_address,
            tester.l1_provider.clone(),
        );

        Ok(Self {
            tester,
            bridgehub,
            bridgehub_owner,
            ctm,
            ctm_owner,
            diamond_proxy,
            diamond_proxy_admin,
            l1_chain_admin,
            l1_chain_admin_owner,
            bytecode_supplier,
            protocol_version,
        })
    }

    async fn enable_impersonation(&self) -> anyhow::Result<()> {
        // Enable impersonation and fund all governance accounts
        for addr in [
            self.bridgehub_owner,
            self.ctm_owner,
            self.diamond_proxy_admin,
            self.l1_chain_admin_owner,
        ] {
            self.tester
                .l1_provider
                .anvil_impersonate_account(addr)
                .await?;
            self.tester
                .l1_provider
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

    async fn fund_wallets(&self) -> anyhow::Result<()> {
        for wallet in [self.bridgehub_owner] {
            self.tester
                .l1_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(wallet)
                        .with_value(U256::from(10).pow(U256::from(18u64))), // 1 ETH
                )
                .await?
                .expect_successful_receipt()
                .await?;
        }
        Ok(())
    }

    async fn wait_for_genesis_upgrade(&self) -> anyhow::Result<()> {
        // Wait until the genesis upgrade is executed
        while self.diamond_proxy.getTotalBatchesExecuted().call().await? == U256::ZERO {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Ok(())
    }

    pub async fn pause_bridgehub_migrations(&self) -> anyhow::Result<()> {
        let pause_migration_tx = self
            .bridgehub
            .pauseMigration()
            .into_transaction_request()
            .with_from(self.bridgehub_owner);
        let hash = self
            .tester
            .l1_provider
            .anvil_send_impersonated_transaction(pause_migration_tx)
            .await?;
        PendingTransactionBuilder::new(self.tester.l1_provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;

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
            self.protocol_version,
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
        let tx = self
            .ctm
            .setNewVersionUpgrade(upgrade_data, self.protocol_version, deadline, new_version)
            .into_transaction_request()
            .with_from(self.ctm_owner);
        let hash = self
            .tester
            .l1_provider
            .anvil_send_impersonated_transaction(tx)
            .await?;
        PendingTransactionBuilder::new(self.tester.l1_provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;
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
        let hash = self
            .tester
            .l1_provider
            .anvil_send_impersonated_transaction(tx)
            .await?;
        PendingTransactionBuilder::new(self.tester.l1_provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;
        Ok(())
    }

    pub async fn upgrade_chain(
        &self,
        upgrade_data: interfaces::DiamondCutData,
    ) -> anyhow::Result<()> {
        let tx = self
            .diamond_proxy
            .upgradeChainFromVersion(self.protocol_version, upgrade_data)
            .into_transaction_request()
            .with_from(self.diamond_proxy_admin);
        let hash = self
            .tester
            .l1_provider
            .anvil_send_impersonated_transaction(tx)
            .await?;
        PendingTransactionBuilder::new(self.tester.l1_provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;
        Ok(())
    }
}

async fn wait_finalized(provider: &impl Provider, target_block: u64) -> anyhow::Result<()> {
    while provider
        .get_block_number_by_id(BlockId::finalized())
        .await?
        < Some(target_block)
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
