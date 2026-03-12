//! Plugin-side adapters.
//!
//! Wraps C-ABI host callbacks back into Rust trait implementations so that
//! the `forward_system` code can use its normal `ReadStorage`, `PreimageSource`,
//! `TxSource`, and `TxResultCallback` traits transparently.

use std::ffi::c_void;

use alloy::primitives::{Address, B256};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::traits::{
    EncodedTx, NextTxResponse, PreimageSource, ReadStorage, TxResultCallback, TxSource,
};
use zksync_os_interface::types::TxProcessingOutputOwned;

use crate::ffi_types::*;

// ---------------------------------------------------------------------------
// Adapter: HostVTable + ctx → ReadStorage
// ---------------------------------------------------------------------------

pub struct FfiReadStorage {
    vtable: *const HostVTable,
    ctx: *mut c_void,
}

impl FfiReadStorage {
    /// # Safety
    /// `vtable` and `ctx` must remain valid for the lifetime of this struct.
    pub unsafe fn new(vtable: *const HostVTable, ctx: *mut c_void) -> Self {
        Self { vtable, ctx }
    }
}

impl ReadStorage for FfiReadStorage {
    fn read(&mut self, key: B256) -> Option<B256> {
        let key_bytes: &[u8; 32] = key.as_ref();
        let result = unsafe { ((*self.vtable).read_storage)(self.ctx, key_bytes) };
        if result.present {
            Some(B256::from(result.value))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter: HostVTable + ctx → PreimageSource
// ---------------------------------------------------------------------------

pub struct FfiPreimageSource {
    vtable: *const HostVTable,
    ctx: *mut c_void,
}

impl FfiPreimageSource {
    /// # Safety
    /// `vtable` and `ctx` must remain valid for the lifetime of this struct.
    pub unsafe fn new(vtable: *const HostVTable, ctx: *mut c_void) -> Self {
        Self { vtable, ctx }
    }
}

impl PreimageSource for FfiPreimageSource {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        let hash_bytes: &[u8; 32] = hash.as_ref();
        let result = unsafe { ((*self.vtable).get_preimage)(self.ctx, hash_bytes) };
        if result.data.is_null() {
            None
        } else {
            let data = unsafe { std::slice::from_raw_parts(result.data, result.len) }.to_vec();
            unsafe { ((*self.vtable).free_host_bytes)(result.data, result.len) };
            Some(data)
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter: HostVTable + ctx → TxSource
// ---------------------------------------------------------------------------

pub struct FfiTxSource {
    vtable: *const HostVTable,
    ctx: *mut c_void,
}

impl FfiTxSource {
    /// # Safety
    /// `vtable` and `ctx` must remain valid for the lifetime of this struct.
    pub unsafe fn new(vtable: *const HostVTable, ctx: *mut c_void) -> Self {
        Self { vtable, ctx }
    }
}

impl TxSource for FfiTxSource {
    fn get_next_tx(&mut self) -> NextTxResponse {
        let resp = unsafe { ((*self.vtable).get_next_tx)(self.ctx) };
        if resp.variant == 1 {
            return NextTxResponse::SealBlock;
        }
        let tx = &resp.tx;
        let data = unsafe { std::slice::from_raw_parts(tx.data, tx.data_len) }.to_vec();
        unsafe { ((*self.vtable).free_host_bytes)(tx.data as *mut u8, tx.data_len) };

        let encoded = match tx.variant {
            0 => EncodedTx::Abi(data),
            1 => {
                let addr = Address::from_slice(&tx.sender);
                EncodedTx::Rlp(data, addr)
            }
            _ => panic!("unknown EncodedTx variant {}", tx.variant),
        };
        NextTxResponse::Tx(encoded)
    }
}

// ---------------------------------------------------------------------------
// Adapter: HostVTable + ctx → TxResultCallback
// ---------------------------------------------------------------------------

pub struct FfiTxResultCallback {
    vtable: *const HostVTable,
    ctx: *mut c_void,
}

impl FfiTxResultCallback {
    /// # Safety
    /// `vtable` and `ctx` must remain valid for the lifetime of this struct.
    pub unsafe fn new(vtable: *const HostVTable, ctx: *mut c_void) -> Self {
        Self { vtable, ctx }
    }
}

impl TxResultCallback for FfiTxResultCallback {
    fn tx_executed(
        &mut self,
        tx_execution_result: Result<TxProcessingOutputOwned, InvalidTransaction>,
    ) {
        // Pass the result as an opaque Box pointer. The host will
        // reconstitute it on the other side of the FFI boundary.
        let boxed = Box::new(tx_execution_result);
        let ffi_result = FfiTxResult {
            data: Box::into_raw(boxed) as *mut c_void,
        };
        unsafe { ((*self.vtable).tx_executed)(self.ctx, ffi_result) };
    }
}
