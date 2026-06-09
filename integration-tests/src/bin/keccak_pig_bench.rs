use anyhow::Context;
use tracing_subscriber::EnvFilter;
use zksync_os_integration_tests::keccak_pig_bench::run_keccak_burner_bench;
use zksync_os_server::default_protocol_version::{PROTOCOL_VERSION_V31_0, PROTOCOL_VERSION_V32_1};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,node=info,zksync_os_server=info")),
        )
        .with_target(false)
        .try_init();

    let arg = std::env::args().nth(1).context(
        "usage: cargo run -p zksync_os_integration_tests --bin keccak_pig_bench -- <v31|v32>",
    )?;

    let (protocol_version, bench_label) = match arg.as_str() {
        "v31" | "v31.0" => (PROTOCOL_VERSION_V31_0, "v31"),
        "v32" | "v32.1" => (PROTOCOL_VERSION_V32_1, "v32"),
        other => anyhow::bail!("unknown bench target `{other}`; expected `v31` or `v32`"),
    };

    let result = run_keccak_burner_bench(protocol_version, bench_label).await?;
    println!("bench_label={}", result.bench_label);
    println!("protocol_version={}", result.protocol_version);
    println!("iterations={}", result.iterations);
    println!("warmup_gas_used={}", result.warmup_gas_used);
    println!("per_tx_gas_limit={}", result.per_tx_gas_limit);
    println!("block_gas_limit={}", result.block_gas_limit);
    println!("full_blocks_per_batch={}", result.full_blocks_per_batch);
    println!("target_batch_gas={}", result.target_batch_gas);
    println!("actual_batch_gas={}", result.actual_batch_gas);
    println!("total_receipt_gas_used={}", result.total_receipt_gas_used);
    println!("txs_to_fill_one_block={}", result.txs_to_fill_one_block);
    println!("txs_in_first_block={}", result.txs_in_first_block);
    println!("total_stress_txs={}", result.total_stress_txs);
    println!("txs_observed_in_batch={}", result.txs_observed_in_batch);
    println!("unique_blocks_in_batch={}", result.unique_blocks_in_batch);
    println!("first_stress_block={}", result.first_stress_block);
    println!("last_stress_block={}", result.last_stress_block);
    println!("first_batch={}", result.first_batch);
    println!("last_batch={}", result.last_batch);
    println!("batch_count={}", result.batch_count);
    println!(
        "batch_computational_native_used={}",
        result.batch_computational_native_used
    );
    println!("batch_pig_mode={}", result.batch_pig_mode);
    println!("block_pig_ms={}", result.block_pig_ms);
    println!("batch_pig_ms={}", result.batch_pig_ms);
    println!("total_pig_ms={}", result.total_pig_ms);
    println!(
        "batch_pig_ms_per_million_native={:.6}",
        result.batch_pig_ms_per_million_native
    );
    println!(
        "total_pig_ms_per_million_native={:.6}",
        result.total_pig_ms_per_million_native
    );
    println!(
        "batch_pig_prover_input_words={}",
        result.batch_pig_prover_input_words
    );
    println!("env_prepare_ms={}", result.env_prepare_ms);
    println!("initial_launch_ms={}", result.initial_launch_ms);
    println!("catchup_ms={}", result.catchup_ms);
    println!("restart_ms={}", result.restart_ms);
    println!("deploy_ms={}", result.deploy_ms);
    println!("warmup_ms={}", result.warmup_ms);
    println!("submit_ms={}", result.submit_ms);
    println!("receipts_ms={}", result.receipts_ms);
    println!("batch_lookup_ms={}", result.batch_lookup_ms);
    println!("block_fetch_ms={}", result.block_fetch_ms);
    println!("total_ms={}", result.total_ms);
    Ok(())
}
