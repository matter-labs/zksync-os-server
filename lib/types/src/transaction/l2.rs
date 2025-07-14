use alloy::consensus::transaction::Recovered;
use alloy::consensus::{EthereumTxEnvelope, TxEip4844Variant};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;

// TODO: document
pub type L2Transaction<Eip4844 = TxEip4844Variant<BlobTransactionSidecarVariant>> =
    Recovered<crate::L2Envelope<Eip4844>>;

// TODO: document
pub type L2Envelope<Eip4844 = TxEip4844Variant<BlobTransactionSidecarVariant>> =
    EthereumTxEnvelope<Eip4844>;
