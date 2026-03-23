use std::str::FromStr;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::multi_node::MultiNodeTester;

fn consensus_1_nodes_test_keys() -> anyhow::Result<Vec<zksync_os_network::SecretKey>> {
    Ok(vec![
        zksync_os_network::SecretKey::from_str(
            "0af6153646bbf600f55ce455e1995283542b1ae25ce2622ce1fda443927c5308",
        )?,
        // zksync_os_network::SecretKey::from_str(
        //     "c2c8042b03801e2e14b395ed24f970ead7646a9ff315b54f747bcefdb99afda7",
        // )?,
        // zksync_os_network::SecretKey::from_str(
        //     "8b50ece5c94762fb0b8dcd2f859fb0132b86c0540c388806b6a03e0b1c25978d",
        // )?,
    ])
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction2() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_1_nodes_test_keys()?)
        .build()
        .await?;
    let leader_index = cluster
        .wait_for_raft_cluster_formation(Duration::from_secs(15))
        .await?;

    cluster.node(0).wait_for_initial_deposit().await?;

    let deploy_receipt = EventEmitter::deploy_builder(cluster.node(leader_index).l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    assert!(
        deploy_receipt.block_number.is_some(),
        "deployment transaction was not included in a block"
    );

    Ok(())
}
#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction1() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_1_nodes_test_keys()?)
        .build()
        .await?;
    let leader_index = cluster
        .wait_for_raft_cluster_formation(Duration::from_secs(15))
        .await?;
    cluster.node(0).wait_for_initial_deposit().await?;

    let deploy_receipt = EventEmitter::deploy_builder(cluster.node(leader_index).l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    assert!(
        deploy_receipt.block_number.is_some(),
        "deployment transaction was not included in a block"
    );

    Ok(())
}


#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction3_with_wait() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_1_nodes_test_keys()?)
        .build()
        .await?;
    tokio::time::sleep(Duration::from_secs(12)).await;

    let leader_index = cluster
        .wait_for_raft_cluster_formation(Duration::from_secs(15))
        .await?;
    cluster.node(0).wait_for_initial_deposit().await?;

    let deploy_receipt = EventEmitter::deploy_builder(cluster.node(leader_index).l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    assert!(
        deploy_receipt.block_number.is_some(),
        "deployment transaction was not included in a block"
    );

    Ok(())
}

// #[test_log::test(tokio::test)]
// async fn consensus_cluster_rotates_leader_after_failure() -> anyhow::Result<()> {
//     // build cluster normally
//     let mut cluster = MultiNodeTester::builder()
//         .with_consensus_secret_keys(consensus_3_nodes_test_keys()?)
//         .build()
//         .await?;
//     tokio::time::sleep(Duration::from_secs(12)).await;
//
//     let initial_leader_idx = cluster
//         .wait_for_raft_cluster_formation(Duration::from_secs(20))
//         .await?;
    // tokio::time::sleep(Duration::from_secs(12)).await;
    //
    // let initial_leader_node_id = cluster
    //     .node(initial_leader_idx)
    //     .status()
    //     .await?
    //     .consensus
    //     .raft
    //     .expect("raft status should be present")
    //     .node_id;
    //
    // // kill the leader node to trigger new election
    // tokio::time::sleep(Duration::from_secs(12)).await;
    // cluster.kill_node(initial_leader_idx);
    // tokio::time::sleep(Duration::from_secs(6)).await;
    //
    // let new_leader_idx = cluster
    //     .wait_for_raft_cluster_formation(Duration::from_secs(20))
    //     .await?;
    // let new_leader_id = cluster
    //     .node(new_leader_idx)
    //     .status()
    //     .await?
    //     .consensus
    //     .raft
    //     .expect("raft status should be present")
    //     .node_id;
    //
    // assert_ne!(
    //     initial_leader_node_id, new_leader_id,
    //     "leader did not rotate after leader node was killed"
    // );
    //
    // // make sure the cluster is functional
    // cluster.node(new_leader_idx).wait_for_initial_deposit().await?;
    // let deploy_receipt = EventEmitter::deploy_builder(cluster.node(new_leader_idx).l2_provider.clone())
    //     .send()
    //     .await?
    //     .expect_successful_receipt()
    //     .await?;
    // assert!(
    //     deploy_receipt.block_number.is_some(),
    //     "deployment transaction was not included after leader rotation"
    // );
    //
    // // now we'll kill the replica and ensure blocks are not mined (quorum is lost)
    // let replica_idx = if new_leader_idx == 0 { 1 } else { 0 };
    // cluster.kill_node(replica_idx);
    // tokio::time::sleep(Duration::from_secs(2)).await;
    //
    // let send_result = EventEmitter::deploy_builder(cluster.node(0).l2_provider.clone())
    //     .send()
    //     .await;
    // if let Ok(pending_tx) = send_result {
    //     let receipt_result =
    //         timeout(Duration::from_secs(8), pending_tx.expect_successful_receipt()).await;
    //     assert!(
    //         receipt_result.is_err() || receipt_result.expect("timeout checked").is_err(),
    //         "transaction unexpectedly got included after losing quorum"
    //     );
    // }
    //
//     Ok(())
// }
