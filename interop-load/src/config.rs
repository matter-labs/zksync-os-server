use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::Serialize;
use zeroize::Zeroizing;

const ASSUMED_P95_PIPELINE_SECS: f64 = 10.0;
const WALLET_SAFETY_FACTOR: f64 = 2.0;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RateMode {
    OpenLoop,
    ClosedLoop,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Resilience {
    None,
    GatewayKill,
}

#[derive(Debug, Parser)]
#[command(author, version, about = "Interop gateway throughput load harness")]
pub struct Args {
    #[arg(long)]
    pub chain_a_rpc: String,
    #[arg(long)]
    pub chain_b_rpc: String,
    /// Optional repeated source RPC list. If provided, the scheduler stripes
    /// aggregate --rate across these source chains, all targeting --chain-b-rpc.
    /// The first source is treated as chain A for ERC20 setup; extra sources
    /// send Base + Message only.
    #[arg(long)]
    pub source_rpc: Vec<String>,
    #[arg(long)]
    pub gateway_rpc: String,
    #[arg(long)]
    pub l1_rpc: String,
    #[arg(long)]
    pub rich_privkey: String,

    #[arg(long)]
    pub duration: humantime::Duration,
    #[arg(long)]
    pub rate: f64,
    #[arg(long)]
    pub wallets: usize,
    #[arg(long)]
    pub seed: Option<u64>,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub strict_wallet_sizing: bool,

    #[arg(long, value_enum, default_value_t = RateMode::OpenLoop)]
    pub rate_mode: RateMode,
    #[arg(long)]
    pub max_in_flight: Option<usize>,
    /// Max concurrent proof/root polling RPC calls across propagation tails.
    #[arg(long, default_value_t = 64)]
    pub proof_rpc_window: usize,

    #[arg(long, default_value_t = 32)]
    pub payload_bytes: usize,
    #[arg(long, default_value = "1000000000000000000")]
    pub wallet_fund_wei: String,

    #[arg(long, default_value = "30s")]
    pub warmup: humantime::Duration,

    #[arg(long)]
    pub output_dir: PathBuf,
    #[arg(long)]
    pub setup: PathBuf,

    #[arg(long, default_value_t = false)]
    pub skip_smoke_test: bool,

    /// Skip wallet ETH funding and ERC20 seeding/approval. Use on the second
    /// and later runs of a ramp: the derived wallets keep their balances from
    /// the prior run since the chain state persists.
    #[arg(long, default_value_t = false)]
    pub skip_funding: bool,

    /// Symmetric A↔B mode: chain B also acts as a source, sending bundles
    /// back to chain A. Round-robin scheduling alternates between A→B and
    /// B→A. Chain B's source mix omits the ERC20 shape (we only seeded an
    /// ERC20 on chain A) — B sends Base + Message only. The aggregate
    /// `--rate` is split across both lanes.
    #[arg(long, default_value_t = false)]
    pub symmetric: bool,

    /// Ring mode for explicit multi-source runs. Each `--source-rpc` sends to
    /// the next source RPC in order, and the last source sends to the first.
    /// For example, five source RPCs form five lanes: 0→1→2→3→4→0.
    #[arg(long, default_value_t = false)]
    pub ring: bool,

    /// Run a short deterministic source-side pubdata probe instead of the
    /// throughput scheduler. The probe emits one simple transfer and one
    /// bundle for each interop shape, then records tx hashes for log joining.
    #[arg(long, default_value_t = false)]
    pub pubdata_probe: bool,

    #[arg(long)]
    pub metrics_url: Vec<String>,

    #[arg(long, value_enum, default_value_t = Resilience::None)]
    pub resilience: Resilience,
    #[arg(long)]
    pub gateway_kill_binary: Option<PathBuf>,
}

#[derive(Clone, Serialize)]
pub struct Config {
    pub chain_a_rpc: String,
    pub chain_b_rpc: String,
    pub source_rpc: Vec<String>,
    pub gateway_rpc: String,
    pub l1_rpc: String,
    #[serde(skip_serializing)]
    pub rich_privkey: SecretString,
    pub duration_ms: u64,
    pub rate: f64,
    pub wallets: usize,
    pub seed: u64,
    pub strict_wallet_sizing: bool,
    pub rate_mode: RateMode,
    pub max_in_flight: usize,
    pub proof_rpc_window: usize,
    pub payload_bytes: usize,
    pub wallet_fund_wei: String,
    pub warmup_ms: u64,
    pub output_dir: PathBuf,
    pub setup: PathBuf,
    pub skip_smoke_test: bool,
    pub skip_funding: bool,
    pub symmetric: bool,
    pub ring: bool,
    pub pubdata_probe: bool,
    pub metrics_url: Vec<String>,
    pub resilience: Resilience,
    pub gateway_kill_binary: Option<PathBuf>,
}

impl Config {
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        anyhow::ensure!(args.rate > 0.0, "--rate must be positive");
        anyhow::ensure!(args.wallets > 0, "--wallets must be positive");
        anyhow::ensure!(
            args.proof_rpc_window > 0,
            "--proof-rpc-window must be positive"
        );
        anyhow::ensure!(args.payload_bytes > 0, "--payload-bytes must be positive");
        if matches!(args.resilience, Resilience::GatewayKill) {
            anyhow::ensure!(
                args.gateway_kill_binary.is_some(),
                "--gateway-kill-binary is required with --resilience gateway-kill"
            );
        }
        if !args.source_rpc.is_empty() {
            anyhow::ensure!(
                !args.symmetric,
                "--symmetric cannot be combined with --source-rpc; pass explicit sources for multi-source mode"
            );
            if !args.ring {
                for source_rpc in &args.source_rpc {
                    anyhow::ensure!(
                        source_rpc != &args.chain_b_rpc,
                        "--source-rpc must not include --chain-b-rpc ({source_rpc})"
                    );
                }
            }
        }
        if args.ring {
            anyhow::ensure!(
                !args.symmetric,
                "--ring cannot be combined with --symmetric"
            );
            anyhow::ensure!(
                args.source_rpc.len() >= 2,
                "--ring requires at least two --source-rpc entries"
            );
        }

        let max_in_flight = args
            .max_in_flight
            .unwrap_or_else(|| default_in_flight(args.rate));
        let seed = args.seed.unwrap_or_else(rand::random);
        let warmup = *args.warmup;
        let duration = *args.duration;
        anyhow::ensure!(duration > Duration::ZERO, "--duration must be positive");
        anyhow::ensure!(warmup <= duration, "--warmup cannot exceed --duration");

        Ok(Self {
            chain_a_rpc: args.chain_a_rpc,
            chain_b_rpc: args.chain_b_rpc,
            source_rpc: args.source_rpc,
            gateway_rpc: args.gateway_rpc,
            l1_rpc: args.l1_rpc,
            rich_privkey: SecretString::new(args.rich_privkey),
            duration_ms: duration.as_millis() as u64,
            rate: args.rate,
            wallets: args.wallets,
            seed,
            strict_wallet_sizing: args.strict_wallet_sizing,
            rate_mode: args.rate_mode,
            max_in_flight,
            proof_rpc_window: args.proof_rpc_window,
            payload_bytes: args.payload_bytes,
            wallet_fund_wei: args.wallet_fund_wei,
            warmup_ms: warmup.as_millis() as u64,
            output_dir: args.output_dir,
            setup: args.setup,
            skip_smoke_test: args.skip_smoke_test,
            skip_funding: args.skip_funding,
            symmetric: args.symmetric,
            ring: args.ring,
            pubdata_probe: args.pubdata_probe,
            metrics_url: args.metrics_url,
            resilience: args.resilience,
            gateway_kill_binary: args.gateway_kill_binary,
        })
    }

    pub fn required_wallets(&self) -> usize {
        required_wallets(self.rate)
    }

    pub fn source_rpcs(&self) -> Vec<String> {
        if self.source_rpc.is_empty() {
            vec![self.chain_a_rpc.clone()]
        } else {
            self.source_rpc.clone()
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("chain_a_rpc", &self.chain_a_rpc)
            .field("chain_b_rpc", &self.chain_b_rpc)
            .field("source_rpc", &self.source_rpc)
            .field("gateway_rpc", &self.gateway_rpc)
            .field("l1_rpc", &self.l1_rpc)
            .field("rich_privkey", &"<redacted>")
            .field("duration_ms", &self.duration_ms)
            .field("rate", &self.rate)
            .field("wallets", &self.wallets)
            .field("seed", &self.seed)
            .field("strict_wallet_sizing", &self.strict_wallet_sizing)
            .field("rate_mode", &self.rate_mode)
            .field("max_in_flight", &self.max_in_flight)
            .field("proof_rpc_window", &self.proof_rpc_window)
            .field("payload_bytes", &self.payload_bytes)
            .field("wallet_fund_wei", &self.wallet_fund_wei)
            .field("warmup_ms", &self.warmup_ms)
            .field("output_dir", &self.output_dir)
            .field("setup", &self.setup)
            .field("skip_funding", &self.skip_funding)
            .field("symmetric", &self.symmetric)
            .field("ring", &self.ring)
            .field("pubdata_probe", &self.pubdata_probe)
            .field("skip_smoke_test", &self.skip_smoke_test)
            .field("metrics_url", &self.metrics_url)
            .field("resilience", &self.resilience)
            .field("gateway_kill_binary", &self.gateway_kill_binary)
            .finish()
    }
}

#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

fn default_in_flight(rate: f64) -> usize {
    (rate * 4.0).ceil().max(1.0) as usize
}

pub fn required_wallets(rate: f64) -> usize {
    (rate * ASSUMED_P95_PIPELINE_SECS * WALLET_SAFETY_FACTOR)
        .ceil()
        .max(1.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_wallets_uses_pipeline_assumption_and_safety_factor() {
        assert_eq!(required_wallets(1.0), 20);
        assert_eq!(required_wallets(10.0), 200);
        assert_eq!(required_wallets(0.1), 2);
    }

    #[test]
    fn config_debug_redacts_private_key() {
        let mut args = Args::parse_from([
            "interop-load",
            "--chain-a-rpc",
            "http://a",
            "--chain-b-rpc",
            "http://b",
            "--gateway-rpc",
            "http://g",
            "--l1-rpc",
            "http://l1",
            "--rich-privkey",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "--duration",
            "60s",
            "--rate",
            "1",
            "--wallets",
            "20",
            "--output-dir",
            "/tmp/interop-load-test",
            "--setup",
            "/tmp/interop-load-setup.json",
        ]);
        args.seed = Some(1);

        let config = Config::from_args(args).unwrap();
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("11111111111111111111111111111111"));
    }

    #[test]
    fn config_rejects_destination_as_explicit_source() {
        let args = Args::parse_from([
            "interop-load",
            "--chain-a-rpc",
            "http://a",
            "--chain-b-rpc",
            "http://b",
            "--source-rpc",
            "http://a",
            "--source-rpc",
            "http://b",
            "--gateway-rpc",
            "http://g",
            "--l1-rpc",
            "http://l1",
            "--rich-privkey",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "--duration",
            "60s",
            "--rate",
            "1",
            "--wallets",
            "20",
            "--output-dir",
            "/tmp/interop-load-test",
            "--setup",
            "/tmp/interop-load-setup.json",
        ]);

        let err = Config::from_args(args).unwrap_err().to_string();
        assert!(err.contains("--source-rpc must not include --chain-b-rpc"));
    }

    #[test]
    fn config_rejects_symmetric_with_explicit_sources() {
        let args = Args::parse_from([
            "interop-load",
            "--chain-a-rpc",
            "http://a",
            "--chain-b-rpc",
            "http://b",
            "--source-rpc",
            "http://a",
            "--symmetric",
            "--gateway-rpc",
            "http://g",
            "--l1-rpc",
            "http://l1",
            "--rich-privkey",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "--duration",
            "60s",
            "--rate",
            "1",
            "--wallets",
            "20",
            "--output-dir",
            "/tmp/interop-load-test",
            "--setup",
            "/tmp/interop-load-setup.json",
        ]);

        let err = Config::from_args(args).unwrap_err().to_string();
        assert!(err.contains("--symmetric cannot be combined with --source-rpc"));
    }

    #[test]
    fn config_allows_destination_as_source_in_ring_mode() {
        let args = Args::parse_from([
            "interop-load",
            "--chain-a-rpc",
            "http://a",
            "--chain-b-rpc",
            "http://b",
            "--source-rpc",
            "http://a",
            "--source-rpc",
            "http://b",
            "--ring",
            "--gateway-rpc",
            "http://g",
            "--l1-rpc",
            "http://l1",
            "--rich-privkey",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "--duration",
            "60s",
            "--rate",
            "1",
            "--wallets",
            "20",
            "--output-dir",
            "/tmp/interop-load-test",
            "--setup",
            "/tmp/interop-load-setup.json",
        ]);

        let config = Config::from_args(args).unwrap();
        assert!(config.ring);
    }
}
