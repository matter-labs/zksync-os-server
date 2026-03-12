//! C-ABI-compatible types for the zksync-os plugin boundary.
//!
//! These types are shared between the host (server) and the plugin (.so/.a).
//! All types are `#[repr(C)]` to ensure stable layout across compilation units.

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// Scalar results
// ---------------------------------------------------------------------------

/// Optional 32-byte value returned from storage reads.
#[repr(C)]
pub struct FfiOptionB256 {
    pub present: bool,
    pub value: [u8; 32],
}

/// Optional byte buffer returned from preimage lookups.
///
/// When `data` is null the preimage was not found.  The host is responsible
/// for allocating the buffer; the plugin must call `free_host_bytes` to
/// release it.
#[repr(C)]
pub struct FfiOptionBytes {
    pub data: *mut u8,
    pub len: usize,
}

// ---------------------------------------------------------------------------
// Encoded transaction
// ---------------------------------------------------------------------------

/// FFI-safe encoded transaction.
///
/// Layout mirrors `EncodedTx`:
/// - variant 0 → `Abi(data[..data_len])`
/// - variant 1 → `Rlp(data[..data_len], sender)`
#[repr(C)]
pub struct FfiEncodedTx {
    pub variant: u8,
    pub data: *const u8,
    pub data_len: usize,
    /// Only meaningful for variant == 1 (Rlp).
    pub sender: [u8; 20],
}

/// Response from `get_next_tx` host callback.
///
/// - variant 0 → `Tx(tx)`
/// - variant 1 → `SealBlock` (tx fields are zeroed / null)
#[repr(C)]
pub struct FfiNextTxResponse {
    pub variant: u8,
    pub tx: FfiEncodedTx,
}

// ---------------------------------------------------------------------------
// Transaction result (plugin → host callback payload)
// ---------------------------------------------------------------------------

/// Transaction execution result passed back through `tx_executed`.
///
/// The payload is an opaque `Box<Result<TxProcessingOutputOwned, InvalidTransaction>>`
/// cast to `*mut c_void`.  The host reconstitutes it by casting back.
///
/// # Safety
/// Both sides must be compiled with the same Rust compiler and same
/// `zksync_os_interface` version.
#[repr(C)]
pub struct FfiTxResult {
    /// Opaque pointer to `Box<Result<TxProcessingOutputOwned, InvalidTransaction>>`.
    pub data: *mut c_void,
}

// ---------------------------------------------------------------------------
// Host virtual-function table
// ---------------------------------------------------------------------------

/// Function-pointer table the host passes to every plugin entry point.
///
/// The `ctx` parameter in each function is an opaque pointer to host state;
/// the plugin must pass it through unchanged.
#[repr(C)]
pub struct HostVTable {
    /// `ReadStorage::read`
    pub read_storage: unsafe extern "C" fn(ctx: *mut c_void, key: *const [u8; 32]) -> FfiOptionB256,

    /// `PreimageSource::get_preimage`
    pub get_preimage:
        unsafe extern "C" fn(ctx: *mut c_void, hash: *const [u8; 32]) -> FfiOptionBytes,

    /// `TxSource::get_next_tx`
    pub get_next_tx: unsafe extern "C" fn(ctx: *mut c_void) -> FfiNextTxResponse,

    /// `TxResultCallback::tx_executed` — the `result` pointer is an opaque
    /// `Box<Result<TxProcessingOutputOwned, InvalidTransaction>>`.
    pub tx_executed: unsafe extern "C" fn(ctx: *mut c_void, result: FfiTxResult),

    /// Free a byte buffer previously returned by `get_preimage` or
    /// `get_next_tx`.
    pub free_host_bytes: unsafe extern "C" fn(data: *mut u8, len: usize),
}

// ---------------------------------------------------------------------------
// Result types returned by plugin entry points
// ---------------------------------------------------------------------------

/// Result from `zksync_os_run_block`.
///
/// On success `block_output` is a `Box<BlockOutput>` cast to `*mut c_void`.
/// The caller must eventually pass it to `zksync_os_free_block_output`.
///
/// On failure `error_msg` / `error_len` contain a UTF-8 error string that
/// the caller must free with `zksync_os_free_error`.
#[repr(C)]
pub struct FfiRunBlockResult {
    pub success: bool,
    pub block_output: *mut c_void,
    pub error_msg: *mut u8,
    pub error_len: usize,
}

/// Result from `zksync_os_simulate_tx`.
///
/// On success (`outer_ok` is true):
///   - If `inner_ok` is true, `payload` is a `Box<TxOutput>`.
///   - If `inner_ok` is false, `payload` is a `Box<InvalidTransaction>`.
///
/// On failure (`outer_ok` is false) `error_msg` / `error_len` carry the
/// error string.
///
/// The caller must free `payload` with `zksync_os_free_tx_output` (inner_ok)
/// or `zksync_os_free_invalid_tx` (!inner_ok).
#[repr(C)]
pub struct FfiSimulateTxResult {
    pub outer_ok: bool,
    pub inner_ok: bool,
    pub payload: *mut c_void,
    pub error_msg: *mut u8,
    pub error_len: usize,
}

// ---------------------------------------------------------------------------
// Plugin symbol table (the functions the .so exports)
// ---------------------------------------------------------------------------

/// Typed function pointers that a loaded plugin must export.
///
/// Populated by [`crate::loader::PluginLibrary`] from `dlsym`.
pub struct PluginSymbols {
    pub run_block: unsafe extern "C" fn(
        block_ctx: *const u8,
        block_ctx_len: usize,
        vtable: *const HostVTable,
        host_ctx: *mut c_void,
    ) -> FfiRunBlockResult,

    pub simulate_tx: unsafe extern "C" fn(
        tx_data: *const u8,
        tx_data_len: usize,
        block_ctx: *const u8,
        block_ctx_len: usize,
        vtable: *const HostVTable,
        host_ctx: *mut c_void,
    ) -> FfiSimulateTxResult,

    pub free_block_output: unsafe extern "C" fn(ptr: *mut c_void),
    pub free_tx_output: unsafe extern "C" fn(ptr: *mut c_void),
    pub free_invalid_tx: unsafe extern "C" fn(ptr: *mut c_void),
    pub free_error: unsafe extern "C" fn(ptr: *mut u8, len: usize),
}
