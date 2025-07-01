use crate::repositories::AccountPropertyRepository;
use crate::CHAIN_ID;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction, TxEnvelope};
use alloy::eips::Decodable2718;
use alloy::primitives::{Bytes, B256, U256};
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_os_mempool::DynPool;
use zksync_os_types::L2Transaction;

/// Handles transactions received in API
pub struct TxHandler {
    mempool: DynPool,
    // we give access to non-canonized blocks here on purpose
    account_property_repository: AccountPropertyRepository,

    // will probably be replaced with an own config
    max_nonce_ahead: u64,
    max_tx_input_bytes: usize,
}
impl TxHandler {
    pub fn new(
        mempool: DynPool,
        account_property_repository: AccountPropertyRepository,
        max_nonce_ahead: u64,
        max_tx_input_bytes: usize,
    ) -> TxHandler {
        Self {
            mempool,
            account_property_repository,
            max_nonce_ahead,
            max_tx_input_bytes,
        }
    }

    pub fn send_raw_transaction_impl(&self, tx_bytes: Bytes) -> anyhow::Result<B256> {
        let transaction = TxEnvelope::decode_2718(&mut tx_bytes.as_ref())?;
        if let Some(chain_id) = transaction.chain_id() {
            anyhow::ensure!(chain_id == CHAIN_ID, "wrong chain id {chain_id}");
        }
        let l2_tx: L2Transaction = transaction.try_into_recovered()?;
        if l2_tx.input().len() > self.max_tx_input_bytes {
            anyhow::bail!(
                "oversized input data. max: {}; actual: {}",
                self.max_tx_input_bytes,
                l2_tx.input().len()
            );
        }

        let sender_account_properties = self
            .account_property_repository
            .get_latest(l2_tx.signer_ref());

        // tracing::info!(
        //     "Processing transaction: {:?}, sender properties: {:?}",
        //     l2_tx,
        //     sender_account_properties
        // );

        Self::validate_tx_sender_balance(&l2_tx, &sender_account_properties)?;

        Self::validate_tx_nonce(&l2_tx, &sender_account_properties, self.max_nonce_ahead)?;

        // let block_number = self.state_handle.last_canonized_block_number() + 1;
        // let block_context = self
        //     .block_replay_storage
        //     .get_context(block_number - 1)
        //     .context("Failed to get block context")?;
        //
        // let storage_view = self.state_handle.view_at(block_number)?;
        //
        // let res = execute(
        //     l2_tx.clone(),
        //     block_context,
        //     storage_view,
        // )?;

        let hash = *l2_tx.hash();
        self.mempool.add_transaction(l2_tx);

        Ok(hash)
    }

    pub fn validate_tx_nonce(
        transaction: &L2Transaction,
        acc_props: &Option<AccountProperties>,
        max_nonce_ahead: u64,
    ) -> anyhow::Result<()> {
        let nonce = transaction.nonce();
        let expected_nonce = acc_props.map(|props| props.nonce).unwrap_or(0);

        if nonce < expected_nonce {
            return Err(anyhow::anyhow!(
                "Nonce too low: expected at least {}, got {}",
                expected_nonce,
                nonce
            ));
        }

        if nonce > expected_nonce + max_nonce_ahead {
            return Err(anyhow::anyhow!(
                "Nonce too high: next nonce {expected_nonce}, accepted to overshoot by {max_nonce_ahead}, got {nonce}"
            ));
        }

        Ok(())
    }
    pub fn validate_tx_sender_balance(
        tx: &L2Transaction,
        acc_props: &Option<AccountProperties>,
    ) -> anyhow::Result<()> {
        let current_balance = match acc_props {
            Some(props) => props.balance,
            None => U256::ZERO,
        };

        // Estimate the minimum fee price user will agree to.
        let gas_price = tx.max_fee_per_gas();
        let max_fee = tx.gas_limit() as u128 * gas_price;
        let max_fee_and_value = U256::from(max_fee) + tx.value();

        if current_balance < max_fee_and_value {
            return Err(anyhow::anyhow!(
                "Insufficient funds for gas + value. Balance: {}, Fee: {}, Value: {}",
                current_balance,
                max_fee,
                tx.value()
            ));
        }
        Ok(())
    }
}
