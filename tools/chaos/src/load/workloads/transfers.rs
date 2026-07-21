//! Plain value transfers — the baseline the rig started with: 1 wei to a
//! random fresh address.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::TRANSFER_GAS;
use alloy::primitives::{Address, Bytes, U256, keccak256};
use rand08::{Rng as _, rngs::StdRng};

pub struct Transfers;

impl Workload for Transfers {
    fn name(&self) -> &'static str {
        "transfers"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        TxPlan {
            to: Address::from_word(keccak256(rng.r#gen::<u64>().to_be_bytes())),
            value: U256::from(1u64),
            input: Bytes::new(),
            gas_limit: TRANSFER_GAS,
            expect: Expectation::Accept,
        }
    }
}
