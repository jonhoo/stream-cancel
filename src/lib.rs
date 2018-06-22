//! This crate provides multiple mechanisms for interrupting a `Stream`.
//!
//! # Stream combinator
//!
//! The extension trait [`StreamExt`] provides a single new `Stream` combinator: `take_until`.
//! [`StreamExt::take_until`] continues yielding elements from the underlying `Stream` until a
//! `Future` resolves, and at that moment immediately yields `None` and stops producing further
//! elements.
//!
//! For convenience, the crate also includes the [`Tripwire`] type, which produces a cloneable
//! `Future` that can then be passed to `take_until`. When a new `Tripwire` is created, an
//! associated [`Trigger`] is also returned, which interrupts the `Stream` when it is dropped.
//!
//!
//! ```
//! # extern crate stream_cancel;
//! extern crate tokio;
//!
//! use stream_cancel::{StreamExt, Tripwire};
//! use tokio::prelude::*;
//!
//! let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
//! let (trigger, tripwire) = Tripwire::new();
//!
//! let mut rt = tokio::runtime::Runtime::new().unwrap();
//! rt.spawn(
//!     listener
//!         .incoming()
//!         .take_until(tripwire)
//!         .map_err(|e| eprintln!("accept failed = {:?}", e))
//!         .for_each(|sock| {
//!             let (reader, writer) = sock.split();
//!             tokio::spawn(
//!                 tokio::io::copy(reader, writer)
//!                     .map(|amt| println!("wrote {:?} bytes", amt))
//!                     .map_err(|err| eprintln!("IO error {:?}", err)),
//!             )
//!         }),
//! );
//!
//! // tell the listener to stop accepting new connections
//! drop(trigger);
//! rt.shutdown_on_idle().wait().unwrap();
//! ```
//!
//! # Stream wrapper
//!
//! Any stream can be wrapped in a [`Valved`], which enables it to be remotely terminated through
//! an associated [`Trigger`]. This can be useful to implement graceful shutdown on "infinite"
//! streams like a `TcpListener`. Once [`Trigger::close`] is called on the handle for a given
//! stream's [`Valved`], the stream will yield `None` to indicate that it has terminated.
//!
//! ```
//! # extern crate stream_cancel;
//! extern crate tokio;
//!
//! use stream_cancel::Valved;
//! use tokio::prelude::*;
//! use std::thread;
//!
//! let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
//! let (exit, incoming) = Valved::new(listener.incoming());
//!
//! let server = thread::spawn(move || {
//!     // start a tokio echo server
//!     tokio::run(
//!         incoming
//!             .map_err(|e| eprintln!("accept failed = {:?}", e))
//!             .for_each(|sock| {
//!                 let (reader, writer) = sock.split();
//!                 tokio::spawn(
//!                     tokio::io::copy(reader, writer)
//!                         .map(|amt| println!("wrote {:?} bytes", amt))
//!                         .map_err(|err| eprintln!("IO error {:?}", err)),
//!                 )
//!             }),
//!     )
//! });
//!
//! // the server thread will normally never exit, since more connections
//! // can always arrive. however, with a Valved, we can turn off the
//! // stream of incoming connections to initiate a graceful shutdown
//! drop(exit);
//! server.join().unwrap();
//! ```
//!
//! You can share the same [`Trigger`] between multiple streams by first creating a [`Valve`],
//! and then wrapping multiple streams using [`Valve::Wrap`]:
//!
//! ```
//! # extern crate stream_cancel;
//! extern crate tokio;
//!
//! use stream_cancel::Valve;
//! use tokio::prelude::*;
//!
//! let (exit, valve) = Valve::new();
//! let listener1 = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
//! let listener2 = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
//! let incoming1 = valve.wrap(listener1.incoming());
//! let incoming2 = valve.wrap(listener2.incoming());
//!
//! let mut rt = tokio::runtime::Runtime::new().unwrap();
//! rt.spawn(
//!     incoming1
//!         .select(incoming2)
//!         .map_err(|e| eprintln!("accept failed = {:?}", e))
//!         .for_each(|sock| {
//!             let (reader, writer) = sock.split();
//!             tokio::spawn(
//!                 tokio::io::copy(reader, writer)
//!                     .map(|amt| println!("wrote {:?} bytes", amt))
//!                     .map_err(|err| eprintln!("IO error {:?}", err)),
//!             )
//!         }),
//! );
//!
//! // the runtime will not become idle until both incoming1 and incoming2 have stopped
//! // (due to the select). this checks that they are indeed both interrupted when the
//! // valve is closed.
//! drop(exit);
//! rt.shutdown_on_idle().wait().unwrap();
//! ```

