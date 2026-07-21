//! The workload registry: every kind of transaction `chaos load` can send.
//!
//! A tick-driven workload is a payload factory: the engine owns accounts,
//! nonces, fees and rate, calls `fire` whenever its weighted pick lands on the
//! workload, and sends whatever comes back. Adding one is a contract (if it
//! needs one), a module here, a `match` arm in [`build_enabled`], and a profile
//! entry. Episodic sagas (multi-step behaviors with their own cadence and
//! assertions) live beside them — see [`nonce_race`].

use super::bank::Bank;
use super::contracts::Deployments;
use super::profile::Profile;
use alloy::primitives::{Address, Bytes, U256};
use rand08::rngs::StdRng;

mod blobs;
mod call_maze;
mod context_probe;
pub mod deposits;
mod erc20;
pub mod failed_deposits;
mod failing;
mod gas_guzzler;
pub mod l1_support;
pub mod nonce_race;
mod precompiles;
mod transfers;
pub mod withdrawals;

/// What a healthy chain should do with a workload's transaction. The final
/// report samples receipts and flags any transaction whose fate disagrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Expectation {
    /// Included with status 1.
    Accept,
    /// Included with status 0 — failing is the workload's whole point.
    RevertOnChain,
}

/// One planned transaction: the payload half of a `TransactionRequest`. The
/// engine fills in the account half (from, nonce, fees, chain id).
pub struct TxPlan {
    pub to: Address,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: u64,
    pub expect: Expectation,
}

pub trait Workload: Send {
    fn name(&self) -> &'static str;
    /// Builds the next payload. Infallible and instant by design: anything
    /// that can fail or wait (deployments, calibration, seeding) happened in
    /// the workload's constructor.
    fn fire(&mut self, rng: &mut StdRng) -> TxPlan;
}

/// Builds every workload the profile enables, deploying and seeding what each
/// one needs. Returns `(workload, weight)` pairs for the engine's pick table.
pub async fn build_enabled(
    profile: &Profile,
    bank: &mut Bank,
    deployments: &mut Deployments,
) -> anyhow::Result<Vec<(Box<dyn Workload>, u32)>> {
    let mut enabled: Vec<(Box<dyn Workload>, u32)> = Vec::new();
    for (name, weight) in &profile.weights {
        if *weight == 0 {
            continue;
        }
        let workload: Box<dyn Workload> = match name.as_str() {
            "transfers" => Box::new(transfers::Transfers),
            "erc20" => Box::new(erc20::Erc20::deploy_and_seed(bank, deployments).await?),
            "call_maze" => Box::new(call_maze::CallMaze::deploy(bank, deployments).await?),
            "context_probe" => {
                Box::new(context_probe::ContextProbe::deploy(bank, deployments).await?)
            }
            "precompiles" => {
                match precompiles::Precompiles::deploy_and_calibrate(bank, deployments).await? {
                    Some(gym) => Box::new(gym),
                    None => {
                        println!("precompiles: nothing passed calibration; workload off");
                        continue;
                    }
                }
            }
            "gas_guzzler" => Box::new(
                gas_guzzler::GasGuzzler::deploy(bank, deployments, profile.knobs.guzzler_gas)
                    .await?,
            ),
            "failing" => Box::new(failing::Failing::deploy(bank, deployments).await?),
            "blobs" => Box::new(blobs::Blobs::new(profile.knobs.blob_kib)),
            other => anyhow::bail!("profile enables unknown workload {other:?}"),
        };
        enabled.push((workload, *weight));
    }
    anyhow::ensure!(!enabled.is_empty(), "no workloads survived preparation");
    Ok(enabled)
}
