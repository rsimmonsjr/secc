
# Skip Enabled Concurrent Channel for Rust (SECC)

[![Latest version](https://img.shields.io/crates/v/secc.svg)](https://crates.io/crates/secc)
[![Build Status](https://api.travis-ci.org/rsimmonsjr/secc.svg?branch=master)](https://travis-ci.org/rsimmonsjr/secc)
[![Average time to resolve an issue](https://isitmaintained.com/badge/resolution/rsimmonsjr/secc.svg)](https://isitmaintained.com/project/rsimmonsjr/secc)
[![License](https://img.shields.io/crates/l/secc.svg)](https://github.com/rsimmonsjr/secc#license)

# Description

An Skip Enabled Concurrent Channel (SECC) is a bounded capacity channel that supports multiple
senders and multiple recievers and allows the receiver to temporarily skip receiving messages
if they desire.

Messages in the channel need to be clonable to implement the [`peek`] functionality (which
returns a clone of the message). For this reason it is advisable that the user chose a type
that is efficiently clonable, such as an [`Arc`] to enclose a message that cannot be
efficiently cloned.

The channel is a FIFO structure unless the user intends to skip one or more messages
in which case a message could be read in a different order. The channel does, however,
guarantee that the messages will remain in the same order as sent and, unless skipped, will
be received in order.

SECC is implemented using two linked lists where one list acts as a pool of nodes and the
other list acts as the queue holding the messages. This allows us to move nodes in and out
of the list and even skip a message with O(1) efficiency. If there are 1000 messages and
the user desires to skip one in the middle they will incur virtually the exact same
performance cost as a normal read operation. There are only a couple of additional pointer
operations necessary to remove a node out of the middle of the linked list that implements
the queue.  When a message is received from the channel the node holding the message is
removed from the queue and appended to the tail of the pool. Conversely, when a  message is
sent to the channel the node moves from the head of the pool to the tail of the queue. In
this manner nodes are constantly cycled in and out of the queue so we only need to allocate
them once when the channel is created.

# Examples
```rust
use secc::*;
use std::time::Duration;

let channel = create::<u8>(5, Duration::from_millis(10));
let (sender, receiver) = channel;
assert_eq!(Ok(()), sender.send(17));
assert_eq!(Ok(()), sender.send(19));
assert_eq!(Ok(()), sender.send(23));
assert_eq!(Ok(()), sender.send(29));
assert_eq!(Ok(17), receiver.receive());
assert_eq!(Ok(()), receiver.skip());
assert_eq!(Ok(23), receiver.receive());
assert_eq!(Ok(()), receiver.reset_skip());
assert_eq!(Ok(19), receiver.receive());
```

This code creates the channel and then sends it a series of messages. The first is received
normally but then the user wants to skip the next message. The user can then receive in
the middle of the channel, reset the skip and resume receiving normally.

### What's New

* 2019-??-??: 0.1.0-beta1
  * This build requires Rust 1.39.0 which is in beta at the time of publishing; you can build 
  with `cargo +beta build`. 
  * Added `async_send`, `receive_stream` and `peek_stream` functions which use Rust futures,
  now in beta. Until those features end up in stable `master` branch will require you build
  with the beta toolchain.
* 2019-09-13: 0.0.10
  * Issue #13: A Deadlock would occur if the timeout occurred while waiting for space or data.
  * BREAKING CHANGE Timeouts are in `Duration` objects now rather than milliseconds.
* 2019-08-18: 0.0.9
  * Most `unsafe` code has been eliminated, enhancing stability.

[Release Notes for All Versions](https://github.com/rsimmonsjr/secc/blob/master/RELEASE_NOTES.md)

### Design Principals

SECC was driven by the need for a multi-sender, multi-consumer channel that would have the
ability to skip processing messages. There are many situations in which this is needed by a
consumer such as the use case with Axiom where actors implement a finite state machine. That
led me to go through many iterations of different designs until it became clear that a linked
list was the only legitimate approach. The problem with a linked lists is that they typically
burn a lot of CPU time in allocating new nodes on each enqueue. The solution was to use two
linked lists, allocate all nodes up front and just logically move nodes around. The actual
pointers to the next node or the various heads and tails are the indexes in the statically
allocated slice of nodes.
