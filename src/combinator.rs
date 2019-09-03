use crate::Trigger;
use futures_core::{ready, stream::Stream};
use futures_util::future::{FutureExt, Shared};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::oneshot;

/// A stream combinator which takes elements from a stream until a future resolves.
///
/// This structure is produced by the [`StreamExt::take_until`] method.
#[pin_project]
#[derive(Clone, Debug)]
pub struct TakeUntil<S, F> {
    #[pin]
    stream: S,
    #[pin]
    until: F,
    free: bool,
}

/// This `Stream` extension trait provides a `take_until` method that terminates the stream once
/// the given future resolves.
pub trait StreamExt: Stream {
    /// Take elements from this stream until the given future resolves.
    ///
    /// This function will take elements from this stream until the given future resolves with
    /// `true`. Once it resolves, the stream will yield `None`, and produce no further elements.
    ///
    /// If the future resolves with `false`, the stream will be allowed to continue indefinitely.
    ///
    /// ```
    ///
    /// use stream_cancel::StreamExt;
    /// use tokio::prelude::*;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    ///     let (tx, rx) = tokio::sync::oneshot::channel();
    ///
    ///     tokio::spawn(async move {
    ///         let mut incoming = listener.incoming().take_until(rx.map(|_| true));
    ///         while let Some(s) = incoming.next().await.transpose().unwrap() {
    ///             let (mut r, mut w) = s.split();
    ///             tokio::spawn(async move {
    ///                 println!("copied {} bytes", r.copy(&mut w).await.unwrap());
    ///             });
    ///         }
    ///     });
    ///
    ///     // tell the listener to stop accepting new connections
    ///     tx.send(()).unwrap();
    ///     // the spawned async block will terminate cleanly, allowing main to return
    /// }
    /// ```
    fn take_until<U>(self, until: U) -> TakeUntil<Self, U>
    where
        U: Future<Output = bool>,
        Self: Sized,
    {
        TakeUntil {
            stream: self,
            until,
            free: false,
        }
    }
}

impl<S> StreamExt for S where S: Stream {}

impl<S, F> Stream for TakeUntil<S, F>
where
    S: Stream,
    F: Future<Output = bool>,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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
/// [`StreamExt::take_until`]. All `Tripwire` clones are associated with a single [`Trigger`],
/// which is then used to signal that all the associated streams should be terminated.
///
/// The `Tripwire` future resolves to `true` if the stream should be considered closed, and `false`
/// if the `Trigger` has been disabled.
///
/// `Tripwire` is internally implemented using a `Shared<oneshot::Receiver<()>>`, with the
/// `Trigger` holding the associated `oneshot::Sender`. There is very little magic.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Tripwire(#[pin] Shared<ResultTrueFalse<oneshot::Receiver<()>>>);

impl Tripwire {
    /// Make a new `Tripwire` and an associated [`Trigger`].
    pub fn new() -> (Trigger, Self) {
        let (tx, rx) = oneshot::channel();
        (Trigger(Some(tx)), Tripwire(ResultTrueFalse(rx).shared()))
    }
}

impl Future for Tripwire {
    type Output = bool;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().0.poll(cx)
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
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(self.project().0.poll(cx)).is_ok().into()
    }
}
