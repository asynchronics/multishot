# multishot

An async, lock-free, reusable channel for sending single values to asynchronous
tasks.

[![Cargo](https://img.shields.io/crates/v/multishot.svg)](https://crates.io/crates/multishot)
[![Documentation](https://docs.rs/multishot/badge.svg)](https://docs.rs/multishot)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/asynchronics/multishot#license)

## Overview

In a multi-shot channel, the receiver half is reusable and able to recycle the
sender half without ever re-allocating. Sending or awaiting a value and
recycling the sender are all lock-free operations, the last two being
additionally wait-free. Producing a new sender does not require additional
synchronization or spinning: it is guaranteed to succeed immediately if the
value sent by a previous sender was received.

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
multishot = "0.3.2"
```

## Example

```rust
use std::thread;

async {
    let (s, mut r) = multishot::channel();

    // Send a value to the channel from another thread.
    thread::spawn(move || {
        s.send("42");
    });

    // Receive the value.
    let res = r.recv().await;
    assert_eq!(res, Ok("42"));

    // Recycle the sender. This is guaranteed to succeed if the previous
    // message has been read.
    let s = r.sender().unwrap();

    // Drop the sender on another thread without sending a message.
    thread::spawn(move || {
        drop(s);
    });

    // Receive an error.
    let res = r.recv().await;
    assert_eq!(res, Err(multishot::RecvError {}));
};
```

## Safety

This is a low-level primitive and as such its implementation relies on `unsafe`.
The test suite makes extensive use of [Loom] and MIRI to assess its correctness.
As amazing as they are, however, Loom and MIRI cannot formally prove the absence
of data races so soundness issues _are_ possible.

[Loom]: https://github.com/tokio-rs/loom


## Implementation

Sending, receiving and recycling a sender are lock-free operations; the last two
are additionally wait-free.

Polling requires no read-modify-write (RMW) operation if the value is readily
available, 1 RMW if this is the first waker update and 2 RMWs otherwise. Sending
needs 1 RMW if no waker was registered, and typically 2 RMW if one was
registered. Compared to a non-reusable one-shot channel such as Tokio's, the
only extra cost is 1 RMW in case the waker was updated (which is rare in
practice). Also, the implementation of `multishot` partially offsets this extra
cost by using arithmetic atomic operations when sending rather than the
typically more expensive compare-and-swap operation.


## License

This software is licensed under the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
