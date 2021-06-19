use crate::Trigger;
use futures_core::{ready, stream::Stream};
use pin_project::pin_project;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::watch;

/// A stream combinator which takes elements from a stream until a future resolves.
///
/// This structure is produced by the [`StreamExt::take_until_if`] method.
#[pin_project]
#[derive(Clone, Debug)]
pub struct TakeUntilIf<S, F> {
    #[pin]
    stream: S,
    #[pin]
    until: F,
    free: bool,
}

/// This `Stream` extension trait provides a `take_until_if` method that terminates the stream once
/// the given future resolves.
pub trait StreamExt: Stream {
    /// Take elements from this stream until the given future resolves.
    ///
    /// This function takes elements from this stream until the given future resolves with
    /// `true`. Once it resolves, the stream yields `None`, and produces no further elements.
    ///
    /// If the future resolves with `false`, the stream is allowed to continue indefinitely.
    ///
    /// This method is essentially a wrapper around `futures_util::stream::StreamExt::take_until`
    /// that ascribes particular semantics to the output of the provided future.
    ///
    /// ```
    /// use stream_cancel::StreamExt;
    /// use futures::prelude::*;
    /// use tokio_stream::wrappers::TcpListenerStream;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    ///     let (tx, rx) = tokio::sync::oneshot::channel();
    ///
    ///     tokio::spawn(async move {
    ///         let mut incoming = TcpListenerStream::new(listener).take_until_if(rx.map(|_| true));
    ///         while let Some(mut s) = incoming.next().await.transpose().unwrap() {
    ///             tokio::spawn(async move {
    ///                 let (mut r, mut w) = s.split();
    ///                 println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
    ///             });
    ///         }
    ///     });
    ///
    ///     // tell the listener to stop accepting new connections
    ///     tx.send(()).unwrap();
    ///     // the spawned async block will terminate cleanly, allowing main to return
    /// }
    /// ```
    fn take_until_if<U>(self, until: U) -> TakeUntilIf<Self, U>
    where
        U: Future<Output = bool>,
        Self: Sized,
    {
        TakeUntilIf {
            stream: self,
            until,
            free: false,
        }
    }
}

impl<S> StreamExt for S where S: Stream {}

impl<S, F> Stream for TakeUntilIf<S, F>
where
    S: Stream,
    F: Future<Output = bool>,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if !*this.free {
            if let Poll::Ready(terminate) = this.until.poll(cx) {
                if terminate {
                    // future resolved -- terminate stream
                    return Poll::Ready(None);
                }
                // to provide a mechanism for the creator to let the stream run forever,
                // we interpret this as "run forever".
                *this.free = true;
            }
        }

        this.stream.poll_next(cx)
    }
}

/// A `Tripwire` is a convenient mechanism for implementing graceful shutdown over many
/// asynchronous streams. A `Tripwire` is a `Future` that is `Clone`, and that can be passed to
/// [`StreamExt::take_until_if`]. All `Tripwire` clones are associated with a single [`Trigger`],
/// which is then used to signal that all the associated streams should be terminated.
///
/// The `Tripwire` future resolves to `true` if the stream should be considered closed, and `false`
/// if the `Trigger` has been disabled.
///
/// `Tripwire` is internally implemented using a `Shared<oneshot::Receiver<()>>`, with the
/// `Trigger` holding the associated `oneshot::Sender`. There is very little magic.
#[pin_project]
pub struct Tripwire {
    watch: watch::Receiver<bool>,

    // TODO: existential type
    #[pin]
    fut: Option<Pin<Box<dyn Future<Output = bool> + Send + Sync>>>,
}

#[cfg(test)]
static_assertions::assert_impl_all!(Tripwire: Sync, Send);

impl Clone for Tripwire {
    fn clone(&self) -> Self {
        Self {
            watch: self.watch.clone(),
            fut: None,
        }
    }
}

impl fmt::Debug for Tripwire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Tripwire").field(&self.watch).finish()
    }
}

impl Tripwire {
    /// Make a new `Tripwire` and an associated [`Trigger`].
    pub fn new() -> (Trigger, Self) {
        let (tx, rx) = watch::channel(false);
        (
            Trigger(Some(tx)),
            Tripwire {
                watch: rx,
                fut: None,
            },
        )
    }
}

impl Future for Tripwire {
    type Output = bool;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if this.fut.is_none() {
            let mut watch = this.watch.clone();
            this.fut.set(Some(Box::pin(async move {
                while !*watch.borrow() {
                    // value is currently false; wait for it to change
                    if let Err(_) = watch.changed().await {
                        // channel was closed -- we return whatever the latest value was
                        return *watch.borrow();
                    }
                }
                // value change to true, and we should exit
                true
            })));
        }

        // Safety: we never move the value inside the option.
        // If the Tripwire is pinned, the Option is pinned, and the future inside is as well.
        unsafe { this.fut.map_unchecked_mut(|f| f.as_mut().unwrap()) }
            .as_mut()
            .poll(cx)
    }
}

/// Map any Future<Output = Result<T, E>> to a Future<Output = bool>
///
/// The output is `true` if the `Result` was `Ok`, and `false` otherwise.
#[pin_project]
struct ResultTrueFalse<F>(#[pin] F);

impl<F, T, E> Future for ResultTrueFalse<F>
where
    F: Future<Output = Result<T, E>>,
{
    type Output = bool;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(self.project().0.poll(cx)).is_ok().into()
    }
}
