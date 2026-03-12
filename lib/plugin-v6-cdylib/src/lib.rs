//! V6 execution plugin compiled as a `cdylib` / `staticlib`.
//!
//! Exports C-ABI entry points that the host loads via `zksync_os_sys`.
//! Internally delegates to `zk_os_forward_system_dev::run::RunBlockForward`.

use std::ffi::c_void;

use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::types::{BlockContext, BlockOutput, TxOutput};
use zksync_os_sys::ffi_types::*;
use zksync_os_sys::plugin::*;

use zk_os_forward_system_dev::run::RunBlockForward;
use zksync_os_interface::traits::{EncodedTx, RunBlock, SimulateTx};

// ---------------------------------------------------------------------------
// Exported C symbols
// ---------------------------------------------------------------------------

/// Execute a block.
///
/// # Safety
/// All pointer arguments must be valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_run_block(
    block_ctx: *const u8,
    block_ctx_len: usize,
    vtable: *const HostVTable,
    host_ctx: *mut c_void,
) -> FfiRunBlockResult {
    let block_ctx_bytes = unsafe { std::slice::from_raw_parts(block_ctx, block_ctx_len) };
    let block_context: BlockContext =
        match bincode::serde::decode_from_slice(block_ctx_bytes, bincode::config::standard()) {
            Ok((v, _)) => v,
            Err(e) => return make_error_result(format!("failed to deserialize BlockContext: {e}")),
        };

    let storage = unsafe { FfiReadStorage::new(vtable, host_ctx) };
    let preimage = unsafe { FfiPreimageSource::new(vtable, host_ctx) };
    let tx_source = unsafe { FfiTxSource::new(vtable, host_ctx) };
    let tx_callback = unsafe { FfiTxResultCallback::new(vtable, host_ctx) };
    let mut tracer = NopTracer;
    let mut validator = NopValidator;

    let result = RunBlockForward {}.run_block(
        (),
        block_context,
        storage,
        preimage,
        tx_source,
        tx_callback,
        &mut tracer,
        &mut validator,
    );

    match result {
        Ok(output) => {
            let boxed = Box::new(output);
            FfiRunBlockResult {
                success: true,
                block_output: Box::into_raw(boxed) as *mut c_void,
                error_msg: std::ptr::null_mut(),
                error_len: 0,
            }
        }
        Err(e) => make_error_result(e.to_string()),
    }
}

/// Simulate a single transaction.
///
/// # Safety
/// All pointer arguments must be valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_simulate_tx(
    tx_data: *const u8,
    tx_data_len: usize,
    block_ctx: *const u8,
    block_ctx_len: usize,
    vtable: *const HostVTable,
    host_ctx: *mut c_void,
) -> FfiSimulateTxResult {
    let tx_bytes = unsafe { std::slice::from_raw_parts(tx_data, tx_data_len) };
    let transaction: EncodedTx =
        match bincode::serde::decode_from_slice(tx_bytes, bincode::config::standard()) {
            Ok((v, _)) => v,
            Err(e) => {
                return make_simulate_error(format!("failed to deserialize EncodedTx: {e}"));
            }
        };

    let block_ctx_bytes = unsafe { std::slice::from_raw_parts(block_ctx, block_ctx_len) };
    let block_context: BlockContext =
        match bincode::serde::decode_from_slice(block_ctx_bytes, bincode::config::standard()) {
            Ok((v, _)) => v,
            Err(e) => {
                return make_simulate_error(format!("failed to deserialize BlockContext: {e}"));
            }
        };

    let storage = unsafe { FfiReadStorage::new(vtable, host_ctx) };
    let preimage = unsafe { FfiPreimageSource::new(vtable, host_ctx) };
    let mut tracer = NopTracer;
    let mut validator = NopValidator;

    let result = RunBlockForward {}.simulate_tx(
        (),
        transaction,
        block_context,
        storage,
        preimage,
        &mut tracer,
        &mut validator,
    );

    match result {
        Ok(Ok(tx_output)) => {
            let boxed = Box::new(tx_output);
            FfiSimulateTxResult {
                outer_ok: true,
                inner_ok: true,
                payload: Box::into_raw(boxed) as *mut c_void,
                error_msg: std::ptr::null_mut(),
                error_len: 0,
            }
        }
        Ok(Err(invalid_tx)) => {
            let boxed = Box::new(invalid_tx);
            FfiSimulateTxResult {
                outer_ok: true,
                inner_ok: false,
                payload: Box::into_raw(boxed) as *mut c_void,
                error_msg: std::ptr::null_mut(),
                error_len: 0,
            }
        }
        Err(e) => make_simulate_error(e.to_string()),
    }
}

/// Free a `BlockOutput` previously returned by `zksync_os_run_block`.
///
/// # Safety
/// `ptr` must be a valid pointer returned by `zksync_os_run_block`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_free_block_output(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut BlockOutput) });
    }
}

/// Free a `TxOutput` previously returned by `zksync_os_simulate_tx`.
///
/// # Safety
/// `ptr` must be a valid pointer returned by `zksync_os_simulate_tx`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_free_tx_output(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut TxOutput) });
    }
}

/// Free an `InvalidTransaction` previously returned by `zksync_os_simulate_tx`.
///
/// # Safety
/// `ptr` must be a valid pointer returned by `zksync_os_simulate_tx`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_free_invalid_tx(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut InvalidTransaction) });
    }
}

/// Free an error string.
///
/// # Safety
/// `ptr` must be a valid pointer returned by a plugin function, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zksync_os_free_error(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_error_result(msg: String) -> FfiRunBlockResult {
    let bytes = msg.into_bytes();
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    FfiRunBlockResult {
        success: false,
        block_output: std::ptr::null_mut(),
        error_msg: ptr,
        error_len: len,
    }
}

fn make_simulate_error(msg: String) -> FfiSimulateTxResult {
    let bytes = msg.into_bytes();
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    FfiSimulateTxResult {
        outer_ok: false,
        inner_ok: false,
        payload: std::ptr::null_mut(),
        error_msg: ptr,
        error_len: len,
    }
}
