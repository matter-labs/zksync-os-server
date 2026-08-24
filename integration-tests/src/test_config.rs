use crate::config::{ChainLayout, load_chain_config};
use crate::{AnvilL1, BATCH_VERIFICATION_ADDRESSES, BATCH_VERIFICATION_KEYS};
use alloy::primitives::Address;
use std::net::Ipv4Addr;
use std::time::Duration;
use zksync_os_server::config::{Config, ProviderConfig};

pub(crate) const TEST_PROVIDER_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Configures the node to commit batches on L1 but never execute them: FRI proofs are faked,
/// while SNARK proving is disabled. This keeps batches in the committed-but-not-executed state.
pub fn make_commit_only_config(config: &mut Config) {
    config.prover_api_config.fake_fri_provers.enabled = true;
    config.prover_api_config.fake_fri_provers.compute_time = Duration::from_millis(200);
    config.prover_api_config.fake_fri_provers.min_age = Duration::ZERO;
    config.prover_api_config.fake_snark_provers.enabled = false;
}

/// Runs the full settlement pipeline so batches commit, prove, and execute on L1.
pub fn make_full_pipeline_config(config: &mut Config) {
    config.prover_api_config.fake_fri_provers.enabled = true;
    config.prover_api_config.fake_fri_provers.compute_time = Duration::from_millis(200);
    config.prover_api_config.fake_fri_provers.min_age = Duration::ZERO;
    config.prover_api_config.fake_snark_provers.enabled = true;
    config.prover_api_config.fake_snark_provers.max_batch_age = Duration::ZERO;
}

/// Enables the ZiSK second proof system for a test config.
///
/// The general suite runs without the ZiSK lane. See `build_node_config`. The
/// lane assembles a ZiSK input for each batch. This work slows each batch seal.
/// It can push a timing-sensitive L1-settlement test past its deadline.
///
/// Only three groups of callers opt in: the tests that check the ZiSK lane, the
/// equivalence run, and the real-prover path.
///
/// The single-batch ZiSK guest proves one batch per SNARK. So the server pairs
/// each SNARK with one FRI. A `max_fris_per_snark` of 1 holds this pairing. The
/// aggregated real-prover lane proves a wider range. That lane sets a larger
/// `max_fris_per_snark` after it calls this function.
pub fn enable_second_proof_system(config: &mut Config) {
    config.prover_input_generator_config.second_proof_system = true;
    config.prover_api_config.max_fris_per_snark = 1;
}

pub(crate) fn disable_prover_input_generation(config: &mut Config) {
    // In equivalence mode input generation stays on everywhere so every
    // sealed batch is shadow-executed (see `build_node_config`).
    if equivalence_mode() {
        return;
    }
    if config.prover_api_config.fake_fri_provers.enabled
        && config.prover_api_config.fake_snark_provers.enabled
    {
        config.prover_input_generator_config.enable_input_generation = false;
        config.prover_input_generator_config.second_proof_system = false;
    }
}

/// Opt-in deep equivalence mode for the whole suite: every sealed batch's
/// ZiSK input is re-executed in-process with the guest executor and a batch
/// public input divergence fails batch sealing. Slower (witness generation
/// runs in every test), hence env-gated rather than default.
pub(crate) fn equivalence_mode() -> bool {
    std::env::var("ZKOS_TESTS_EQUIVALENCE").is_ok_and(|v| v == "1")
}

pub(crate) async fn build_node_config(
    l1: &AnvilL1,
    chain_layout: ChainLayout<'static>,
    with_proofs: bool,
) -> anyhow::Result<Config> {
    let mut config = load_chain_config(chain_layout).await;
    config.l1_provider_config =
        ProviderConfig::new(l1.address.clone(), TEST_PROVIDER_POLL_INTERVAL);
    // The L1 senders poll receipts on their own cadence (1s default) — keep tests fast
    // against anvil's 0.25s blocks.
    config.l1_sender_config.poll_interval = TEST_PROVIDER_POLL_INTERVAL;
    config.sequencer_config.fee_collector_address = Address::random();
    config.rpc_config.send_raw_transaction_sync_timeout = Duration::from_secs(10);
    config.prover_api_config.fake_fri_provers.enabled = !with_proofs;
    config.prover_api_config.fake_snark_provers.enabled = !with_proofs;
    // A native-vs-REVM divergence (state diffs, event logs, L2→L1 logs)
    // fails the node — and therefore the test — instead of only logging.
    config
        .sequencer_config
        .revm_consistency_checker_revert_on_divergence = true;
    // The ZiSK lane stays off by default. The general suite then runs
    // upstream-lean and fast. Only the tests that check the lane opt in (see
    // `enable_second_proof_system`). Equivalence mode turns the lane on for the
    // whole suite. Then the node shadow-executes every sealed batch and halts
    // on any commitment mismatch.
    if equivalence_mode() {
        enable_second_proof_system(&mut config);
        config.prover_input_generator_config.zisk_shadow_execution = true;
        config
            .prover_input_generator_config
            .halt_on_zisk_commitment_mismatch = true;
    }
    config.batch_verification_config.server_enabled = false;
    config.batch_verification_config.client_enabled = false;
    config.batch_verification_config.threshold = 1;
    config.batch_verification_config.accepted_signers = BATCH_VERIFICATION_ADDRESSES.clone();
    config.batch_verification_config.request_timeout = Duration::from_millis(500);
    config.batch_verification_config.retry_delay = Duration::from_secs(1);
    config.batch_verification_config.signing_key = BATCH_VERIFICATION_KEYS[0].into();
    config.status_server_config.enabled = true;
    config.network_config.enabled = true;
    config.network_config.address = Ipv4Addr::LOCALHOST;
    config.network_config.interface = None;
    config.network_config.boot_nodes.clear();
    Ok(config)
}
