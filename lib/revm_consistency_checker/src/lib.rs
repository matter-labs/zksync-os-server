//! Re-executes sealed blocks on REVM and compares the results against the
//! native ZKsync OS outputs: storage writes, account diffs, per-tx event
//! logs, and per-tx L2→L1 logs.
//!
//! Gas is NOT independently checked: every replayed tx carries
//! `gas_used_override` set to the native `gas_used` (see `helpers.rs`), so
//! REVM reports the native figure by construction. Gas divergence instead
//! shows up indirectly, through the balance columns of the account diff
//! comparison (fee deductions, coinbase credit, refunds).

pub mod bytecode_hash;
pub mod helpers;
mod metrics;
pub mod node;
pub mod revm_state_provider;
pub mod storage_diff_comp;
