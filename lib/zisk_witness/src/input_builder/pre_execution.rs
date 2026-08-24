//! REVM pre-execution pass discovering every storage read the batch performs.

use super::{
    AccountInfo, Address, B256, BlockOutput, Bytecode, Bytes, CacheDB, DBErrorMarker, DatabaseRef,
    HashMap, HashSet, KECCAK_EMPTY, PreExecutionReads, RefCell, TxAuth, TxInput, TxKind, U256,
    ViewState, ZKsyncTx, ZKsyncTxBuilder, ZkSpecId, zisk_merkle,
};
use revm::ExecuteCommitEvm;
use zksync_os_revm::{ZkBuilder, zk_context};
#[allow(clippy::too_many_arguments)]
pub(super) fn pre_execute_for_reads(
    ctx: &zksync_os_storage_api::BlockContext,
    spec_id: ZkSpecId,
    basefee: u64,
    prev_randao: B256,
    transactions: &[TxInput],
    block_output: &BlockOutput,
    accounts: &HashMap<Address, AccountInfo>,
    storage_prestate: &HashMap<(Address, U256), U256>,
    bytecodes: &HashMap<B256, Bytecode>,
    state_view: impl ViewState,
) -> PreExecutionReads {
    let read_keys: RefCell<HashSet<B256>> = RefCell::new(HashSet::new());
    let read_accounts: RefCell<HashSet<Address>> = RefCell::new(HashSet::new());
    let read_storage: RefCell<Vec<(Address, U256, U256)>> = RefCell::new(Vec::new());
    let resolved_preimages: RefCell<Vec<(B256, Vec<u8>)>> = RefCell::new(Vec::new());

    let tracking_db = TrackingDB {
        accounts,
        storage_prestate,
        state_view: RefCell::new(state_view),
        bytecodes,
        block_hashes: &ctx.block_hashes,
        block_number: ctx.block_number,
        read_keys: &read_keys,
        read_accounts: &read_accounts,
        read_storage: &read_storage,
        resolved_preimages: &resolved_preimages,
    };

    let cache_db = CacheDB::new(tracking_db);
    run_pre_execution(
        BlockEnv {
            chain_id: ctx.chain_id,
            spec_id,
            block_number: ctx.block_number,
            timestamp: ctx.timestamp,
            coinbase: ctx.coinbase,
            basefee,
            gas_limit: ctx.gas_limit,
            prev_randao,
            transactions,
        },
        block_output,
        cache_db,
    );

    PreExecutionReads {
        keys: read_keys.into_inner(),
        addresses: read_accounts.into_inner(),
        storage: read_storage.into_inner(),
        preimages: resolved_preimages.into_inner(),
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Merkle proof extraction
// ---------------------------------------------------------------------------

pub(super) struct TrackingDB<'a, S> {
    accounts: &'a HashMap<Address, AccountInfo>,
    storage_prestate: &'a HashMap<(Address, U256), U256>,
    state_view: RefCell<S>,
    bytecodes: &'a HashMap<B256, Bytecode>,
    block_hashes: &'a zksync_os_storage_api::BlockHashes,
    block_number: u64,
    read_keys: &'a RefCell<HashSet<B256>>,
    read_accounts: &'a RefCell<HashSet<Address>>,
    /// Storage reads captured during pre-execution: (address, slot, value).
    read_storage: &'a RefCell<Vec<(Address, U256, U256)>>,
    /// Bytecode preimages resolved from state_view during pre-execution.
    /// These need to be included in extra_bytecodes.
    resolved_preimages: &'a RefCell<Vec<(B256, Vec<u8>)>>,
}

#[derive(Debug)]
pub(super) struct TrackingDBError;
impl core::fmt::Display for TrackingDBError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "tracking db")
    }
}
impl std::error::Error for TrackingDBError {}
impl DBErrorMarker for TrackingDBError {}

