// The code in this file was copied from reth with some minor changes. Source:
// https://github.com/paradigmxyz/reth/blob/fcf58cb5acc2825e7c046f6741e90a8c5dab7847/crates/rpc/rpc-server-types/src/result.rs
#![allow(dead_code)]

//! Additional helpers for converting errors.

use crate::api::eth_call_handler::EthCallError;
use crate::api::tx_handler::EthSendRawTransactionError;
use jsonrpsee::core::RpcResult;
use std::fmt;

/// Helper trait to easily convert various `Result` types into [`RpcResult`]
pub trait ToRpcResult<Ok, Err>: Sized {
    /// Converts result to [`RpcResult`] by converting error variant to
    /// [`jsonrpsee::types::error::ErrorObject`]
    fn to_rpc_result(self) -> RpcResult<Ok>
    where
        Err: fmt::Display,
    {
        self.map_internal_err(|err| err.to_string())
    }

    /// Converts this type into an [`RpcResult`]
    fn map_rpc_err<'a, F, M>(self, op: F) -> RpcResult<Ok>
    where
        F: FnOnce(Err) -> (i32, M, Option<&'a [u8]>),
        M: Into<String>;

    /// Converts this type into an [`RpcResult`] with the
    /// [`jsonrpsee::types::error::INTERNAL_ERROR_CODE`] and the given message.
    fn map_internal_err<F, M>(self, op: F) -> RpcResult<Ok>
    where
        F: FnOnce(Err) -> M,
        M: Into<String>;

    /// Converts this type into an [`RpcResult`] with the
    /// [`jsonrpsee::types::error::INTERNAL_ERROR_CODE`] and given message and data.
    fn map_internal_err_with_data<'a, F, M>(self, op: F) -> RpcResult<Ok>
    where
        F: FnOnce(Err) -> (M, &'a [u8]),
        M: Into<String>;

    /// Adds a message to the error variant and returns an internal Error.
    ///
    /// This is shorthand for `Self::map_internal_err(|err| format!("{msg}: {err}"))`.
    fn with_message(self, msg: &str) -> RpcResult<Ok>;
}

/// A macro that implements the `ToRpcResult` for a specific error type
#[macro_export]
macro_rules! impl_to_rpc_result {
    ($err:ty) => {
        impl<Ok> ToRpcResult<Ok, $err> for Result<Ok, $err> {
            #[inline]
            fn map_rpc_err<'a, F, M>(self, op: F) -> jsonrpsee::core::RpcResult<Ok>
            where
                F: FnOnce($err) -> (i32, M, Option<&'a [u8]>),
                M: Into<String>,
            {
                match self {
                    Ok(t) => Ok(t),
                    Err(err) => {
                        let (code, msg, data) = op(err);
                        Err($crate::api::result::rpc_err(code, msg, data))
                    }
                }
            }

            #[inline]
            fn map_internal_err<'a, F, M>(self, op: F) -> jsonrpsee::core::RpcResult<Ok>
            where
                F: FnOnce($err) -> M,
                M: Into<String>,
            {
                self.map_err(|err| $crate::api::result::internal_rpc_err(op(err)))
            }

            #[inline]
            fn map_internal_err_with_data<'a, F, M>(self, op: F) -> jsonrpsee::core::RpcResult<Ok>
            where
                F: FnOnce($err) -> (M, &'a [u8]),
                M: Into<String>,
            {
                match self {
                    Ok(t) => Ok(t),
                    Err(err) => {
                        let (msg, data) = op(err);
                        Err($crate::api::result::internal_rpc_err_with_data(msg, data))
                    }
                }
            }

            #[inline]
            fn with_message(self, msg: &str) -> jsonrpsee::core::RpcResult<Ok> {
                match self {
                    Ok(t) => Ok(t),
                    Err(err) => {
                        let msg = format!("{msg}: {err}");
                        Err($crate::api::result::internal_rpc_err(msg))
                    }
                }
            }
        }
    };
}

impl_to_rpc_result!(EthCallError);
impl_to_rpc_result!(EthSendRawTransactionError);

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
