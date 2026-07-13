use std::time::Duration;
use vise::{Buckets, EncodeLabelValue, Histogram, LabeledFamily, Metrics, Unit};
use zksync_os_types::ProvingVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "mode", rename_all = "snake_case")]
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

#[derive(Debug, Metrics)]
#[metrics(prefix = "pig")]
pub struct PigMetrics {
    /// Time spent generating the proof input for a whole batch.
    #[metrics(unit = Unit::Seconds, labels = ["mode"], buckets = Buckets::LATENCIES)]
    pub batch_elapsed: LabeledFamily<BatchPigMode, Histogram<Duration>>,
    /// Batch proof-input generation time normalized by millions of computational native used.
    #[metrics(unit = Unit::Seconds, labels = ["mode"], buckets = Buckets::LATENCIES)]
    pub batch_elapsed_per_million_native: LabeledFamily<BatchPigMode, Histogram<Duration>>,
    /// Size of the generated batch prover input in u32 words.
    #[metrics(labels = ["mode"], buckets = Buckets::exponential(100_000.0..=10_000_000_000.0, 4.0))]
    pub batch_prover_input_words: LabeledFamily<BatchPigMode, Histogram<u64>>,
    /// Time spent generating the proof input for a single block.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES)]
    pub block_elapsed: Histogram<Duration>,
    /// Size of the generated block prover input in u32 words.
    #[metrics(buckets = Buckets::exponential(100_000.0..=10_000_000_000.0, 4.0))]
    pub block_prover_input_words: Histogram<u64>,
}

#[vise::register]
pub(crate) static PIG_METRICS: vise::Global<PigMetrics> = vise::Global::new();

pub(crate) fn record_batch_pig_telemetry(telemetry: BatchPigTelemetry) {
    let elapsed_per_million_native = if telemetry.computational_native_used == 0 {
        None
    } else {
        Some(
            telemetry
                .elapsed
                .div_f64(telemetry.computational_native_used as f64 / 1_000_000.0),
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
        elapsed_per_million_native_ms = ?elapsed_per_million_native.map(|d| d.as_millis()),
        "Batch PIG completed",
    );
    PIG_METRICS.batch_elapsed[&telemetry.mode].observe(telemetry.elapsed);
    if let Some(per_million) = elapsed_per_million_native {
        PIG_METRICS.batch_elapsed_per_million_native[&telemetry.mode].observe(per_million);
    }
    PIG_METRICS.batch_prover_input_words[&telemetry.mode]
        .observe(telemetry.prover_input_words as u64);
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
    PIG_METRICS.block_elapsed.observe(telemetry.elapsed);
    PIG_METRICS
        .block_prover_input_words
        .observe(telemetry.prover_input_words as u64);
}
