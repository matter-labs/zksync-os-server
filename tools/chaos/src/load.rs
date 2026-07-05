//! Transaction load against a running chaos cluster.
//!
//! Deterministic sender accounts are funded once through a real L1→L2 deposit
//! (the bridgehub path, signed by anvil's default rich account), then drive plain
//! value transfers at a configured rate and shape. Under chaos, submission errors
//! are expected — validators go down mid-run — so failures are counted, never
//! fatal; the point is to put real traffic through consensus while the driver and
//! watcher do their work, and to report what happened.
//!
//! Patterns:
//! - `sustained`: a steady `--tps` for the whole run (use a low value for
//!   background traffic, a high one for stress).
//! - `bursts`: alternate `--burst-secs` at `--tps` with `--idle-secs` of silence.
//!
//! Spread:
//! - `even`: senders are pinned round-robin across validators (tx gossip carries
//!   their transactions to whoever leads).
//! - `single:<i>`: every sender submits to validator `i` only.

use crate::setup::Manifest;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context as _;
use clap::Args;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE;

/// Anvil's default account #0 — rich on the checked-in L1 state; pays for deposits.
const ANVIL_RICH_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// L2 funds per sender; transfers are 1 wei + gas, so this lasts any realistic run.
const FUNDING_ETH: u64 = 100;
/// Gas per transfer. Generous on purpose: zksync-os transfers cost far more than
/// bare 21k (account abstraction + pubdata overhead), and an under-gassed
/// transaction passes pool validation but aborts in the VM at build time —
/// leaving it stuck in the pool as a permanent pending ghost.
const TRANSFER_GAS: u64 = 300_000;
/// Fee floors. This chain's basefee tracks L1 pricing and swings by orders of
/// magnitude on a young chain; a transaction priced honestly during a trough
/// becomes unincludable when fees climb — and, being lowest-nonce, wedges its
/// whole sender. Overpaying a floor of 1 gwei costs nothing on a rig and makes
/// pricing insensitive to the swings.
const MIN_MAX_FEE: u128 = 1_000_000_000;
const MIN_TIP: u128 = 1_000;
/// Maintenance cadence: fee refresh + wedged-sender rescue.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);
/// Consecutive maintenance passes without on-chain nonce progress (while a local
/// backlog exists) before a sender's pending range is repriced.
const STALLED_PASSES_BEFORE_REPRICE: u8 = 3;

#[derive(Args)]
pub struct LoadArgs {
    /// Work directory produced by `chaos setup` (with a running cluster).
    #[arg(long)]
    pub workdir: PathBuf,
    /// Transactions per second while sending.
    #[arg(long, default_value_t = 10)]
    pub tps: u32,
    /// How long to run; omit to run until interrupted.
    #[arg(long)]
    pub duration: Option<humantime::Duration>,
    /// Traffic shape over time.
    #[arg(long, value_enum, default_value_t = Pattern::Sustained)]
    pub pattern: Pattern,
    /// Seconds of sending per burst (bursts pattern only).
    #[arg(long, default_value_t = 5)]
    pub burst_secs: u64,
    /// Seconds of silence between bursts (bursts pattern only).
    #[arg(long, default_value_t = 15)]
    pub idle_secs: u64,
    /// Where transactions go: `even` (senders pinned round-robin over all
    /// validators) or `single:<i>` (everything to validator i).
    #[arg(long, default_value = "even")]
    pub spread: String,
    /// Number of sender accounts (also the submission parallelism).
    #[arg(long, default_value_t = 8)]
    pub senders: usize,
    /// Seed for deriving deterministic sender keys.
    #[arg(long, default_value_t = 7777)]
    pub key_seed: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Pattern {
    Sustained,
    Bursts,
}

/// One funded account pinned to one validator's RPC.
struct Sender {
    address: Address,
    provider: DynProvider,
    validator: usize,
    nonce: u64,
    last_hash: Option<B256>,
    /// On-chain (latest) nonce at the previous maintenance pass, for detecting a
    /// wedged sender.
    last_chain_nonce: u64,
    stalled_passes: u8,
}

#[derive(Default, Clone)]
struct ValidatorStats {
    accepted: u64,
    rejected: u64,
}

pub async fn run(args: LoadArgs) -> anyhow::Result<()> {
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(
        args.workdir.join("manifest.json"),
    )?)?;
    let validators = manifest.validators.len();
    let target_of = parse_spread(&args.spread, validators)?;

    let mut senders = build_senders(&args, &manifest, &target_of).await?;
    fund_senders(&manifest, &senders).await?;

    let chain_id = senders[0].provider.get_chain_id().await?;
    println!(
        "loading {} validators with {} senders at {} tps ({}), chain id {chain_id}",
        validators,
        senders.len(),
        args.tps,
        args.spread,
    );

