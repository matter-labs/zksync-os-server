use crate::Tester;
use crate::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL};
use alloy::providers::Provider;
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_contract_interface::l1_discovery::L1State;

/// Fetches the current L1 state from the given tester.
pub async fn fetch_l1_state(tester: &Tester) -> anyhow::Result<L1State> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    L1State::fetch(tester.l1_provider().clone(), bridgehub_address, chain_id).await
}

/// Polls the L1 state until a predicate is satisfied or timeout is reached.
///
/// Uses the global `DEFAULT_TIMEOUT` and `POLL_INTERVAL` for polling parameters.
pub async fn wait_for_l1_state(
    tester: &Tester,
    description: &str,
    predicate: impl Fn(&L1State) -> bool,
) -> anyhow::Result<L1State> {
    let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT;
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        // The L1 state lives on anvil, so a dead node would otherwise burn the whole timeout
        // and report an unhelpful "waiting for ..." error; fail fast with the real cause.
        anyhow::ensure!(
            !tester.has_crashed(),
            "node crashed while waiting for L1 state: {description}",
        );
        match fetch_l1_state(tester).await {
            Ok(state) if predicate(&state) => return Ok(state),
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "timed out waiting for L1 state: {description} (last fetch error: {last_err:?})",
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
