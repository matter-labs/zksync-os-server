use crate::dyn_wallet_provider::EthDynProvider;
use crate::provider::ZksyncApi;
use alloy::primitives::{Address, B256, BlockNumber, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use backon::{ConstantBuilder, Retryable};
use reqwest::{Client, StatusCode};
use std::time::Duration;
use zksync_os_contract_interface::Bridgehub;

pub struct ProverApi {
    client: Client,
    base_url: String,
}

impl ProverApi {
    /// Create a new client targeting the given base URL
    pub fn new<U: Into<String>>(base_url: U) -> Self {
        ProverApi {
            client: Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Checks batch status by verifying that the proof has been verified on L1.
    /// Returns `true` if batch has been proven and verified on L1, `false` otherwise.
    pub async fn check_batch_status(
        &self,
        batch_number: u64,
        l1_provider: EthDynProvider,
        l1_address: Address,
    ) -> anyhow::Result<bool> {
        let blocks_verification_signature = keccak256(b"BlocksVerification(uint256,uint256)");
        let filter = Filter::new()
            .from_block(batch_number)
            .to_block(batch_number)
            .event_signature(blocks_verification_signature)
            .address(l1_address);
        let logs = l1_provider.get_logs(&filter).await?;
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
    pub async fn wait_for_batch_proven(
        &self,
        batch_number: u64,
        l1_provider: EthDynProvider,
        l1_address: Address,
    ) -> anyhow::Result<()> {
        (|| async {
            let status = self
                .check_batch_status(batch_number, l1_provider.clone(), l1_address)
                .await?;
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
