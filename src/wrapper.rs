use futures::{Poll, Stream};
use {StreamExt, TakeUntil, Trigger, Tripwire};

/// A `Valve` is associated with a [`Trigger`], and can be used to wrap one or more
/// asynchronous streams. All streams wrapped by a given `Valve` (or its clones) will be
/// interrupted when [`Trigger::close`] is called on the valve's associated handle.
#[derive(Clone, Debug)]
pub struct Valve(Tripwire);

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
        Valved(stream.take_until(self.0.clone()))
    }
}

/// A `Valved` is wrapper around a `Stream` that enables the stream to be turned off remotely to
/// initiate a graceful shutdown. When a new `Valved` is created with [`Valved::new`], a handle to
/// that `Valved` is also produced; when [`Trigger::close`] is called on that handle, the
/// wrapped stream will immediately yield `None` to indicate that it has completed.
#[derive(Clone, Debug)]
pub struct Valved<S>(TakeUntil<S, Tripwire>);

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
}

impl<S> Stream for Valved<S>
where
    S: Stream,
{
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::empty;
    #[test]
    fn valved_stream_may_be_dropped_safely() {
        let _orphan = {
            let s = empty::<(), ()>();
            let (trigger, valve) = Valve::new();
            let _wrapped = valve.wrap(s);
            trigger
        };
    }
}
