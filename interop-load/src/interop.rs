use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::{
    network::{EthereumWallet, ReceiptResponse, TransactionBuilder},
    primitives::{Address, B256, Bytes, FixedBytes, TxHash, U256, address},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{SolCall, SolValue},
};
use anyhow::{Context, bail};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::MissedTickBehavior;

use crate::config::{Config, RateMode};
use crate::events::{EventWriter, now_ms};
use crate::json_rpc;
use crate::setup::SetupRecord;
use crate::summary::LatencySample;

const L2_INTEROP_CENTER_ADDRESS: Address = address!("000000000000000000000000000000000001000d");
const L2_INTEROP_ROOT_STORAGE_ADDRESS: Address =
    address!("0000000000000000000000000000000000010008");

const MAX_FEE_PER_GAS: u128 = 1_000_000_000;
const SEND_BUNDLE_GAS: u64 = 10_000_000;
const APPROVE_GAS: u64 = 500_000;
const TRANSFER_GAS: u64 = 200_000;
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);
const PROOF_TIMEOUT: Duration = Duration::from_secs(600);
const ROOT_IMPORT_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn local_reqwest_client() -> alloy::transports::http::reqwest::Client {
    alloy::transports::http::reqwest::ClientBuilder::new()
        .no_proxy()
        .build()
        .expect("local reqwest client")
}

/// Per-bundle ERC20 transfer amount.
const ERC20_TRANSFER_AMOUNT: u128 = 1_000_000_000_000; // 1e12 ILT-wei
/// Per-bundle base-token transfer amount.
const BASE_TRANSFER_AMOUNT: u128 = 1_000_000_000_000; // 1e12 wei
/// Per-wallet ILT seed (covers many transfers).
const ERC20_PER_WALLET_SEED: u128 = 1_000_000_000_000_000_000; // 1e18 ILT-wei = 1 ILT

