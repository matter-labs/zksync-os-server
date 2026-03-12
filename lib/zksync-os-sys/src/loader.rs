//! Dynamic library loader for zksync-os plugins.
//!
//! Loads a `.so` (or `.dylib` / `.dll`) at runtime and resolves the
//! well-known C symbols into [`PluginSymbols`], then wraps them in a
//! [`DynExecutionPlugin`].

use std::path::Path;

use crate::ffi_types::PluginSymbols;
use crate::host::DynExecutionPlugin;

/// Load a plugin shared library from `path`.
///
/// The library must export these C symbols:
///
/// - `zksync_os_run_block`
/// - `zksync_os_simulate_tx`
/// - `zksync_os_free_block_output`
/// - `zksync_os_free_tx_output`
/// - `zksync_os_free_invalid_tx`
/// - `zksync_os_free_error`
///
/// # Safety
///
/// The loaded library must be compiled with the same Rust toolchain and
/// the same version of `zksync_os_interface` as the host binary.
pub fn load_plugin(path: &Path) -> Result<DynExecutionPlugin, anyhow::Error> {
    let lib = unsafe { libloading::Library::new(path) }
        .map_err(|e| anyhow::anyhow!("failed to load plugin from {}: {}", path.display(), e))?;

    let symbols = unsafe {
        PluginSymbols {
            run_block: *lib.get::<unsafe extern "C" fn(
                *const u8,
                usize,
                *const crate::ffi_types::HostVTable,
                *mut std::ffi::c_void,
            )
                -> crate::ffi_types::FfiRunBlockResult>(
                b"zksync_os_run_block\0"
            )?,
            simulate_tx: *lib.get::<unsafe extern "C" fn(
                *const u8,
                usize,
                *const u8,
                usize,
                *const crate::ffi_types::HostVTable,
                *mut std::ffi::c_void,
            )
                -> crate::ffi_types::FfiSimulateTxResult>(
                b"zksync_os_simulate_tx\0"
            )?,
            free_block_output: *lib.get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(
                b"zksync_os_free_block_output\0",
            )?,
            free_tx_output: *lib.get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(
                b"zksync_os_free_tx_output\0",
            )?,
            free_invalid_tx: *lib.get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(
                b"zksync_os_free_invalid_tx\0",
            )?,
            free_error: *lib
                .get::<unsafe extern "C" fn(*mut u8, usize)>(b"zksync_os_free_error\0")?,
        }
    };

    Ok(DynExecutionPlugin { symbols, _lib: lib })
}
