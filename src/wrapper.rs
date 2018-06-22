use futures::{future::Shared, sync::oneshot, Async, Future, Poll, Stream};
use Trigger;

/// A `Valve` is associated with a [`Trigger`], and can be used to wrap one or more
/// asynchronous streams. All streams wrapped by a given `Valve` (or its clones) will be
/// interrupted when [`Trigger::close`] is called on the valve's associated handle.
#[derive(Clone, Debug)]
pub struct Valve(Shared<oneshot::Receiver<()>>);

impl Valve {
    /// Make a new `Valve` and an associated [`Trigger`].
    pub fn new() -> (Trigger, Self) {
        let (tx, rx) = oneshot::channel();
        (Trigger(Some(tx)), Valve(rx.shared()))
    }

    /// Wrap the given `stream` with this `Valve`.
    ///
    /// When [`Trigger::close`] is called on the handle associated with this valve, the given
    /// stream will immediately yield `None`.
    pub fn wrap<S>(&self, stream: S) -> Valved<S> {
        Valved {
            stream,
            valve: self.0.clone(),
            free: false,
        }
    }
}

/// A `Valved` is wrapper around a `Stream` that enables the stream to be turned off remotely to
/// initiate a graceful shutdown. When a new `Valved` is created with [`Valved::new`], a handle to
/// that `Valved` is also produced; when [`Trigger::close`] is called on that handle, the
/// wrapped stream will immediately yield `None` to indicate that it has completed.
#[derive(Clone, Debug)]
pub struct Valved<S> {
    stream: S,
    valve: Shared<oneshot::Receiver<()>>,
    free: bool,
}

impl<S> Valved<S> {
    /// Make the given stream cancellable.
    ///
    /// To cancel the stream, call [`Trigger::close`] on the returned handle.
    pub fn new(stream: S) -> (Trigger, Self) {
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
        if !self.free {
            match self.valve.poll() {
                Ok(Async::Ready(_)) => {
                    // valve closed -- terminate stream
                    return Ok(Async::Ready(None));
                }
                Err(_) => {
                    // valve handle was dropped, but nothing was sent
                    // then handle must have been disabled, so let the stream go forever
                    self.free = true;
                }
                Ok(Async::NotReady) => {}
            }
        }

        self.stream.poll()
    }
}
