use jsonrpsee::MethodResponse;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::types::error::ErrorObject;
use tokio::sync::watch;

const NOT_READY_ERROR_CODE: i32 = -32000;

fn not_ready_err() -> ErrorObject<'static> {
    ErrorObject::owned(NOT_READY_ERROR_CODE, "Node is not ready", None::<()>)
}

/// Methods that return static config present since node startup, never gated on DB readiness.
/// A downstream EN calls these during `load_remote_config` before it can boot.
fn is_always_available(method: &str) -> bool {
    matches!(
        method,
        "zks_getBridgehubContract"
            | "zks_getBytecodeSupplierContract"
            | "zks_getGenesis"
            | "eth_chainId"
            | "net_version"
            | "web3_clientVersion"
    )
}

/// JSON-RPC middleware that gates state-dependent methods on DB readiness.
///
/// The RPC server starts immediately so static config methods are always available - a
/// downstream EN can call `load_remote_config` even during this node's startup burst. All other
/// methods return `-32000 Node is not ready` until the readiness signal fires, rather than
/// silently hanging (the server binds before the DB is ready, so without this middleware the
/// connection is accepted but the request is never processed).
#[derive(Clone)]
pub(crate) struct Readiness<S = RpcService> {
    inner: S,
    ready: watch::Receiver<bool>,
}

impl<S> Readiness<S> {
    pub(crate) fn new(inner: S, ready: watch::Receiver<bool>) -> Self {
        Self { inner, ready }
    }
}

impl<S> RpcServiceT for Readiness<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            NotificationResponse = MethodResponse,
            BatchResponse = MethodResponse,
        > + Clone
        + Send
        + 'static,
{
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;
    type BatchResponse = MethodResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let is_ready = *self.ready.borrow();
        let always_available = is_always_available(request.method_name());
        let inner = self.inner.clone();
        async move {
            if !is_ready && !always_available {
                return MethodResponse::error(request.id.clone().into_owned(), not_ready_err());
            }
            inner.call(request).await
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let inner = self.inner.clone();
        async move { inner.batch(batch).await }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let inner = self.inner.clone();
        async move { inner.notification(n).await }
    }
}
