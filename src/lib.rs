//! This crate provides multiple mechanisms for interrupting a `Stream`.
//!
//! # Stream combinator
//!
//! The extension trait [`StreamExt`] provides a single new `Stream` combinator: `take_until_if`.
//! [`StreamExt::take_until_if`] continues yielding elements from the underlying `Stream` until a
//! `Future` resolves, and at that moment immediately yields `None` and stops producing further
//! elements.
//!
//! For convenience, the crate also includes the [`Tripwire`] type, which produces a cloneable
//! `Future` that can then be passed to `take_until_if`. When a new `Tripwire` is created, an
//! associated [`Trigger`] is also returned, which interrupts the `Stream` when it is dropped.
//!
//!
//! ```
//! use stream_cancel::{StreamExt, Tripwire};
//! use futures::prelude::*;
//! use tokio_stream::wrappers::TcpListenerStream;
//!
//! #[tokio::main]
//! async fn main() {
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
//!     let (trigger, tripwire) = Tripwire::new();
//!
//!     tokio::spawn(async move {
//!         let mut incoming = TcpListenerStream::new(listener).take_until_if(tripwire);
//!         while let Some(mut s) = incoming.next().await.transpose().unwrap() {
//!             tokio::spawn(async move {
//!                 let (mut r, mut w) = s.split();
//!                 println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
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
//! streams like a `TcpListener`. Once [`Trigger::cancel`] is called on the handle for a given
//! stream's [`Valved`], the stream will yield `None` to indicate that it has terminated.
//!
//! ```
//! use stream_cancel::Valved;
//! use futures::prelude::*;
//! use tokio_stream::wrappers::TcpListenerStream;
//! use std::thread;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
//!
//!     tokio::spawn(async move {
//!         let (exit, mut incoming) = Valved::new(TcpListenerStream::new(listener));
//!         exit_tx.send(exit).unwrap();
//!         while let Some(mut s) = incoming.next().await.transpose().unwrap() {
//!             tokio::spawn(async move {
//!                 let (mut r, mut w) = s.split();
//!                 println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
//!             });
//!         }
//!     });
//!
//!     let exit = exit_rx.await;
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
//! use futures::prelude::*;
//! use tokio_stream::wrappers::TcpListenerStream;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (exit, valve) = Valve::new();
//!     let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
//!     let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
//!
//!     tokio::spawn(async move {
//!         let incoming1 = valve.wrap(TcpListenerStream::new(listener1));
//!         let incoming2 = valve.wrap(TcpListenerStream::new(listener2));
//!
//!         use futures_util::stream::select;
//!         let mut incoming = select(incoming1, incoming2);
//!         while let Some(mut s) = incoming.next().await.transpose().unwrap() {
//!             tokio::spawn(async move {
//!                 let (mut r, mut w) = s.split();
//!                 println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
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

use tokio::sync::watch;

mod combinator;
mod wrapper;

pub use crate::combinator::{StreamExt, TakeUntilIf, Tripwire};
pub use crate::wrapper::{Valve, Valved};

/// A handle to a set of cancellable streams.
///
/// If the `Trigger` is dropped, any streams associated with it are interrupted (this is equivalent
/// to calling [`Trigger::cancel`]. To override this behavior, call [`Trigger::disable`].
#[derive(Debug)]
pub struct Trigger(Option<watch::Sender<bool>>);

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
            let _ = tx.send(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::prelude::*;
    use futures_util::stream::select;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_stream::wrappers::TcpListenerStream;

    #[test]
    fn tokio_run() {
        use std::thread;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();

            // start a tokio echo server
            rt.block_on(async move {
                let (exit, mut incoming) = Valved::new(TcpListenerStream::new(listener));
                exit_tx.send(exit).unwrap();
                while let Some(mut s) = incoming.next().await.transpose().unwrap() {
                    tokio::spawn(async move {
                        let (mut r, mut w) = s.split();
                        tokio::io::copy(&mut r, &mut w).await.unwrap();
                    });
                }
                tx.send(()).unwrap();
            });
            let _ = rt.block_on(rx).unwrap();
        });

        let exit = futures::executor::block_on(exit_rx);

        // the server thread will normally never exit, since more connections
        // can always arrive. however, with a Valved, we can turn off the
        // stream of incoming connections to initiate a graceful shutdown
        drop(exit);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tokio_rt_on_idle() {
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (exit, mut incoming) = Valved::new(TcpListenerStream::new(listener));
            exit_tx.send(exit).unwrap();
            while let Some(mut s) = incoming.next().await.transpose().unwrap() {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    tokio::io::copy(&mut r, &mut w).await.unwrap();
                });
            }
        });

        let exit = exit_rx.await;
        drop(exit);
    }

    #[tokio::test]
    async fn multi_interrupt() {
        let (exit, valve) = Valve::new();
        tokio::spawn(async move {
            let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let incoming1 = valve.wrap(TcpListenerStream::new(listener1));
            let incoming2 = valve.wrap(TcpListenerStream::new(listener2));

            let mut incoming = select(incoming1, incoming2);
            while let Some(mut s) = incoming.next().await.transpose().unwrap() {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    tokio::io::copy(&mut r, &mut w).await.unwrap();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let reqs = Arc::new(AtomicUsize::new(0));
        let got = reqs.clone();
        tokio::spawn(async move {
            let mut incoming = valve.wrap(TcpListenerStream::new(listener));
            while let Some(mut s) = incoming.next().await.transpose().unwrap() {
                reqs.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    tokio::io::copy(&mut r, &mut w).await.unwrap();
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
        let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr1 = listener1.local_addr().unwrap();
        let addr2 = listener2.local_addr().unwrap();

        let reqs = Arc::new(AtomicUsize::new(0));
        let got = reqs.clone();

        tokio::spawn(async move {
            let incoming1 = valve.wrap(TcpListenerStream::new(listener1));
            let incoming2 = valve.wrap(TcpListenerStream::new(listener2));
            let mut incoming = select(incoming1, incoming2);
            while let Some(mut s) = incoming.next().await.transpose().unwrap() {
                reqs.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    tokio::io::copy(&mut r, &mut w).await.unwrap();
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
