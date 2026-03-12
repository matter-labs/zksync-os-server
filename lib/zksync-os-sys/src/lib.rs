//! `zksync_os_sys` — C-ABI bridge between the zksync-os server (host) and
//! version-specific execution engines (plugins).
//!
//! This crate follows the common Rust `-sys` convention: it defines the
//! foreign-function interface types and provides both a **host-side** loader
//! and **plugin-side** trait adapters so that a `cdylib` / `staticlib` built
//! from `forward_system` can be loaded at link-time or run-time.
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────┐         C ABI          ┌────────────────────────┐
//!  │  host/server  │ ◄──────────────────── │  plugin (.so / .a)     │
//!  │               │  HostVTable callbacks  │  e.g. plugin-v6-cdylib │
//!  │  DynExecPlug  │ ────────────────────► │  zksync_os_run_block() │
//!  └──────────────┘   plugin entry points  └────────────────────────┘
//! ```
//!
//! ## Host side
//!
//! Use [`loader::load_plugin`] to open a `.so` at runtime, or link
//! statically and call the symbols directly.  The returned
//! [`host::DynExecutionPlugin`] exposes `run_block` / `simulate_tx`
//! with the same semantics as `ExecutionPlugin` from `plugin-api`.
//!
//! ## Plugin side
//!
//! A plugin crate should depend on `zksync_os_sys` and use the adapters
//! in [`plugin`] to reconstruct Rust trait objects from the `HostVTable`
//! callbacks, then forward to the real `forward_system` implementation.
//!
//! # Safety
//!
//! Both sides **must** be compiled with the same Rust compiler version and
//! the same `zksync_os_interface` crate version.  `BlockOutput` and
//! `TxOutput` cross the boundary as opaque heap pointers — their memory
//! layout is **not** stable across compiler versions.

pub mod ffi_types;
pub mod host;
pub mod loader;
pub mod plugin;
