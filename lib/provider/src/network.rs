use alloy::eips::eip2930::AccessList;
use alloy::network::{
    BuildResult, Network, NetworkTransactionBuilder, NetworkWallet, TransactionBuilder,
    TransactionBuilderError, UnbuiltTransactionError,
};
use alloy::primitives::{Address, B256, BlockHash, Bytes, ChainId, TxHash, TxKind, U256};
use alloy::providers::fillers::{
    ChainIdFiller, GasFiller, JoinFill, NonceFiller, RecommendedFillers,
};
use alloy::rpc::types::{Log, TransactionRequest};
use serde::{Deserialize, Serialize};
use zksync_os_types::{ZkReceiptEnvelope, ZkTxType};

pub use alloy::network::Ethereum;

// ─── ZKsync receipt response types ───────────────────────────────────────────
//
// These types define what the [`Zksync`] network returns from `get_transaction_receipt`.
// Historically they lived in `zksync_os_rpc_api`, but defining them here keeps this crate the
// source of truth for the `Zksync` network type; `zksync_os_rpc_api` re-exports them.

/// JSON-RPC representation of a ZKsync L2→L1 log. Used as the parameterization of
/// [`ZkReceiptEnvelope`] inside [`ZkTransactionReceipt`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2ToL1Log {
    /// Hash of the block the transaction that emitted this log was mined in
    pub block_hash: Option<BlockHash>,
    /// Number of the block the transaction that emitted this log was mined in
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_number: Option<u64>,
    /// The timestamp of the block.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub block_timestamp: Option<u64>,
    /// Transaction Hash
    #[doc(alias = "tx_hash")]
    pub transaction_hash: Option<TxHash>,
    /// Index of the Transaction in the block
    #[serde(with = "alloy::serde::quantity::opt")]
    #[doc(alias = "tx_index")]
    pub transaction_index: Option<u64>,
    /// Log Index in Block
    #[serde(with = "alloy::serde::quantity::opt")]
    pub log_index: Option<u64>,
    /// Log Index in Transaction, needed for compatibility with ZKSync Era L2->L1 log format.
    #[serde(with = "alloy::serde::quantity::opt")]
    pub transaction_log_index: Option<u64>,
    /// Deprecated, kept for compatibility, always set to 0.
    #[serde(with = "alloy::serde::quantity")]
    pub shard_id: u64,
    /// Deprecated, kept for compatibility, always set to `true`.
    pub is_service: bool,
    /// The L2 address which sent the log.
    /// For user messages set to `L1Messenger` system hook address,
    /// for l1 -> l2 txs logs - `BootloaderFormalAddress`.
    pub sender: Address,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message sender address(padded),
    /// for l1 -> l2 txs logs - transaction hash.
    pub key: B256,
    /// The 32 bytes of information that was sent in the log.
    /// For user messages used to save message hash.
    /// for l1 -> l2 txs logs - success flag(padded).
    pub value: B256,
}

impl From<L2ToL1Log> for zksync_os_types::L2ToL1Log {
    fn from(value: L2ToL1Log) -> Self {
        Self {
            l2_shard_id: value.shard_id as u8,
            is_service: value.is_service,
            tx_number_in_block: value.transaction_index.expect("Missing transaction index") as u16,
            sender: value.sender,
            key: value.key,
            value: value.value,
        }
    }
}

/// JSON-RPC transaction receipt for the [`Zksync`] network.
pub type ZkTransactionReceipt =
    alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<Log, L2ToL1Log>>;

/// A settlement layer network: either Ethereum L1 or a ZKsync Gateway chain.
///
/// Both serve the standard Ethereum JSON-RPC surface, so the associated types mirror
/// [`Ethereum`]'s wholesale — a `Provider<SettlementLayer>` is used exactly like a
/// `Provider<Ethereum>`. The network is nonetheless distinct so APIs can express "some
/// settlement layer, don't care which" at the type level, while
/// `NodeProvider::<SettlementLayer>::zksync()` recovers the ZK-typed [`Zksync`] view when the
/// underlying connection is actually a Gateway.
#[derive(Clone, Copy, Debug)]
pub struct SettlementLayer {
    _private: (),
}