sol! {
    #[sol(rpc)]
    contract IInteropCenter {
        function sendBundle(
            bytes calldata _destinationChainId,
            InteropCallStarter[] calldata _callStarters,
            bytes[] calldata _bundleAttributes
        ) external payable returns (bytes32);

        function interopProtocolFee() external view returns (uint256);

        struct InteropCallStarter {
            bytes to;
            bytes data;
            bytes[] callAttributes;
        }
    }

    #[sol(rpc)]
    contract IL2InteropRootStorage {
        function interopRoots(uint256 chainId, uint256 batchNumber) external view returns (bytes32);
    }

    #[sol(rpc)]
    contract IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 value) external returns (bool);
        function balanceOf(address who) external view returns (uint256);
    }

    function indirectCall(uint256 _gasLimit) external pure returns (bytes memory);
    function unbundlerAddress(bytes calldata _address) external pure returns (bytes memory);
    function interopCallValue(uint256 _interopCallValue) external pure;
    function emitEvent(uint256 number) external;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct L2ToL1LogProof {
    batch_number: u64,
    proof: Vec<B256>,
    id: u32,
    root: B256,
    gateway_block_number: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum LogProofTarget {
    MessageRoot,
}

#[derive(Debug, Default, Serialize)]
pub struct RunStats {
    pub source_submitted: u64,
    pub source_included: u64,
    pub proof_available: u64,
    pub root_imported: u64,
    pub failed_classified: u64,
    pub open_loop_violated: bool,
    pub final_backlog: u64,
    pub erc20_submitted: u64,
    pub base_submitted: u64,
    pub message_submitted: u64,
    /// End-to-end latency samples for measured bundles that reached the
    /// destination chain (`root_imported`). Not serialized into events; the
    /// summary computes percentiles from these at the end of the run.
    #[serde(skip)]
    pub latency_samples: Vec<LatencySample>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum BundleShape {
    Erc20,
    Base,
    Message,
}

// Per-lane shape picking lives in `pick_shape_from(&[BundleShape], &mut StdRng)`
// further below — each lane carries its own allowed-shape slice.

#[derive(Clone, Copy, Debug)]
enum FailureStage {
    Submit,
    Receipt,
    Proof,
    RootImport,
}

#[derive(Clone)]
struct PropagationTailContext {
    source_rpc: Arc<String>,
    destination_rpc: Arc<String>,
    http: Arc<Client>,
    proof_rpc_window: Arc<Semaphore>,
    root_storage: IL2InteropRootStorage::IL2InteropRootStorageInstance<DynProvider>,
    gateway_chain_id: u64,
    outcomes: mpsc::UnboundedSender<BundleOutcome>,
}

impl FailureStage {
    fn reason_class(self) -> &'static str {
        match self {
            FailureStage::Submit => "submit_rpc_error",
            FailureStage::Receipt => "source_tx_dropped",
            FailureStage::Proof => "proof_timeout",
            FailureStage::RootImport => "root_import_timeout",
        }
    }
}

struct BundleProgress {
    bundle_id: u64,
    measured: bool,
    wallet_idx: usize,
    wallet: Address,
    source_chain_id: u64,
    source_lane_idx: usize,
    destination_chain_id: u64,
    shape: BundleShape,
    requested_at_ms: u128,
    source_tx_hash: Option<TxHash>,
    source_block: Option<u64>,
    source_gas_used: Option<u64>,
    proof_batch_number: Option<u64>,
    proof_id: Option<u32>,
    proof_path_len: Option<usize>,
    proof_root: Option<B256>,
    gateway_block_number: Option<u64>,
    destination_import_block: Option<u64>,
    timings: BundleTimings,
}

#[derive(Default)]
struct BundleTimings {
    source_submitted_at_ms: Option<u128>,
    source_included_at_ms: Option<u128>,
    proof_available_at_ms: Option<u128>,
    root_imported_at_ms: Option<u128>,
}

enum BundleOutcome {
    Stage(StageEvent),
    Failed {
        stage: FailureStage,
        progress: Box<BundleProgress>,
        error: String,
    },
    Done,
    /// Emitted as soon as the source side of a bundle completes (or fails in
    /// a way that frees the wallet) so the scheduler can return the wallet to
    /// the idle queue. The propagation tail (proof + root_imported) is
    /// tracked by a detached task that emits Stage/Done/Failed independently.
    WalletFreed {
        wallet_idx: usize,
        source_lane_idx: usize,
    },
}

enum StageEvent {
    SourceSubmitted(Box<BundleProgress>),
    SourceIncluded(Box<BundleProgress>),
    ProofAvailable(Box<BundleProgress>),
    RootImported(Box<BundleProgress>),
}

/// One source-chain lane: its wallet pool, its set of allowed bundle shapes,
/// and the destination chain bundles from this lane go to.
#[allow(dead_code)]
struct Lane {
    idx: usize,
    source_rpc: Arc<String>,
    source_chain_id: u64,
    destination_rpc: Arc<String>,
    destination_chain_id: u64,
    /// Shapes this lane is allowed to send. Picker chooses uniformly from
    /// this slice.
    allowed_shapes: Vec<BundleShape>,
    wallet_senders: Vec<mpsc::Sender<WalletJob>>,
    idle_wallets: VecDeque<usize>,
}

pub async fn run(
    config: &Config,
    events: &mut EventWriter,
    setup: &SetupRecord,
) -> anyhow::Result<RunStats> {
    let http = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to build RPC client")?;
    let chain_a_id = json_rpc::chain_id(&http, &config.chain_a_rpc).await?;
    let chain_b_id = json_rpc::chain_id(&http, &config.chain_b_rpc).await?;
    let gateway_chain_id = json_rpc::chain_id(&http, &config.gateway_rpc).await?;
    let source_rpcs = config.source_rpcs();
    let mut source_chain_ids = Vec::with_capacity(source_rpcs.len());
    for source_rpc in &source_rpcs {
        source_chain_ids.push(json_rpc::chain_id(&http, source_rpc).await?);
    }
    let (destination_rpcs, destination_chain_ids) = if config.ring {
        let mut rpcs = source_rpcs.clone();
        rpcs.rotate_left(1);
        let mut ids = source_chain_ids.clone();
        ids.rotate_left(1);
        (rpcs, ids)
    } else {
        (
            vec![config.chain_b_rpc.clone(); source_rpcs.len()],
            vec![chain_b_id; source_rpcs.len()],
        )
    };
    anyhow::ensure!(
        source_rpcs
            .iter()
            .zip(destination_rpcs.iter())
            .all(|(source, destination)| source != destination),
        "source and destination RPC lists contain a self-lane"
    );
    anyhow::ensure!(
        chain_a_id == setup.chain_a_id && chain_b_id == setup.chain_b_id,
        "setup.json chain ids ({}, {}) do not match RPC chain ids ({}, {})",
        setup.chain_a_id,
        setup.chain_b_id,
        chain_a_id,
        chain_b_id
    );

    let rich = PrivateKeySigner::from_str(config.rich_privkey.expose())
        .context("failed to parse --rich-privkey")?;

    let mut lane_signers = Vec::with_capacity(source_rpcs.len());
    for idx in 0..source_rpcs.len() {
        lane_signers.push(derive_wallet_signers(
            config.seed ^ ((idx as u64) << 32) ^ 0x5A5A5A5A5A5A5A5A,
            config.wallets,
        )?);
    }
    let lane_b_signers = if config.symmetric && config.source_rpc.is_empty() {
        Some(derive_wallet_signers(
            config.seed ^ 0xB1B1B1B1B1B1B1B1,
            config.wallets,
        )?)
    } else {
        None
    };

    if config.skip_funding {
        eprintln!(
            "interop-load: --skip-funding set; assuming wallets are already funded and approved"
        );
    } else {
        for (idx, (source_rpc, signers)) in source_rpcs.iter().zip(&lane_signers).enumerate() {
            fund_wallets_eth_on(source_rpc, &config.wallet_fund_wei, &rich, signers)
                .await
                .with_context(|| format!("fund source lane {idx} wallets"))?;
        }
        if source_chain_ids.first() == Some(&setup.chain_a_id) {
            seed_and_approve_erc20_on(
                &source_rpcs[0],
                setup.l2_token_address_chain_a,
                setup.l2_native_token_vault,
                &rich,
                &lane_signers[0],
            )
            .await
            .context("seed+approve lane 0 ERC20 wallets")?;
        }

        if let Some(lane_b) = &lane_b_signers {
            // Lane B: ETH on chain B only (no ERC20 — lane B sends Base+Message).
            fund_wallets_eth_on(&config.chain_b_rpc, &config.wallet_fund_wei, &rich, lane_b)
                .await
                .context("fund lane B wallets")?;
        }
    }

    let http = Arc::new(http);
    let setup_arc: Arc<SetupRecord> = Arc::new(setup.clone());

    check_fee_headroom(&http, &config.chain_b_rpc).await?;
    for source_rpc in &source_rpcs {
        check_fee_headroom(&http, source_rpc).await?;
    }
    for destination_rpc in &destination_rpcs {
        check_fee_headroom(&http, destination_rpc).await?;
    }
    if config.symmetric {
        check_fee_headroom(&http, &config.chain_b_rpc).await?;
    }

    // Cache the protocol fee per chain at startup.
    let mut protocol_fees = Vec::with_capacity(source_rpcs.len());
    for source_rpc in &source_rpcs {
        protocol_fees.push(query_protocol_fee(source_rpc).await?);
    }
    let protocol_fee_b = if config.symmetric {
        query_protocol_fee(&config.chain_b_rpc).await?
    } else {
        protocol_fees.first().copied().unwrap_or_default()
    };

    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<BundleOutcome>();
    let source_window = Arc::new(Semaphore::new(config.max_in_flight));
    let proof_rpc_window = Arc::new(Semaphore::new(config.proof_rpc_window));

    let mut lanes: Vec<Lane> = Vec::new();

    for idx in 0..source_rpcs.len() {
        let source_rpc = &source_rpcs[idx];
        let source_chain_id = source_chain_ids[idx];
        let destination_rpc = &destination_rpcs[idx];
        let destination_chain_id = destination_chain_ids[idx];
        let signers = lane_signers[idx].clone();
        let mut allowed_shapes = vec![BundleShape::Base, BundleShape::Message];
        if idx == 0 && source_chain_id == setup.chain_a_id {
            allowed_shapes.insert(0, BundleShape::Erc20);
        }
        let destination_read_provider = ProviderBuilder::new()
            .connect_reqwest(local_reqwest_client(), destination_rpc.parse()?)
            .erased();
        let root_storage =
            IL2InteropRootStorage::new(L2_INTEROP_ROOT_STORAGE_ADDRESS, destination_read_provider);
        let lane = build_lane(
            idx,
            source_rpc,
            source_chain_id,
            destination_rpc,
            destination_chain_id,
            allowed_shapes,
            signers,
            root_storage.clone(),
            protocol_fees[idx],
            gateway_chain_id,
            setup_arc.clone(),
            http.clone(),
            proof_rpc_window.clone(),
            outcome_tx.clone(),
        )
        .await?;
        lanes.push(lane);
    }

    if let Some(lane_b_signers) = lane_b_signers {
        let chain_a_read_provider = ProviderBuilder::new()
            .connect_reqwest(local_reqwest_client(), config.chain_a_rpc.parse()?)
            .erased();
        let root_storage_a =
            IL2InteropRootStorage::new(L2_INTEROP_ROOT_STORAGE_ADDRESS, chain_a_read_provider);
        // Lane 1: B -> A with Base + Message only (no ERC20 on B in this setup).
        let lane_b = build_lane(
            lanes.len(),
            &config.chain_b_rpc,
            chain_b_id,
            &config.chain_a_rpc,
            chain_a_id,
            vec![BundleShape::Base, BundleShape::Message],
            lane_b_signers,
            root_storage_a.clone(),
            protocol_fee_b,
            gateway_chain_id,
            setup_arc.clone(),
            http.clone(),
            proof_rpc_window.clone(),
            outcome_tx.clone(),
        )
        .await?;
        lanes.push(lane_b);
    }
    drop(outcome_tx);

    let mut stats = RunStats::default();
    let started = Instant::now();
    let duration = Duration::from_millis(config.duration_ms);
    let warmup = Duration::from_millis(config.warmup_ms);
    let mut scheduler = tokio::time::interval(rate_period(config.rate));
    scheduler.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut throttle_tick = tokio::time::interval(Duration::from_secs(1));
    throttle_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut requested_cumulative = 0_u64;
    let mut actual_cumulative = 0_u64;
    let mut throttle_events_this_second = 0_u64;
    let mut warmup_completed = warmup.is_zero();
    let mut next_bundle_id = 0_u64;
    let mut pending_inclusion_successes = 0_u64;
    let mut shape_rng = StdRng::seed_from_u64(config.seed ^ 0xA1A1A1A1A1A1A1A1);
    let mut lane_rr: usize = 0;

    if warmup_completed {
        emit_warmup_completed(events, config)?;
    }

    while started.elapsed() < duration {
        let now = started.elapsed();
        if !warmup_completed && now >= warmup {
            warmup_completed = true;
            requested_cumulative = 0;
            actual_cumulative = 0;
            emit_warmup_completed(events, config)?;
        }

        tokio::select! {
            biased;
            _ = throttle_tick.tick() => {
                let inflight = config.max_in_flight.saturating_sub(source_window.available_permits());
                let submission_lag = requested_cumulative.saturating_sub(actual_cumulative);
                events.emit(
                    "throttle_tick",
                    json!({
                        "lane": "aggregate",
                        "requested_cumulative": requested_cumulative,
                        "actual_cumulative": actual_cumulative,
                        "submission_lag": submission_lag,
                        "throttle_events_this_second": throttle_events_this_second,
                        "inflight_at_tick": inflight,
                    }),
                )?;
                events.flush()?;
                throttle_events_this_second = 0;
            }
            _ = scheduler.tick() => {
                if warmup_completed {
                    requested_cumulative += 1;
                }
                // Round-robin lane selection. If the chosen lane has no idle
                // wallet, try the other lanes once before giving up this tick.
                let mut chosen_lane: Option<usize> = None;
                for offset in 0..lanes.len() {
                    let candidate = (lane_rr + offset) % lanes.len();
                    if !lanes[candidate].idle_wallets.is_empty() {
                        chosen_lane = Some(candidate);
                        break;
                    }
                }
                let Some(lane_idx) = chosen_lane else {
                    throttle_events_this_second += 1;
                    if matches!(config.rate_mode, RateMode::OpenLoop) {
                        stats.open_loop_violated = true;
                    }
                    continue;
                };
                lane_rr = (lane_idx + 1) % lanes.len();
                let wallet_idx = lanes[lane_idx].idle_wallets.pop_front().expect("checked above");

                let permit = match source_window.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        lanes[lane_idx].idle_wallets.push_front(wallet_idx);
                        throttle_events_this_second += 1;
                        if matches!(config.rate_mode, RateMode::OpenLoop) {
                            stats.open_loop_violated = true;
                        }
                        continue;
                    }
                };
                let bundle_id = next_bundle_id;
                next_bundle_id += 1;
                let measured = warmup_completed;
                let shape = pick_shape_from(&lanes[lane_idx].allowed_shapes, &mut shape_rng);
                if measured {
                    match shape {
                        BundleShape::Erc20 => stats.erc20_submitted += 1,
                        BundleShape::Base => stats.base_submitted += 1,
                        BundleShape::Message => stats.message_submitted += 1,
                    }
                }
                let job = WalletJob {
                    bundle_id,
                    measured,
                    wallet_idx,
                    source_lane_idx: lane_idx,
                    shape,
                    requested_at_ms: now_ms(),
                    _permit: permit,
                };
                if let Err(err) = lanes[lane_idx].wallet_senders[wallet_idx].try_send(job) {
                    lanes[lane_idx].idle_wallets.push_front(wallet_idx);
                    throttle_events_this_second += 1;
                    eprintln!("interop-load: lane {lane_idx} wallet send failed: {err}");
                    continue;
                }
                if warmup_completed {
                    actual_cumulative += 1;
                }
            }
            Some(outcome) = outcome_rx.recv() => {
                handle_outcome(events, &mut stats, outcome, &mut lanes, &mut pending_inclusion_successes)?;
            }
        }
    }

    // Drop senders so workers exit their recv loops.
    for lane in &mut lanes {
        lane.wallet_senders.clear();
    }
    while let Some(outcome) = outcome_rx.recv().await {
        handle_outcome(
            events,
            &mut stats,
            outcome,
            &mut lanes,
            &mut pending_inclusion_successes,
        )?;
    }

    stats.final_backlog = pending_inclusion_successes;
    Ok(stats)
}

