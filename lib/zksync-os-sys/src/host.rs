//! Host-side adapters.
//!
//! Converts Rust trait objects (`ReadStorage`, `PreimageSource`, etc.) into
//! C-ABI function-pointer callbacks, and wraps loaded plugin symbols into a
//! safe Rust API.

use std::ffi::c_void;

use alloy::primitives::B256;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::traits::{
    EncodedTx, NextTxResponse, PreimageSource, ReadStorage, TxResultCallback, TxSource,
};
use zksync_os_interface::types::{BlockContext, BlockOutput, TxOutput, TxProcessingOutputOwned};

use crate::ffi_types::*;

// ---------------------------------------------------------------------------
// Host context – bundles all trait objects the plugin will call back into
// ---------------------------------------------------------------------------

/// Erased host state passed as `ctx: *mut c_void` to every callback.
///
/// The lifetime is stack-pinned: the `HostContext` lives on the caller's
/// stack for the duration of the FFI call.
pub(crate) struct HostContext<'a> {
    pub storage: &'a mut dyn ReadStorage,
    pub preimage: &'a mut dyn PreimageSource,
    pub tx_source: &'a mut dyn TxSource,
    pub tx_callback: &'a mut dyn TxResultCallback,
}

// ---------------------------------------------------------------------------
// C callbacks that the plugin invokes (host implementations)
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn host_read_storage(
    ctx: *mut c_void,
    key: *const [u8; 32],
) -> FfiOptionB256 {
    let host = unsafe { &mut *(ctx as *mut HostContext<'_>) };
    let key = B256::from(unsafe { *key });
    match host.storage.read(key) {
        Some(val) => FfiOptionB256 {
            present: true,
            value: val.0,
        },
        None => FfiOptionB256 {
            present: false,
            value: [0u8; 32],
        },
    }
}

pub(crate) unsafe extern "C" fn host_get_preimage(
    ctx: *mut c_void,
    hash: *const [u8; 32],
) -> FfiOptionBytes {
    let host = unsafe { &mut *(ctx as *mut HostContext<'_>) };
    let hash = B256::from(unsafe { *hash });
    match host.preimage.get_preimage(hash) {
        Some(data) => {
            let len = data.len();
            let ptr = Box::into_raw(data.into_boxed_slice()) as *mut u8;
            FfiOptionBytes { data: ptr, len }
        }
        None => FfiOptionBytes {
            data: std::ptr::null_mut(),
            len: 0,
        },
    }
}

pub(crate) unsafe extern "C" fn host_get_next_tx(ctx: *mut c_void) -> FfiNextTxResponse {
    let host = unsafe { &mut *(ctx as *mut HostContext<'_>) };
    match host.tx_source.get_next_tx() {
        NextTxResponse::SealBlock => FfiNextTxResponse {
            variant: 1,
            tx: FfiEncodedTx {
                variant: 0,
                data: std::ptr::null(),
                data_len: 0,
                sender: [0u8; 20],
            },
        },
        NextTxResponse::Tx(encoded) => {
            let (variant, bytes, sender) = match &encoded {
                EncodedTx::Abi(b) => (0u8, b.clone(), [0u8; 20]),
                EncodedTx::Rlp(b, addr) => {
                    let mut sender = [0u8; 20];
                    sender.copy_from_slice(addr.as_slice());
                    (1u8, b.clone(), sender)
                }
            };
            let data_len = bytes.len();
            let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
            FfiNextTxResponse {
                variant: 0,
                tx: FfiEncodedTx {
                    variant,
                    data: ptr,
                    data_len,
                    sender,
                },
            }
        }
    }
}

pub(crate) unsafe extern "C" fn host_tx_executed(ctx: *mut c_void, result: FfiTxResult) {
    let host = unsafe { &mut *(ctx as *mut HostContext<'_>) };
    // Reconstitute the Box from the opaque pointer.
    let boxed = unsafe {
        Box::from_raw(result.data as *mut Result<TxProcessingOutputOwned, InvalidTransaction>)
    };
    host.tx_callback.tx_executed(*boxed);
}

pub(crate) unsafe extern "C" fn host_free_bytes(data: *mut u8, len: usize) {
    if !data.is_null() && len > 0 {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(data, len)) });
    }
}

/// Build a `HostVTable` wired to the static callback functions above.
pub(crate) fn make_vtable() -> HostVTable {
    HostVTable {
        read_storage: host_read_storage,
        get_preimage: host_get_preimage,
        get_next_tx: host_get_next_tx,
        tx_executed: host_tx_executed,
        free_host_bytes: host_free_bytes,
    }
}

// ---------------------------------------------------------------------------
// Safe wrapper around loaded plugin symbols
// ---------------------------------------------------------------------------

/// A loaded execution plugin backed by a shared library.
///
/// # Safety
///
/// The plugin .so **must** be compiled with the same Rust compiler version
/// and the same `zksync_os_interface` crate version as the host.
pub struct DynExecutionPlugin {
    pub(crate) symbols: PluginSymbols,
    pub(crate) _lib: libloading::Library,
}

unsafe impl Send for DynExecutionPlugin {}
unsafe impl Sync for DynExecutionPlugin {}

impl DynExecutionPlugin {
    /// Execute a block through the loaded plugin.
    pub fn run_block(
        &self,
        block_context: BlockContext,
        storage: &mut dyn ReadStorage,
        preimage_source: &mut dyn PreimageSource,
        tx_source: &mut dyn TxSource,
        tx_result_callback: &mut dyn TxResultCallback,
    ) -> Result<BlockOutput, anyhow::Error> {
        let block_ctx_bytes =
            bincode::serde::encode_to_vec(block_context, bincode::config::standard())?;
        let vtable = make_vtable();
        let mut host = HostContext {
            storage,
            preimage: preimage_source,
            tx_source,
            tx_callback: tx_result_callback,
        };
        let host_ptr = &mut host as *mut HostContext<'_> as *mut c_void;

        let result = unsafe {
            (self.symbols.run_block)(
                block_ctx_bytes.as_ptr(),
                block_ctx_bytes.len(),
                &vtable,
                host_ptr,
            )
        };

        if result.success {
            let output = unsafe { Box::from_raw(result.block_output as *mut BlockOutput) };
            Ok(*output)
        } else {
            let msg = extract_error(&self.symbols, result.error_msg, result.error_len);
            Err(anyhow::anyhow!(msg))
        }
    }

    /// Simulate a single transaction through the loaded plugin.
    pub fn simulate_tx(
        &self,
        transaction: EncodedTx,
        block_context: BlockContext,
        storage: &mut dyn ReadStorage,
        preimage_source: &mut dyn PreimageSource,
    ) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
        let tx_bytes = bincode::serde::encode_to_vec(transaction, bincode::config::standard())?;
        let block_ctx_bytes =
            bincode::serde::encode_to_vec(block_context, bincode::config::standard())?;
        let vtable = make_vtable();

        struct NoopTxSource;
        impl TxSource for NoopTxSource {
            fn get_next_tx(&mut self) -> NextTxResponse {
                NextTxResponse::SealBlock
            }
        }
        struct NoopCallback;
        impl TxResultCallback for NoopCallback {
            fn tx_executed(&mut self, _: Result<TxProcessingOutputOwned, InvalidTransaction>) {}
        }

        let mut noop_tx = NoopTxSource;
        let mut noop_cb = NoopCallback;
        let mut host = HostContext {
            storage,
            preimage: preimage_source,
            tx_source: &mut noop_tx,
            tx_callback: &mut noop_cb,
        };
        let host_ptr = &mut host as *mut HostContext<'_> as *mut c_void;

        let result = unsafe {
            (self.symbols.simulate_tx)(
                tx_bytes.as_ptr(),
                tx_bytes.len(),
                block_ctx_bytes.as_ptr(),
                block_ctx_bytes.len(),
                &vtable,
                host_ptr,
            )
        };

        if result.outer_ok {
            if result.inner_ok {
                let output = unsafe { Box::from_raw(result.payload as *mut TxOutput) };
                Ok(Ok(*output))
            } else {
                let inv = unsafe { Box::from_raw(result.payload as *mut InvalidTransaction) };
                Ok(Err(*inv))
            }
        } else {
            let msg = extract_error(&self.symbols, result.error_msg, result.error_len);
            Err(anyhow::anyhow!(msg))
        }
    }
}

fn extract_error(symbols: &PluginSymbols, error_msg: *mut u8, error_len: usize) -> String {
    if !error_msg.is_null() && error_len > 0 {
        let s = unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(error_msg, error_len)).into_owned()
        };
        unsafe { (symbols.free_error)(error_msg, error_len) };
        s
    } else {
        "unknown plugin error".to_string()
    }
}
