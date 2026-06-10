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

pub(crate) fn disable_prover_input_generation(config: &mut Config) {
    if config.prover_api_config.fake_fri_provers.enabled
        && config.prover_api_config.fake_snark_provers.enabled
    {
        config.prover_input_generator_config.enable_input_generation = false;
    }
}

pub(crate) async fn build_node_config(
    l1: &AnvilL1,
    chain_layout: ChainLayout<'static>,
    with_proofs: bool,
) -> anyhow::Result<Config> {
    let mut config = load_chain_config(chain_layout).await;
    config.l1_provider_config =
        ProviderConfig::new(l1.address.clone(), TEST_PROVIDER_POLL_INTERVAL);
    if let Some(gateway_provider_config) = &mut config.gateway_provider_config {
        gateway_provider_config.rpc_poll_interval = TEST_PROVIDER_POLL_INTERVAL;
    }
    config.sequencer_config.fee_collector_address = Address::random();
    config.rpc_config.send_raw_transaction_sync_timeout = Duration::from_secs(10);
    config.prover_api_config.fake_fri_provers.enabled = !with_proofs;
    config.prover_api_config.fake_snark_provers.enabled = !with_proofs;
    config.batch_verification_config.server_enabled = false;
    config.batch_verification_config.client_enabled = false;
    config.batch_verification_config.threshold = 1;
    config.batch_verification_config.accepted_signers = BATCH_VERIFICATION_ADDRESSES.clone();
    config.batch_verification_config.request_timeout = Duration::from_millis(500);
    config.batch_verification_config.retry_delay = Duration::from_secs(1);
    config.batch_verification_config.total_timeout = Duration::from_secs(300);
    config.batch_verification_config.signing_key = BATCH_VERIFICATION_KEYS[0].into();
    config.status_server_config.enabled = true;
    config.network_config.enabled = true;
    config.network_config.address = Ipv4Addr::LOCALHOST;
    config.network_config.interface = None;
    config.network_config.boot_nodes.clear();
    Ok(config)
}
