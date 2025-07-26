#![cfg(feature = "prover-tests")]

use zksync_os_integration_tests::Tester;

#[test_log::test(tokio::test)]
async fn prover() -> anyhow::Result<()> {
    // Test that prover can successfully prove at least one batch
    let tester = Tester::builder().enable_prover().build().await?;

    // Test environment comes with some L1 transactions by default, so one batch should be provable
    // without any new transactions inside the test.
    tester.prover_api.wait_for_batch_proven(1).await?;

    // todo: consider expanding this test to prove multiple batches on top of the first batch
    //       also to test L2 transactions are provable too

    Ok(())
}
