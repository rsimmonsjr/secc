[![Latest version](https://img.shields.io/crates/v/secc.svg)](https://crates.io/crates/secc)
[![Build Status](https://api.travis-ci.org/rsimmonsjr/secc.svg?branch=master)](https://travis-ci.org/rsimmonsjr/secc)
[![Average time to resolve an issue](https://isitmaintained.com/badge/resolution/rsimmonsjr/secc.svg)](https://isitmaintained.com/project/rsimmonsjr/secc)
[![License](https://img.shields.io/crates/l/secc.svg)](https://github.com/rsimmonsjr/secc#license)

# SECC
Skip Enabled Concurrent Channel for Rust

An Skip Enabled Concurrent Channel (SECC) is a bounded capacity channel that supports multiple 
senders and multiple recievers and allows the receiver to temporarily skip receiving messages 
if they desire.

The channel is a FIFO structure unless the user intends to skip one or more messages
in which case a message could be read in a different order. The channel does, however,
guarantee that the messages will remain in the same order as sent and, unless skipped, will
be received in order.

The module is implemented using two linked lists where one list acts as a pool of nodes and
the other list acts as the queue holding the messages. This allows us to move nodes in and out
of the list and even skip a message with O(1) efficiency. If there are 1000 messages and
the user desires to skip one in the middle they will incur virtually the exact same
performance cost as a normal read operation. There are only a couple of additional pointer
operations necessary to remove a node out of the middle of the linked list that implements
the queue.  When a message is received from the channel the node holding the message is
removed from the queue and appended to the tail of the pool. Conversely, when a  message is
sent to the channel the node moves from the head of the pool to the tail of the queue. In
this manner nodes are constantly cycled in and out of the queue so we only need to allocate
them once when the channel is created.

### Getting Started

Using SECC is pretty simple as the following example shows. 

```rust
use secc::*;

let channel = create::<u8>(5, 10);
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


### Design Principals

SECC was driven by the need for a multi-sender, multi-consumer channel that would have the ability
to skip processing messages. There are many situation in which this is needed by a consumer
such as the use case with Axiom where actors implement a finite state machine. That led me to 
go through many iterations of different designs until it became clear that a linked list was the
only legitimate approach. The problem with a linked lists is that they typically burn a lot of 
CPU time in allocating new nodes on each enqueue. The solution was to use two linked lists, 
allocate all nodes up front and just logically move nodes around. The actual pointers to the 
next node or the various heads and tails are the indexes in the statically allocated slice of 
nodes. When send and receive operations happen, nodes are merely moved around logically but not
physically.

### What's New
* 2019-08-11: 0.0.6 
  * Improved README and Lib documentation.
* 2019-08-11: 0.0.5 
  * Migration from Axiom. SECC has been split off from Axiom into its own crate.

[Release Notes for All Versions](https://github.com/rsimmonsjr/secc/blob/master/RELEASE_NOTES.md)

## Acknowledgements

This product would not be possible without all the help and support I have received on the Rust
discord server, especially from the user Talchas.