impl Network for SettlementLayer {
    type TxType = alloy::consensus::TxType;

    type TxEnvelope = alloy::consensus::TxEnvelope;

    type UnsignedTx = alloy::consensus::TypedTransaction;

    type ReceiptEnvelope = alloy::consensus::ReceiptEnvelope;

    type Header = alloy::consensus::Header;

    type TransactionRequest = TransactionRequest;

    type TransactionResponse = alloy::rpc::types::Transaction;

    type ReceiptResponse = alloy::rpc::types::TransactionReceipt;

    type HeaderResponse = alloy::rpc::types::Header;

    type BlockResponse = alloy::rpc::types::Block;
}

impl RecommendedFillers for SettlementLayer {
    type RecommendedFillers = <Ethereum as RecommendedFillers>::RecommendedFillers;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Ethereum::recommended_fillers()
    }
}

/// Marker for networks whose associated types are exactly [`Ethereum`]'s — i.e. `Ethereum`
/// itself and [`SettlementLayer`]. Lets generic code that names the concrete Ethereum structs
/// (`TransactionRequest`, `Transaction`, `TransactionReceipt`, `Block`, …) stay written against
/// those structs while being generic over which of the two networks it scans.
pub trait EthereumLike:
    Network<
        TxType = alloy::consensus::TxType,
        TxEnvelope = alloy::consensus::TxEnvelope,
        UnsignedTx = alloy::consensus::TypedTransaction,
        ReceiptEnvelope = alloy::consensus::ReceiptEnvelope,
        Header = alloy::consensus::Header,
        TransactionRequest = TransactionRequest,
        TransactionResponse = alloy::rpc::types::Transaction,
        ReceiptResponse = alloy::rpc::types::TransactionReceipt,
        HeaderResponse = alloy::rpc::types::Header,
        BlockResponse = alloy::rpc::types::Block,
    >
{
}

impl<N> EthereumLike for N where
    N: Network<
            TxType = alloy::consensus::TxType,
            TxEnvelope = alloy::consensus::TxEnvelope,
            UnsignedTx = alloy::consensus::TypedTransaction,
            ReceiptEnvelope = alloy::consensus::ReceiptEnvelope,
            Header = alloy::consensus::Header,
            TransactionRequest = TransactionRequest,
            TransactionResponse = alloy::rpc::types::Transaction,
            ReceiptResponse = alloy::rpc::types::TransactionReceipt,
            HeaderResponse = alloy::rpc::types::Header,
            BlockResponse = alloy::rpc::types::Block,
        >
{
}

/// [`Network`] requires `TransactionRequest: NetworkTransactionBuilder<SettlementLayer>`; since
/// [`SettlementLayer`] reuses all of [`Ethereum`]'s associated types, every method delegates to
/// the [`Ethereum`] impl verbatim (only the network-tagged error type needs re-wrapping).
impl NetworkTransactionBuilder<SettlementLayer> for TransactionRequest {
    fn complete_type(&self, ty: alloy::consensus::TxType) -> Result<(), Vec<&'static str>> {
        <Self as NetworkTransactionBuilder<Ethereum>>::complete_type(self, ty)
    }

    fn can_submit(&self) -> bool {
        <Self as NetworkTransactionBuilder<Ethereum>>::can_submit(self)
    }

    fn can_build(&self) -> bool {
        <Self as NetworkTransactionBuilder<Ethereum>>::can_build(self)
    }

    fn output_tx_type(&self) -> alloy::consensus::TxType {
        <Self as NetworkTransactionBuilder<Ethereum>>::output_tx_type(self)
    }

    fn output_tx_type_checked(&self) -> Option<alloy::consensus::TxType> {
        <Self as NetworkTransactionBuilder<Ethereum>>::output_tx_type_checked(self)
    }

    fn prep_for_submission(&mut self) {
        <Self as NetworkTransactionBuilder<Ethereum>>::prep_for_submission(self)
    }

    fn build_unsigned(self) -> BuildResult<alloy::consensus::TypedTransaction, SettlementLayer> {
        <Self as NetworkTransactionBuilder<Ethereum>>::build_unsigned(self).map_err(|e| {
            UnbuiltTransactionError {
                request: e.request,
                error: match e.error {
                    TransactionBuilderError::InvalidTransactionRequest(tx_type, keys) => {
                        TransactionBuilderError::InvalidTransactionRequest(tx_type, keys)
                    }
                    TransactionBuilderError::UnsupportedSignatureType => {
                        TransactionBuilderError::UnsupportedSignatureType
                    }
                    TransactionBuilderError::Signer(e) => TransactionBuilderError::Signer(e),
                    TransactionBuilderError::Custom(e) => TransactionBuilderError::Custom(e),
                },
            }
        })
    }

    async fn build<W: NetworkWallet<SettlementLayer>>(
        self,
        wallet: &W,
    ) -> Result<alloy::consensus::TxEnvelope, TransactionBuilderError<SettlementLayer>> {
        Ok(wallet.sign_request(self).await?)
    }
}