    let started = Instant::now();
    let deadline = args.duration.map(|duration| started + *duration);
    let tick = Duration::from_secs_f64(1.0 / f64::from(args.tps.max(1)));
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stats = vec![ValidatorStats::default(); validators];
    let mut next_sender = 0usize;
    // A cache of (max_fee, tip) refreshed periodically from whichever validator
    // answers; stale fees are fine for a rig.
    let mut fees: (u128, u128) = (2_000_000_000, 100_000_000);
    let mut fees_refreshed = Instant::now() - Duration::from_secs(60);

    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break;
        }
        if args.pattern == Pattern::Bursts {
            let cycle = args.burst_secs + args.idle_secs;
            let position = started.elapsed().as_secs() % cycle.max(1);
            if position >= args.burst_secs {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        }
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => break,
        }

        if fees_refreshed.elapsed() > MAINTENANCE_INTERVAL {
            fees_refreshed = Instant::now();
            for sender in senders.iter() {
                if let Ok(gas_price) = sender.provider.get_gas_price().await {
                    fees = (
                        gas_price.saturating_mul(4).max(MIN_MAX_FEE),
                        (gas_price / 10).max(MIN_TIP),
                    );
                    break;
                }
            }
            rescue_wedged_senders(&mut senders, &mut stats, chain_id, fees).await;
        }

        let sender = &mut senders[next_sender];
        next_sender = (next_sender + 1) % args.senders;
        let request = TransactionRequest::default()
            .with_chain_id(chain_id)
            .with_from(sender.address)
            .with_to(Address::from_word(keccak256(sender.nonce.to_be_bytes())))
            .with_value(U256::from(1u64))
            .with_nonce(sender.nonce)
            .with_gas_limit(TRANSFER_GAS)
            .with_max_fee_per_gas(fees.0)
            .with_max_priority_fee_per_gas(fees.1);
        match sender.provider.send_transaction(request).await {
            Ok(pending) => {
                sender.last_hash = Some(*pending.tx_hash());
                sender.nonce += 1;
                stats[sender.validator].accepted += 1;
            }
            Err(error) => {
                stats[sender.validator].rejected += 1;
                // Down validators refuse connections — routine under chaos. A
                // nonce drift (e.g. after an RPC error whose tx still landed)
                // resyncs from the chain.
                let text = error.to_string();
                if text.contains("nonce")
                    && let Ok(fresh) = sender
                        .provider
                        .get_transaction_count(sender.address)
                        .pending()
                        .await
                {
                    sender.nonce = fresh;
                }
            }
        }
    }

    report(&senders, &stats, started.elapsed()).await;
    Ok(())
}

/// Revives senders whose lowest pending transaction became unincludable (e.g.
/// priced during a fee trough before fees climbed): if a sender has a local
/// backlog and its on-chain nonce has not moved for several passes, the whole
/// pending range is resubmitted at current prices. The recipient of each nonce is
/// derived from the nonce, so a replacement is the same transfer, better paid.
async fn rescue_wedged_senders(
    senders: &mut [Sender],
    stats: &mut [ValidatorStats],
    chain_id: u64,
    fees: (u128, u128),
) {
    for sender in senders.iter_mut() {
        let Ok(chain_nonce) = sender.provider.get_transaction_count(sender.address).await else {
            continue;
        };
        if chain_nonce >= sender.nonce || chain_nonce > sender.last_chain_nonce {
            sender.last_chain_nonce = chain_nonce;
            sender.stalled_passes = 0;
            continue;
        }
        sender.stalled_passes += 1;
        if sender.stalled_passes < STALLED_PASSES_BEFORE_REPRICE {
            continue;
        }
        sender.stalled_passes = 0;
        let upper = sender.nonce.min(chain_nonce + 32);
        println!(
            "repricing wedged sender {} (nonces {chain_nonce}..{upper})",
            sender.address
        );
        for nonce in chain_nonce..upper {
            let request = TransactionRequest::default()
                .with_chain_id(chain_id)
                .with_from(sender.address)
                .with_to(Address::from_word(keccak256(nonce.to_be_bytes())))
                .with_value(U256::from(1u64))
                .with_nonce(nonce)
                .with_gas_limit(TRANSFER_GAS)
                .with_max_fee_per_gas(fees.0)
                .with_max_priority_fee_per_gas(fees.1);
            match sender.provider.send_transaction(request).await {
                Ok(pending) => {
                    sender.last_hash = Some(*pending.tx_hash());
                    stats[sender.validator].accepted += 1;
                }
                Err(_) => stats[sender.validator].rejected += 1,
            }
        }
    }
}

fn parse_spread(spread: &str, validators: usize) -> anyhow::Result<Vec<usize>> {
    // Maps sender index -> validator index; senders beyond the map wrap around.
    if spread == "even" {
        return Ok((0..validators).collect());
    }
    if let Some(index) = spread.strip_prefix("single:") {
        let index: usize = index.parse().context("spread: expected `single:<i>`")?;
        anyhow::ensure!(
            index < validators,
            "spread targets validator {index}, but there are only {validators}"
        );
        return Ok(vec![index]);
    }
    anyhow::bail!("unknown spread {spread:?}; use `even` or `single:<i>`")
}

