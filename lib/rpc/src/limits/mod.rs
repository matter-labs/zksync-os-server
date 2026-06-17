//! Rate-limit spec and runtime enforcement for the JSON-RPC server.

mod limiter;

pub(crate) use limiter::{Limiter, Limits};
