use crate::batcher_metrics::BatchExecutionStage;
use crate::batcher_model::{BatchEnvelope, FriProof};
use crate::config::L1SenderConfig;
use alloy::network::TransactionBuilder;
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use itertools::Itertools;
use std::fmt::Display;

pub mod commit;
pub mod execute;
pub mod prove;

pub trait L1SenderCommand:
    Into<Vec<BatchEnvelope<FriProof>>>
    + AsRef<[BatchEnvelope<FriProof>]>
    + AsMut<[BatchEnvelope<FriProof>]>
    + Display
{
    const NAME: &'static str;
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    fn solidity_call(&self) -> impl SolCall;

    /// Only used for logging - as we send commands in bulk, it's natural to print a single range
    /// for the whole group, e.g. "1-3, 4, 5-6" instead of "1, 2, 3, 4, 5, 6"
    /// Note that one `L1SenderCommand` is still always a single L1 transaction.
    fn display_range(cmds: &[Self]) -> String {
        cmds.iter()
            .map(|cmd| {
                let envelopes = cmd.as_ref();
                // Safe unwraps as each command contains at least one envelope
                let first = envelopes.first().unwrap().batch_number();
                let last = envelopes.last().unwrap().batch_number();
                if first == last {
                    format!("{first}")
                } else {
                    format!("{first}-{last}")
                }
            })
            .join(", ")
    }

    async fn into_transaction_request(
        &self,
        provider: DynProvider,
        operator_address: Address,
        config: &L1SenderConfig<Self>,
        to_address: Address,
        cmd: &Self,
    ) -> anyhow::Result<TransactionRequest> {
        Ok(
            tx_request_with_gas_fields(
                &provider,
                operator_address,
                config.max_fee_per_gas(),
                config.max_priority_fee_per_gas(),
            )
            .await?
            .with_to(to_address)
            .with_call(&cmd.solidity_call())
        )
    }
}

pub async fn tx_request_with_gas_fields(
    provider: &DynProvider,
    operator_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> anyhow::Result<TransactionRequest> {
    let eip1559_est = provider.estimate_eip1559_fees().await?;
    tracing::debug!(
        eip1559_est.max_priority_fee_per_gas,
        "estimated median priority fee (20% percentile) for the last 10 blocks"
    );
    if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(
            max_fee_per_gas = max_fee_per_gas,
            estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
            "L1 sender's configured maxFeePerGas is lower than the one estimated from network"
        );
    }
    if eip1559_est.max_priority_fee_per_gas > max_priority_fee_per_gas {
        tracing::warn!(
            max_priority_fee_per_gas = max_priority_fee_per_gas,
            estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
            "L1 sender's configured maxPriorityFeePerGas is lower than the one estimated from network"
        );
    }

    let tx = TransactionRequest::default()
        .with_from(operator_address)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
        .with_gas_limit(15000000);
    Ok(tx)
}