impl<S: ViewState> DatabaseRef for TrackingDB<'_, S> {
    type Error = TrackingDBError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read_accounts.borrow_mut().insert(address);
        if let Some(info) = self.accounts.get(&address) {
            return Ok(Some(info.clone()));
        }
        // Fallback: try loading from state_view (post-execution state for upgrade blocks).
        // Pre-load bytecode just like the server's REVM adapter does, since code_by_hash_ref
        // can't resolve keccak256 hashes from the blake2s-keyed preimage DB.
        let mut sv = self.state_view.borrow_mut();
        if let Some(props) = sv.get_account(address) {
            let obs_hash = B256::from(props.observable_bytecode_hash.as_u8_array());
            let pre_hash = B256::from(props.bytecode_hash.as_u8_array());
            let effective = if obs_hash.is_zero() {
                if props.nonce == 0 && props.balance == U256::ZERO {
                    return Ok(None);
                }
                KECCAK_EMPTY
            } else {
                obs_hash
            };
            // Pre-load bytecode via blake2s hash (the preimage DB key).
            let code = if !pre_hash.is_zero() {
                sv.get_preimage(pre_hash).map(|padded_code| {
                    let raw_len = props.unpadded_code_len as usize;
                    let raw = if raw_len > 0 && raw_len <= padded_code.len() {
                        &padded_code[..raw_len]
                    } else {
                        &padded_code[..]
                    };
                    Bytecode::new_raw(Bytes::copy_from_slice(raw))
                })
            } else {
                None
            };
            return Ok(Some(AccountInfo {
                nonce: props.nonce,
                balance: props.balance,
                code_hash: effective,
                code,
                account_id: None,
            }));
        }
        Ok(None)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(bytecode) = self.bytecodes.get(&code_hash) {
            return Ok(bytecode.clone());
        }
        // Fallback: try preimage DB via state_view (like the server's OverriddenStateView).
        if code_hash != KECCAK_EMPTY
            && code_hash != B256::ZERO
            && let Some(preimage) = self.state_view.borrow_mut().get_preimage(code_hash)
            && !preimage.is_empty()
        {
            self.resolved_preimages
                .borrow_mut()
                .push((code_hash, preimage.clone()));
            return Ok(Bytecode::new_raw(Bytes::copy_from_slice(&preimage)));
        }
        Ok(Bytecode::default())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let flat_key = zisk_merkle::derive_flat_storage_key(
            &address.into_array(),
            &B256::from(index.to_be_bytes::<32>()),
        );
        self.read_keys.borrow_mut().insert(flat_key);

        let val = if let Some(&val) = self.storage_prestate.get(&(address, index)) {
            val
        } else {
            self.state_view
                .borrow_mut()
                .read(flat_key)
                .map(|v| U256::from_be_bytes(v.0))
                .unwrap_or(U256::ZERO)
        };
        // Capture read to seed the next pre-execution iteration's prestate.
        self.read_storage.borrow_mut().push((address, index, val));
        Ok(val)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if self.block_number > 0 && number < self.block_number {
            let idx = 256usize.saturating_sub((self.block_number - number) as usize);
            if idx < 256 {
                return Ok(B256::from(self.block_hashes.0[idx].to_be_bytes::<32>()));
            }
        }
        Ok(B256::ZERO)
    }
}

/// The block environment the pre-execution pass replays against.
pub(super) struct BlockEnv<'a> {
    pub chain_id: u64,
    pub spec_id: ZkSpecId,
    pub block_number: u64,
    pub timestamp: u64,
    pub coinbase: Address,
    pub basefee: u64,
    pub gas_limit: u64,
    pub prev_randao: B256,
    pub transactions: &'a [TxInput],
}

