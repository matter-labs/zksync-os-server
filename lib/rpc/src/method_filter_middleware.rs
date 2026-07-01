use jsonrpsee::MethodResponse;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::types::error::{ErrorObject, METHOD_NOT_FOUND_CODE};
use std::collections::HashSet;
use std::sync::Arc;

fn method_disabled_err() -> ErrorObject<'static> {
    ErrorObject::owned(METHOD_NOT_FOUND_CODE, "Method disabled", None::<()>)
}

/// JSON-RPC middleware that rejects filtered methods with -32601 and emits a warning.
#[derive(Clone)]
pub(crate) struct MethodFiltering<S = RpcService> {
    inner: S,
    filter: Arc<HashSet<String>>,
}

impl<S> MethodFiltering<S> {
    pub(crate) fn new(inner: S, filter: Arc<HashSet<String>>) -> Self {
        Self { inner, filter }
    }
}

impl<S> RpcServiceT for MethodFiltering<S>
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
        let rejected = self.filter.contains(request.method_name());
        let inner = self.inner.clone();
        async move {
            if rejected {
                tracing::warn!(
                    method = request.method_name(),
                    "rpc method rejected by filter",
                );
                let id = request.id.clone().into_owned();
                return MethodResponse::error(id, method_disabled_err());
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
