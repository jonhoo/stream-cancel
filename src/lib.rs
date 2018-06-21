//! This crate provides a mechanism for interrupting a `Stream`.
//!
//! Any stream can be wrapped in a [`Valved`], which enables it to be remotely terminated through
//! an associated [`ValveHandle`]. This can be useful to implement graceful shutdown on "infinite"
//! streams like a `TcpListener`. Once [`ValveHandle::close`] is called on the handle for a given
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
//! You can share the same [`ValveHandle`] between multiple streams by first creating a [`Valve`],
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

use futures::{future::Shared, sync::oneshot, Async, Future, Poll, Stream};

/// A `Valved` is wrapper around a `Stream` that enables the stream to be turned off remotely to
/// initiate a graceful shutdown. When a new `Valved` is created with [`Valved::new`], a handle to
/// that `Valved` is also produced; when [`ValveHandle::close`] is called on that handle, the
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
    /// To cancel the stream, call [`ValveHandle::close`] on the returned handle.
    pub fn new(stream: S) -> (ValveHandle, Self) {
        let (vh, v) = Valve::new();
        (vh, v.wrap(stream))
    }
}

/// A `Valve` is associated with a [`ValveHandle`], and can be used to wrap one or more
/// asynchronous streams. All streams wrapped by a given `Valve` (or its clones) will be
/// interrupted when [`ValveHandle::close`] is called on the valve's associated handle.
#[derive(Clone, Debug)]
pub struct Valve(Shared<oneshot::Receiver<()>>);

impl Valve {
    /// Make a new `Valve` and an associated [`ValveHandle`].
    pub fn new() -> (ValveHandle, Self) {
        let (tx, rx) = oneshot::channel();
        (ValveHandle(Some(tx)), Valve(rx.shared()))
    }

    /// Wrap the given `stream` with this `Valve`.
    ///
    /// When [`ValveHandle::close`] is called on the handle associated with this valve, the given
    /// stream will immediately yield `None`.
    pub fn wrap<S>(&self, stream: S) -> Valved<S> {
        Valved {
            stream,
            valve: self.0.clone(),
            free: false,
        }
    }
}

/// A handle to a wrapped stream.
///
/// If the `ValveHandle` is dropped, any streams wrapped by associated valves will be interrupted
/// (this is equivalent to calling [`ValveHandle::close`]. To override this behavior, call
/// [`ValveHandle::disable`].
#[derive(Debug)]
pub struct ValveHandle(Option<oneshot::Sender<()>>);

impl ValveHandle {
    /// Close the valve for the associated stream, and make it immediately yield `None`.
    pub fn close(self) {
        drop(self);
    }

    /// Disable all associated valves and leave their associated streams running to completion.
    pub fn disable(mut self) {
        let _ = self.0.take();
        drop(self);
    }
}

impl Drop for ValveHandle {
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