pub(super) fn run_pre_execution<DB: DatabaseRef>(
    env: BlockEnv<'_>,
    block_output: &BlockOutput,
    mut cache_db: CacheDB<DB>,
) where
    DB::Error: core::fmt::Debug,
{
    let BlockEnv {
        chain_id,
        spec_id,
        block_number,
        timestamp,
        coinbase,
        basefee,
        gas_limit,
        prev_randao,
        transactions,
    } = env;
    let mut evm = zk_context(&mut cache_db, spec_id)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
            cfg.spec = spec_id;
        })
        .modify_block_chained(|blk| {
            blk.number = U256::from(block_number);
            blk.timestamp = U256::from(timestamp);
            blk.beneficiary = coinbase;
            blk.basefee = basefee;
            blk.gas_limit = gas_limit;
            blk.prevrandao = Some(prev_randao);
        })
        .build_zk();

    for (i, tx_input) in transactions.iter().enumerate() {
        let (gas_override, force_fail) = match block_output.tx_results.get(i) {
            Some(Ok(o)) => (Some(o.gas_used), false),
            Some(Err(_)) => (Some(0), true),
            None => (None, false),
        };
        // Decode tx fields from the authenticated source, matching the guest's logic.
        let (
            caller,
            kind,
            value,
            data,
            nonce,
            gas_limit,
            gas_price,
            gas_priority_fee,
            chain_id,
            tx_type,
            mint,
            refund_recipient,
            tx_hash,
        ) = match &tx_input.auth {
            TxAuth::L1 {
                tx_hash,
                abi_encoded,
            }
            | TxAuth::Upgrade {
                tx_hash,
                abi_encoded,
            } => {
                let w = |f: usize| {
                    alloy::primitives::U256::from_be_slice(
                        &abi_encoded[32 + f * 32..32 + (f + 1) * 32],
                    )
                };
                let a = |f: usize| Address::from_slice(&w(f).to_be_bytes::<32>()[12..]);
                let raw_gl: u64 = w(3).to();
                let tt: u8 = w(0).to();
                let gl = if tt == 0x7e {
                    raw_gl.saturating_mul(10)
                } else {
                    raw_gl
                };
                let data_rel: usize = w(14).to();
                let data_abs = 32 + data_rel;
                let data_len: usize =
                    alloy::primitives::U256::from_be_slice(&abi_encoded[data_abs..data_abs + 32])
                        .to();
                let data = abi_encoded[data_abs + 32..data_abs + 32 + data_len].to_vec();
                // Always pass the recipient, zero address included: the Atlas
                // handler requires one for every L1->L2 tx (the consistency
                // checker passes it unconditionally too).
                (
                    a(1),
                    TxKind::Call(a(2)),
                    w(9),
                    data,
                    w(8).to::<u64>(),
                    gl,
                    w(5).to::<u128>(),
                    None,
                    tx_input.chain_id,
                    tt,
                    w(10),
                    Some(a(11)),
                    *tx_hash,
                )
            }
            TxAuth::L2 { signed_bytes } => {
                use alloy::consensus::Transaction;
                use alloy::consensus::TxEnvelope;
                use alloy::eips::Decodable2718;
                let env = TxEnvelope::decode_2718(&mut &signed_bytes[..]).expect("decode");
                let signer = alloy::consensus::transaction::SignerRecoverable::recover_signer(&env)
                    .expect("ecrecover");
                let k = match env.to() {
                    Some(a) => TxKind::Call(a),
                    None => TxKind::Create,
                };
                let h = alloy::primitives::keccak256(signed_bytes);
                (
                    signer,
                    k,
                    env.value(),
                    env.input().to_vec(),
                    env.nonce(),
                    env.gas_limit(),
                    env.max_fee_per_gas(),
                    env.max_priority_fee_per_gas(),
                    env.chain_id().or(tx_input.chain_id),
                    env.tx_type() as u8,
                    U256::ZERO,
                    None,
                    h,
                )
            }
            TxAuth::System {
                tx_hash,
                encoded_2718,
            } => {
                // Mirrors the consistency checker's System arm: bootloader
                // caller, zero fees/value/nonce, block gas limit (the tx's
                // own is zero), service tx type 0x7d.
                use alloy::consensus::Transaction;
                use alloy::eips::Decodable2718;
                let env = zksync_os_types::SystemTxEnvelope::decode_2718(&mut &encoded_2718[..])
                    .expect("decode system tx");
                let to = env.to().expect("system tx always has `to`");
                (
                    zksync_os_types::BOOTLOADER_FORMAL_ADDRESS,
                    TxKind::Call(to),
                    U256::ZERO,
                    env.input().to_vec(),
                    0,
                    gas_limit,
                    0,
                    Some(0),
                    None,
                    zksync_os_types::SYSTEM_TX_TYPE_ID,
                    U256::ZERO,
                    None,
                    *tx_hash,
                )
            }
        };
        let mut b = revm::context::TxEnv::builder()
            .caller(caller)
            .gas_limit(gas_limit)
            .gas_price(gas_price)
            .kind(kind)
            .value(value)
            .data(Bytes::from(data))
            .nonce(nonce)
            .tx_type(Some(tx_type))
            .chain_id(chain_id)
            .blob_hashes(vec![]);
        if let Some(fee) = gas_priority_fee {
            b = b.gas_priority_fee(Some(fee));
        }
        let tx: ZKsyncTx<revm::context::TxEnv> = ZKsyncTxBuilder::new()
            .base(b)
            .mint(mint)
            .refund_recipient(refund_recipient)
            .gas_used_override(gas_override)
            .force_fail(force_fail)
            .tx_hash(tx_hash)
            .build()
            .expect("tx build failed");
        match evm.transact_commit(tx) {
            Ok(result) => {
                if matches!(tx_input.auth, TxAuth::Upgrade { .. }) {
                    tracing::info!(
                        success = result.is_success(),
                        gas_used = result.tx_gas_used(),
                        "pre-execution upgrade tx result"
                    );
                }
            }
            Err(e) => {
                if matches!(tx_input.auth, TxAuth::Upgrade { .. }) {
                    tracing::warn!("pre-execution upgrade tx error: {e:?}");
                }
            }
        }
    }
}
