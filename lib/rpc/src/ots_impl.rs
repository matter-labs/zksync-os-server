use crate::eth_impl::{EthError, EthResult, build_api_receipt, build_api_tx};
use crate::result::ToRpcResult;
use crate::rpc_storage::ReadRpcStorage;
use alloy::consensus::Typed2718;
use alloy::eips::BlockId;
use alloy::eips::eip1898::LenientBlockNumberOrTag;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, BlockHash, BlockNumber, Bytes, TxHash, U256};
use alloy::rpc::types::trace::otterscan::{
    BlockDetails, ContractCreator, InternalOperation, OtsBlock, OtsBlockTransactions, OtsReceipt,
    OtsSlimBlock, OtsTransactionReceipt, TraceEntry, TransactionsWithReceipts,
};
use alloy::rpc::types::{Header, Log};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use zk_ee::utils::Bytes32;
use zksync_os_rpc_api::ots::OtsApiServer;
use zksync_os_rpc_api::types::{RpcBlockConvert, ZkApiTransaction};
use zksync_os_storage_api::{StoredTxData, ViewState};
use zksync_os_types::{L1PriorityTxType, L1TxType, UpgradeTxType, ZkReceiptEnvelope};

/// Max Otterscan API level we support.
const API_LEVEL: u64 = 8;

/// Maximum number of blocks to scan in `ots_searchTransactionsBefore` and `ots_searchTransactionsAfter`.
/// We do not have proper indices on tx's from/to so the implementation employs linear scanning
/// which works for simple cases that we currently use Otterscan for.
const MAX_BLOCKS_TO_SCAN: u64 = 1000;

pub struct OtsNamespace<RpcStorage> {
    storage: RpcStorage,
}

impl<RpcStorage> OtsNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage) -> Self {
        Self { storage }
    }
}

impl<RpcStorage: ReadRpcStorage> OtsNamespace<RpcStorage> {
    fn get_header_by_number_impl(
        &self,
        block_number: LenientBlockNumberOrTag,
    ) -> EthResult<Option<Header>> {
        let Some(block) = self.storage.get_block_by_id(block_number.into())? else {
            return Ok(None);
        };
        Ok(Some(block.into_rpc().header))
    }

