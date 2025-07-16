use alloy::consensus::transaction::Recovered;
use alloy::consensus::{EthereumTxEnvelope, TxEip4844Variant};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;

/// L2 transaction with a known signer (usually EC recovered or simulated). Unlike alloy/reth we
/// mostly operate on this type as ZKsync OS expects signer to be provided externally (e.g., from the
/// sequencer). This could change in the future.
pub type L2Transaction = Recovered<L2Envelope>;

/// ZKsync OS reuses the main `alloy` envelope to enforce compatibility with Ethereum. This type
/// describes all transactions that are executable on L2.
///
/// Although ZKsync OS does not support EIP-4844 transactions right now, we future-proof by using a
/// sidecar-agnostic EIP-4844 variant. Moreover, sidecar itself can have one of two forms: EIP-4844
/// or EIP-7594.
pub type L2Envelope = EthereumTxEnvelope<TxEip4844Variant<BlobTransactionSidecarVariant>>;