pub async fn run_pubdata_probe(
    config: &Config,
    events: &mut EventWriter,
    setup: &SetupRecord,
) -> anyhow::Result<RunStats> {
    let http = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to build RPC client")?;
    let chain_a_id = json_rpc::chain_id(&http, &config.chain_a_rpc).await?;
    let chain_b_id = json_rpc::chain_id(&http, &config.chain_b_rpc).await?;
    let gateway_chain_id = json_rpc::chain_id(&http, &config.gateway_rpc).await?;
    anyhow::ensure!(
        chain_a_id == setup.chain_a_id && chain_b_id == setup.chain_b_id,
        "setup.json chain ids ({}, {}) do not match RPC chain ids ({}, {})",
        setup.chain_a_id,
        setup.chain_b_id,
        chain_a_id,
        chain_b_id
    );

    let rich = PrivateKeySigner::from_str(config.rich_privkey.expose())
        .context("failed to parse --rich-privkey")?;
    let signers = derive_wallet_signers(config.seed, config.wallets)?;
    let signer = signers
        .first()
        .cloned()
        .context("--wallets must provide at least one probe wallet")?;

    if config.skip_funding {
        eprintln!("interop-load: --skip-funding set; assuming probe wallet is funded and approved");
    } else {
        fund_wallets_eth_on(
            &config.chain_a_rpc,
            &config.wallet_fund_wei,
            &rich,
            std::slice::from_ref(&signer),
        )
        .await
        .context("fund pubdata probe wallet")?;
        seed_and_approve_erc20_on(
            &config.chain_a_rpc,
            setup.l2_token_address_chain_a,
            setup.l2_native_token_vault,
            &rich,
            std::slice::from_ref(&signer),
        )
        .await
        .context("seed+approve pubdata probe wallet")?;
    }

    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(signer.clone()))
        .connect_reqwest(local_reqwest_client(), config.chain_a_rpc.parse()?)
        .erased();
    let protocol_fee = query_protocol_fee(&config.chain_a_rpc).await?;
    let mut stats = RunStats::default();

    let probe_address = signer.address();
    emit_probe_simple_transfer(events, &provider, probe_address, chain_a_id, chain_b_id).await?;
    for shape in [BundleShape::Base, BundleShape::Message, BundleShape::Erc20] {
        emit_probe_interop_bundle(
            events,
            &provider,
            probe_address,
            setup,
            chain_a_id,
            chain_b_id,
            gateway_chain_id,
            protocol_fee,
            shape,
            &mut stats,
        )
        .await?;
    }
    events.flush()?;
    Ok(stats)
}