/// Dummy network that works on ZKsync OS-specific types.
#[derive(Clone, Copy, Debug)]
pub struct Zksync {
    _private: (),
}

impl Network for Zksync {
    type TxType = ZkTxType;

    type TxEnvelope = alloy::consensus::TxEnvelope;

    type UnsignedTx = alloy::consensus::TypedTransaction;

    type ReceiptEnvelope = ZkReceiptEnvelope;

    type Header = alloy::consensus::Header;

    type TransactionRequest = ZkTransactionRequest;

    type TransactionResponse = alloy::rpc::types::Transaction;

    type ReceiptResponse = ZkTransactionReceipt;

    type HeaderResponse = alloy::rpc::types::Header;

    type BlockResponse = alloy::rpc::types::Block;
}

impl RecommendedFillers for Zksync {
    type RecommendedFillers = JoinFill<GasFiller, JoinFill<NonceFiller, ChainIdFiller>>;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Default::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZkTransactionRequest(TransactionRequest);

impl From<<Zksync as Network>::TxEnvelope> for ZkTransactionRequest {
    fn from(value: <Zksync as Network>::TxEnvelope) -> Self {
        Self(value.into())
    }
}

impl From<<Zksync as Network>::UnsignedTx> for ZkTransactionRequest {
    fn from(value: <Zksync as Network>::UnsignedTx) -> Self {
        Self(value.into())
    }
}

impl From<<Zksync as Network>::TransactionResponse> for ZkTransactionRequest {
    fn from(value: <Zksync as Network>::TransactionResponse) -> Self {
        Self(value.into())
    }
}

impl TransactionBuilder for ZkTransactionRequest {
    fn chain_id(&self) -> Option<ChainId> {
        <TransactionRequest as TransactionBuilder>::chain_id(&self.0)
    }

    fn set_chain_id(&mut self, chain_id: ChainId) {
        <TransactionRequest as TransactionBuilder>::set_chain_id(&mut self.0, chain_id)
    }

    fn nonce(&self) -> Option<u64> {
        <TransactionRequest as TransactionBuilder>::nonce(&self.0)
    }

    fn set_nonce(&mut self, nonce: u64) {
        <TransactionRequest as TransactionBuilder>::set_nonce(&mut self.0, nonce)
    }

    fn take_nonce(&mut self) -> Option<u64> {
        <TransactionRequest as TransactionBuilder>::take_nonce(&mut self.0)
    }

    fn input(&self) -> Option<&Bytes> {
        <TransactionRequest as TransactionBuilder>::input(&self.0)
    }

    fn set_input<T: Into<Bytes>>(&mut self, input: T) {
        <TransactionRequest as TransactionBuilder>::set_input(&mut self.0, input)
    }

    fn from(&self) -> Option<Address> {
        <TransactionRequest as TransactionBuilder>::from(&self.0)
    }

    fn set_from(&mut self, from: Address) {
        <TransactionRequest as TransactionBuilder>::set_from(&mut self.0, from)
    }

    fn kind(&self) -> Option<TxKind> {
        <TransactionRequest as TransactionBuilder>::kind(&self.0)
    }

