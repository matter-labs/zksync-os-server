//! Known-vector precompile exercises against `PrecompileGym.sol`. A wrong
//! precompile result reverts on-chain — visible in the expectation audit and in
//! cross-validator receipt comparison with no off-chain oracle.
//!
//! Which precompiles this chain's VM implements is not knowable from here, so
//! the constructor calibrates: it `eth_call`s every exercise once and keeps the
//! ones that pass, reporting the dropped ones loudly.

use super::{Expectation, TxPlan, Workload};
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IPrecompileGym};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use rand08::{Rng as _, rngs::StdRng};

/// Must match `PrecompileGym.EXERCISES` and its id order.
const EXERCISE_NAMES: [&str; 8] = [
    "ecrecover",
    "sha256",
    "ripemd160",
    "identity",
    "modexp",
    "ecadd",
    "ecmul",
    "ecpairing",
];

pub struct Precompiles {
    gym: Address,
    supported: Vec<u8>,
}

impl Precompiles {
    /// Deploys the gym and calibrates. `None` when no exercise passes — the
    /// workload disables itself rather than spamming guaranteed failures.
    pub async fn deploy_and_calibrate(
        bank: &mut Bank,
        deployments: &mut Deployments,
    ) -> anyhow::Result<Option<Precompiles>> {
        let gym = deployments.ensure(bank, "PrecompileGym").await?;

        let mut supported = Vec::new();
        let mut dropped = Vec::new();
        for id in 0..EXERCISE_NAMES.len() as u8 {
            let call = TransactionRequest::default()
                .with_to(gym)
                .with_input(IPrecompileGym::exerciseCall { id }.abi_encode());
            match bank.deployer.provider.call(call).await {
                Ok(_) => supported.push(id),
                Err(_) => dropped.push(EXERCISE_NAMES[id as usize]),
            }
        }
        if !dropped.is_empty() {
            println!(
                "precompiles: calibration dropped {} (unsupported or wrong on this chain: \
                 worth a look if unexpected)",
                dropped.join(", "),
            );
        }
        if supported.is_empty() {
            return Ok(None);
        }
        println!(
            "precompiles: {} of {} exercises active",
            supported.len(),
            EXERCISE_NAMES.len(),
        );
        Ok(Some(Precompiles { gym, supported }))
    }
}

impl Workload for Precompiles {
    fn name(&self) -> &'static str {
        "precompiles"
    }

    fn fire(&mut self, rng: &mut StdRng) -> TxPlan {
        let id = self.supported[rng.gen_range(0..self.supported.len())];
        TxPlan {
            to: self.gym,
            value: U256::ZERO,
            input: IPrecompileGym::exerciseCall { id }.abi_encode().into(),
            gas_limit: 400_000,
            expect: Expectation::Accept,
        }
    }
}