async fn emit_probe_simple_transfer(
    events: &mut EventWriter,
    provider: &DynProvider,
    sender: Address,
    source_chain_id: u64,
    destination_chain_id: u64,
) -> anyhow::Result<()> {
    let tx = TransactionRequest::default()
        .with_to(sender)
        .with_value(U256::from(1_u64))
        .with_max_fee_per_gas(MAX_FEE_PER_GAS)
        .with_max_priority_fee_per_gas(0);
    let pending = provider
        .send_transaction(tx)
        .await
        .context("send pubdata probe simple transfer")?;
    let tx_hash = *pending.tx_hash();
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .context("simple transfer receipt")?;
    anyhow::ensure!(receipt.status(), "simple transfer reverted");
    events.emit(
        "pubdata_probe_tx",
        json!({
            "label": "simple_transfer",
            "shape": null,
            "source_chain_id": source_chain_id,
            "destination_chain_id": destination_chain_id,
            "source_tx_hash": tx_hash,
            "source_block": receipt.block_number(),
            "source_gas_used": receipt.gas_used(),
            "join_hint": "grep this tx_hash in chain source logs for `Executed transaction pubdata measurement`",
        }),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn emit_probe_interop_bundle(
    events: &mut EventWriter,
    provider: &DynProvider,
    sender: Address,
    setup: &SetupRecord,
    source_chain_id: u64,
    destination_chain_id: u64,
    gateway_chain_id: u64,
    protocol_fee: U256,
    shape: BundleShape,
    stats: &mut RunStats,
) -> anyhow::Result<()> {
    let interop_center = IInteropCenter::new(L2_INTEROP_CENTER_ADDRESS, provider.clone());
    let bundle_id = match shape {
        BundleShape::Base => {
            stats.base_submitted += 1;
            1
        }
        BundleShape::Message => {
            stats.message_submitted += 1;
            2
        }
        BundleShape::Erc20 => {
            stats.erc20_submitted += 1;
            3
        }
    };
    let (calls, attributes, value) =
        build_bundle_for_shape(shape, setup, sender, bundle_id, protocol_fee)?;
    let pending = interop_center
        .sendBundle(format_evm_v1(destination_chain_id), calls, attributes)
        .value(value)
        .gas(SEND_BUNDLE_GAS)
        .max_fee_per_gas(MAX_FEE_PER_GAS)
        .max_priority_fee_per_gas(0)
        .send()
        .await
        .with_context(|| format!("send pubdata probe {shape:?} bundle"))?;
    let tx_hash = *pending.tx_hash();
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .with_context(|| format!("{shape:?} bundle receipt"))?;
    anyhow::ensure!(receipt.status(), "{shape:?} bundle reverted");
    stats.source_submitted += 1;
    stats.source_included += 1;
    events.emit(
        "pubdata_probe_tx",
        json!({
            "label": "interop_bundle",
            "shape": shape,
            "source_chain_id": source_chain_id,
            "destination_chain_id": destination_chain_id,
            "gateway_chain_id": gateway_chain_id,
            "source_tx_hash": tx_hash,
            "source_block": receipt.block_number(),
            "source_gas_used": receipt.gas_used(),
            "join_hint": "grep this tx_hash in chain source logs for `Executed transaction pubdata measurement`",
        }),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build_lane(
    idx: usize,
    source_rpc: &str,
    source_chain_id: u64,
    destination_rpc: &str,
    destination_chain_id: u64,
    allowed_shapes: Vec<BundleShape>,
    signers: Vec<PrivateKeySigner>,
    root_storage: IL2InteropRootStorage::IL2InteropRootStorageInstance<DynProvider>,
    protocol_fee: U256,
    gateway_chain_id: u64,
    setup_arc: Arc<SetupRecord>,
    http: Arc<Client>,
    proof_rpc_window: Arc<Semaphore>,
    outcome_tx: mpsc::UnboundedSender<BundleOutcome>,
) -> anyhow::Result<Lane> {
    let source_rpc_arc = Arc::new(source_rpc.to_string());
    let destination_rpc_arc = Arc::new(destination_rpc.to_string());

    let mut wallet_senders: Vec<mpsc::Sender<WalletJob>> = Vec::with_capacity(signers.len());
    let wallet_http = local_reqwest_client();
    for signer in signers.into_iter() {
        let wallet_addr = signer.address();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(signer))
            .connect_reqwest(wallet_http.clone(), source_rpc.parse()?)
            .erased();
        let (tx, rx) = mpsc::channel::<WalletJob>(8);
        wallet_senders.push(tx);
        let outcome_tx = outcome_tx.clone();
        tokio::spawn(run_wallet_worker(
            wallet_addr,
            provider,
            source_rpc_arc.clone(),
            destination_rpc_arc.clone(),
            http.clone(),
            root_storage.clone(),
            setup_arc.clone(),
            source_chain_id,
            destination_chain_id,
            gateway_chain_id,
            protocol_fee,
            proof_rpc_window.clone(),
            rx,
            outcome_tx,
        ));
    }

    let idle_wallets: VecDeque<usize> = (0..wallet_senders.len()).collect();
    Ok(Lane {
        idx,
        source_rpc: source_rpc_arc,
        source_chain_id,
        destination_rpc: destination_rpc_arc,
        destination_chain_id,
        allowed_shapes,
        wallet_senders,
        idle_wallets,
    })
}

fn pick_shape_from(allowed: &[BundleShape], rng: &mut StdRng) -> BundleShape {
    let mut buf = [0_u8; 1];
    rng.fill_bytes(&mut buf);
    allowed[(buf[0] as usize) % allowed.len()]
}

async fn query_protocol_fee(rpc: &str) -> anyhow::Result<U256> {
    let provider = ProviderBuilder::new()
        .connect_reqwest(local_reqwest_client(), rpc.parse()?)
        .erased();
    let center = IInteropCenter::new(L2_INTEROP_CENTER_ADDRESS, provider);
    center
        .interopProtocolFee()
        .call()
        .await
        .with_context(|| format!("query interopProtocolFee on {rpc}"))
}

fn handle_outcome(
    events: &mut EventWriter,
    stats: &mut RunStats,
    outcome: BundleOutcome,
    lanes: &mut [Lane],
    pending_inclusion_successes: &mut u64,
) -> anyhow::Result<()> {
    match outcome {
        BundleOutcome::Stage(stage) => {
            emit_stage_event(events, stats, stage, pending_inclusion_successes)
        }
        BundleOutcome::Failed {
            stage,
            progress,
            error,
        } => {
            if progress.measured {
                stats.failed_classified += 1;
                if matches!(stage, FailureStage::Proof | FailureStage::RootImport) {
                    *pending_inclusion_successes = pending_inclusion_successes.saturating_sub(1);
                }
            }
            events.emit(
                "bundle_failed",
                json!({
                    "bundle_id": progress.bundle_id,
                    "measured": progress.measured,
                    "wallet_idx": progress.wallet_idx,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "wallet": progress.wallet,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "destination_chain_id": progress.destination_chain_id,
                    "shape": progress.shape,
                    "stage": match stage {
                        FailureStage::Submit => "submit",
                        FailureStage::Receipt => "receipt",
                        FailureStage::Proof => "proof",
                        FailureStage::RootImport => "root_import",
                    },
                    "reason_class": stage.reason_class(),
                    "reason_detail": error,
                    "source_tx_hash": progress.source_tx_hash,
                    "source_block": progress.source_block,
                    "gateway_block_number": progress.gateway_block_number,
                    "requested_at_ms": progress.requested_at_ms,
                    "source_submitted_at_ms": progress.timings.source_submitted_at_ms,
                    "source_included_at_ms": progress.timings.source_included_at_ms,
                    "proof_available_at_ms": progress.timings.proof_available_at_ms,
                }),
            )?;
            Ok(())
        }
        BundleOutcome::Done => Ok(()),
        BundleOutcome::WalletFreed {
            wallet_idx,
            source_lane_idx,
        } => {
            if let Some(lane) = lanes.get_mut(source_lane_idx) {
                lane.idle_wallets.push_back(wallet_idx);
            }
            Ok(())
        }
    }
}

fn emit_stage_event(
    events: &mut EventWriter,
    stats: &mut RunStats,
    stage: StageEvent,
    pending_inclusion_successes: &mut u64,
) -> anyhow::Result<()> {
    match stage {
        StageEvent::SourceSubmitted(progress) => {
            if progress.measured {
                stats.source_submitted += 1;
            }
            events.emit(
                "source_submitted",
                json!({
                    "bundle_id": progress.bundle_id,
                    "measured": progress.measured,
                    "wallet_idx": progress.wallet_idx,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "wallet": progress.wallet,
                    "destination_chain_id": progress.destination_chain_id,
                    "shape": progress.shape,
                    "requested_at_ms": progress.requested_at_ms,
                    "source_tx_hash": progress.source_tx_hash,
                    "source_submitted_at_ms": progress.timings.source_submitted_at_ms,
                }),
            )
        }
        StageEvent::SourceIncluded(progress) => {
            if progress.measured {
                stats.source_included += 1;
                *pending_inclusion_successes += 1;
            }
            events.emit(
                "source_included",
                json!({
                    "bundle_id": progress.bundle_id,
                    "measured": progress.measured,
                    "wallet_idx": progress.wallet_idx,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "wallet": progress.wallet,
                    "destination_chain_id": progress.destination_chain_id,
                    "shape": progress.shape,
                    "requested_at_ms": progress.requested_at_ms,
                    "source_tx_hash": progress.source_tx_hash,
                    "source_block": progress.source_block,
                    "source_gas_used": progress.source_gas_used,
                    "source_included_at_ms": progress.timings.source_included_at_ms,
                }),
            )
        }
        StageEvent::ProofAvailable(progress) => {
            if progress.measured {
                stats.proof_available += 1;
            }
            events.emit(
                "proof_available",
                json!({
                    "bundle_id": progress.bundle_id,
                    "measured": progress.measured,
                    "wallet_idx": progress.wallet_idx,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "destination_chain_id": progress.destination_chain_id,
                    "shape": progress.shape,
                    "requested_at_ms": progress.requested_at_ms,
                    "source_tx_hash": progress.source_tx_hash,
                    "proof_batch_number": progress.proof_batch_number,
                    "proof_id": progress.proof_id,
                    "proof_path_len": progress.proof_path_len,
                    "proof_root": progress.proof_root,
                    "gateway_block_number": progress.gateway_block_number,
                    "proof_available_at_ms": progress.timings.proof_available_at_ms,
                }),
            )
        }
        StageEvent::RootImported(progress) => {
            if progress.measured {
                stats.root_imported += 1;
                *pending_inclusion_successes = pending_inclusion_successes.saturating_sub(1);
                // Record an end-to-end latency sample. All four timestamps are
                // set by the time a bundle reaches `root_imported`; if any is
                // somehow missing we skip the sample rather than fabricate one.
                if let (Some(submitted), Some(included), Some(proof), Some(imported)) = (
                    progress.timings.source_submitted_at_ms,
                    progress.timings.source_included_at_ms,
                    progress.timings.proof_available_at_ms,
                    progress.timings.root_imported_at_ms,
                ) {
                    stats.latency_samples.push(LatencySample {
                        source_chain_id: progress.source_chain_id,
                        destination_chain_id: progress.destination_chain_id,
                        source_submitted_at_ms: submitted,
                        source_included_at_ms: included,
                        proof_available_at_ms: proof,
                        root_imported_at_ms: imported,
                    });
                }
            }
            events.emit(
                "root_imported",
                json!({
                    "bundle_id": progress.bundle_id,
                    "measured": progress.measured,
                    "wallet_idx": progress.wallet_idx,
                    "source_chain_id": progress.source_chain_id,
                    "source_lane_idx": progress.source_lane_idx,
                    "destination_chain_id": progress.destination_chain_id,
                    "shape": progress.shape,
                    "requested_at_ms": progress.requested_at_ms,
                    "source_tx_hash": progress.source_tx_hash,
                    "gateway_block_number": progress.gateway_block_number,
                    "destination_import_block": progress.destination_import_block,
                    "root_imported_at_ms": progress.timings.root_imported_at_ms,
                }),
            )
        }
    }
}

struct WalletJob {
    bundle_id: u64,
    measured: bool,
    wallet_idx: usize,
    source_lane_idx: usize,
    shape: BundleShape,
    requested_at_ms: u128,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[allow(clippy::too_many_arguments)]
async fn run_wallet_worker(
    wallet: Address,
    provider: DynProvider,
    source_rpc: Arc<String>,
    destination_rpc: Arc<String>,
    http: Arc<Client>,
    root_storage: IL2InteropRootStorage::IL2InteropRootStorageInstance<DynProvider>,
    setup: Arc<SetupRecord>,
    source_chain_id: u64,
    destination_chain_id: u64,
    gateway_chain_id: u64,
    protocol_fee: U256,
    proof_rpc_window: Arc<Semaphore>,
    mut jobs: mpsc::Receiver<WalletJob>,
    outcomes: mpsc::UnboundedSender<BundleOutcome>,
) {
    let interop_center = IInteropCenter::new(L2_INTEROP_CENTER_ADDRESS, provider.clone());

    while let Some(job) = jobs.recv().await {
        let shape = job.shape;
        let wallet_idx = job.wallet_idx;
        let source_lane_idx = job.source_lane_idx;
        let mut progress = new_progress(job, wallet, source_chain_id, destination_chain_id, shape);

        let (calls, attributes, value) =
            match build_bundle_for_shape(shape, &setup, wallet, progress.bundle_id, protocol_fee) {
                Ok(parts) => parts,
                Err(err) => {
                    let _ = outcomes.send(BundleOutcome::Failed {
                        stage: FailureStage::Submit,
                        progress: Box::new(progress),
                        error: format!("build bundle failed: {err}"),
                    });
                    let _ = outcomes.send(BundleOutcome::WalletFreed {
                        wallet_idx,
                        source_lane_idx,
                    });
                    continue;
                }
            };

        let destination = format_evm_v1(destination_chain_id);
        let pending = match interop_center
            .sendBundle(destination, calls, attributes)
            .value(value)
            .gas(SEND_BUNDLE_GAS)
            .max_fee_per_gas(MAX_FEE_PER_GAS)
            .max_priority_fee_per_gas(0)
            .send()
            .await
        {
            Ok(p) => p,
            Err(err) => {
                let _ = outcomes.send(BundleOutcome::Failed {
                    stage: FailureStage::Submit,
                    progress: Box::new(progress),
                    error: format!("{err:#}"),
                });
                let _ = outcomes.send(BundleOutcome::WalletFreed {
                    wallet_idx,
                    source_lane_idx,
                });
                continue;
            }
        };
        progress.source_tx_hash = Some(*pending.tx_hash());
        progress.timings.source_submitted_at_ms = Some(now_ms());
        let _ = outcomes.send(BundleOutcome::Stage(StageEvent::SourceSubmitted(Box::new(
            clone_progress(&progress),
        ))));

        let receipt = match tokio::time::timeout(RECEIPT_TIMEOUT, pending.get_receipt()).await {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(err)) => {
                let _ = outcomes.send(BundleOutcome::Failed {
                    stage: FailureStage::Receipt,
                    progress: Box::new(progress),
                    error: format!("get_receipt failed: {err}"),
                });
                let _ = outcomes.send(BundleOutcome::WalletFreed {
                    wallet_idx,
                    source_lane_idx,
                });
                continue;
            }
            Err(_) => {
                let _ = outcomes.send(BundleOutcome::Failed {
                    stage: FailureStage::Receipt,
                    progress: Box::new(progress),
                    error: "timed out waiting for source receipt".to_string(),
                });
                let _ = outcomes.send(BundleOutcome::WalletFreed {
                    wallet_idx,
                    source_lane_idx,
                });
                continue;
            }
        };
        if !receipt.status() {
            let _ = outcomes.send(BundleOutcome::Failed {
                stage: FailureStage::Receipt,
                progress: Box::new(progress),
                error: "source sendBundle reverted".to_string(),
            });
            let _ = outcomes.send(BundleOutcome::WalletFreed {
                wallet_idx,
                source_lane_idx,
            });
            continue;
        }
        progress.source_block = receipt.block_number();
        progress.source_gas_used = Some(receipt.gas_used());
        progress.timings.source_included_at_ms = Some(now_ms());
        let _ = outcomes.send(BundleOutcome::Stage(StageEvent::SourceIncluded(Box::new(
            clone_progress(&progress),
        ))));

        // Free the wallet immediately so the scheduler can submit the next
        // bundle while the gateway propagation tail runs in a detached task.
        // This decouples source-side throughput from gateway pipeline latency.
        let _ = outcomes.send(BundleOutcome::WalletFreed {
            wallet_idx,
            source_lane_idx,
        });
        tokio::spawn(run_propagation_tail(
            progress,
            PropagationTailContext {
                source_rpc: source_rpc.clone(),
                destination_rpc: destination_rpc.clone(),
                http: http.clone(),
                proof_rpc_window: proof_rpc_window.clone(),
                root_storage: root_storage.clone(),
                gateway_chain_id,
                outcomes: outcomes.clone(),
            },
        ));
    }
}

async fn run_propagation_tail(mut progress: BundleProgress, ctx: PropagationTailContext) {
    let PropagationTailContext {
        source_rpc,
        destination_rpc,
        http,
        proof_rpc_window,
        root_storage,
        gateway_chain_id,
        outcomes,
    } = ctx;
    let tx_hash = progress.source_tx_hash.expect("set before tail");
    let proof = match wait_message_root_proof(&http, &source_rpc, tx_hash, &proof_rpc_window).await
    {
        Ok(p) => p,
        Err(err) => {
            let _ = outcomes.send(BundleOutcome::Failed {
                stage: FailureStage::Proof,
                progress: Box::new(progress),
                error: format!("{err:#}"),
            });
            return;
        }
    };
    progress.proof_batch_number = Some(proof.batch_number);
    progress.proof_id = Some(proof.id);
    progress.proof_path_len = Some(proof.proof.len());
    progress.proof_root = Some(proof.root);
    let Some(gw_block) = proof.gateway_block_number else {
        let _ = outcomes.send(BundleOutcome::Failed {
            stage: FailureStage::Proof,
            progress: Box::new(progress),
            error: "proof did not include gateway_block_number".to_string(),
        });
        return;
    };
    progress.gateway_block_number = Some(gw_block);
    progress.timings.proof_available_at_ms = Some(now_ms());
    let _ = outcomes.send(BundleOutcome::Stage(StageEvent::ProofAvailable(Box::new(
        clone_progress(&progress),
    ))));

    match wait_root_imported(root_storage, gateway_chain_id, gw_block, &proof_rpc_window).await {
        Ok(()) => {
            progress.timings.root_imported_at_ms = Some(now_ms());
            progress.destination_import_block =
                json_rpc::block_number(&http, &destination_rpc).await.ok();
            let _ = outcomes.send(BundleOutcome::Stage(StageEvent::RootImported(Box::new(
                clone_progress(&progress),
            ))));
            let _ = outcomes.send(BundleOutcome::Done);
        }
        Err(err) => {
            let _ = outcomes.send(BundleOutcome::Failed {
                stage: FailureStage::RootImport,
                progress: Box::new(progress),
                error: format!("{err:#}"),
            });
        }
    }
}

fn build_bundle_for_shape(
    shape: BundleShape,
    setup: &SetupRecord,
    sender: Address,
    bundle_id: u64,
    protocol_fee: U256,
) -> anyhow::Result<(Vec<IInteropCenter::InteropCallStarter>, Vec<Bytes>, U256)> {
    let bundle_attributes = vec![Bytes::from(
        unbundlerAddressCall {
            _address: format_evm_v1_address_only(sender),
        }
        .abi_encode(),
    )];
    let call_attributes = vec![Bytes::from(
        indirectCallCall {
            _gasLimit: U256::ZERO,
        }
        .abi_encode(),
    )];
    let l2_asset_router = address!("0000000000000000000000000000000000010003");

    match shape {
        BundleShape::Erc20 => {
            let data = build_second_bridge_calldata(
                setup.erc20_asset_id,
                U256::from(ERC20_TRANSFER_AMOUNT),
                sender,
                Address::ZERO,
            );
            let call = IInteropCenter::InteropCallStarter {
                to: format_evm_v1_address_only(l2_asset_router),
                data,
                callAttributes: call_attributes,
            };
            Ok((vec![call], bundle_attributes, protocol_fee))
        }
        BundleShape::Base => {
            // Same-base-token interop transfer (ETH → ETH between A and B).
            // The InteropCenter burns the value on the source via
            // L2_BASE_TOKEN_HOLDER and reissues it on the destination, where
            // the InteropHandler invokes IERC7786Recipient.receiveMessage with
            // msg.value = transferred amount. Our InteropRecipient contract
            // implements that and, when msg.value > 0 and the payload encodes
            // a 20-byte recipient address, forwards the value to that address.
            //
            // Source-side: no indirectCall — only an interopCallValue attribute
            // (so the InteropCenter knows how much of msg.value to attribute to
            // the call vs. the protocol fee).
            let _ = l2_asset_router;
            let _ = bundle_id;
            let payload = Bytes::from(sender.as_slice().to_vec()); // bytes20(recipient)
            let value_attr = Bytes::from(
                interopCallValueCall {
                    _interopCallValue: U256::from(BASE_TRANSFER_AMOUNT),
                }
                .abi_encode(),
            );
            let call = IInteropCenter::InteropCallStarter {
                to: format_evm_v1_address_only(setup.interop_recipient),
                data: payload,
                callAttributes: vec![value_attr],
            };
            Ok((
                vec![call],
                bundle_attributes,
                protocol_fee + U256::from(BASE_TRANSFER_AMOUNT),
            ))
        }
        BundleShape::Message => {
            // Plain direct call (no `indirectCall` attribute): the source-side
            // InteropCenter does not invoke `to` on chain A; it only validates
            // bundle structure. On the destination, the InteropHandler calls
            // `IERC7786Recipient(to).receiveMessage(receiveId, sender, payload)`
            // and checks the returned selector — so `to` must be a deployed
            // `InteropRecipient` contract at the same address on both chains.
            let _ = sender;
            let payload = Bytes::from(bundle_id.to_be_bytes().to_vec());
            let call = IInteropCenter::InteropCallStarter {
                to: format_evm_v1_address_only(setup.interop_recipient),
                data: payload,
                // No `indirectCall` attribute — direct ERC-7786 dispatch.
                callAttributes: vec![],
            };
            Ok((vec![call], bundle_attributes, protocol_fee))
        }
    }
}

fn new_progress(
    job: WalletJob,
    wallet: Address,
    source_chain_id: u64,
    destination_chain_id: u64,
    shape: BundleShape,
) -> BundleProgress {
    BundleProgress {
        bundle_id: job.bundle_id,
        measured: job.measured,
        wallet_idx: job.wallet_idx,
        wallet,
        source_chain_id,
        source_lane_idx: job.source_lane_idx,
        destination_chain_id,
        shape,
        requested_at_ms: job.requested_at_ms,
        source_tx_hash: None,
        source_block: None,
        source_gas_used: None,
        proof_batch_number: None,
        proof_id: None,
        proof_path_len: None,
        proof_root: None,
        gateway_block_number: None,
        destination_import_block: None,
        timings: BundleTimings::default(),
    }
}

fn clone_progress(p: &BundleProgress) -> BundleProgress {
    BundleProgress {
        bundle_id: p.bundle_id,
        measured: p.measured,
        wallet_idx: p.wallet_idx,
        wallet: p.wallet,
        source_chain_id: p.source_chain_id,
        source_lane_idx: p.source_lane_idx,
        destination_chain_id: p.destination_chain_id,
        shape: p.shape,
        requested_at_ms: p.requested_at_ms,
        source_tx_hash: p.source_tx_hash,
        source_block: p.source_block,
        source_gas_used: p.source_gas_used,
        proof_batch_number: p.proof_batch_number,
        proof_id: p.proof_id,
        proof_path_len: p.proof_path_len,
        proof_root: p.proof_root,
        gateway_block_number: p.gateway_block_number,
        destination_import_block: p.destination_import_block,
        timings: BundleTimings {
            source_submitted_at_ms: p.timings.source_submitted_at_ms,
            source_included_at_ms: p.timings.source_included_at_ms,
            proof_available_at_ms: p.timings.proof_available_at_ms,
            root_imported_at_ms: p.timings.root_imported_at_ms,
        },
    }
}

async fn wait_message_root_proof(
    client: &Client,
    chain_a_rpc: &str,
    tx_hash: TxHash,
    proof_rpc_window: &Arc<Semaphore>,
) -> anyhow::Result<L2ToL1LogProof> {
    let started = Instant::now();
    let mut last_error: Option<anyhow::Error> = None;
    while started.elapsed() < PROOF_TIMEOUT {
        let permit = proof_rpc_window
            .clone()
            .acquire_owned()
            .await
            .context("proof RPC semaphore closed")?;
        let value = json_rpc::rpc(
            client,
            chain_a_rpc,
            "zks_getL2ToL1LogProof",
            json!([tx_hash, 0, LogProofTarget::MessageRoot]),
        )
        .await;
        drop(permit);
        match value {
            Ok(value) if !value.is_null() => {
                return serde_json::from_value(value)
                    .context("decode zks_getL2ToL1LogProof result");
            }
            Ok(_) => {}
            Err(err) => {
                last_error = Some(err);
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if let Some(last_error) = last_error {
        bail!("timed out waiting for message-root proof for {tx_hash}; last error: {last_error:#}");
    }
    bail!("timed out waiting for message-root proof for {tx_hash}");
}

async fn wait_root_imported(
    root_storage: IL2InteropRootStorage::IL2InteropRootStorageInstance<DynProvider>,
    gateway_chain_id: u64,
    gateway_block_number: u64,
    proof_rpc_window: &Arc<Semaphore>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut last_error: Option<anyhow::Error> = None;
    while started.elapsed() < ROOT_IMPORT_TIMEOUT {
        let permit = proof_rpc_window
            .clone()
            .acquire_owned()
            .await
            .context("root RPC semaphore closed")?;
        let root_result = root_storage
            .interopRoots(
                U256::from(gateway_chain_id),
                U256::from(gateway_block_number),
            )
            .call()
            .await
            .context("query interopRoots on destination chain");
        drop(permit);
        let root = match root_result {
            Ok(root) => root,
            Err(err) => {
                last_error = Some(err);
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        if root != B256::ZERO {
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if let Some(last_error) = last_error {
        bail!(
            "timed out waiting for gateway root {gateway_block_number} on chain B; last error: {last_error:#}"
        );
    }
    bail!("timed out waiting for gateway root {gateway_block_number} on chain B")
}

async fn fund_wallets_eth_on(
    rpc: &str,
    wallet_fund_wei: &str,
    rich: &PrivateKeySigner,
    wallets: &[PrivateKeySigner],
) -> anyhow::Result<()> {
    let amount = U256::from_str(wallet_fund_wei).context("parse --wallet-fund-wei")?;
    if amount.is_zero() {
        return Ok(());
    }
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(rich.clone()))
        .connect_reqwest(local_reqwest_client(), rpc.parse()?);
    let mut nonce = provider
        .get_transaction_count(rich.address())
        .await
        .with_context(|| format!("get rich wallet nonce on {rpc}"))?;
    let mut pending_receipts = Vec::with_capacity(wallets.len());
    for wallet in wallets {
        let tx = TransactionRequest::default()
            .with_to(wallet.address())
            .with_value(amount)
            .with_max_fee_per_gas(MAX_FEE_PER_GAS)
            .with_max_priority_fee_per_gas(0)
            .with_nonce(nonce);
        nonce += 1;
        let pending = provider
            .send_transaction(tx)
            .await
            .with_context(|| format!("fund wallet {} on {rpc}", wallet.address()))?;
        pending_receipts.push((wallet.address(), pending));
    }
    for (wallet, pending) in pending_receipts {
        let receipt = pending
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await
            .with_context(|| format!("funding receipt for {wallet} on {rpc}"))?;
        anyhow::ensure!(
            receipt.status(),
            "funding transaction reverted for {wallet} on {rpc}"
        );
    }
    Ok(())
}

async fn seed_and_approve_erc20_on(
    rpc: &str,
    l2_token: Address,
    l2_native_token_vault: Address,
    rich: &PrivateKeySigner,
    wallets: &[PrivateKeySigner],
) -> anyhow::Result<()> {
    let seed_amount = U256::from(ERC20_PER_WALLET_SEED);
    if seed_amount.is_zero() {
        return Ok(());
    }
    let rich_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(rich.clone()))
        .connect_reqwest(local_reqwest_client(), rpc.parse()?);
    let token = IERC20::new(l2_token, rich_provider.clone());
    let mut nonce = rich_provider
        .get_transaction_count(rich.address())
        .await
        .with_context(|| format!("get rich wallet nonce on {rpc}"))?;
    let mut transfer_receipts = Vec::with_capacity(wallets.len());
    for wallet in wallets {
        let pending = token
            .transfer(wallet.address(), seed_amount)
            .gas(TRANSFER_GAS)
            .max_fee_per_gas(MAX_FEE_PER_GAS)
            .max_priority_fee_per_gas(0)
            .nonce(nonce)
            .send()
            .await
            .with_context(|| format!("ERC20 transfer to {} on {rpc}", wallet.address()))?;
        nonce += 1;
        transfer_receipts.push((wallet.address(), pending));
    }
    for (wallet, pending) in transfer_receipts {
        let receipt = pending
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await?;
        anyhow::ensure!(
            receipt.status(),
            "ERC20 seed transfer reverted for {wallet} on {rpc}"
        );
    }

    let approve_amount = U256::MAX;
    let mut approve_receipts = Vec::with_capacity(wallets.len());
    for wallet in wallets {
        let wallet_provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(wallet.clone()))
            .connect_reqwest(local_reqwest_client(), rpc.parse()?);
        let token = IERC20::new(l2_token, wallet_provider);
        let pending = token
            .approve(l2_native_token_vault, approve_amount)
            .gas(APPROVE_GAS)
            .max_fee_per_gas(MAX_FEE_PER_GAS)
            .max_priority_fee_per_gas(0)
            .send()
            .await
            .with_context(|| format!("approve NTV from {} on {rpc}", wallet.address()))?;
        approve_receipts.push((wallet.address(), pending));
    }
    for (wallet, pending) in approve_receipts {
        let receipt = pending
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await?;
        anyhow::ensure!(receipt.status(), "approve reverted for {wallet} on {rpc}");
    }
    Ok(())
}

async fn check_fee_headroom(client: &Client, chain_a_rpc: &str) -> anyhow::Result<()> {
    let value = json_rpc::rpc(client, chain_a_rpc, "eth_gasPrice", json!([])).await?;
    let hex = value
        .as_str()
        .context("eth_gasPrice did not return a string")?
        .strip_prefix("0x")
        .unwrap_or_default();
    let gas_price = u128::from_str_radix(hex, 16).context("parse eth_gasPrice")?;
    if gas_price >= MAX_FEE_PER_GAS / 2 {
        eprintln!(
            "warning: eth_gasPrice {gas_price} approaches MAX_FEE_PER_GAS {MAX_FEE_PER_GAS}; \
             consider raising the constant before measuring throughput"
        );
    }
    Ok(())
}

fn derive_wallet_signers(seed: u64, count: usize) -> anyhow::Result<Vec<PrivateKeySigner>> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut signers = Vec::with_capacity(count);
    while signers.len() < count {
        let mut bytes = [0_u8; 32];
        rng.fill_bytes(&mut bytes);
        if bytes.iter().all(|byte| *byte == 0) {
            continue;
        }
        signers.push(
            PrivateKeySigner::from_bytes(&B256::from(bytes))
                .context("build deterministic wallet")?,
        );
    }
    Ok(signers)
}

/// Encode asset-router second-bridge calldata for `_decreaseChainBalance` /
/// `transfer` flow, matching the integration test's `build_second_bridge_calldata`.
fn build_second_bridge_calldata(
    asset_id: FixedBytes<32>,
    amount: U256,
    receiver: Address,
    maybe_token_address: Address,
) -> Bytes {
    let inner = (amount, receiver, maybe_token_address).abi_encode();
    let mut result = vec![0x01]; // NEW_ENCODING_VERSION
    result.extend_from_slice(asset_id.as_slice());
    result.extend_from_slice(&[0_u8; 31]);
    result.push(0x40);
    result.extend_from_slice(&U256::from(inner.len()).to_be_bytes::<32>());
    result.extend_from_slice(&inner);
    let padding = (32 - (inner.len() % 32)) % 32;
    result.extend_from_slice(&vec![0_u8; padding]);
    Bytes::from(result)
}

fn format_evm_v1_address_only(addr: Address) -> Bytes {
    let mut result = Vec::new();
    result.extend_from_slice(&[0x00, 0x01]);
    result.extend_from_slice(&[0x00, 0x00]);
    result.push(0x00);
    result.push(0x14);
    result.extend_from_slice(addr.as_slice());
    Bytes::from(result)
}

fn format_evm_v1(chain_id: u64) -> Bytes {
    let chain_ref = to_chain_reference(chain_id);
    let mut result = Vec::new();
    result.extend_from_slice(&[0x00, 0x01]);
    result.extend_from_slice(&[0x00, 0x00]);
    result.push(chain_ref.len() as u8);
    result.extend_from_slice(&chain_ref);
    result.push(0x00);
    Bytes::from(result)
}

fn to_chain_reference(chain_id: u64) -> Vec<u8> {
    if chain_id == 0 {
        return vec![0];
    }
    let mut bytes = chain_id.to_be_bytes().to_vec();
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    bytes
}

fn rate_period(rate: f64) -> Duration {
    Duration::from_secs_f64((1.0 / rate).max(0.001))
}

fn emit_warmup_completed(events: &mut EventWriter, config: &Config) -> anyhow::Result<()> {
    events.emit(
        "warmup_completed",
        json!({
            "observed_root_import_p95_ms": null,
            "required_wallets_from_observed_p95": null,
            "configured_wallets": config.wallets,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::{format_evm_v1, to_chain_reference};

    #[test]
    fn chain_reference_is_minimal_big_endian() {
        assert_eq!(to_chain_reference(0), vec![0]);
        assert_eq!(to_chain_reference(1), vec![1]);
        assert_eq!(to_chain_reference(0x1234), vec![0x12, 0x34]);
    }

    #[test]
    fn evm_v1_chain_reference_uses_empty_address() {
        assert_eq!(format_evm_v1(1).as_ref(), &[0, 1, 0, 0, 1, 1, 0]);
    }
}
