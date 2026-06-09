use crate::assert_traits::ReceiptAssert;
use crate::contracts::KeccakBurner;
use crate::{SettlementLayer, TestCase};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::Context;
use futures::future::join_all;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::{Duration, Instant};
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_server::default_protocol_version::PROTOCOL_VERSION_V31_0;
use zksync_os_server::pig_telemetry::{
    BatchPigMode, clear_batch_pig_telemetry, clear_block_pig_telemetry, take_batch_pig_telemetry,
    take_block_pig_telemetry,
};

const DEFAULT_BLOCK_TIME_MS: u64 = 10_000;
const DEFAULT_BLOCK_GAS_LIMIT: u64 = 100_000_000;
const DEFAULT_KECCAK_ITERATIONS: u64 = 50_000;
const DEFAULT_OVERSUBSCRIBE_TXS: usize = 2;
const DEFAULT_FULL_BLOCKS_PER_BATCH: usize = 4;
const DEFAULT_BATCH_TIMEOUT_SECS: u64 = 180;
const DEFAULT_MAX_TRANSACTIONS_IN_BLOCK: usize = 10_000;
const DEFAULT_TXS_PER_BATCH_LIMIT: u64 = 100_000;
const V31_BRIDGEHUB_ADDRESS: &str = "0x2884bff314c1b8f4ae42966beedf0350cca05886";
const V31_BYTECODE_SUPPLIER_ADDRESS: &str = "0x4ee23e9eaf0310f19b3fd38118a7419da5accd45";

