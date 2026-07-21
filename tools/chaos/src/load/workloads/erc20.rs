//! ERC20 churn against `ChaosERC20.sol`: transfers, approvals, and the odd
//! mint — storage read/write paths plus log/bloom traffic.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IChaosERC20};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, keccak256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

/// Plenty for any run: transfers move ≤ 1000 units of a 10^24 seed balance.
const SEED_BALANCE: u128 = 1_000_000_000_000_000_000_000_000;

pub struct Erc20 {
    token: Address,
}

impl Erc20 {
    /// Deploys (or reuses) the token, then mints every bank sender a balance —
    /// unconditionally, so a cached token deployment still works when the
    /// sender set changed since the run that deployed it.
    pub async fn deploy_and_seed(
        bank: &mut Bank,
        deployments: &mut Deployments,
    ) -> anyhow::Result<Erc20> {
        let token = deployments.ensure(bank, "ChaosERC20").await?;

        let mut pending = Vec::new();
        for index in 0..bank.senders.len() {
            let input = IChaosERC20::mintCall {
                to: bank.senders[index].address,
                value: U256::from(SEED_BALANCE),
            }
            .abi_encode();
            let request = TransactionRequest::default()
                .with_chain_id(bank.chain_id)
                .with_from(bank.deployer.address)
                .with_to(token)
                .with_input(input)
                .with_nonce(bank.deployer.nonce)
                .with_gas_limit(300_000)
                .with_max_fee_per_gas(bank.fees.0)
                .with_max_priority_fee_per_gas(bank.fees.1);
            let sent = bank.deployer.send_patiently(request).await?;
            bank.deployer.nonce += 1;
            pending.push(sent);
        }
        for sent in pending {
            let receipt = sent.get_receipt().await?;
            anyhow::ensure!(receipt.status(), "an ERC20 seeding mint reverted");
        }
        println!("erc20: seeded {} senders", bank.senders.len());
        Ok(Erc20 { token })
    }
}

impl Workload for Erc20 {
    fn name(&self) -> &'static str {
        "erc20"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let counterparty = Address::from_word(keccak256(rng.r#gen::<u64>().to_be_bytes()));
        let input = match rng.gen_range(0..10u8) {
            0..=6 => IChaosERC20::transferCall {
                to: counterparty,
                value: U256::from(rng.gen_range(1..1000u64)),
            }
            .abi_encode(),
            7 | 8 => IChaosERC20::approveCall {
                spender: counterparty,
                value: U256::from(rng.r#gen::<u64>()),
            }
            .abi_encode(),
            _ => IChaosERC20::mintCall {
                to: counterparty,
                value: U256::from(10u128.pow(18)),
            }
            .abi_encode(),
        };
        TxPlan {
            to: self.token,
            value: U256::ZERO,
            input: input.into(),
            gas_limit: 300_000,
            expect: Expectation::Accept,
        }
    }
}
