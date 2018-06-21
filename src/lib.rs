//! This crate provides a mechanism for interrupting a `Stream`.
//!
//! Any stream can be wrapped in a [`Valve`], which enables it to be remotely terminated through an
//! associated [`ValveHandle`]. This can be useful to implement graceful shutdown on "infinite"
//! streams like a `TcpListener`. Once [`ValveHandle::close`] is called on the handle for a given
//! stream's [`Valve`], the stream will yield `None` to indicate that it has terminated.
//!
//! ```
//! # extern crate stream_cancel;
//! extern crate tokio;
//!
//! use stream_cancel::Valve;
//! use tokio::prelude::*;
//! use std::thread;
//!
//! let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
//! let (exit, incoming) = Valve::new(listener.incoming());
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
//! // can always arrive. however, with a Valve, we can turn off the
//! // stream of incoming connections to initiate a graceful shutdown
//! exit.close();
//! server.join().unwrap();
//! ```

#![deny(missing_docs)]

extern crate futures;

#[cfg(test)]
extern crate tokio;

use futures::{future::Shared, sync::oneshot, Async, Future, Poll, Stream};

/// A `Valve` is wrapper around a `Stream` that enables the stream to be turned off remotely to
/// initiate a graceful shutdown. When a new `Valve` is created with [`Valve::new`], a handle to
/// that `Valve` is also produced; when [`ValveHandle::close`] is called on that handle, the
/// wrapped stream will immediately yield `None` to indicate that it has completed.
#[derive(Clone, Debug)]
pub struct Valve<S> {
    stream: S,
    valve: Shared<oneshot::Receiver<()>>,
    free: bool,
}

impl<S> Valve<S> {
    /// Make the given stream cancellable.
    ///
    /// To cancel the stream, call [`ValveHandle::close`] on the returned handle.
    pub fn new(stream: S) -> (ValveHandle, Self) {
        let (tx, rx) = oneshot::channel();
        (
            ValveHandle(tx),
            Valve {
                stream,
                valve: rx.shared(),
                free: false,
            },
        )
    }
}

/// A handle to a wrapped stream.
#[derive(Debug)]
pub struct ValveHandle(oneshot::Sender<()>);

impl ValveHandle {
    /// Close the valve for the associated stream, and make it immediately yield `None`.
    pub fn close(self) {
        self.0.send(()).unwrap();
    }
}

impl<S> Stream for Valve<S>
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
                    // valve handle was dropped -- let the stream go forever
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
        let (exit, incoming) = Valve::new(listener.incoming());

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
        // can always arrive. however, with a Valve, we can turn off the
        // stream of incoming connections to initiate a graceful shutdown
        exit.close();
        server.join().unwrap();
    }

    #[test]
    fn tokio_rt_on_idle() {
        let listener = tokio::net::TcpListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
        let (exit, incoming) = Valve::new(listener.incoming());

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

        exit.close();
        rt.shutdown_on_idle().wait().unwrap();
    }
}
