use crate::peekable::PeekableTxStream;
use futures::Stream;
use std::pin::Pin;
use zksync_os_types::ZkTransaction;

pub trait TxStream: Stream<Item = ZkTransaction> {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>);
}

pub trait TxStreamExt: TxStream {
    /// Wrap the transaction stream in a Box, pinning it.
    fn peekable(self) -> PeekableTxStream<Self>
    where
        Self: Sized,
    {
        PeekableTxStream::new(self)
    }

    /// Wrap the transaction stream in a Box, pinning it.
    fn boxed<'a>(self) -> BoxTxStream<'a>
    where
        Self: Sized + Send + 'a,
    {
        Box::pin(self)
    }
}

impl<S: TxStream> TxStreamExt for S {}

/// An owned dynamically typed [`TxStream`] for use in cases where you can't
/// statically type your result or need to add some indirection.
///
/// This type is often created by the [`boxed`] method on [`TxStreamExt`].
pub type BoxTxStream<'a> = Pin<Box<dyn TxStream + Send + 'a>>;
