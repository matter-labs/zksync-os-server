use crate::Tester;
use crate::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL};
use alloy::providers::Provider;
use backon::{ConstantBuilder, Retryable};
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_contract_interface::l1_discovery::L1State;

/// Fetches the current L1 state from the given tester.
pub async fn fetch_l1_state(tester: &Tester) -> anyhow::Result<L1State> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    L1State::fetch(
        tester.l1_provider().clone(),
        tester.gateway_eth_provider(),
        bridgehub_address,
        chain_id,
    )
    .await
}

/// Polls the L1 state until a predicate is satisfied or timeout is reached.
///
/// Uses the global `DEFAULT_TIMEOUT` and `POLL_INTERVAL` for polling parameters.
pub async fn wait_for_l1_state(
    tester: &Tester,
    description: &str,
    predicate: impl Fn(&L1State) -> bool,
) -> anyhow::Result<L1State> {
    let max_times = DEFAULT_TIMEOUT.div_duration_f64(POLL_INTERVAL).floor() as usize;
    (|| async {
        let state = fetch_l1_state(tester).await?;
        if predicate(&state) {
            Ok(state)
        } else {
            anyhow::bail!("waiting for L1 state: {description}")
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(POLL_INTERVAL)
            .with_max_times(max_times),
    )
    .await
}
