//! Seeded walks through `CallMaze.sol`: nested calls, delegatecalls,
//! staticcalls, CREATE/CREATE2 leaves, and reverts bubbling to a top-level
//! catch — call-frame semantics as a transaction stream.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, ICallMaze};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

pub struct CallMaze {
    maze: Address,
}

impl CallMaze {
    pub async fn deploy(
        bank: &mut Bank,
        deployments: &mut Deployments,
    ) -> anyhow::Result<CallMaze> {
        Ok(CallMaze {
            maze: deployments.ensure(bank, "CallMaze").await?,
        })
    }
}

impl Workload for CallMaze {
    fn name(&self) -> &'static str {
        "call_maze"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let input = ICallMaze::walkCall {
            seed: U256::from(rng.r#gen::<u128>()),
            depth: rng.gen_range(2..6u8),
        }
        .abi_encode();
        TxPlan {
            to: self.maze,
            // A little value now and then so value-forwarding legs have wei to move.
            value: U256::from(rng.gen_range(0..3u64)),
            input: input.into(),
            gas_limit: 1_500_000,
            expect: Expectation::Accept,
        }
    }
}
