use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::{U256, address};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use std::str::FromStr;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;

/// Submits an L1->L2 deposit transaction to local L1
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = "http://localhost:8545";
    // Private key for 0x36615cf349d7f6344891b1e7ca7c72883f5dc049
    let private_key = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";
    // Replace with your Bridgehub address here!
    let bridgehub_address = address!("0xb02237aee96515ae7b70308ba4d95e4bae047079");
    // Deposit 10k ETH by default
    let amount = U256::from(100u128 * 10u128.pow(18));
    let l2_chain_id = 270;

    let l1_wallet = EthereumWallet::new(LocalSigner::from_str(private_key).unwrap());
    let l1_provider = ProviderBuilder::new()
        .wallet(l1_wallet.clone())
        .connect(url)
        .await
        .unwrap();

    let l1_balance = l1_provider
        .get_balance(l1_wallet.default_signer().address())
        .await?;
    println!("L1 balance: {l1_balance}");

    // todo: copied over from alloy-zksync, use directly once it is EIP-712 agnostic
    let bridgehub = Bridgehub::new(bridgehub_address, l1_provider.clone(), l2_chain_id);
    let gas_limit = U256::from(500_000);
    let gas_per_pubdata = U256::from(800);
    let max_priority_fee_per_gas = l1_provider.get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = l1_provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            gas_limit,
            gas_per_pubdata,
        )
        .await?;
    let l1_deposit_request = bridgehub
        .request_l2_transaction_direct(
            amount + tx_base_cost,
            l1_wallet.default_signer().address(),
            amount,
            vec![],
            gas_limit,
            gas_per_pubdata,
            l1_wallet.default_signer().address(),
        )
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let l1_deposit_receipt = l1_provider
        .send_transaction(l1_deposit_request)
        .await?
        .get_receipt()
        .await?;
    assert!(l1_deposit_receipt.status());
    let l1_to_l2_tx_log = l1_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by deposit tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

    println!("Successfully submitted L1->L2 deposit tx with hash '{l2_tx_hash}'");
    Ok(())
}
