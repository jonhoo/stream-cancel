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

use futures::sync::oneshot;

mod combinator;
mod wrapper;

pub use combinator::{StreamExt, TakeUntil, Tripwire};
pub use wrapper::{Valve, Valved};

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
