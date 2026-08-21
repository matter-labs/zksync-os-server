use crate::Tester;
use crate::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL};
use alloy::primitives::U256;
use alloy::providers::Provider;
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_contract_interface::calldata::decode_prove_proof_words;
use zksync_os_contract_interface::l1_discovery::L1State;

/// The multi-proof (type-5) payload header word: exactly the bare type — the
/// on-chain MultiProofVerifier rejects any bit at or above 8.
const MULTI_PROOF_HEADER: u64 = 5;

/// Assert that every proof settled on the test L1 was a bare-header type-5
/// multi-proof, by decoding the actual `proveBatchesSharedBridge` calldata out
/// of the anvil blocks.
///
/// `last_executed_batch` advancing does NOT prove a real proof settled: the
/// fixture's chain verifier is a testnet wrapper that accepts empty and mock
/// proofs unconditionally, so a regression to the fake path would settle just
/// as green. The submitted proof type is the only on-chain trace of which path
/// ran. The test anvil starts from a state dump with no historical blocks, so
/// the scan is over this run's blocks only.
pub async fn assert_settled_with_multiproof(tester: &Tester) -> anyhow::Result<()> {
    let l1 = tester.l1_provider();
    let latest = l1.get_block_number().await?;
    let mut prove_headers: Vec<U256> = Vec::new();
    for number in 0..=latest {
        let Some(block) = l1.get_block(number.into()).full().await? else {
            continue;
        };
        for tx in block.transactions.txns() {
            use alloy::consensus::Transaction;
            if let Some(words) = decode_prove_proof_words(tx.inner.input()) {
                let header = *words
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("prove tx with an empty proof payload"))?;
                prove_headers.push(header);
            }
        }
    }
    anyhow::ensure!(
        !prove_headers.is_empty(),
        "no proveBatchesSharedBridge transaction found on L1 — nothing settled a proof"
    );
    for header in &prove_headers {
        anyhow::ensure!(
            *header == U256::from(MULTI_PROOF_HEADER),
            "a settled proof was not a bare-header type-5 multi-proof: header word {header:#x} \
             (a mock or single-lane proof settled where only the two-lane payload may)"
        );
    }
    tracing::info!(
        proofs = prove_headers.len(),
        "every settled proof was a bare-header type-5 multi-proof"
    );
    Ok(())
}

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
