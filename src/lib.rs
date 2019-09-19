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
//! use stream_cancel::{StreamExt, Tripwire};
//! use tokio::prelude::*;
//!
//! #[tokio::main]
//! async fn main() {
//!     let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
//!     let (trigger, tripwire) = Tripwire::new();
//!
//!     tokio::spawn(async move {
//!         let mut incoming = listener.incoming().take_until(tripwire);
//!         while let Some(s) = incoming.next().await.transpose().unwrap() {
//!             let (mut r, mut w) = tokio::io::split(s);
//!             tokio::spawn(async move {
//!                 println!("copied {} bytes", r.copy(&mut w).await.unwrap());
//!             });
//!         }
//!     });
//!
//!     // tell the listener to stop accepting new connections
//!     drop(trigger);
//!     // the spawned async block will terminate cleanly, allowing main to return
//! }
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
//! use stream_cancel::Valved;
//! use tokio::prelude::*;
//! use std::thread;
//!
//! #[tokio::main]
//! async fn main() {
//!     let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
//!     let (exit, mut incoming) = Valved::new(listener.incoming());
//!
//!     tokio::spawn(async move {
//!         while let Some(s) = incoming.next().await.transpose().unwrap() {
//!             let (mut r, mut w) = tokio::io::split(s);
//!             tokio::spawn(async move {
//!                 println!("copied {} bytes", r.copy(&mut w).await.unwrap());
//!             });
//!         }
//!     });
//!
//!     // the server thread will normally never exit, since more connections
//!     // can always arrive. however, with a Valved, we can turn off the
//!     // stream of incoming connections to initiate a graceful shutdown
//!     drop(exit);
//! }
//! ```
//!
//! You can share the same [`Trigger`] between multiple streams by first creating a [`Valve`],
//! and then wrapping multiple streams using [`Valve::Wrap`]:
//!
//! ```
//! use stream_cancel::Valve;
//! use tokio::prelude::*;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (exit, valve) = Valve::new();
//!     let listener1 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
//!     let listener2 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
//!     let incoming1 = valve.wrap(listener1.incoming());
//!     let incoming2 = valve.wrap(listener2.incoming());
//!
//!     tokio::spawn(async move {
//!         use futures_util::stream::select;
//!         let mut incoming = select(incoming1, incoming2);
//!         while let Some(s) = incoming.next().await.transpose().unwrap() {
//!             let (mut r, mut w) = tokio::io::split(s);
//!             tokio::spawn(async move {
//!                 println!("copied {} bytes", r.copy(&mut w).await.unwrap());
//!             });
//!         }
//!     });
//!
//!     // the runtime will not become idle until both incoming1 and incoming2 have stopped
//!     // (due to the select). this checks that they are indeed both interrupted when the
//!     // valve is closed.
//!     drop(exit);
//! }
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

use tokio::sync::oneshot;

mod combinator;
mod wrapper;

pub use crate::combinator::{StreamExt, TakeUntil, Tripwire};
pub use crate::wrapper::{Valve, Valved};

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
            // Send may fail when all associated rx'es are dropped already
            // so code here cannot panic on error
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::select;
    use tokio::prelude::*;

    #[test]
    fn tokio_run() {
        use std::thread;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("0.0.0.0:0"))
            .unwrap();
        let (exit, mut incoming) = Valved::new(listener.incoming());

        let server = thread::spawn(move || {
            // start a tokio echo server
            rt.block_on(async move {
                while let Some(s) = incoming.next().await.transpose().unwrap() {
                    let (mut r, mut w) = tokio::io::split(s);
                    tokio::spawn(async move {
                        r.copy(&mut w).await.unwrap();
                    });
                }
            });
            rt.shutdown_on_idle();
        });

        // the server thread will normally never exit, since more connections
        // can always arrive. however, with a Valved, we can turn off the
        // stream of incoming connections to initiate a graceful shutdown
        drop(exit);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tokio_rt_on_idle() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let (exit, mut incoming) = Valved::new(listener.incoming());

        tokio::spawn(async move {
            while let Some(s) = incoming.next().await.transpose().unwrap() {
                let (mut r, mut w) = tokio::io::split(s);
                tokio::spawn(async move {
                    r.copy(&mut w).await.unwrap();
                });
            }
        });

        drop(exit);
    }

    #[tokio::test]
    async fn multi_interrupt() {
        let (exit, valve) = Valve::new();
        let listener1 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let listener2 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let incoming1 = valve.wrap(listener1.incoming());
        let incoming2 = valve.wrap(listener2.incoming());

        tokio::spawn(async move {
            let mut incoming = select(incoming1, incoming2);
            while let Some(s) = incoming.next().await.transpose().unwrap() {
                let (mut r, mut w) = tokio::io::split(s);
                tokio::spawn(async move {
                    r.copy(&mut w).await.unwrap();
                });
            }
        });

        // the runtime will not become idle until both incoming1 and incoming2 have stopped (due to
        // the select). this checks that they are indeed both interrupted when the valve is closed.
        drop(exit);
    }

    #[tokio::test]
    async fn yields_many() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let (exit, valve) = Valve::new();
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut incoming = valve.wrap(listener.incoming());

        let reqs = Arc::new(AtomicUsize::new(0));
        let got = reqs.clone();
        tokio::spawn(async move {
            while let Some(s) = incoming.next().await.transpose().unwrap() {
                reqs.fetch_add(1, Ordering::SeqCst);
                let (mut r, mut w) = tokio::io::split(s);
                tokio::spawn(async move {
                    r.copy(&mut w).await.unwrap();
                });
            }
        });

        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        s.write_all(b"hello").await.unwrap();
        let mut buf = [0; 5];
        s.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop(s);

        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        s.write_all(b"world").await.unwrap();
        let mut buf = [0; 5];
        s.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(&buf, b"world");
        drop(s);

        assert_eq!(got.load(Ordering::SeqCst), 2);

        drop(exit);
    }

    #[tokio::test]
    async fn yields_some() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let (exit, valve) = Valve::new();
        let listener1 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let listener2 = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let addr1 = listener1.local_addr().unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let incoming1 = valve.wrap(listener1.incoming());
        let incoming2 = valve.wrap(listener2.incoming());

        let reqs = Arc::new(AtomicUsize::new(0));
        let got = reqs.clone();

        tokio::spawn(async move {
            let mut incoming = select(incoming1, incoming2);
            while let Some(s) = incoming.next().await.transpose().unwrap() {
                reqs.fetch_add(1, Ordering::SeqCst);
                let (mut r, mut w) = tokio::io::split(s);
                tokio::spawn(async move {
                    r.copy(&mut w).await.unwrap();
                });
            }
        });

        let mut s = tokio::net::TcpStream::connect(&addr1).await.unwrap();
        s.write_all(b"hello").await.unwrap();
        let mut buf = [0; 5];
        s.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop(s);

        let mut s = tokio::net::TcpStream::connect(&addr2).await.unwrap();
        s.write_all(b"world").await.unwrap();
        let mut buf = [0; 5];
        s.read_exact(&mut buf[..]).await.unwrap();
        assert_eq!(&buf, b"world");
        drop(s);

        assert_eq!(got.load(Ordering::SeqCst), 2);

        drop(exit);
    }
}