#[derive(Debug, Clone)]
pub struct KeccakPigBenchResult {
    pub bench_label: String,
    pub protocol_version: String,
    pub iterations: u64,
    pub warmup_gas_used: u64,
    pub per_tx_gas_limit: u64,
    pub block_gas_limit: u64,
    pub full_blocks_per_batch: usize,
    pub target_batch_gas: u128,
    pub actual_batch_gas: u128,
    pub total_receipt_gas_used: u128,
    pub txs_to_fill_one_block: usize,
    pub txs_in_first_block: usize,
    pub total_stress_txs: usize,
    pub txs_observed_in_batch: usize,
    pub unique_blocks_in_batch: usize,
    pub first_stress_block: u64,
    pub last_stress_block: u64,
    pub first_batch: u64,
    pub last_batch: u64,
    pub batch_count: usize,
    pub batch_computational_native_used: u64,
    pub batch_pig_mode: String,
    pub block_pig_ms: u128,
    pub batch_pig_ms: u128,
    pub total_pig_ms: u128,
    pub batch_pig_ms_per_million_native: f64,
    pub total_pig_ms_per_million_native: f64,
    pub batch_pig_prover_input_words: usize,
    pub env_prepare_ms: u128,
    pub initial_launch_ms: u128,
    pub catchup_ms: u128,
    pub restart_ms: u128,
    pub deploy_ms: u128,
    pub warmup_ms: u128,
    pub submit_ms: u128,
    pub receipts_ms: u128,
    pub batch_lookup_ms: u128,
    pub block_fetch_ms: u128,
    pub total_ms: u128,
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn burn_calldata(iterations: u64, seed: B256) -> Vec<u8> {
    KeccakBurner::burnCall {
        iterations: U256::from(iterations),
        seed,
    }
    .abi_encode()
}

fn seed_for_tx(index: usize) -> B256 {
    B256::from(U256::from(index + 1))
}

fn patch_direct_v31_l1_config(config: &mut zksync_os_server::config::Config) -> anyhow::Result<()> {
    if config.genesis_config.chain_id == Some(6565) {
        config.genesis_config.bridgehub_address = Some(Address::from_str(V31_BRIDGEHUB_ADDRESS)?);
        config.genesis_config.bytecode_supplier_address =
            Some(Address::from_str(V31_BYTECODE_SUPPLIER_ADDRESS)?);
    }
    Ok(())
}

pub async fn run_keccak_burner_bench(
    protocol_version: &'static str,
    bench_label: &'static str,
) -> anyhow::Result<KeccakPigBenchResult> {
    let total_started_at = Instant::now();
    let block_time_ms = env_u64("ZKOS_KECCAK_BENCH_BLOCK_TIME_MS", DEFAULT_BLOCK_TIME_MS);
    let block_gas_limit = env_u64("ZKOS_KECCAK_BENCH_BLOCK_GAS_LIMIT", DEFAULT_BLOCK_GAS_LIMIT);
    let iterations = env_u64("ZKOS_KECCAK_BENCH_ITERATIONS", DEFAULT_KECCAK_ITERATIONS);
    let oversubscribe_txs = env_usize(
        "ZKOS_KECCAK_BENCH_OVERSUBSCRIBE_TXS",
        DEFAULT_OVERSUBSCRIBE_TXS,
    );
    let full_blocks_per_batch = env_usize(
        "ZKOS_KECCAK_BENCH_FULL_BLOCKS_PER_BATCH",
        DEFAULT_FULL_BLOCKS_PER_BATCH,
    );
    let batch_timeout_secs = env_u64(
        "ZKOS_KECCAK_BENCH_BATCH_TIMEOUT_SECS",
        DEFAULT_BATCH_TIMEOUT_SECS,
    );

    let settlement_layer = if protocol_version == PROTOCOL_VERSION_V31_0 {
        SettlementLayer::Gateway
    } else {
        SettlementLayer::L1
    };
    let env_started_at = Instant::now();
    let env = TestCase {
        protocol_version,
        settlement_layer,
    }
    .environment()
    .await?;
    let env_prepare_elapsed = env_started_at.elapsed();

    let launch_started_at = Instant::now();
    let mut initial_config = env.default_config().await?;
    if protocol_version == PROTOCOL_VERSION_V31_0 {
        patch_direct_v31_l1_config(&mut initial_config)?;
    }
    let tester = env.launch(initial_config).await?;
    let initial_launch_elapsed = launch_started_at.elapsed();
    let latest_block_before_restart = tester.l2_provider.get_block_number().await?;

    let catchup_started_at = Instant::now();
    let latest_batch_before_restart = tester
        .l2_zk_provider
        .wait_batch_number_by_block_number(latest_block_before_restart)
        .await?;
    let catchup_elapsed = catchup_started_at.elapsed();

    let mut config = tester.config().clone();
    config.sequencer_config.block_time = Duration::from_millis(block_time_ms);
    config.sequencer_config.block_gas_limit = block_gas_limit;
    config.sequencer_config.max_transactions_in_block = DEFAULT_MAX_TRANSACTIONS_IN_BLOCK;
    config.batcher_config.batch_timeout = Duration::from_secs(batch_timeout_secs);
    config.batcher_config.tx_per_batch_limit = DEFAULT_TXS_PER_BATCH_LIMIT;

    let restart_started_at = Instant::now();
    let tester = tester.restart_with_config(config).await?;
    tester.wait_for_initial_deposit().await?;
    let restart_elapsed = restart_started_at.elapsed();

    let deploy_started_at = Instant::now();
    let burner = KeccakBurner::deploy(tester.l2_provider.clone()).await?;
    let deploy_elapsed = deploy_started_at.elapsed();
    let sender = tester.l2_wallet.default_signer().address();
    let sender_balance_before_warmup = tester.l2_provider.get_balance(sender).await?;

    let warmup_started_at = Instant::now();
    let warmup_receipt = burner
        .burn(U256::from(iterations), seed_for_tx(0))
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let warmup_elapsed = warmup_started_at.elapsed();
    let warmup_gas_used = u64::try_from(warmup_receipt.gas_used)
        .context("warm-up receipt gas used does not fit into u64")?;
    anyhow::ensure!(
        warmup_gas_used < block_gas_limit,
        "single keccak-burn tx used {warmup_gas_used} gas, which exceeds configured block gas limit {block_gas_limit}; lower iterations or raise ZKOS_KECCAK_BENCH_BLOCK_GAS_LIMIT"
    );

    let txs_to_fill_one_block = usize::try_from(block_gas_limit / warmup_gas_used)
        .context("block gas limit / warm-up gas used does not fit into usize")?;
    anyhow::ensure!(
        txs_to_fill_one_block > 0,
        "warm-up tx consumed too much gas to calibrate a block-filling run"
    );

    let total_stress_txs = txs_to_fill_one_block * full_blocks_per_batch + oversubscribe_txs;
    let target_batch_gas = u128::from(block_gas_limit) * full_blocks_per_batch as u128;
    let estimated_total_tx_gas = u128::from(warmup_gas_used) * total_stress_txs as u128;
    let per_tx_gas_limit = warmup_gas_used + warmup_gas_used / 4 + 50_000;
    let first_nonce = tester
        .l2_provider
        .get_transaction_count(sender)
        .pending()
        .await?;

    tracing::info!(
        bench_label,
        protocol_version,
        block_time_ms,
        block_gas_limit,
        batch_timeout_secs,
        iterations,
        warmup_gas_used,
        per_tx_gas_limit,
        full_blocks_per_batch,
        txs_to_fill_one_block,
        total_stress_txs,
        target_batch_gas,
        estimated_total_tx_gas,
        latest_block_before_restart,
        latest_batch_before_restart,
        env_prepare_ms = env_prepare_elapsed.as_millis(),
        initial_launch_ms = initial_launch_elapsed.as_millis(),
        catchup_ms = catchup_elapsed.as_millis(),
        restart_ms = restart_elapsed.as_millis(),
        deploy_ms = deploy_elapsed.as_millis(),
        sender_balance_before_warmup = %sender_balance_before_warmup,
        warmup_ms = warmup_elapsed.as_millis(),
        "Starting keccak burner stress run"
    );

    clear_batch_pig_telemetry();
    clear_block_pig_telemetry();

    let submit_started_at = Instant::now();
    let mut pending_txs = Vec::with_capacity(total_stress_txs);
    for index in 0..total_stress_txs {
        let tx = TransactionRequest::default()
            .with_from(sender)
            .with_to(*burner.address())
            .with_input(burn_calldata(iterations, seed_for_tx(index + 1)))
            .with_nonce(first_nonce + index as u64)
            .with_gas_limit(per_tx_gas_limit);
        pending_txs.push(tester.l2_provider.send_transaction(tx).await?);
    }
    let submit_elapsed = submit_started_at.elapsed();

    let receipts_started_at = Instant::now();
    let receipts = join_all(
        pending_txs
            .into_iter()
            .map(|pending_tx| pending_tx.expect_successful_receipt()),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    let receipts_elapsed = receipts_started_at.elapsed();

    let first_stress_block = receipts
        .first()
        .and_then(|receipt| receipt.block_number)
        .context("first stress receipt is missing block number")?;
    let last_stress_block = receipts
        .last()
        .and_then(|receipt| receipt.block_number)
        .context("last stress receipt is missing block number")?;

    let unique_block_numbers: BTreeSet<u64> = receipts
        .iter()
        .map(|receipt| {
            receipt
                .block_number
                .context("stress receipt is missing block number")
        })
        .collect::<Result<_, _>>()?;
    let txs_in_first_block = receipts
        .iter()
        .take_while(|receipt| receipt.block_number == Some(first_stress_block))
        .count();
    let total_receipt_gas_used: u128 = receipts
        .iter()
        .map(|receipt| u128::from(receipt.gas_used))
        .sum();

    anyhow::ensure!(
        unique_block_numbers.len() >= full_blocks_per_batch,
        "stress workload only filled {} block(s), expected at least {full_blocks_per_batch}; increase ZKOS_KECCAK_BENCH_ITERATIONS or lower ZKOS_KECCAK_BENCH_BLOCK_GAS_LIMIT",
        unique_block_numbers.len(),
    );
    anyhow::ensure!(
        txs_in_first_block >= txs_to_fill_one_block.saturating_sub(1),
        "expected first stress block to be compute-saturated, but only packed {txs_in_first_block} txs vs calibrated target {txs_to_fill_one_block}"
    );

    let target_chain_id = tester
        .config()
        .genesis_config
        .chain_id
        .context("test config is missing genesis chain id")?;
    let batch_lookup_started_at = Instant::now();
    let telemetry_wait_timeout = Duration::from_secs(batch_timeout_secs.saturating_add(120));
    let telemetry_wait_started_at = Instant::now();
    let mut batch_pig_telemetry = Vec::new();
    let mut block_pig_telemetry = Vec::new();
    let stress_block_count = usize::try_from(last_stress_block - first_stress_block + 1)
        .context("stress block span does not fit into usize")?;
    let mut matching_batches = Vec::new();
    let mut covered_stress_blocks = BTreeSet::new();
    loop {
        batch_pig_telemetry.extend(take_batch_pig_telemetry());
        block_pig_telemetry.extend(take_block_pig_telemetry());

        let mut index = 0;
        while index < batch_pig_telemetry.len() {
            let telemetry = &batch_pig_telemetry[index];
            let overlaps_stress_range = telemetry.chain_id == target_chain_id
                && telemetry.first_block_number <= last_stress_block
                && telemetry.last_block_number >= first_stress_block;
            if overlaps_stress_range {
                let telemetry = batch_pig_telemetry.remove(index);
                let covered_from = telemetry.first_block_number.max(first_stress_block);
                let covered_to = telemetry.last_block_number.min(last_stress_block);
                for block_number in covered_from..=covered_to {
                    covered_stress_blocks.insert(block_number);
                }
                matching_batches.push(telemetry);
            } else {
                index += 1;
            }
        }

        if covered_stress_blocks.len() == stress_block_count {
            break;
        }
        anyhow::ensure!(
            telemetry_wait_started_at.elapsed() <= telemetry_wait_timeout,
            "did not capture batch PIG telemetry for chain_id={target_chain_id}, block_range={first_stress_block}..={last_stress_block}; covered blocks: {covered_stress_blocks:?}; saw batch telemetry: {batch_pig_telemetry:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let batch_lookup_elapsed = batch_lookup_started_at.elapsed();
    matching_batches.sort_by_key(|telemetry| telemetry.batch_number);
    let first_batch = matching_batches
        .first()
        .context("missing matching batch telemetry after coverage loop")?
        .batch_number;
    let batch_pig_mode = matching_batches
        .first()
        .context("missing matching batch telemetry after coverage loop")?
        .mode;
    let last_batch = matching_batches
        .last()
        .context("missing matching batch telemetry after coverage loop")?
        .batch_number;
    let batch_count = matching_batches.len();
    anyhow::ensure!(
        matching_batches
            .iter()
            .all(|telemetry| telemetry.mode == batch_pig_mode),
        "stress workload spanned mixed batch PIG modes: {matching_batches:?}"
    );
    let block_pig_ms: u128 = block_pig_telemetry
        .iter()
        .filter(|telemetry| {
            telemetry.chain_id == target_chain_id
                && telemetry.block_number >= first_stress_block
                && telemetry.block_number <= last_stress_block
        })
        .map(|telemetry| telemetry.elapsed.as_millis())
        .sum();
    let batch_pig_ms: u128 = matching_batches
        .iter()
        .map(|telemetry| telemetry.elapsed.as_millis())
        .sum();
    let batch_computational_native_used: u64 = matching_batches
        .iter()
        .map(|telemetry| telemetry.computational_native_used)
        .sum();
    let total_pig_ms = block_pig_ms + batch_pig_ms;
    let batch_pig_ms_per_million_native = if batch_computational_native_used == 0 {
        0.0
    } else {
        batch_pig_ms as f64 / (batch_computational_native_used as f64 / 1_000_000.0)
    };
    let total_pig_ms_per_million_native = if batch_computational_native_used == 0 {
        0.0
    } else {
        total_pig_ms as f64 / (batch_computational_native_used as f64 / 1_000_000.0)
    };

    let block_fetch_started_at = Instant::now();
    let mut covered_batch_blocks = BTreeSet::new();
    for telemetry in &matching_batches {
        for block_number in telemetry.first_block_number..=telemetry.last_block_number {
            covered_batch_blocks.insert(block_number);
        }
    }
    let mut block_gas_used_in_batch: u128 = 0;
    let mut txs_observed_in_batch: usize = 0;
    for block_number in &covered_batch_blocks {
        let block = tester
            .l2_provider
            .get_block_by_number((*block_number).into())
            .await?
            .with_context(|| format!("stress block {block_number} should exist"))?;
        block_gas_used_in_batch += u128::from(block.header.gas_used);
        txs_observed_in_batch += block.transactions.len();
    }
    let block_fetch_elapsed = block_fetch_started_at.elapsed();

    tracing::info!(
        bench_label,
        protocol_version,
        sender = %sender,
        iterations,
        warmup_gas_used,
        per_tx_gas_limit,
        target_batch_gas,
        actual_batch_gas = block_gas_used_in_batch,
        total_receipt_gas_used,
        full_blocks_per_batch,
        unique_blocks_in_batch = covered_batch_blocks.len(),
        txs_to_fill_one_block,
        txs_in_first_block,
        total_stress_txs,
        txs_observed_in_batch,
        first_stress_block,
        last_stress_block,
        first_batch,
        last_batch,
        batch_count,
        block_pig_ms,
        batch_pig_mode = ?batch_pig_mode,
        batch_pig_ms,
        total_pig_ms,
        batch_pig_prover_input_words = matching_batches
            .iter()
            .map(|telemetry| telemetry.prover_input_words)
            .sum::<usize>(),
        batch_computational_native_used,
        batch_pig_ms_per_million_native,
        total_pig_ms_per_million_native,
        submit_ms = submit_elapsed.as_millis(),
        receipts_ms = receipts_elapsed.as_millis(),
        batch_lookup_ms = batch_lookup_elapsed.as_millis(),
        block_fetch_ms = block_fetch_elapsed.as_millis(),
        total_ms = total_started_at.elapsed().as_millis(),
        "Keccak burner stress run reached batch sealing"
    );

    Ok(KeccakPigBenchResult {
        bench_label: bench_label.to_owned(),
        protocol_version: protocol_version.to_owned(),
        iterations,
        warmup_gas_used,
        per_tx_gas_limit,
        block_gas_limit,
        full_blocks_per_batch,
        target_batch_gas,
        actual_batch_gas: block_gas_used_in_batch,
        total_receipt_gas_used,
        txs_to_fill_one_block,
        txs_in_first_block,
        total_stress_txs,
        txs_observed_in_batch,
        unique_blocks_in_batch: covered_batch_blocks.len(),
        first_stress_block,
        last_stress_block,
        first_batch,
        last_batch,
        batch_count,
        batch_computational_native_used,
        batch_pig_mode: match batch_pig_mode {
            BatchPigMode::LegacyBatch => "legacy_batch".to_owned(),
            BatchPigMode::NativeBatch => "native_batch".to_owned(),
        },
        block_pig_ms,
        batch_pig_ms,
        total_pig_ms,
        batch_pig_ms_per_million_native,
        total_pig_ms_per_million_native,
        batch_pig_prover_input_words: matching_batches
            .iter()
            .map(|telemetry| telemetry.prover_input_words)
            .sum(),
        env_prepare_ms: env_prepare_elapsed.as_millis(),
        initial_launch_ms: initial_launch_elapsed.as_millis(),
        catchup_ms: catchup_elapsed.as_millis(),
        restart_ms: restart_elapsed.as_millis(),
        deploy_ms: deploy_elapsed.as_millis(),
        warmup_ms: warmup_elapsed.as_millis(),
        submit_ms: submit_elapsed.as_millis(),
        receipts_ms: receipts_elapsed.as_millis(),
        batch_lookup_ms: batch_lookup_elapsed.as_millis(),
        block_fetch_ms: block_fetch_elapsed.as_millis(),
        total_ms: total_started_at.elapsed().as_millis(),
    })
}
