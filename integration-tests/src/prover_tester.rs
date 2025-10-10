use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::provider::ZksyncApi;
use alloy::primitives::{U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use backon::{ConstantBuilder, Retryable};
use std::time::Duration;
use zksync_os_contract_interface::l1_discovery::L1State;

pub struct ProverTester {
    l1_provider: EthDynProvider,
    l2_provider: EthDynProvider,
    l2_zk_provider: DynProvider<Zksync>,
}

impl ProverTester {
    /// Create a new client targeting the given base URL
    pub fn new(
        l1_provider: EthDynProvider,
        l2_provider: EthDynProvider,
        l2_zk_provider: DynProvider<Zksync>,
    ) -> Self {
        Self {
            l1_provider,
            l2_provider,
            l2_zk_provider,
        }
    }

    /// Checks batch status by verifying that the proof has been verified on L1.
    /// Returns `true` if batch has been proven and verified on L1, `false` otherwise.
    pub async fn check_batch_status(&self, batch_number: u64) -> anyhow::Result<bool> {
        // Try to get bridgehub address from L2, fallback to default
        let bridgehub_address = self.l2_zk_provider.get_bridgehub_contract().await?;
        let chain_id = self.l2_provider.get_chain_id().await?;

        // Get L1 state which contains diamond proxy address
        let l1_state = L1State::fetch(
            self.l1_provider.clone().erased(),
            bridgehub_address,
            chain_id,
        )
        .await?;
        let diamond_proxy_address = l1_state.diamond_proxy_address();

        let blocks_verification_signature = keccak256(b"BlocksVerification(uint256,uint256)");
        let filter = Filter::new()
            .event_signature(blocks_verification_signature)
            .address(diamond_proxy_address);
        let logs = self.l1_provider.get_logs(&filter).await?;
        for log in logs {
            if log.topics().len() >= 3 {
                // Parse previousLastVerifiedBatch (topic[1]) and currentLastVerifiedBatch (topic[2])
                let previous_verified = U256::from_be_bytes(log.topics()[1].0);
                let current_verified = U256::from_be_bytes(log.topics()[2].0);

                let batch_u256 = U256::from(batch_number);

                // Check if our batch_number is within the verified range
                if batch_u256 > previous_verified && batch_u256 <= current_verified {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Resolves when the requested batch gets reported as proven by prover API.
    pub async fn wait_for_batch_proven(&self, batch_number: u64) -> anyhow::Result<()> {
        (|| async {
            let status = self.check_batch_status(batch_number).await?;
            if status {
                Ok(())
            } else {
                Err(anyhow::anyhow!("batch is not ready yet"))
            }
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_secs(30))
                .with_max_times(40),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(?err, ?dur, "proof not ready yet, retrying");
        })
        .await
    }
}
