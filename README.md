# stream-cancel

[![Crates.io](https://img.shields.io/crates/v/stream-cancel.svg)](https://crates.io/crates/stream-cancel)
[![Documentation](https://docs.rs/stream-cancel/badge.svg)](https://docs.rs/stream-cancel/)
[![Build Status](https://travis-ci.org/jonhoo/stream-cancel.svg?branch=master)](https://travis-ci.org/jonhoo/stream-cancel)

This crate provides a mechanism for interrupting a `Stream`.

Any stream can be wrapped in a [`Valve`], which enables it to be remotely terminated through an
associated [`ValveHandle`]. This can be useful to implement graceful shutdown on "infinite"
streams like a `TcpListener`. Once [`ValveHandle::close`] is called on the handle for a given
stream's [`Valve`], the stream will yield `None` to indicate that it has terminated.

```rust
extern crate tokio;

use stream_cancel::Valve;
use tokio::prelude::*;
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
```
