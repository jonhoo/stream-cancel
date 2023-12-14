use crate::{StreamExt, TakeUntilIf, Trigger, Tripwire};
use futures_core::stream::Stream;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A `Valve` is associated with a [`Trigger`], and can be used to wrap one or more
/// asynchronous streams. All streams wrapped by a given `Valve` (or its clones) will be
/// interrupted when [`Trigger::close`] is called on the valve's associated handle.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Valve(#[pin] Tripwire);

impl Valve {
    /// Make a new `Valve` and an associated [`Trigger`].
    pub fn new() -> (Trigger, Self) {
        let (t, tw) = Tripwire::new();
        (t, Valve(tw))
    }

    /// Wrap the given `stream` with this `Valve`.
    ///
    /// When [`Trigger::close`] is called on the handle associated with this valve, the given
    /// stream will immediately yield `None`.
    pub fn wrap<S>(&self, stream: S) -> Valved<S>
    where
        S: Stream,
    {
        Valved(stream.take_until_if(self.0.clone()))
    }

    /// Check if the valve has been closed.
    ///
    /// If `Ready`, contains `true` if the stream should be considered closed, and `false`
    /// if the `Trigger` has been disabled.
    pub fn poll_closed(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        self.project().0.poll(cx)
    }
}

/// A `Valved` is wrapper around a `Stream` that enables the stream to be turned off remotely to
/// initiate a graceful shutdown. When a new `Valved` is created with [`Valved::new`], a handle to
/// that `Valved` is also produced; when [`Trigger::close`] is called on that handle, the
/// wrapped stream will immediately yield `None` to indicate that it has completed.
#[derive(Clone, Debug)]
pub struct Valved<S>(TakeUntilIf<S, Tripwire>);

impl<S> Valved<S> {
    /// Make the given stream cancellable.
    ///
    /// To cancel the stream, call [`Trigger::close`] on the returned handle.
    pub fn new(stream: S) -> (Trigger, Self)
    where
        S: Stream,
    {
        let (vh, v) = Valve::new();
        (vh, v.wrap(stream))
    }

    /// Consumes this wrapper, returning the underlying stream.
    pub fn into_inner(self) -> S {
        self.0.into_inner()
    }
}

impl<S> Stream for Valved<S>
where
    S: Stream,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // safe since we never move nor leak &mut
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0) };
        inner.poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::empty;
    #[test]
    fn valved_stream_may_be_dropped_safely() {
        let _orphan = {
            let s = empty::<()>();
            let (trigger, valve) = Valve::new();
            let _wrapped = valve.wrap(s);
            trigger
        };
    }
}
