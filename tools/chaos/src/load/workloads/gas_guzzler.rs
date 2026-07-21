//! Deliberately expensive transactions via `GasGuzzler.sol`: block-full
//! conditions, and worst-case verify-before-vote timing — every validator
//! re-executes these before voting.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IGasGuzzler};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

pub struct GasGuzzler {
    guzzler: Address,
    /// From the profile's `knobs.guzzler_gas`: the burn consumes nearly all of it.
    gas: u64,
}

impl GasGuzzler {
    pub async fn deploy(
        bank: &mut Bank,
        deployments: &mut Deployments,
        gas: u64,
    ) -> anyhow::Result<GasGuzzler> {
        Ok(GasGuzzler {
            guzzler: deployments.ensure(bank, "GasGuzzler").await?,
            gas,
        })
    }
}

impl Workload for GasGuzzler {
    fn name(&self) -> &'static str {
        "gas_guzzler"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let input = IGasGuzzler::burnCall {
            mode: rng.gen_range(0..4u8),
            seed: U256::from(rng.r#gen::<u128>()),
        }
        .abi_encode();
        TxPlan {
            to: self.guzzler,
            value: U256::ZERO,
            input: input.into(),
            gas_limit: self.gas,
            expect: Expectation::Accept,
        }
    }
}
