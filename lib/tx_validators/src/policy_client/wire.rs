//! JSON wire types for `/admit` and `/judge`.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use zksync_os_interface::tracing::BeginTxContext;

use super::AccessType;
use super::tracer::CapturedFrame;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdmitRequest<'a> {
    pub protocol_version: &'a str,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    pub value: U256,
    #[serde(with = "alloy::hex")]
    pub calldata: &'a [u8],
    pub gas_limit: u64,
    pub access_type: AccessType,
}

impl<'a> AdmitRequest<'a> {
    pub(super) fn from_context(
        ctx: &'a BeginTxContext<'a>,
        protocol_version: &'a str,
        access_type: AccessType,
    ) -> Self {
        Self {
            protocol_version,
            from: ctx.from,
            to: ctx.to,
            value: ctx.value,
            calldata: ctx.calldata,
            gas_limit: ctx.gas_limit,
            access_type,
        }
    }
}

/// Shared response shape for `/admit` and `/judge`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PolicyResponse {
    pub allow: bool,
    #[serde(default)]
    pub rule_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JudgeRequest<'a> {
    pub protocol_version: &'a str,
    /// Original tx signer when known. Frame trace doesn't carry it
    /// reliably (an EOA-to-EOA simulation captures zero frames), so it
    /// rides on the body so the service can attribute the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
    pub trace: JudgeTrace<'a>,
    pub access_type: AccessType,
}

#[derive(Debug, Serialize)]
pub(super) struct JudgeTrace<'a> {
    pub frame: Option<&'a CapturedFrame>,
}

impl<'a> JudgeRequest<'a> {
    pub(super) fn new(
        protocol_version: &'a str,
        from: Option<Address>,
        root: Option<&'a CapturedFrame>,
        access_type: AccessType,
    ) -> Self {
        Self {
            protocol_version,
            from,
            trace: JudgeTrace { frame: root },
            access_type,
        }
    }
}
