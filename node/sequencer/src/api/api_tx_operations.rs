use crate::api::eth_impl::EthNamespace;
use crate::conversions::ruint_u256_to_api_u256;
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_types::l2::L2Tx;
use zksync_types::{Nonce, U256};

impl EthNamespace {
    pub fn validate_tx_nonce(
        &self,
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
