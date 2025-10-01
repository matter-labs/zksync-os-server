use alloy::consensus::private::alloy_primitives;
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{BlobTransactionValidationError, Transaction, Typed2718};
use alloy::eips::Encodable2718;
use alloy::eips::eip2930::AccessList;
use alloy::eips::eip4844::env_settings::KzgSettings;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::eip7702::SignedAuthorization;
use alloy::primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use reth_primitives_traits::InMemorySize;
use reth_transaction_pool::{EthBlobTransactionSidecar, EthPoolTransaction, PoolTransaction};
use std::convert::Infallible;
use std::sync::Arc;
use zksync_os_types::{L2Envelope, L2Transaction};

/// ZKsync OS version of reth's [`reth_transaction_pool::EthPooledTransaction`]. Re-implements most
/// of the logic but with extra flexibility for custom transaction types.
///
/// Blob sidecar is currently ignored but was copied over to be future-proof when we'll need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L2PooledTransaction {
    /// `EcRecovered` transaction, the consensus format.
    pub transaction: L2Transaction,

    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    /// For EIP-4844 blob transactions: `max_fee_per_gas * gas_limit + tx_value +
    /// max_blob_fee_per_gas * blob_gas_used`.
    pub cost: U256,

    /// This is the RLP length of the transaction, computed when the transaction is added to the
    /// pool.
    pub encoded_length: usize,

    /// The blob side car for this transaction
    pub blob_sidecar: EthBlobTransactionSidecar,
}

impl L2PooledTransaction {
    /// Create new instance of [Self].
    ///
    /// Caution: In case of blob transactions, this does marks the blob sidecar as
    /// [`EthBlobTransactionSidecar::Missing`]
    pub fn new(transaction: L2Transaction, encoded_length: usize) -> Self {
        let mut blob_sidecar = EthBlobTransactionSidecar::None;

        let gas_cost = U256::from(transaction.max_fee_per_gas())
            .saturating_mul(U256::from(transaction.gas_limit()));

        let mut cost = gas_cost.saturating_add(transaction.value());

        if let (Some(blob_gas_used), Some(max_fee_per_blob_gas)) = (
            transaction.blob_gas_used(),
            transaction.max_fee_per_blob_gas(),
        ) {
            // Add max blob cost using saturating math to avoid overflow
            cost = cost.saturating_add(U256::from(
                max_fee_per_blob_gas.saturating_mul(blob_gas_used as u128),
            ));

            // because the blob sidecar is not included in this transaction variant, mark it as
            // missing
            blob_sidecar = EthBlobTransactionSidecar::Missing;
        }

        Self {
            transaction,
            cost,
            encoded_length,
            blob_sidecar,
        }
    }

    /// Return the reference to the underlying transaction.
    pub const fn transaction(&self) -> &L2Transaction {
        &self.transaction
    }
}

impl PoolTransaction for L2PooledTransaction {
    type TryFromConsensusError = Infallible;

    type Consensus = L2Envelope;

    type Pooled = L2Envelope;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.transaction().clone()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.transaction
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        let encoded_length = tx.encode_2718_len();
        // todo(EIP-4844): uncomment when we support EIP-4844 transactions.
        // let (tx, signer) = tx.into_parts();
        // match tx {
        //     PooledTransactionVariant::Eip4844(tx) => {
        //         // include the blob sidecar
        //         let (tx, sig, hash) = tx.into_parts();
        //         let (tx, blob) = tx.into_parts();
        //         let tx = Signed::new_unchecked(tx, sig, hash);
        //         let tx = TransactionSigned::from(tx);
        //         let tx = Recovered::new_unchecked(tx, signer);
        //         let mut pooled = Self::new(tx, encoded_length);
        //         pooled.blob_sidecar = EthBlobTransactionSidecar::Present(blob);
        //         pooled
        //     }
        //     tx => {
        //         // no blob sidecar
        //         let tx = Recovered::new_unchecked(tx.into(), signer);
        //         Self::new(tx, encoded_length)
        //     }
        // }
        Self::new(tx, encoded_length)
    }

    /// Returns hash of the transaction.
    fn hash(&self) -> &TxHash {
        self.transaction.tx_hash()
    }

    /// Returns the Sender of the transaction.
    fn sender(&self) -> Address {
        self.transaction.signer()
    }

    /// Returns a reference to the Sender of the transaction.
    fn sender_ref(&self) -> &Address {
        self.transaction.signer_ref()
    }

    /// Returns the cost that this transaction is allowed to consume:
    ///
    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    /// For EIP-4844 blob transactions: `max_fee_per_gas * gas_limit + tx_value +
    /// max_blob_fee_per_gas * blob_gas_used`.
    fn cost(&self) -> &U256 {
        &self.cost
    }

    /// Returns the length of the rlp encoded object
    fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

impl Typed2718 for L2PooledTransaction {
    fn ty(&self) -> u8 {
        self.transaction.ty()
    }
}

impl InMemorySize for L2PooledTransaction {
    fn size(&self) -> usize {
        self.transaction.size()
    }
}

impl alloy::consensus::Transaction for L2PooledTransaction {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        self.transaction.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.transaction.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.transaction.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.transaction.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.transaction.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.transaction.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.transaction.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.transaction.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.transaction.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.transaction.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.transaction.kind()
    }

    fn is_create(&self) -> bool {
        self.transaction.is_create()
    }

    fn value(&self) -> U256 {
        self.transaction.value()
    }

    fn input(&self) -> &Bytes {
        self.transaction.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.transaction.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.transaction.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.transaction.authorization_list()
    }
}

impl EthPoolTransaction for L2PooledTransaction {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        if self.is_eip4844() {
            std::mem::replace(&mut self.blob_sidecar, EthBlobTransactionSidecar::Missing)
        } else {
            EthBlobTransactionSidecar::None
        }
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        unimplemented!("EIP-4844 transactions are not supported yet")
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        unimplemented!("EIP-4844 transactions are not supported yet")
    }

    fn validate_blob(
        &self,
        _sidecar: &BlobTransactionSidecarVariant,
        _settings: &KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        unimplemented!("EIP-4844 transactions are not supported yet")
    }
}
