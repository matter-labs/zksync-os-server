// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-server-types/src/result.rs
#![allow(dead_code)]

//! Additional helpers for converting errors.

use crate::debug_impl::DebugError;
use crate::eth_call_handler::EthCallError;
use crate::eth_filter_impl::EthFilterError;
use crate::eth_impl::EthError;
use crate::tx_handler::EthSendRawTransactionError;
use crate::zks_impl::ZksError;
use alloy::primitives::Bytes;
use alloy::rpc::types::error::EthRpcErrorCode;
use alloy::sol_types::{ContractError, RevertReason};
use jsonrpsee::core::RpcResult;
use std::fmt;

/// Helper trait to easily convert various `Result` types into [`RpcResult`]
pub trait ToRpcResult<Ok, Err>: Sized {
    /// Converts result to [`RpcResult`] by converting error variant to
    /// [`jsonrpsee::types::error::ErrorObject`]
    fn to_rpc_result(self) -> RpcResult<Ok>
    where
        Err: fmt::Display;
}

/// A macro that implements the `ToRpcResult` for a specific error type
#[macro_export]
macro_rules! impl_to_rpc_result {
    ($err:ty) => {
        impl<Ok> ToRpcResult<Ok, $err> for Result<Ok, $err> {
            fn to_rpc_result(self) -> RpcResult<Ok> {
                self.map_err(|err| $crate::result::internal_rpc_err(err.to_string()))
            }
        }
    };
}

impl_to_rpc_result!(EthSendRawTransactionError);
impl_to_rpc_result!(EthFilterError);
impl_to_rpc_result!(EthError);
impl_to_rpc_result!(ZksError);
impl_to_rpc_result!(DebugError);

impl<Ok> ToRpcResult<Ok, EthCallError> for Result<Ok, EthCallError> {
    fn to_rpc_result(self) -> RpcResult<Ok> {
        self.map_err(|err| match err {
            EthCallError::Revert(revert) => rpc_err(
                EthRpcErrorCode::ExecutionError.code(),
                revert.to_string(),
                revert.output.as_ref().map(|out| out.as_ref()),
            ),
            err => internal_rpc_err(err.to_string()),
        })
    }
}

/// Constructs an unimplemented JSON-RPC error.
pub fn unimplemented_rpc_err() -> jsonrpsee::types::error::ErrorObject<'static> {
    internal_rpc_err("unimplemented")
}

/// Constructs an invalid params JSON-RPC error.
pub fn invalid_params_rpc_err(
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INVALID_PARAMS_CODE, msg, None)
}

/// Constructs an internal JSON-RPC error.
pub fn internal_rpc_err(msg: impl Into<String>) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INTERNAL_ERROR_CODE, msg, None)
}

/// Constructs an internal JSON-RPC error with data
pub fn internal_rpc_err_with_data(
    msg: impl Into<String>,
    data: &[u8],
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(
        jsonrpsee::types::error::INTERNAL_ERROR_CODE,
        msg,
        Some(data),
    )
}

/// Constructs an internal JSON-RPC error with code and message
pub fn rpc_error_with_code(
    code: i32,
    msg: impl Into<String>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(code, msg, None)
}

/// Constructs a JSON-RPC error, consisting of `code`, `message` and optional `data`.
pub fn rpc_err(
    code: i32,
    msg: impl Into<String>,
    data: Option<&[u8]>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    jsonrpsee::types::error::ErrorObject::owned(
        code,
        msg.into(),
        data.map(|data| {
            jsonrpsee::core::to_json_raw_value(&alloy::primitives::hex::encode_prefixed(data))
                .expect("serializing String can't fail")
        }),
    )
}

/// Represents a reverted transaction and its output data.
///
/// Displays "execution reverted(: reason)?" if the reason is a string.
#[derive(Debug, Clone, thiserror::Error)]
pub struct RevertError {
    /// The transaction output data
    ///
    /// Note: this is `None` if output was empty
    output: Option<Bytes>,
}

impl RevertError {
    /// Wraps the output bytes
    ///
    /// Note: this is intended to wrap a VM output
    pub fn new(output: Bytes) -> Self {
        if output.is_empty() {
            Self { output: None }
        } else {
            Self {
                output: Some(output),
            }
        }
    }

    /// Returns error code to return for this error.
    pub const fn error_code(&self) -> i32 {
        EthRpcErrorCode::ExecutionError.code()
    }
}

impl fmt::Display for RevertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("execution reverted")?;
        if let Some(reason) = self
            .output
            .as_ref()
            .and_then(|out| RevertReason::decode(out))
        {
            let error = reason.to_string();
            let mut error = error.as_str();
            if matches!(
                reason,
                RevertReason::ContractError(ContractError::Revert(_))
            ) {
                // we strip redundant `revert: ` prefix from the revert reason
                error = error.trim_start_matches("revert: ");
            }
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}
