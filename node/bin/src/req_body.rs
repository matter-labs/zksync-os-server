//! We could use ReqBody from micro-http if the library made create_req_body public.

use futures::Stream;
use http_body::{Body, Frame, SizeHint};
use hyper::body::Bytes;
use micro_http::protocol::{Message, ParseError, PayloadItem, PayloadSize, RequestHeader};
use std::pin::Pin;
use std::task::{Context, Poll};

pub(crate) enum ReqBody<S> {
    Stream { size: PayloadSize, stream: S },
    NoBody,
}

impl<S> ReqBody<S> {
    pub(crate) fn new(body_stream: S, payload_size: PayloadSize) -> ReqBody<S>
    where
        S: Stream<Item = Result<Message<(RequestHeader, PayloadSize)>, ParseError>> + Unpin,
    {
        match payload_size {
            PayloadSize::Empty | PayloadSize::Length(0) => ReqBody::NoBody,
            _ => ReqBody::Stream {
                size: payload_size,
                stream: body_stream,
            },
        }
    }
}

impl<S> Body for ReqBody<S>
where
    S: Stream<Item = Result<Message<(RequestHeader, PayloadSize)>, ParseError>> + Unpin,
{
    type Data = Bytes;
    type Error = ParseError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match this {
            ReqBody::Stream { stream, .. } => {
                // Pin the stream and poll for the next item
                Pin::new(stream).poll_next(cx).map(|opt| {
                    opt.unwrap()
                        .map(|msg| match msg {
                            Message::Payload(PayloadItem::Chunk(payload)) => {
                                Some(Frame::data(payload))
                            }
                            Message::Payload(PayloadItem::Eof) => None,
                            Message::Header(_) => unreachable!(),
                        })
                        .transpose()
                })
            }
            ReqBody::NoBody => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self {
            ReqBody::NoBody => true,
            ReqBody::Stream { .. } => false, // This means we don't know
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self {
            ReqBody::NoBody => SizeHint::with_exact(0),
            ReqBody::Stream { size, .. } => (*size).into(),
        }
    }
}
