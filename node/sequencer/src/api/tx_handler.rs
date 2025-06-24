use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_types::{api, L2ChainId, Nonce, H256, U256};
use zksync_types::l2::L2Tx;
use zksync_types::web3::Bytes;
use crate::CHAIN_ID;
use crate::conversions::ruint_u256_to_api_u256;
use crate::mempool::Mempool;
use crate::repositories::AccountPropertyRepository;

/// Handles transactions received in API
pub struct TxHandler {
    mempool: Mempool,
    // we give access to non-canonized blocks here on purpose
    account_property_repository: AccountPropertyRepository,

    // will probably be replaced with an own config
    max_nonce_ahead: u32,
    max_tx_size_bytes: usize,
}
impl TxHandler {
    pub fn new(
        mempool: Mempool,
        account_property_repository: AccountPropertyRepository,
        max_nonce_ahead: u32,
        max_tx_size_bytes: usize,
    ) -> TxHandler {
        Self {
            mempool,
            account_property_repository,
            max_nonce_ahead,
            max_tx_size_bytes
        }
    }

    pub fn send_raw_transaction_impl(&self, tx_bytes: Bytes) -> anyhow::Result<H256> {
        // todo: don't use Transaction types from Types
        let (tx_request, hash) =
            api::TransactionRequest::from_bytes(&tx_bytes.0, L2ChainId::new(CHAIN_ID).unwrap())?;
        let mut l2_tx = L2Tx::from_request(tx_request, self.max_tx_size_bytes, true)?;
        l2_tx.set_input(tx_bytes.0, hash);

        let sender_account_properties = self
            .account_property_repository
            .get_latest(&l2_tx.initiator_account());

        // tracing::info!(
        //     "Processing transaction: {:?}, sender properties: {:?}",
        //     l2_tx,
        //     sender_account_properties
        // );

        Self::validate_tx_sender_balance(&l2_tx, &sender_account_properties)?;

        Self::validate_tx_nonce(
            &l2_tx,
            &sender_account_properties,
            self.max_nonce_ahead,
        )?;

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

        self.mempool.insert(l2_tx.into());

        Ok(hash)
    }


    pub fn validate_tx_nonce(
        transaction: &L2Tx,
        acc_props: &Option<AccountProperties>,
        max_nonce_ahead: u32,
    ) -> anyhow::Result<()> {
        let nonce = transaction.nonce();

        let expected_nonce = Nonce(match acc_props {
            Some(props) => props.nonce,
            None => 0,
        } as u32);

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
        tx: &L2Tx,
        acc_props: &Option<AccountProperties>,
    ) -> anyhow::Result<()> {
        let current_balance = match acc_props {
            Some(props) => ruint_u256_to_api_u256(props.balance),
            None => U256::zero(),
        };

        // Estimate the minimum fee price user will agree to.
        let gas_price = tx.common_data.fee.max_fee_per_gas;
        let max_fee = tx.common_data.fee.gas_limit * gas_price;
        let max_fee_and_value = max_fee + tx.execute.value;

        if current_balance < max_fee_and_value {
            return Err(anyhow::anyhow!(
                "Insufficient funds for gas + value. Balance: {}, Fee: {}, Value: {}",
                current_balance,
                max_fee,
                tx.execute.value
            ));
        }
        Ok(())
    }
}