    fn has_code_impl(
        &self,
        address: Address,
        block_id: Option<LenientBlockNumberOrTag>,
    ) -> EthResult<bool> {
        // todo(#36): re-implement, move to a state handler
        let block_id = block_id.unwrap_or_default().into();
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthError::BlockNotFound(block_id));
        };

        // todo(#36): distinguish between N/A blocks and actual missing accounts
        let mut view = self.storage.state_view_at(block_number)?;
        let Some(props) = view.get_account(B160::from_be_bytes(address.into_array())) else {
            return Ok(false);
        };
        Ok(props.bytecode_hash != Bytes32::ZERO)
    }

    fn get_block_details_by_id_impl(&self, block_id: BlockId) -> EthResult<BlockDetails> {
        let block = self
            .storage
            .get_block_by_id(block_id)?
            .ok_or(EthError::BlockNotFound(block_id))?;
        let block = block.into_rpc();
        let transaction_count = block.transactions.len();
        let mut total_fees = U256::ZERO;
        for tx_hash in block.transactions.hashes() {
            let meta = self
                .storage
                .repository()
                .get_transaction_meta(tx_hash)?
                .ok_or(EthError::BlockNotFound(block_id))?;
            total_fees += U256::from(meta.gas_used) * U256::from(meta.effective_gas_price);
        }
        Ok(BlockDetails {
            block: OtsSlimBlock {
                header: block.header,
                uncles: vec![],
                withdrawals: None,
                transaction_count,
            },
            issuance: Default::default(),
            total_fees,
        })
    }

    fn get_block_transactions_impl(
        &self,
        block_number: LenientBlockNumberOrTag,
        page_number: usize,
        page_size: usize,
    ) -> EthResult<OtsBlockTransactions<ZkApiTransaction>> {
        let block_id = block_number.into();
        let block = self
            .storage
            .get_block_by_id(block_id)?
            .ok_or(EthError::BlockNotFound(block_id))?;
        let block = block.into_rpc();
        let transaction_count = block.transactions.len();

        // Crop page
        let page_end = transaction_count.saturating_sub(page_number * page_size);
        let page_start = page_end.saturating_sub(page_size);

        // Crop transactions
        let transactions = block
            .transactions
            .hashes()
            .skip(page_start)
            .take(page_size)
            .collect::<Vec<_>>();

        let mut full_transactions = Vec::with_capacity(transactions.len());
        let mut receipts = Vec::with_capacity(transactions.len());
        for tx_hash in transactions {
            let StoredTxData { tx, receipt, meta } = self
                .storage
                .repository()
                .get_stored_transaction(tx_hash)?
                .ok_or(EthError::BlockNotFound(block_id))?;

            let receipt = build_api_receipt(tx_hash, receipt, &tx, &meta).map_inner(|receipt| {
                let r#type = api_receipt_type(&receipt);
                // Report logs/logs_bloom as `null` here to avoid unnecessary network traffic.
                // See https://docs.otterscan.io/api-docs/ots-api#ots_getblocktransactions
                OtsReceipt {
                    status: receipt.status(),
                    cumulative_gas_used: receipt.cumulative_gas_used(),
                    logs: None,
                    logs_bloom: None,
                    r#type,
                }
            });
            full_transactions.push(build_api_tx(tx, Some(&meta)));
            receipts.push(OtsTransactionReceipt {
                receipt,
                timestamp: Some(block.header.timestamp),
            });
        }

        Ok(OtsBlockTransactions {
            fullblock: OtsBlock {
                block: block.with_transactions(BlockTransactions::Full(full_transactions)),
                transaction_count,
            },
            receipts,
        })
    }

    fn search_transactions_impl(
        &self,
        address: Address,
        block_number_iter: impl IntoIterator<Item = BlockNumber>,
        page_size: usize,
    ) -> EthResult<(Vec<ZkApiTransaction>, Vec<OtsTransactionReceipt>, bool)> {
        let mut txs = Vec::with_capacity(page_size);
        let mut receipts = Vec::with_capacity(page_size);
        let mut has_more = false;
        for block_number in block_number_iter {
            if txs.len() >= page_size {
                has_more = true;
                break;
            }
            let Some(block) = self
                .storage
                .repository()
                .get_block_by_number(block_number)?
            else {
                break;
            };
            let (block, _) = block.into_parts();
            for tx_hash in block.body.transactions {
                let Some(StoredTxData { tx, receipt, meta }) =
                    self.storage.repository().get_stored_transaction(tx_hash)?
                else {
                    continue;
                };
                if !(tx.signer() == address || tx.to() == Some(address)) {
                    continue;
                }

                let receipt =
                    build_api_receipt(tx_hash, receipt, &tx, &meta).map_inner(|receipt| {
                        let logs_bloom = Some(*receipt.logs_bloom());
                        let r#type = api_receipt_type(&receipt);
                        OtsReceipt {
                            status: receipt.status(),
                            cumulative_gas_used: receipt.cumulative_gas_used(),
                            logs: Some(receipt.into_logs()),
                            logs_bloom,
                            r#type,
                        }
                    });

                txs.push(build_api_tx(tx, Some(&meta)));
                receipts.push(OtsTransactionReceipt {
                    receipt,
                    timestamp: Some(block.header.timestamp),
                });
            }
        }
        Ok((txs, receipts, has_more))
    }

    fn search_transactions_before_impl(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> EthResult<TransactionsWithReceipts<ZkApiTransaction>> {
        let block_id = block_number.into();
        let mut upper_block_number = self
            .storage
            .resolve_block_number(block_id)?
            .ok_or(EthError::BlockNotFound(block_id))?;
        let latest_block_number = self.storage.repository().get_latest_block();
        if upper_block_number == 0 {
            upper_block_number = latest_block_number + 1;
        }
        let lower_block_number = upper_block_number.saturating_sub(MAX_BLOCKS_TO_SCAN);
        let (txs, receipts, has_more) = self.search_transactions_impl(
            address,
            (lower_block_number..upper_block_number).rev(),
            page_size,
        )?;
        Ok(TransactionsWithReceipts {
            txs,
            receipts,
            first_page: upper_block_number == latest_block_number,
            last_page: !has_more,
        })
    }

    fn search_transactions_after_impl(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> EthResult<TransactionsWithReceipts<ZkApiTransaction>> {
        let block_id = block_number.into();
        // Logic below uses inclusive lower bound so we add +1 here to adjust the boundary
        let lower_block_number = self
            .storage
            .resolve_block_number(block_id)?
            .ok_or(EthError::BlockNotFound(block_id))?
            + 1;
        let latest_block_number = self.storage.repository().get_latest_block();
        let upper_block_number = lower_block_number
            .saturating_add(MAX_BLOCKS_TO_SCAN)
            .min(latest_block_number);

        let (mut txs, mut receipts, has_more) = self.search_transactions_impl(
            address,
            lower_block_number..upper_block_number,
            page_size,
        )?;
        txs.reverse();
        receipts.reverse();
        Ok(TransactionsWithReceipts {
            txs,
            receipts,
            first_page: !has_more,
            last_page: lower_block_number == 1,
        })
    }

    fn get_transaction_by_sender_and_nonce_impl(
        &self,
        sender: Address,
        nonce: u64,
    ) -> EthResult<Option<TxHash>> {
        let Some(tx_hash) = self
            .storage
            .repository()
            .get_transaction_hash_by_sender_nonce(sender, nonce)?
        else {
            return Ok(None);
        };
        Ok(Some(tx_hash))
    }

    fn get_contract_creator_impl(&self, _address: Address) -> EthResult<Option<ContractCreator>> {
        // Unclear how to implement this as we have limited historical state available to us.
        // This method being unimplemented has limited impact - contracts' creator and deployment
        // information is not immediately accessible on Otterscan frontend.
        Ok(None)
    }
}

#[async_trait]
impl<Repository: ReadRpcStorage> OtsApiServer for OtsNamespace<Repository> {
    async fn get_header_by_number(
        &self,
        block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<Option<Header>> {
        self.get_header_by_number_impl(block_number).to_rpc_result()
    }

    async fn has_code(
        &self,
        address: Address,
        block_id: Option<LenientBlockNumberOrTag>,
    ) -> RpcResult<bool> {
        self.has_code_impl(address, block_id).to_rpc_result()
    }

    async fn get_api_level(&self) -> RpcResult<u64> {
        Ok(API_LEVEL)
    }

    async fn get_internal_operations(&self, _tx_hash: TxHash) -> RpcResult<Vec<InternalOperation>> {
        // todo(tracing)
        Ok(vec![])
    }

    async fn get_transaction_error(&self, _tx_hash: TxHash) -> RpcResult<Option<Bytes>> {
        // todo: consider saving tx's output or replaying the block here
        Ok(None)
    }

    async fn trace_transaction(&self, _tx_hash: TxHash) -> RpcResult<Option<Vec<TraceEntry>>> {
        // todo(tracing)
        Ok(Some(vec![]))
    }

    async fn get_block_details(
        &self,
        block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<BlockDetails> {
        self.get_block_details_by_id_impl(block_number.into())
            .to_rpc_result()
    }

    async fn get_block_details_by_hash(&self, block_hash: BlockHash) -> RpcResult<BlockDetails> {
        self.get_block_details_by_id_impl(block_hash.into())
            .to_rpc_result()
    }

    async fn get_block_transactions(
        &self,
        block_number: LenientBlockNumberOrTag,
        page_number: usize,
        page_size: usize,
    ) -> RpcResult<OtsBlockTransactions<ZkApiTransaction>> {
        self.get_block_transactions_impl(block_number, page_number, page_size)
            .to_rpc_result()
    }

    async fn search_transactions_before(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts<ZkApiTransaction>> {
        self.search_transactions_before_impl(address, block_number, page_size)
            .to_rpc_result()
    }

    async fn search_transactions_after(
        &self,
        address: Address,
        block_number: LenientBlockNumberOrTag,
        page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts<ZkApiTransaction>> {
        self.search_transactions_after_impl(address, block_number, page_size)
            .to_rpc_result()
    }

    async fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> RpcResult<Option<TxHash>> {
        self.get_transaction_by_sender_and_nonce_impl(sender, nonce)
            .to_rpc_result()
    }

    async fn get_contract_creator(&self, address: Address) -> RpcResult<Option<ContractCreator>> {
        self.get_contract_creator_impl(address).to_rpc_result()
    }
}

fn api_receipt_type(receipt: &ZkReceiptEnvelope<Log>) -> u8 {
    match receipt {
        ZkReceiptEnvelope::Upgrade(_) => UpgradeTxType::TX_TYPE,
        ZkReceiptEnvelope::L1(_) => L1PriorityTxType::TX_TYPE,
        r => r.tx_type().ty(),
    }
}
