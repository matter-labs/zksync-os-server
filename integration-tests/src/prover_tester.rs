use crate::dyn_wallet_provider::EthDynProvider;
use crate::network::Zksync;
use crate::provider::ZksyncApi;
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use std::time::Duration;
use zksync_os_contract_interface::l1_discovery::L1State;

#[derive(Debug)]
pub struct ProverTester {
    l1_provider: EthDynProvider,
    gateway_provider: Option<EthDynProvider>,
    l2_provider: EthDynProvider,
    l2_zk_provider: DynProvider<Zksync>,
}

impl ProverTester {
    /// Create a new client targeting the given base URL
    pub fn new(
        l1_provider: EthDynProvider,
        gateway_provider: Option<EthDynProvider>,
        l2_provider: EthDynProvider,
        l2_zk_provider: DynProvider<Zksync>,
    ) -> Self {
        Self {
            l1_provider,
            gateway_provider,
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

        // Get L1/SL state which contains diamond proxy address
        let l1_state = L1State::fetch(
            self.l1_provider.clone().erased(),
            self.gateway_provider.as_ref().map(|p| p.clone().erased()),
            bridgehub_address,
            chain_id,
        )
        .await?;
        let diamond_proxy_address = l1_state.diamond_proxy_address_sl();
        tracing::info!(
            batch_number,
            %diamond_proxy_address,
            "checking batch #{batch_number} status on L1 state: {l1_state:?}"
        );

        let blocks_verification_signature = keccak256(b"BlocksVerification(uint256,uint256)");
        let filter = Filter::new()
            .event_signature(blocks_verification_signature)
            .address(diamond_proxy_address)
            .from_block(0)
            .to_block(BlockNumberOrTag::Latest);
        let sl_provider = self.gateway_provider.as_ref().unwrap_or(&self.l1_provider);
        let logs = sl_provider.get_logs(&filter).await?;
        if logs.is_empty() {
            tracing::info!("no `BlocksVerification` events discovered on L1");
            return Ok(false);
        }
        for log in logs {
            if log.topics().len() >= 3 {
                // Parse previousLastVerifiedBatch (topic[1]) and currentLastVerifiedBatch (topic[2])
                let previous_verified = U256::from_be_bytes(log.topics()[1].0);
                let current_verified = U256::from_be_bytes(log.topics()[2].0);

                let batch_u256 = U256::from(batch_number);

                // Check if our batch_number is within the verified range
                if batch_u256 > previous_verified && batch_u256 <= current_verified {
                    tracing::info!(
                        batch_number,
                        "batch #{batch_number} was proved by log `BlocksVerification({}, {})`",
                        previous_verified,
                        current_verified
                    );
                    return Ok(true);
                } else {
                    tracing::info!(
                        "discovered unrelated log `BlocksVerification({}, {})`",
                        previous_verified,
                        current_verified
                    );
                }
            } else {
                tracing::warn!(
                    "discovered `BlocksVerification` log that does not follow correct format: {log:?}",
                );
            }
        }
        Ok(false)
    }

    /// Resolves when the requested batch gets reported as proven by prover API.
    pub async fn wait_for_batch_proven(&self, batch_number: u64) -> anyhow::Result<()> {
        let mut retries = 40;
        while retries > 0 {
            let status = self.check_batch_status(batch_number).await?;
            if status {
                return Ok(());
            } else {
                tracing::info!("proof not ready yet, retrying");
                retries -= 1;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
        Err(anyhow::anyhow!("proof was not submitted to L1 in time"))
    }
}
