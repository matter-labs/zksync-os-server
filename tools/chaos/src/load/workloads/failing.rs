//! Transactions that are supposed to fail, via `Reverter.sol`: require/custom
//! error/panic/revert-data-bomb/out-of-gas. Failed transactions are
//! first-class traffic — every node must serve the same status-0 receipt, and
//! none of it may wedge a mempool.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IReverter};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

pub struct Failing {
    reverter: Address,
}

impl Failing {
    pub async fn deploy(bank: &mut Bank, deployments: &mut Deployments) -> anyhow::Result<Failing> {
        Ok(Failing {
            reverter: deployments.ensure(bank, "Reverter").await?,
        })
    }
}

impl Workload for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let mode = rng.gen_range(0..5u8);
        let input = IReverter::failCall {
            mode,
            seed: U256::from(rng.r#gen::<u64>()),
        }
        .abi_encode();
        // Mode 4 spins until out of gas, so its limit *is* the burn size; keep
        // it modest. The explicit reverts get room to reach their revert.
        let gas_limit = if mode == 4 { 150_000 } else { 250_000 };
        TxPlan {
            to: self.reverter,
            value: U256::ZERO,
            input: input.into(),
            gas_limit,
            expect: Expectation::RevertOnChain,
        }
    }
}
