//! Storage implementation based on DAL.

use crate::consensus::types::Payload;

pub mod rocksdb;
mod store;

pub(crate) use rocksdb::*;
pub(crate) use store::*;

#[derive(thiserror::Error, Debug)]
pub enum InsertCertificateError {
    #[error("corresponding payload is missing")]
    MissingPayload,
    #[error("certificate doesn't match the payload, payload = {0:?}")]
    PayloadMismatch(Payload),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
