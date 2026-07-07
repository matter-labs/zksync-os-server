//! The withdrawals saga: the full L2→L1 round trip, continuously — withdraw on
//! L2, wait for the batch containing it to be executed on the L1 (that is when
//! the log proof becomes available), finalize through the L1Nullifier, and
//! assert the exact balance landed. This keeps the settlement pipeline honest
//! under chaos, not just in one integration test.
//!
//! Withdrawal latency is settlement latency (commit→prove→execute), so the
//! saga is a small state machine: each tick advances every in-flight
//! withdrawal one step and tops the pipeline back up, rather than blocking on
//! any single one.

use super::l1_support::{IL1Nullifier, L1Side};
use crate::load::bank::Sender;
use crate::load::stats::LoadStats;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256, address, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SAGA: &str = "withdrawals";
/// In-flight withdrawals the saga keeps in the pipeline.
const PIPELINE: usize = 3;
/// How long a withdrawal may wait for its proof before the episode is written
/// off as unresolved (settlement may be legitimately paused under chaos).
const PROOF_DEADLINE: Duration = Duration::from_secs(600);

/// The L2 base-token system contract; withdrawals are `withdraw(l1Receiver)`
/// calls on it carrying the amount as value.
const L2_BASE_TOKEN: Address = address!("000000000000000000000000000000000000800a");
/// The L1Messenger system hook: authors the withdrawal's L2→L1 log and the
/// `L1MessageSent` event the finalization message comes from.
const L1_MESSENGER: Address = address!("0000000000000000000000000000000000008008");

alloy::sol! {
    interface IBaseToken {
        function withdraw(address _l1Receiver) external payable;
    }
}

struct InFlight {
    l2_tx: B256,
    l1_receiver: Address,
    amount: U256,
    /// Extracted from the withdrawal receipt once, kept for finalization.
    message: Vec<u8>,
    l2_sender: Address,
    l2_to_l1_index: u64,
    tx_index_in_block: u64,
    submitted: Instant,
}

pub struct Withdrawals {
    l1: L1Side,
    nullifier: Address,
    /// The withdrawing L2 account (bank-funded).
    account: Sender,
    /// Wallet-less L2 endpoint for raw receipt/proof queries.
    l2: DynProvider,
    in_flight: Vec<InFlight>,
    episode: u64,
}

