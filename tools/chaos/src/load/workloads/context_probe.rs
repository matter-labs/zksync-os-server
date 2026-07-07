//! Environment-opcode reads via `ContextProbe.sol`, emitted as logs. The logs
//! land in the block's output commitment, so any cross-validator disagreement
//! on these values fails verification before it can finalize.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IContextProbe};
use alloy::primitives::{Address, U256, keccak256};
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

pub struct ContextProbe {
    probe: Address,
}

impl ContextProbe {
    pub async fn deploy(
        bank: &mut Bank,
        deployments: &mut Deployments,
    ) -> anyhow::Result<ContextProbe> {
        Ok(ContextProbe {
            probe: deployments.ensure(bank, "ContextProbe").await?,
        })
    }
}

impl Workload for ContextProbe {
    fn name(&self) -> &'static str {
        "context_probe"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        // A fresh address: its EXTCODESIZE/EXTCODEHASH answers are part of the log.
        let some_eoa = Address::from_word(keccak256(rng.r#gen::<u64>().to_be_bytes()));
        let input = IContextProbe::probeCall { someEoa: some_eoa }.abi_encode();
        TxPlan {
            to: self.probe,
            // Nonzero CALLVALUE some of the time, so both branches show up.
            value: U256::from(rng.gen_range(0..5u64)),
            input: input.into(),
            gas_limit: 500_000,
            expect: Expectation::Accept,
        }
    }
}
