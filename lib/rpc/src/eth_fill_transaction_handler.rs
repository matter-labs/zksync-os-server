use crate::call_fees::CallFeesError;
use crate::eth_call_handler::{EthCallError, EthCallHandler, build_pending_block_context};
use crate::rpc_storage::ReadRpcStorage;
use alloy::eips::{BlockId, Encodable2718};
use alloy::primitives::U256;
use alloy::rpc::types::{FillTransaction, TransactionRequest};
use zksync_os_types::{L2Envelope, ZkEnvelope};

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    pub(crate) fn fill_transaction_impl(
        &self,
        mut request: TransactionRequest,
        pending_nonce: u64,
        fill_gas_price: U256,
    ) -> Result<FillTransaction<ZkEnvelope>, EthCallError> {
        request.normalize_input();

        if request.has_eip4844_fields() {
            return Err(EthCallError::Eip4844NotSupported);
        }
        if request.authorization_list.is_some() {
            return Err(EthCallError::Eip7702NotSupported);
        }
        if request.gas_price.is_some()
            && (request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some())
        {
            return Err(CallFeesError::ConflictingFeeFieldsInRequest.into());
        }

        if request.value.is_none() {
            request.value = Some(U256::ZERO);
        }
        if request.nonce.is_none() {
            request.nonce = Some(pending_nonce);
        }
        request.chain_id = Some(self.chain_id);

        if request.gas.is_none() {
            let estimated_gas =
                self.estimate_gas_impl(request.clone(), Some(BlockId::pending()), None)?;
            request.gas = Some(estimated_gas.saturating_to());
        }

        if request.gas_price.is_none() {
            let tip = request.max_priority_fee_per_gas.unwrap_or(0);
            if request.max_priority_fee_per_gas.is_none() {
                request.max_priority_fee_per_gas = Some(tip);
            }
            if request.max_fee_per_gas.is_none() {
                let max_fee_per_gas = fill_gas_price
                    .checked_add(U256::from(tip))
                    .ok_or(CallFeesError::TipVeryHigh)?
                    .saturating_to();
                request.max_fee_per_gas = Some(max_fee_per_gas);
            }
            if request.max_fee_per_gas.unwrap_or_default() < tip {
                return Err(CallFeesError::TipAboveFeeCap.into());
            }
        }

        let max_fee_per_gas = request.max_fee_per_gas;
        let block_context = build_pending_block_context(&self.storage, self.chain_id);
        let mut tx = self
            .create_tx_from_request(request, &block_context, false)?
            .into_envelope();
        if let (Some(max_fee_per_gas), ZkEnvelope::L2(L2Envelope::Eip1559(inner))) =
            (max_fee_per_gas, &mut tx)
        {
            inner.tx_mut().max_fee_per_gas = max_fee_per_gas;
        }
        let raw = tx.encoded_2718().into();
        Ok(FillTransaction { raw, tx })
    }
}