#![deny(missing_docs)]

extern crate futures;

#[cfg(test)]
extern crate tokio;

use futures::{future::Shared, sync::oneshot, Async, Future, IntoFuture, Poll, Stream};

/// A stream combinator which takes elements from a stream until a future resolves.
///
/// This structure is produced by the [`StreamExt::take_until`] method.
pub struct TakeUntil<S, F> {
    stream: S,
    until: F,
    free: bool,
}

/// This `Stream` extension trait provides a `take_until` method that terminates the stream once
/// the given future resolves.
pub trait StreamExt: Stream {
    /// Take elements from this stream until the given future resolves.
    ///
    /// This function will take elements from this stream until the given future resolves. Once it
    /// resolves, the stream will yield `None`, and produce no further elements.
    ///
    /// If the future produces an error, the stream will be allowed to continue indefinitely.
    ///
    /// ```
    /// # extern crate stream_cancel;
    /// extern crate tokio;
    /// extern crate futures;
    ///
    /// use stream_cancel::StreamExt;
    /// use tokio::prelude::*;
    ///
    /// let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
    /// let (tx, rx) = futures::sync::oneshot::channel();
    ///
    /// let mut rt = tokio::runtime::Runtime::new().unwrap();
    /// rt.spawn(
    ///     listener
    ///         .incoming()
    ///         .take_until(rx.map_err(|_| ()))
    ///         .map_err(|e| eprintln!("accept failed = {:?}", e))
    ///         .for_each(|sock| {
    ///             let (reader, writer) = sock.split();
    ///             tokio::spawn(
    ///                 tokio::io::copy(reader, writer)
    ///                     .map(|amt| println!("wrote {:?} bytes", amt))
    ///                     .map_err(|err| eprintln!("IO error {:?}", err)),
    ///             )
    ///         }),
    /// );
    ///
    /// // tell the listener to stop accepting new connections
    /// tx.send(()).unwrap();
    /// rt.shutdown_on_idle().wait().unwrap();
    /// ```
    fn take_until<U>(self, until: U) -> TakeUntil<Self, U::Future>
    where
        U: IntoFuture<Item = (), Error = ()>,
        Self: Sized,
    {
        TakeUntil {
            stream: self,
            until: until.into_future(),
            free: false,
        }
    }
}

impl<S> StreamExt for S
where
    S: Stream,
{
}

impl<S, F> Stream for TakeUntil<S, F>
where
    S: Stream,
    F: Future<Item = (), Error = ()>,
{
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if !self.free {
            match self.until.poll() {
                Ok(Async::Ready(_)) => {
                    // future resolved -- terminate stream
                    return Ok(Async::Ready(None));
                }
                Err(_) => {
                    // future failed -- unclear whether we should stop or continue?
                    // to provide a mechanism for the creator to let the stream run forever,
                    // we interpret this as "run forever".
                    self.free = true;
                }
                Ok(Async::NotReady) => {}
            }
        }

        self.stream.poll()
    }
}

/// A `Tripwire` is a convenient mechanism for implementing graceful shutdown over many
/// asynchronous streams. A `Tripwire` is a `Future` that is `Clone`, and that can be passed to
/// [`StreamExt::take_until`]. All `Tripwire` clones are associated with a single [`Trigger`],
/// which is then used to signal that all the associated streams should be terminated.
///
/// `Tripwire` is internally implemented using a `Shared<oneshot::Receiver<()>>`, with the
/// `Trigger` holding the associated `oneshot::Sender`. There is very little magic.
#[derive(Clone, Debug)]
pub struct Tripwire(Shared<oneshot::Receiver<()>>);

