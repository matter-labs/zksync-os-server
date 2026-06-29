use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use zksync_os_types::ProvingVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPigMode {
    LegacyBatch,
    NativeBatch,
}

#[derive(Debug, Clone)]
pub struct BatchPigTelemetry {
    pub batch_number: u64,
    pub chain_id: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub proving_version: ProvingVersion,
    pub mode: BatchPigMode,
    pub prover_input_words: usize,
    pub computational_native_used: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct BlockPigTelemetry {
    pub chain_id: u64,
    pub block_number: u64,
    pub proving_version: ProvingVersion,
    pub prover_input_words: usize,
    pub elapsed: Duration,
}

static BATCH_PIG_TELEMETRY: OnceLock<Mutex<Vec<BatchPigTelemetry>>> = OnceLock::new();
static BLOCK_PIG_TELEMETRY: OnceLock<Mutex<Vec<BlockPigTelemetry>>> = OnceLock::new();

fn batch_pig_telemetry() -> &'static Mutex<Vec<BatchPigTelemetry>> {
    BATCH_PIG_TELEMETRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn block_pig_telemetry() -> &'static Mutex<Vec<BlockPigTelemetry>> {
    BLOCK_PIG_TELEMETRY.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn clear_batch_pig_telemetry() {
    batch_pig_telemetry().lock().unwrap().clear();
}

pub fn take_batch_pig_telemetry() -> Vec<BatchPigTelemetry> {
    let mut telemetry = batch_pig_telemetry().lock().unwrap();
    std::mem::take(&mut *telemetry)
}

pub fn clear_block_pig_telemetry() {
    block_pig_telemetry().lock().unwrap().clear();
}

pub fn take_block_pig_telemetry() -> Vec<BlockPigTelemetry> {
    let mut telemetry = block_pig_telemetry().lock().unwrap();
    std::mem::take(&mut *telemetry)
}

pub(crate) fn record_batch_pig_telemetry(telemetry: BatchPigTelemetry) {
    let elapsed_per_million_native_ms = if telemetry.computational_native_used == 0 {
        None
    } else {
        Some(
            telemetry.elapsed.as_secs_f64() * 1000.0
                / (telemetry.computational_native_used as f64 / 1_000_000.0),
        )
    };
    tracing::info!(
        batch_number = telemetry.batch_number,
        chain_id = telemetry.chain_id,
        first_block_number = telemetry.first_block_number,
        last_block_number = telemetry.last_block_number,
        ?telemetry.proving_version,
        pig_mode = ?telemetry.mode,
        prover_input_words = telemetry.prover_input_words,
        computational_native_used = telemetry.computational_native_used,
        elapsed_ms = telemetry.elapsed.as_millis(),
        elapsed_per_million_native_ms = ?elapsed_per_million_native_ms,
        "Batch PIG completed",
    );
    batch_pig_telemetry().lock().unwrap().push(telemetry);
}

pub(crate) fn record_block_pig_telemetry(telemetry: BlockPigTelemetry) {
    tracing::info!(
        chain_id = telemetry.chain_id,
        block_number = telemetry.block_number,
        ?telemetry.proving_version,
        prover_input_words = telemetry.prover_input_words,
        elapsed_ms = telemetry.elapsed.as_millis(),
        "Block PIG completed",
    );
    block_pig_telemetry().lock().unwrap().push(telemetry);
}
