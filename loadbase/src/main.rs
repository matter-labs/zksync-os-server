mod erc20;
mod erc20_worker;
mod gas;
mod metrics;
mod output;
mod wallets;

use clap::{Parser, ValueEnum};
use erc20::{distribute_varied, SimpleERC20};
use erc20_worker::{spawn_erc20_workers, WorkerConfig};
use ethers::{middleware::NonceManagerMiddleware, prelude::*, types::U256};
use gas::{resolve, GasMode};
use metrics::Metrics;
use output::{OutputMode, RunMetadata};
use parking_lot::RwLock;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, ValueEnum)]
enum DestMode {
    Wallet,
    Random,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Csv,
    All,
}

impl From<OutputFormat> for OutputMode {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Text => OutputMode::Text,
            OutputFormat::Json => OutputMode::Json,
            OutputFormat::Csv => OutputMode::Csv,
            OutputFormat::All => OutputMode::All,
        }
    }
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long)]
    rpc_url: String,
    #[arg(long)]
    rich_privkey: String,
    #[arg(
        long,
        default_value = "legal winner thank year wave sausage worth useful legal winner thank yellow"
    )]
    mnemonic: String,
    #[arg(long, default_value_t = 10)]
    wallets: u32,
    #[arg(long)]
    duration: humantime::Duration,
    #[arg(long, default_value_t = 10)]
    max_in_flight: u32,
    #[arg(long, default_value = "100000000000000")]
    amount_fund: String,
    #[arg(long)]
    estimate_gas: bool,
    #[arg(long, value_enum, default_value_t = DestMode::Wallet)]
    dest: DestMode,
    #[arg(long)]
    erc20_address: Option<String>,
    #[arg(long, default_value = "TestToken")]
    erc20_name: String,
    #[arg(long, default_value = "TTK")]
    erc20_symbol: String,
    #[arg(long)]
    output_dir: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //-------------------------------- env ----------------------------------//
    let args = Args::parse();
    let provider = Provider::<Http>::try_from(&args.rpc_url)?.interval(Duration::from_millis(100));
    let chain_id = provider.get_chainid().await?.as_u64();

    //-------------------------------- signers ------------------------------//
    let rich_wallet: LocalWallet = args
        .rich_privkey
        .parse::<LocalWallet>()?
        .with_chain_id(chain_id);
    let rich_std_arc = Arc::new(SignerMiddleware::new(provider.clone(), rich_wallet.clone()));
    let rich_nm_arc = Arc::new(SignerMiddleware::new(
        NonceManagerMiddleware::new(provider.clone(), rich_wallet.address()),
        rich_wallet.clone(),
    ));

    //-------------------------------- wallets + ETH prefund ---------------//
    let wallets = wallets::derive(&args.mnemonic, args.wallets, chain_id)?;
    let addrs: Vec<_> = wallets.iter().map(|w| w.address()).collect();

    let base_eth: U256 = args.amount_fund.parse()?;
    let mut rng = StdRng::from_entropy();
    let eth_amounts: Vec<U256> = (0..addrs.len())
        .map(|_| U256::from(((base_eth.as_u128() as f64) * rng.gen_range(1.5..3.5)) as u128))
        .collect();
    wallets::prefund_varied(&*rich_std_arc, &addrs, &eth_amounts).await?;

    //-------------------------------- transfer size ------------------------//
    let mut list: Vec<u128> = eth_amounts.iter().map(|x| x.as_u128()).collect();
    list.sort_unstable();
    let mean_transfer = U256::from(list[list.len() / 2] / 2); // 50 %

    //-------------------------------- ERC-20 deploy/distribute -------------//
    let supply = U256::from_dec_str("1000000000000000000000000000000")?; // 1e6 tokens
    let per_wallet = supply / U256::from(args.wallets);
    let token_amounts = vec![per_wallet; args.wallets as usize];

    let token_std: SimpleERC20<_> = match &args.erc20_address {
        Some(hex) => SimpleERC20::new(hex.parse::<Address>()?, rich_std_arc.clone()),
        None => {
            erc20::deploy_and_mint(
                rich_std_arc.clone(),
                &args.erc20_name,
                &args.erc20_symbol,
                supply,
            )
            .await?
        }
    };
    println!("ERC-20 at {}\n", token_std.address());

    let token_nm = SimpleERC20::new(token_std.address(), rich_nm_arc.clone());
    distribute_varied(&token_nm, &addrs, &token_amounts).await?;

    //-------------------------------- metrics & workers --------------------//
    let metrics = Metrics::new()?;
    metrics.spawn_reporter(Instant::now()); // start *after* distribution

    let running = Arc::new(AtomicBool::new(true));
    let dest_rand = matches!(args.dest, DestMode::Random);

    let gas = resolve(
        &provider,
        rich_wallet.address(),
        U256::zero(),
        if args.estimate_gas {
            GasMode::EstimateOnce
        } else {
            GasMode::Fixed(U256::from(120_000))
        },
    )
    .await?;

    let cfg = WorkerConfig {
        gas_limit: gas,
        mean_amt: mean_transfer,
        token_addr: token_nm.address(),
        dest_random: dest_rand,
        rpc_url: args.rpc_url.clone(),
        all_addrs: wallets.iter().map(|w| w.address()).collect(),
        rng: Arc::new(RwLock::new(StdRng::from_entropy())),
    };
    spawn_erc20_workers(
        provider.clone(),
        wallets.clone(),
        metrics.clone(),
        running.clone(),
        args.max_in_flight,
        cfg,
    );
    println!(
        "▶ ERC-20 test started with {} wallets, gas: {}",
        args.wallets, gas
    );

    //-------------------------------- run ----------------------------------//
    tokio::time::sleep(*args.duration).await;
    running.store(false, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(1)).await;

    if let Some(output_dir) = &args.output_dir {
        let (receipt_timeouts, receipt_errors) = metrics.receipt_outcomes();
        let metadata = RunMetadata {
            chain_id,
            wallets: args.wallets,
            max_in_flight: args.max_in_flight,
            duration_s: args.duration.as_secs(),
            destination_mode: match args.dest {
                DestMode::Wallet => "wallet".to_owned(),
                DestMode::Random => "random".to_owned(),
            },
            rpc_url: args.rpc_url.clone(),
            receipt_timeouts,
            receipt_errors,
        };
        let samples = metrics.samples();
        output::write_outputs(output_dir, args.output_format.into(), &metadata, &samples)?;
        println!("▶ Wrote benchmark artifacts to {}", output_dir.display());
    }

    println!("▶ Test finished");
    Ok(())
}