impl Tripwire {
    /// Make a new `Tripwire` and an associated [`Trigger`].
    pub fn new() -> (Trigger, Self) {
        let (tx, rx) = oneshot::channel();
        (Trigger(Some(tx)), Tripwire(rx.shared()))
    }
}

impl Future for Tripwire {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        match self.0.poll() {
            Ok(Async::Ready(_)) => Ok(Async::Ready(())),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(()),
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

/// A handle to a set of cancellable streams.
///
/// If the `Trigger` is dropped, any streams associated with it are interrupted (this is equivalent
/// to calling [`Trigger::close`]. To override this behavior, call [`Trigger::disable`].
#[derive(Debug)]
pub struct Trigger(Option<oneshot::Sender<()>>);

impl Trigger {
    /// Cancel all associated streams, and make them immediately yield `None`.
    pub fn cancel(self) {
        drop(self);
    }

    /// Disable the `Trigger`, and leave all associated streams running to completion.
    pub fn disable(mut self) {
        let _ = self.0.take();
        drop(self);
    }
}

impl Drop for Trigger {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            tx.send(()).unwrap();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::prelude::*;

    #[test]
    fn tokio_run() {
        use std::thread;

        let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
        let (exit, incoming) = Valved::new(listener.incoming());

        let server = thread::spawn(move || {
            // start a tokio echo server
            tokio::run(
                incoming
                    .map_err(|e| eprintln!("accept failed = {:?}", e))
                    .for_each(|sock| {
                        let (reader, writer) = sock.split();
                        tokio::spawn(
                            tokio::io::copy(reader, writer)
                                .map(|amt| println!("wrote {:?} bytes", amt))
                                .map_err(|err| eprintln!("IO error {:?}", err)),
                        )
                    }),
            )
        });

        // the server thread will normally never exit, since more connections
        // can always arrive. however, with a Valved, we can turn off the
        // stream of incoming connections to initiate a graceful shutdown
        drop(exit);
        server.join().unwrap();
    }

    #[test]
    fn tokio_rt_on_idle() {
        let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
        let (exit, incoming) = Valved::new(listener.incoming());

        let mut rt = tokio::runtime::Runtime::new().unwrap();
        rt.spawn(
            incoming
                .map_err(|e| eprintln!("accept failed = {:?}", e))
                .for_each(|sock| {
                    let (reader, writer) = sock.split();
                    tokio::spawn(
                        tokio::io::copy(reader, writer)
                            .map(|amt| println!("wrote {:?} bytes", amt))
                            .map_err(|err| eprintln!("IO error {:?}", err)),
                    )
                }),
        );

        drop(exit);
        rt.shutdown_on_idle().wait().unwrap();
    }

    #[test]
    fn multi_interrupt() {
        let (exit, valve) = Valve::new();
        let listener1 = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
        let listener2 = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
        let incoming1 = valve.wrap(listener1.incoming());
        let incoming2 = valve.wrap(listener2.incoming());

        let mut rt = tokio::runtime::Runtime::new().unwrap();
        rt.spawn(
            incoming1
                .select(incoming2)
                .map_err(|e| eprintln!("accept failed = {:?}", e))
                .for_each(|sock| {
                    let (reader, writer) = sock.split();
                    tokio::spawn(
                        tokio::io::copy(reader, writer)
                            .map(|amt| println!("wrote {:?} bytes", amt))
                            .map_err(|err| eprintln!("IO error {:?}", err)),
                    )
                }),
        );

        // the runtime will not become idle until both incoming1 and incoming2 have stopped (due to
        // the select). this checks that they are indeed both interrupted when the valve is closed.
        drop(exit);
        rt.shutdown_on_idle().wait().unwrap();
    }
}