async fn build_senders(
    args: &LoadArgs,
    manifest: &Manifest,
    target_of: &[usize],
) -> anyhow::Result<Vec<Sender>> {
    let mut senders = Vec::with_capacity(args.senders);
    for index in 0..args.senders {
        let mut material = b"chaos-load".to_vec();
        material.extend_from_slice(&args.key_seed.to_be_bytes());
        material.extend_from_slice(&index.to_be_bytes());
        let signer = PrivateKeySigner::from_bytes(&keccak256(material))?;
        let address = signer.address();
        let validator = target_of[index % target_of.len()];
        let url = format!(
            "http://127.0.0.1:{}",
            manifest.validators[validator].host_rpc_port
        );
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(url.parse()?)
            .erased();
        // Resuming a key seed against a used cluster continues from the pending
        // nonce; a fresh cluster yields 0.
        let nonce = provider
            .get_transaction_count(address)
            .pending()
            .await
            .unwrap_or(0);
        senders.push(Sender {
            address,
            provider,
            validator,
            nonce,
            last_hash: None,
            last_chain_nonce: 0,
            stalled_passes: 0,
        });
    }
    Ok(senders)
}

/// Funds every sender that has no L2 balance yet, through a real bridgehub
/// deposit per sender, then waits for the L2 balances to appear.
async fn fund_senders(manifest: &Manifest, senders: &[Sender]) -> anyhow::Result<()> {
    let unfunded: Vec<&Sender> = {
        let mut unfunded = Vec::new();
        for sender in senders {
            let balance = sender
                .provider
                .get_balance(sender.address)
                .await
                .context("is the cluster up? cannot reach a validator RPC")?;
            if balance == U256::ZERO {
                unfunded.push(sender);
            }
        }
        unfunded
    };
    if unfunded.is_empty() {
        println!("all senders already funded");
        return Ok(());
    }
    println!("funding {} senders via L1 deposits", unfunded.len());

    let rich: PrivateKeySigner = ANVIL_RICH_KEY.parse()?;
    let l1 = ProviderBuilder::new()
        .wallet(EthereumWallet::from(rich))
        .connect_http(format!("http://127.0.0.1:{}", manifest.host_l1_port).parse()?)
        .erased();
    let chain_id = senders[0].provider.get_chain_id().await?;
    let bridgehub_address: Address = manifest.bridgehub_address.parse()?;
    let bridgehub = Bridgehub::new(bridgehub_address, l1.clone(), chain_id);

    let amount = U256::from(FUNDING_ETH) * U256::from(10u128.pow(18));
    let l1_gas_price = l1.get_gas_price().await?;
    for sender in &unfunded {
        let base_cost = bridgehub
            .l2_transaction_base_cost(
                l1_gas_price.saturating_mul(2),
                TRANSFER_GAS,
                REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            )
            .await?;
        let receipt = l1
            .send_transaction(
                bridgehub
                    .request_l2_transaction_direct(
                        amount + base_cost,
                        sender.address,
                        amount,
                        vec![],
                        TRANSFER_GAS,
                        REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                        sender.address,
                    )
                    .value(amount + base_cost)
                    .into_transaction_request(),
            )
            .await?
            .get_receipt()
            .await?;
        anyhow::ensure!(
            receipt
                .logs()
                .iter()
                .any(|log| log.log_decode::<NewPriorityRequest>().is_ok()),
            "deposit for {} produced no priority request",
            sender.address,
        );
    }

    // The L1 watcher relays deposits into L2 blocks; wait for the balances.
    let deadline = Instant::now() + Duration::from_secs(180);
    for sender in &unfunded {
        loop {
            let balance = sender
                .provider
                .get_balance(sender.address)
                .await
                .unwrap_or(U256::ZERO);
            if balance > U256::ZERO {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "sender {} still unfunded after deposit",
                sender.address,
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    println!("all senders funded");
    Ok(())
}

/// Prints per-validator stats and proves end-to-end inclusion by waiting for the
/// last accepted transaction of each sender.
async fn report(senders: &[Sender], stats: &[ValidatorStats], elapsed: Duration) {
    let mut included = 0usize;
    let mut missing = 0usize;
    for sender in senders {
        let Some(hash) = sender.last_hash else {
            continue;
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match sender.provider.get_transaction_receipt(hash).await {
                Ok(Some(_)) => {
                    included += 1;
                    break;
                }
                _ if Instant::now() >= deadline => {
                    missing += 1;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    let accepted: u64 = stats.iter().map(|stat| stat.accepted).sum();
    let rejected: u64 = stats.iter().map(|stat| stat.rejected).sum();
    println!("--- load report ({:.0}s) ---", elapsed.as_secs_f64());
    for (index, stat) in stats.iter().enumerate() {
        if stat.accepted + stat.rejected > 0 {
            println!(
                "validator {index}: accepted {} rejected {}",
                stat.accepted, stat.rejected
            );
        }
    }
    println!(
        "total accepted {accepted} ({:.1} tps), rejected {rejected}; \
         final txs included {included}, unconfirmed {missing}",
        accepted as f64 / elapsed.as_secs_f64().max(1.0),
    );
}
