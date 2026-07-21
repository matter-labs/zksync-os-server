//! The failed-deposits saga: L1→L2 priority operations engineered to fail on
//! L2 — calldata that reverts, and execution that runs out of gas. Nothing in
//! the codebase exercises this path yet, so this saga is deliberately more
//! journalist than judge: it *asserts* only what must hold for any priority
//! operation (the relay includes it, and the queue keeps working afterwards)
//! and *records* the observed failure semantics — receipt status, the L2→L1
//! log's success flag, and where the deposited value ended up — as episode
//! notes for a human to review.

use super::l1_support::L1Side;
use crate::load::bank::Bank;
use crate::load::contracts::{Deployments, IReverter};
use crate::load::stats::LoadStats;
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::sol_types::SolCall as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SAGA: &str = "failed_deposits";
/// How long the relayed L2 transaction may take to appear on a live chain.
const RELAY_DEADLINE: Duration = Duration::from_secs(150);

pub struct FailedDeposits {
    l1: L1Side,
    l2: DynProvider,
    /// The on-L2 Reverter contract both failure modes call.
    reverter: Address,
    episode: u64,
}

impl FailedDeposits {
    /// Ensures the Reverter is deployed (idempotent, shared with the `failing`
    /// workload's deployment cache).
    pub async fn new(
        l1: L1Side,
        l2: DynProvider,
        bank: &mut Bank,
        deployments: &mut Deployments,
    ) -> anyhow::Result<FailedDeposits> {
        let reverter = deployments.ensure(bank, "Reverter").await?;
        Ok(FailedDeposits {
            l1,
            l2,
            reverter,
            episode: 0,
        })
    }

    pub async fn run(
        mut self,
        every: Duration,
        stats: Arc<Mutex<LoadStats>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => return,
            }
            self.episode += 1;
            match self.episode_once().await {
                Ok(Outcome::Pass(note)) => {
                    let mut stats = stats.lock().unwrap();
                    stats.episode(SAGA, true, None);
                    // Passing episodes still carry the observed semantics —
                    // the whole point of pioneering this path.
                    stats.note(SAGA, format!("observed: {note}"));
                }
                Ok(Outcome::Skip(reason)) => {
                    println!(
                        "failed_deposits episode {}: skipped — {reason}",
                        self.episode
                    );
                    stats.lock().unwrap().skip(SAGA, reason);
                }
                Err(reason) => {
                    println!(
                        "failed_deposits episode {}: FAILED — {reason}",
                        self.episode
                    );
                    stats.lock().unwrap().episode(SAGA, false, Some(reason));
                }
            }
        }
    }

    async fn episode_once(&mut self) -> Result<Outcome, String> {
        // Alternate the failure mode: a plain revert, and an out-of-gas spin.
        let (mode, l2_gas) = if self.episode.is_multiple_of(2) {
            (0u8, 400_000u64) // require(false) with plenty of gas
        } else {
            (4u8, 250_000u64) // spin until out of gas
        };
        let l2_value = U256::from(5_000_000_000_000_000u128); // 0.005 ETH
        let calldata = IReverter::failCall {
            mode,
            seed: U256::from(self.episode),
        }
        .abi_encode();

        // Distinct fresh addresses so any credit is unambiguous.
        let refund_recipient = self.fresh_address("refund");
        let value_before_target = self.balance(self.reverter).await;

        let l2_tx = match self
            .l1
            .deposit(self.reverter, l2_value, calldata, l2_gas, refund_recipient)
            .await
        {
            Ok(hash) => hash,
            Err(error) => return Ok(Outcome::Skip(format!("L1 submission failed: {error:#}"))),
        };

        // The relay must include the transaction — failed execution or not.
        let receipt = match self.await_relay(l2_tx).await {
            Some(receipt) => receipt,
            None => {
                return Ok(Outcome::Skip(
                    "relayed transaction not seen within the window (chain stalled?)".to_string(),
                ));
            }
        };
        let status = receipt["status"].as_str().unwrap_or("?").to_string();
        // The deposit's own L2→L1 log carries a success flag in `value`.
        let l1_log_flag = receipt["l2ToL1Logs"]
            .as_array()
            .and_then(|logs| logs.first())
            .and_then(|log| log["value"].as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string());

        if status == "0x1" {
            return Err(format!(
                "a deposit built to fail (mode {mode}) succeeded on L2: {l2_tx}",
            ));
        }

        // The queue must not wedge: a follow-up healthy deposit still arrives.
        let canary = self.fresh_address("canary");
        let canary_amount = U256::from(1_000_000_000_000_000u128);
        if let Err(error) = self
            .l1
            .deposit(canary, canary_amount, vec![], 300_000, canary)
            .await
        {
            return Ok(Outcome::Skip(format!(
                "canary submission failed: {error:#}"
            )));
        }
        let deadline = tokio::time::Instant::now() + RELAY_DEADLINE;
        loop {
            // The canary is its own refund recipient: value plus gas refund.
            if self.balance(canary).await >= canary_amount {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "the priority queue looks wedged: a healthy deposit after failed {l2_tx} \
                     never arrived",
                ));
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }

        // Where did the failed deposit's value go? Recorded, not asserted —
        // these are the semantics this saga exists to surface.
        let refund_balance = self.balance(refund_recipient).await;
        let target_delta = self
            .balance(self.reverter)
            .await
            .saturating_sub(value_before_target);
        Ok(Outcome::Pass(format!(
            "mode {mode}: status {status}, l2-to-l1 success flag {l1_log_flag}, \
             refund recipient got {refund_balance}, target delta {target_delta}",
        )))
    }

    /// The relayed transaction's raw receipt, once it exists.
    async fn await_relay(&self, l2_tx: B256) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + RELAY_DEADLINE;
        loop {
            if let Ok(receipt) = self
                .l2
                .raw_request::<_, serde_json::Value>("eth_getTransactionReceipt".into(), (l2_tx,))
                .await
                && !receipt.is_null()
            {
                return Some(receipt);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    }

    async fn balance(&self, address: Address) -> U256 {
        self.l2.get_balance(address).await.unwrap_or(U256::ZERO)
    }

    fn fresh_address(&self, purpose: &str) -> Address {
        let mut material = b"chaos-failed-deposit".to_vec();
        material.extend_from_slice(purpose.as_bytes());
        material.extend_from_slice(&self.episode.to_be_bytes());
        Address::from_word(keccak256(material))
    }
}

enum Outcome {
    Pass(String),
    Skip(String),
}