    fn clear_kind(&mut self) {
        <TransactionRequest as TransactionBuilder>::clear_kind(&mut self.0)
    }

    fn set_kind(&mut self, kind: TxKind) {
        <TransactionRequest as TransactionBuilder>::set_kind(&mut self.0, kind)
    }

    fn value(&self) -> Option<U256> {
        <TransactionRequest as TransactionBuilder>::value(&self.0)
    }

    fn set_value(&mut self, value: U256) {
        <TransactionRequest as TransactionBuilder>::set_value(&mut self.0, value)
    }

    fn gas_price(&self) -> Option<u128> {
        <TransactionRequest as TransactionBuilder>::gas_price(&self.0)
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        <TransactionRequest as TransactionBuilder>::set_gas_price(&mut self.0, gas_price)
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        <TransactionRequest as TransactionBuilder>::max_fee_per_gas(&self.0)
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        <TransactionRequest as TransactionBuilder>::set_max_fee_per_gas(
            &mut self.0,
            max_fee_per_gas,
        )
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        <TransactionRequest as TransactionBuilder>::max_priority_fee_per_gas(&self.0)
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        <TransactionRequest as TransactionBuilder>::set_max_priority_fee_per_gas(
            &mut self.0,
            max_priority_fee_per_gas,
        )
    }

    fn gas_limit(&self) -> Option<u64> {
        <TransactionRequest as TransactionBuilder>::gas_limit(&self.0)
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        <TransactionRequest as TransactionBuilder>::set_gas_limit(&mut self.0, gas_limit)
    }

    fn access_list(&self) -> Option<&AccessList> {
        <TransactionRequest as TransactionBuilder>::access_list(&self.0)
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        <TransactionRequest as TransactionBuilder>::set_access_list(&mut self.0, access_list)
    }
}

impl NetworkTransactionBuilder<Zksync> for ZkTransactionRequest {
    fn complete_type(&self, ty: <Zksync as Network>::TxType) -> Result<(), Vec<&'static str>> {
        match ty {
            ZkTxType::L1 | ZkTxType::Upgrade | ZkTxType::System => {
                unimplemented!()
            }
            ZkTxType::L2(ty) => {
                <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::complete_type(
                    &self.0,
                    ty.into(),
                )
            }
        }
    }

    fn can_submit(&self) -> bool {
        <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::can_submit(&self.0)
    }

    fn can_build(&self) -> bool {
        <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::can_build(&self.0)
    }

    fn output_tx_type(&self) -> <Zksync as Network>::TxType {
        ZkTxType::L2(
            <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::output_tx_type(&self.0)
                .into(),
        )
    }

    fn output_tx_type_checked(&self) -> Option<<Zksync as Network>::TxType> {
        Some(ZkTxType::L2(
            <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::output_tx_type_checked(
                &self.0,
            )?
            .into(),
        ))
    }

    fn prep_for_submission(&mut self) {
        <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::prep_for_submission(
            &mut self.0,
        )
    }

    fn build_unsigned(self) -> BuildResult<<Zksync as Network>::UnsignedTx, Zksync> {
        <TransactionRequest as NetworkTransactionBuilder<Ethereum>>::build_unsigned(self.0).map_err(
            |e| UnbuiltTransactionError {
                request: Self(e.request),
                error: match e.error {
                    TransactionBuilderError::InvalidTransactionRequest(tx_type, keys) => {
                        TransactionBuilderError::InvalidTransactionRequest(
                            ZkTxType::L2(tx_type.into()),
                            keys,
                        )
                    }
                    TransactionBuilderError::UnsupportedSignatureType => {
                        TransactionBuilderError::UnsupportedSignatureType
                    }
                    TransactionBuilderError::Signer(e) => TransactionBuilderError::Signer(e),
                    TransactionBuilderError::Custom(e) => TransactionBuilderError::Custom(e),
                },
            },
        )
    }

    async fn build<W: NetworkWallet<Zksync>>(
        self,
        wallet: &W,
    ) -> Result<<Zksync as Network>::TxEnvelope, TransactionBuilderError<Zksync>> {
        Ok(wallet.sign_request(self).await?)
    }
}
