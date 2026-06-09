use anyhow::Result;
use zksync_os_integration_tests::keccak_pig_bench::run_keccak_burner_bench;
use zksync_os_server::default_protocol_version::{PROTOCOL_VERSION_V31_0, PROTOCOL_VERSION_V32_1};

#[test_log::test(tokio::test)]
#[ignore = "manual benchmark; compare V31 and V32 with the same env-tuned workload"]
async fn v31_0_keccak_burner_fills_huge_batch_until_batch_seal() -> Result<()> {
    run_keccak_burner_bench(PROTOCOL_VERSION_V31_0, "v31")
        .await
        .map(|_| ())
}

#[test_log::test(tokio::test)]
#[ignore = "manual benchmark; compare V31 and V32 with the same env-tuned workload"]
async fn v32_1_keccak_burner_fills_huge_batch_until_batch_seal() -> Result<()> {
    run_keccak_burner_bench(PROTOCOL_VERSION_V32_1, "v32")
        .await
        .map(|_| ())
}
