use alloy::eips::eip2930::AccessList;
use alloy::network::{
    BuildResult, Network, NetworkWallet, TransactionBuilder, TransactionBuilderError,
    UnbuiltTransactionError,
};
use alloy::primitives::{Address, Bytes, ChainId, TxKind, U256};
use alloy::providers::fillers::{
    ChainIdFiller, GasFiller, JoinFill, NonceFiller, RecommendedFillers,
};
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use zksync_os_types::{ZkReceiptEnvelope, ZkTxType};

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

    type ReceiptResponse =
        alloy::rpc::types::TransactionReceipt<ZkReceiptEnvelope<alloy::rpc::types::Log>>;

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

impl TransactionBuilder<Zksync> for ZkTransactionRequest {
    fn chain_id(&self) -> Option<ChainId> {
        TransactionBuilder::chain_id(&self.0)
    }

    fn set_chain_id(&mut self, chain_id: ChainId) {
        TransactionBuilder::set_chain_id(&mut self.0, chain_id)
    }

    fn nonce(&self) -> Option<u64> {
        TransactionBuilder::nonce(&self.0)
    }

    fn set_nonce(&mut self, nonce: u64) {
        TransactionBuilder::set_nonce(&mut self.0, nonce)
    }

    fn take_nonce(&mut self) -> Option<u64> {
        TransactionBuilder::take_nonce(&mut self.0)
    }

    fn input(&self) -> Option<&Bytes> {
        TransactionBuilder::input(&self.0)
    }

    fn set_input<T: Into<Bytes>>(&mut self, input: T) {
        TransactionBuilder::set_input(&mut self.0, input)
    }

    fn from(&self) -> Option<Address> {
        TransactionBuilder::from(&self.0)
    }

    fn set_from(&mut self, from: Address) {
        TransactionBuilder::set_from(&mut self.0, from)
    }

    fn kind(&self) -> Option<TxKind> {
        TransactionBuilder::kind(&self.0)
    }

    fn clear_kind(&mut self) {
        TransactionBuilder::clear_kind(&mut self.0)
    }

    fn set_kind(&mut self, kind: TxKind) {
        TransactionBuilder::set_kind(&mut self.0, kind)
    }

    fn value(&self) -> Option<U256> {
        TransactionBuilder::value(&self.0)
    }

    fn set_value(&mut self, value: U256) {
        TransactionBuilder::set_value(&mut self.0, value)
    }

    fn gas_price(&self) -> Option<u128> {
        TransactionBuilder::gas_price(&self.0)
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        TransactionBuilder::set_gas_price(&mut self.0, gas_price)
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        TransactionBuilder::max_fee_per_gas(&self.0)
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        TransactionBuilder::set_max_fee_per_gas(&mut self.0, max_fee_per_gas)
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        TransactionBuilder::max_priority_fee_per_gas(&self.0)
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        TransactionBuilder::set_max_priority_fee_per_gas(&mut self.0, max_priority_fee_per_gas)
    }

    fn gas_limit(&self) -> Option<u64> {
        TransactionBuilder::gas_limit(&self.0)
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        TransactionBuilder::set_gas_limit(&mut self.0, gas_limit)
    }

    fn access_list(&self) -> Option<&AccessList> {
        TransactionBuilder::access_list(&self.0)
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        TransactionBuilder::set_access_list(&mut self.0, access_list)
    }

    fn complete_type(&self, ty: <Zksync as Network>::TxType) -> Result<(), Vec<&'static str>> {
        match ty {
            ZkTxType::L1 | ZkTxType::Upgrade => {
                unimplemented!()
            }
            ZkTxType::L2(ty) => TransactionBuilder::complete_type(&self.0, ty.into()),
        }
    }

    fn can_submit(&self) -> bool {
        TransactionBuilder::can_submit(&self.0)
    }

    fn can_build(&self) -> bool {
        TransactionBuilder::can_build(&self.0)
    }

    fn output_tx_type(&self) -> <Zksync as Network>::TxType {
        ZkTxType::L2(TransactionBuilder::output_tx_type(&self.0).into())
    }

    fn output_tx_type_checked(&self) -> Option<<Zksync as Network>::TxType> {
        Some(ZkTxType::L2(
            TransactionBuilder::output_tx_type_checked(&self.0)?.into(),
        ))
    }

    fn prep_for_submission(&mut self) {
        TransactionBuilder::prep_for_submission(&mut self.0)
    }

    fn build_unsigned(self) -> BuildResult<<Zksync as Network>::UnsignedTx, Zksync> {
        TransactionBuilder::build_unsigned(self.0).map_err(|e| UnbuiltTransactionError {
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
        })
    }

    async fn build<W: NetworkWallet<Zksync>>(
        self,
        wallet: &W,
    ) -> Result<<Zksync as Network>::TxEnvelope, TransactionBuilderError<Zksync>> {
        Ok(wallet.sign_request(self).await?)
    }
}
