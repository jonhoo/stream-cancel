# stream-cancel

[![Crates.io](https://img.shields.io/crates/v/stream-cancel.svg)](https://crates.io/crates/stream-cancel)
[![Documentation](https://docs.rs/stream-cancel/badge.svg)](https://docs.rs/stream-cancel/)
[![Build Status](https://travis-ci.org/jonhoo/stream-cancel.svg?branch=master)](https://travis-ci.org/jonhoo/stream-cancel)

This crate provides a mechanism for interrupting a `Stream`.

Any stream can be wrapped in a [`Valved`], which enables it to be remotely terminated through
an associated [`ValveHandle`]. This can be useful to implement graceful shutdown on "infinite"
streams like a `TcpListener`. Once [`ValveHandle::close`] is called on the handle for a given
stream's [`Valved`], the stream will yield `None` to indicate that it has terminated.

```rust
extern crate tokio;

use stream_cancel::Valved;
use tokio::prelude::*;
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
```

You can share the same [`ValveHandle`] between multiple streams by first creating a [`Valve`],
and then wrapping multiple streams using [`Valve::Wrap`]:

```rust
extern crate tokio;

use stream_cancel::Valve;
use tokio::prelude::*;

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

// the runtime will not become idle until both incoming1 and incoming2 have stopped
// (due to the select). this checks that they are indeed both interrupted when the
// valve is closed.
drop(exit);
rt.shutdown_on_idle().wait().unwrap();
```