impl Withdrawals {
    pub async fn new(l1: L1Side, account: Sender, l2: DynProvider) -> anyhow::Result<Withdrawals> {
        let nullifier = l1.nullifier_address().await?;
        Ok(Withdrawals {
            l1,
            nullifier,
            account,
            l2,
            in_flight: Vec::new(),
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
            self.advance(&stats).await;
            self.top_up(&stats).await;
        }
    }

    /// One step for every in-flight withdrawal: proof ready → finalize and
    /// verify; too old → written off as unresolved.
    async fn advance(&mut self, stats: &Arc<Mutex<LoadStats>>) {
        let mut keep = Vec::new();
        for flight in self.in_flight.drain(..) {
            match probe_proof(&self.l2, &flight).await {
                Some(proof) => {
                    let verdict =
                        finalize_and_verify(&self.l1, self.nullifier, &flight, proof).await;
                    match verdict {
                        Ok(()) => stats.lock().unwrap().episode(SAGA, true, None),
                        Err(reason) => {
                            println!("withdrawals: FAILED — {reason}");
                            stats.lock().unwrap().episode(SAGA, false, Some(reason));
                        }
                    }
                }
                None if flight.submitted.elapsed() > PROOF_DEADLINE => {
                    stats.lock().unwrap().skip(
                        SAGA,
                        format!(
                            "no proof for {} within {PROOF_DEADLINE:?} (settlement paused?)",
                            flight.l2_tx,
                        ),
                    );
                }
                None => keep.push(flight),
            }
        }
        self.in_flight = keep;
    }

    /// Submits new withdrawals until the pipeline is full again.
    async fn top_up(&mut self, stats: &Arc<Mutex<LoadStats>>) {
        while self.in_flight.len() < PIPELINE {
            self.episode += 1;
            match self.submit_one().await {
                Ok(flight) => self.in_flight.push(flight),
                Err(reason) => {
                    // Submission trouble is chaos weather (validator down);
                    // note it and let the next tick retry.
                    stats
                        .lock()
                        .unwrap()
                        .skip(SAGA, format!("submit: {reason}"));
                    break;
                }
            }
        }
    }

    async fn submit_one(&mut self) -> anyhow::Result<InFlight> {
        let amount = U256::from(1_000_000_000_000_000u128); // 0.001 ETH
        let mut material = b"chaos-withdraw".to_vec();
        material.extend_from_slice(&self.episode.to_be_bytes());
        let l1_receiver = Address::from_word(keccak256(material));

        let nonce = self
            .account
            .provider
            .get_transaction_count(self.account.address)
            .pending()
            .await?;
        let gas_price = self.account.provider.get_gas_price().await?;
        let request = TransactionRequest::default()
            .with_from(self.account.address)
            .with_to(L2_BASE_TOKEN)
            .with_value(amount)
            .with_input(
                IBaseToken::withdrawCall {
                    _l1Receiver: l1_receiver,
                }
                .abi_encode(),
            )
            .with_nonce(nonce)
            .with_gas_limit(400_000)
            .with_max_fee_per_gas(gas_price.saturating_mul(4).max(1_000_000_000))
            .with_max_priority_fee_per_gas(1_000);
        let receipt = tokio::time::timeout(
            Duration::from_secs(60),
            self.account
                .provider
                .send_transaction(request)
                .await?
                .get_receipt(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("withdrawal receipt timed out"))??;
        anyhow::ensure!(receipt.status(), "the L2 withdrawal reverted");

        // The finalization inputs, extracted from the raw receipt: the
        // L1MessageSent payload and the L2→L1 log the proof will anchor.
        let raw: serde_json::Value = self
            .l2
            .raw_request(
                "eth_getTransactionReceipt".into(),
                (receipt.transaction_hash,),
            )
            .await?;
        let (l2_to_l1_index, l2_sender) = raw["l2ToL1Logs"]
            .as_array()
            .and_then(|logs| {
                logs.iter().enumerate().find_map(|(index, log)| {
                    // The user-message log comes from the L1Messenger hook; its
                    // key is the padded message-sender address.
                    let sender: Address = log["sender"].as_str()?.parse().ok()?;
                    if sender != L1_MESSENGER {
                        return None;
                    }
                    let key: B256 = log["key"].as_str()?.parse().ok()?;
                    Some((index as u64, Address::from_slice(&key.as_slice()[12..])))
                })
            })
            .ok_or_else(|| anyhow::anyhow!("withdrawal receipt has no L1Messenger L2→L1 log"))?;
        let message = raw["logs"]
            .as_array()
            .and_then(|logs| {
                logs.iter().find_map(|log| {
                    let address: Address = log["address"].as_str()?.parse().ok()?;
                    if address != L1_MESSENGER {
                        return None;
                    }
                    // L1MessageSent(address indexed, bytes32 indexed, bytes):
                    // data = abi.encode(bytes) = 32B offset ++ 32B length ++ payload.
                    let data =
                        alloy::hex::decode(log["data"].as_str()?.trim_start_matches("0x")).ok()?;
                    if data.len() < 64 {
                        return None;
                    }
                    let length = U256::from_be_slice(&data[32..64]).to::<usize>();
                    data.get(64..64 + length).map(|payload| payload.to_vec())
                })
            })
            .ok_or_else(|| anyhow::anyhow!("withdrawal receipt has no L1MessageSent event"))?;
        let tx_index_in_block = receipt
            .transaction_index
            .ok_or_else(|| anyhow::anyhow!("receipt lacks a transaction index"))?;

        Ok(InFlight {
            l2_tx: receipt.transaction_hash,
            l1_receiver,
            amount,
            message,
            l2_sender,
            l2_to_l1_index,
            tx_index_in_block,
            submitted: Instant::now(),
        })
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogProof {
    batch_number: u64,
    proof: Vec<B256>,
    id: u32,
}

/// `Some(proof)` once the containing batch has been executed on the L1.
async fn probe_proof(l2: &DynProvider, flight: &InFlight) -> Option<LogProof> {
    l2.raw_request::<_, Option<LogProof>>(
        "zks_getL2ToL1LogProof".into(),
        (flight.l2_tx, flight.l2_to_l1_index),
    )
    .await
    .ok()
    .flatten()
}

async fn finalize_and_verify(
    l1: &L1Side,
    nullifier: Address,
    flight: &InFlight,
    proof: LogProof,
) -> Result<(), String> {
    let before = l1
        .provider
        .get_balance(flight.l1_receiver)
        .await
        .map_err(|error| format!("balance query: {error:#}"))?;
    let contract = IL1Nullifier::new(nullifier, l1.provider.clone());
    let call = contract.finalizeDeposit(IL1Nullifier::FinalizeL1DepositParams {
        chainId: U256::from(l1.l2_chain_id),
        l2BatchNumber: U256::from(proof.batch_number),
        l2MessageIndex: U256::from(proof.id),
        l2Sender: flight.l2_sender,
        l2TxNumberInBatch: flight.tx_index_in_block as u16,
        message: flight.message.clone().into(),
        merkleProof: proof.proof,
    });
    let receipt = tokio::time::timeout(Duration::from_secs(60), async {
        let pending = call
            .send()
            .await
            .map_err(|error| format!("finalization send failed: {error:#}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|error| format!("finalization receipt failed: {error:#}"))
    })
    .await
    .map_err(|_| "finalization receipt timed out".to_string())??;
    if !receipt.status() {
        return Err(format!("finalization reverted for {}", flight.l2_tx));
    }
    let after = l1
        .provider
        .get_balance(flight.l1_receiver)
        .await
        .map_err(|error| format!("balance query: {error:#}"))?;
    if after != before + flight.amount {
        return Err(format!(
            "withdrawal finalized but the receiver got {} instead of {}",
            after - before,
            flight.amount,
        ));
    }
    Ok(())
}
