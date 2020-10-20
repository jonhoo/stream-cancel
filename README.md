[![Crates.io](https://img.shields.io/crates/v/stream-cancel.svg)](https://crates.io/crates/stream-cancel)
[![Documentation](https://docs.rs/stream-cancel/badge.svg)](https://docs.rs/stream-cancel/)
[![Build Status](https://dev.azure.com/jonhoo/jonhoo/_apis/build/status/stream-cancel?branchName=master)](https://dev.azure.com/jonhoo/jonhoo/_build/latest?definitionId=29&branchName=master)
[![Coverage Status](https://codecov.io/gh/jonhoo/stream-cancel/branch/master/graph/badge.svg)](https://codecov.io/gh/jonhoo/stream-cancel)

This crate provides multiple mechanisms for interrupting a `Stream`.

## Stream combinator

The extension trait [`StreamExt`] provides a single new `Stream` combinator: `take_until_if`.
[`StreamExt::take_until_if`] continues yielding elements from the underlying `Stream` until a
`Future` resolves, and at that moment immediately yields `None` and stops producing further
elements.

For convenience, the crate also includes the [`Tripwire`] type, which produces a cloneable
`Future` that can then be passed to `take_until_if`. When a new `Tripwire` is created, an
associated [`Trigger`] is also returned, which interrupts the `Stream` when it is dropped.


```rust
use stream_cancel::{StreamExt, Tripwire};
use futures::prelude::*;
use tokio::prelude::*;

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (trigger, tripwire) = Tripwire::new();

    tokio::spawn(async move {
        let mut incoming = listener.take_until_if(tripwire);
        while let Some(mut s) = incoming.next().await.transpose().unwrap() {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
            });
        }
    });

    // tell the listener to stop accepting new connections
    drop(trigger);
    // the spawned async block will terminate cleanly, allowing main to return
}
```

## Stream wrapper

Any stream can be wrapped in a [`Valved`], which enables it to be remotely terminated through
an associated [`Trigger`]. This can be useful to implement graceful shutdown on "infinite"
streams like a `TcpListener`. Once [`Trigger::cancel`] is called on the handle for a given
stream's [`Valved`], the stream will yield `None` to indicate that it has terminated.

```rust
use stream_cancel::Valved;
use futures::prelude::*;
use tokio::prelude::*;
use std::thread;

#[tokio::main]
async fn main() {
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    tokio::spawn(async move {
        let (exit, mut incoming) = Valved::new(listener);
        exit_tx.send(exit).unwrap();
        while let Some(mut s) = incoming.next().await.transpose().unwrap() {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
            });
        }
    });

    let exit = exit_rx.await;

    // the server thread will normally never exit, since more connections
    // can always arrive. however, with a Valved, we can turn off the
    // stream of incoming connections to initiate a graceful shutdown
    drop(exit);
}
```

You can share the same [`Trigger`] between multiple streams by first creating a [`Valve`],
and then wrapping multiple streams using [`Valve::Wrap`]:

```rust
use stream_cancel::Valve;
use futures::prelude::*;
use tokio::prelude::*;

#[tokio::main]
async fn main() {
    let (exit, valve) = Valve::new();
    let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    tokio::spawn(async move {
        let incoming1 = valve.wrap(listener1);
        let incoming2 = valve.wrap(listener2);

        use futures_util::stream::select;
        let mut incoming = select(incoming1, incoming2);
        while let Some(mut s) = incoming.next().await.transpose().unwrap() {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                println!("copied {} bytes", tokio::io::copy(&mut r, &mut w).await.unwrap());
            });
        }
    });

    // the runtime will not become idle until both incoming1 and incoming2 have stopped
    // (due to the select). this checks that they are indeed both interrupted when the
    // valve is closed.
    drop(exit);
}
```
