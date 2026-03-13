use crate::metrics::HTTP_METRICS;
use hyper::body::Body;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct HttpTimingLayer;

impl<S> Layer<S> for HttpTimingLayer {
    type Service = HttpTimingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HttpTimingService { inner }
    }
}

#[derive(Clone)]
pub struct HttpTimingService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<hyper::Request<ReqBody>> for HttpTimingService<S>
where
    S: Service<hyper::Request<ReqBody>, Response = hyper::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send,
    ReqBody: Body + Send + 'static,
    ResBody: Body + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: hyper::Request<ReqBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let started = Instant::now();
            let response = inner.call(req).await;
            HTTP_METRICS.response_time.observe(started.elapsed());
            response
        })
    }
}
