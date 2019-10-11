//! # Skip Enabled Concurrent Channel for Rust (SECC)
//!
//! [![Latest version](https://img.shields.io/crates/v/secc.svg)](https://crates.io/crates/secc)
//! [![Build Status](https://api.travis-ci.org/rsimmonsjr/secc.svg?branch=master)](https://travis-ci.org/rsimmonsjr/secc)
//! [![Average time to resolve an issue](https://isitmaintained.com/badge/resolution/rsimmonsjr/secc.svg)](https://isitmaintained.com/project/rsimmonsjr/secc)
//! [![License](https://img.shields.io/crates/l/secc.svg)](https://github.com/rsimmonsjr/secc#license)
//!
//! # Description
//!
//! An Skip Enabled Concurrent Channel (SECC) is a bounded capacity channel that supports multiple
//! senders and multiple recievers and allows the receiver to temporarily skip receiving messages
//! if they desire.
//!
//! Messages in the channel need to be clonable to implement the [`peek`] functionality (which
//! returns a clone of the message). For this reason it is advisable that the user chose a type
//! that is efficiently clonable, such as an [`Arc`] to enclose a message that cannot be
//! efficiently cloned.
//!
//! The channel is a FIFO structure unless the user intends to skip one or more messages
//! in which case a message could be read in a different order. The channel does, however,
//! guarantee that the messages will remain in the same order as sent and, unless skipped, will
//! be received in order.
//!
//! SECC is implemented using two linked lists where one list acts as a pool of nodes and the
//! other list acts as the queue holding the messages. This allows us to move nodes in and out
//! of the list and even skip a message with O(1) efficiency. If there are 1000 messages and
//! the user desires to skip one in the middle they will incur virtually the exact same
//! performance cost as a normal read operation. There are only a couple of additional pointer
//! operations necessary to remove a node out of the middle of the linked list that implements
//! the queue.  When a message is received from the channel the node holding the message is
//! removed from the queue and appended to the tail of the pool. Conversely, when a  message is
//! sent to the channel the node moves from the head of the pool to the tail of the queue. In
//! this manner nodes are constantly cycled in and out of the queue so we only need to allocate
//! them once when the channel is created.
//!
//! # Examples
//! ```rust
//! use secc::*;
//! use std::time::Duration;
//!
//! let channel = create::<u8>(5, Duration::from_millis(10));
//! let (sender, receiver) = channel;
//! assert_eq!(Ok(()), sender.send(17));
//! assert_eq!(Ok(()), sender.send(19));
//! assert_eq!(Ok(()), sender.send(23));
//! assert_eq!(Ok(()), sender.send(29));
//! assert_eq!(Ok(17), receiver.receive());
//! assert_eq!(Ok(()), receiver.skip());
//! assert_eq!(Ok(23), receiver.receive());
//! assert_eq!(Ok(()), receiver.reset_skip());
//! assert_eq!(Ok(19), receiver.receive());
//! ```
//!
//! This code creates the channel and then sends it a series of messages. The first is received
//! normally but then the user wants to skip the next message. The user can then receive in
//! the middle of the channel, reset the skip and resume receiving normally.
//!
//! ### What's New
//!
//! * 2019-??-??: 0.1.0-BETA
//!   * Added `async_send`, `receive_stream` and `peek_stream` functions which use Rust futures,
//!   now in beta. Until those features end up in stable `master` branch will require you build
//!   with the beta toolset. i.e. cargo +beta build.
//! * 2019-09-13: 0.0.10
//!   * Issue #13: A Deadlock would occur if the timeout occurred while waiting for space or data.
//!   * BREAKING CHANGE Timeouts are in `Duration` objects now rather than milliseconds.
//! * 2019-08-18: 0.0.9
//!   * Most `unsafe` code has been eliminated, enhancing stability.
//!
//! [Release Notes for All Versions](https://github.com/rsimmonsjr/secc/blob/master/RELEASE_NOTES.md)
//!
//! ### Design Principals
//!
//! SECC was driven by the need for a multi-sender, multi-consumer channel that would have the
//! ability to skip processing messages. There are many situations in which this is needed by a
//! consumer such as the use case with Axiom where actors implement a finite state machine. That
//! led me to go through many iterations of different designs until it became clear that a linked
//! list was the only legitimate approach. The problem with a linked lists is that they typically
//! burn a lot of CPU time in allocating new nodes on each enqueue. The solution was to use two
//! linked lists, allocate all nodes up front and just logically move nodes around. The actual
//! pointers to the next node or the various heads and tails are the indexes in the statically
//! allocated slice of nodes.

use futures::stream::Stream;
use futures::task::Waker;
use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

/// A message that is used to indicate that a position index points to no other node. Note that
/// this value is something beyond the capability of any user to allocate for the channel size.
const NIL: usize = 1 << 16 as usize;

/// Errors potentially returned from channel operations.
#[derive(Eq, PartialEq)]
pub enum SeccErrors<T: Sync + Send + Clone> {
    /// Channel is full, no more messages can be sent, the enclosed value is the last message
    /// attempted to be sent.
    Full(T),

    /// There are no more possible receivers to the channel so sending data would be pointless.
    /// The element of the tuple in this error is the message that was attempted to be sent.
    NoReceivers(T),

    /// Channel is empty so no more messages can be received. This can also be returned if there
    /// is an active cursor and there are no messages to receive after the cursor even though
    /// there are skipped messages.
    Empty,

    /// Channel is empty and there is no chance of any other process sending to the channel
    /// since no one is holding the sender anymore so receive is impossible.
    EmptyNoSenders,
}

impl<T: Sync + Send + Clone> fmt::Debug for SeccErrors<T> {
    fn fmt(&self, formatter: &'_ mut fmt::Formatter) -> fmt::Result {
        match self {
            SeccErrors::Full(_) => write!(formatter, "SeccErrors::Full"),
            SeccErrors::NoReceivers(_) => write!(formatter, "SeccErrors::NoReceivers"),
            SeccErrors::Empty => write!(formatter, "SeccErrors::Empty"),
            SeccErrors::EmptyNoSenders => write!(formatter, "SeccErrors::EmptyNoSenders"),
        }
    }
}

impl<T: Sync + Send + Clone> fmt::Display for SeccErrors<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl<T: Sync + Send + Clone> std::error::Error for SeccErrors<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// A single node in the channel's buffer.
struct SeccNode<T: Sync + Send + Clone> {
    /// Contains a message in a `Some` or contains `None` if the node is empty. Note that this is
    /// an [`UnsafeCell`] in order to get around Rust mutability locks so that this data structure
    /// can be passed around immutably but also still be able to send and receive.
    cell: UnsafeCell<Option<T>>,
    /// The pointer to the next node in the channel.
    next: AtomicUsize,
    // FIXME (Issue #12) Add tracking of time in channel by milliseconds.
}

impl<T: Sync + Send + Clone> SeccNode<T> {
    /// Creates a new node where the next index is set to `NIL`.
    fn new() -> SeccNode<T> {
        SeccNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(NIL),
        }
    }

    /// Creates a new node where the next index is set to point at the provided index in
    /// the slice of allocated nodes.
    fn with_next(next: usize) -> SeccNode<T> {
        SeccNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(next),
        }
    }
}

pub trait SeccCoreOps<T: Sync + Send + Clone> {
    /// Fetch the core of the channel.
    fn core(&self) -> &SeccCore<T>;

    /// Returns the capacity of the channel.
    fn capacity(&self) -> usize {
        self.core().capacity
    }

    /// Count of the number of times receivers of this channel waited for messages.
    fn awaited_messages(&self) -> usize {
        self.core().awaited_messages.load(Ordering::Relaxed)
    }

    /// Count of the number of times senders to the channel waited for capacity.
    fn awaited_capacity(&self) -> usize {
        self.core().awaited_capacity.load(Ordering::Relaxed)
    }

    /// Returns the number of items are in the channel currently without regard to cursors.
    fn pending(&self) -> usize {
        self.core().pending.load(Ordering::Relaxed)
    }

    /// Number of messages in the channel that are available to be received. This will normally be
    /// the same as `pending` unless there is a skip cursor active; in which case it may be
    /// smaller than pending or even 0.
    fn receivable(&self) -> usize {
        self.core().receivable.load(Ordering::Relaxed)
    }

    /// Returns the total number of messages that have been sent to the channel.
    fn sent(&self) -> usize {
        self.core().sent.load(Ordering::Relaxed)
    }

    /// Returns the total number of messages that have been received from the channel.
    fn received(&self) -> usize {
        self.core().received.load(Ordering::Relaxed)
    }

    /// Creates a debug string for diagnosing problems with the channel.
    fn debug_structure(&self) -> String {
        let (ref mutex, _, _) = &*self.core().lock;
        let pointers = mutex.lock().unwrap();

        let mut pool = Vec::with_capacity(self.core().capacity);
        pool.push(pointers.pool_head);
        let mut next_ptr = self.core().nodes[pointers.pool_head]
            .next
            .load(Ordering::SeqCst);
        while next_ptr != NIL {
            pool.push(next_ptr);
            next_ptr = self.core().nodes[next_ptr].next.load(Ordering::SeqCst);
        }

        let mut queue = Vec::with_capacity(self.core().capacity);
        let mut next_ptr = self.core().nodes[pointers.queue_head]
            .next
            .load(Ordering::SeqCst);
        queue.push(pointers.queue_head);
        while next_ptr != NIL {
            queue.push(next_ptr);
            next_ptr = self.core().nodes[next_ptr].next.load(Ordering::SeqCst);
        }

        format!(
            "queue_head: {}, queue_tail: {}, pool_head: {}, pool_tail: {}, skipped: {}, 
                cursor: {}, queue_size: {}, queue: {:?}, pool_size: {}, pool: {:?}",
            pointers.queue_head,
            pointers.queue_tail,
            pointers.pool_head,
            pointers.pool_tail,
            pointers.skipped,
            pointers.cursor,
            queue.len(),
            queue,
            pool.len(),
            pool
        )
    }
}

/// A struct holding the pointers and other mutable data in the channel that will live inside the
/// channel's mutex to provide interior mutability. Operations on this data are designed to be as
/// minimal as possible to reduce contention on the mutex.
struct SeccPointers {
    /// The tail of the queue which holds messages currently in the channel.
    queue_tail: usize,
    /// The head of the pool of available nodes to be used when sending messages to the channel.  
    /// Note that if there is only one node in the pool then the channel is full as neither the
    /// pool nor queue may be empty.
    pool_head: usize,
    /// The head of the queue which holds messages currently in the channel. Note that if there
    /// is only one node in the queue then the channel is empty as neither the pool nor queue
    /// may be empty.
    queue_head: usize,
    /// The tail of the pool of available nodes to be used when sending messages to the channel.
    pool_tail: usize,
    /// Either `NIL`, when there is no current skip cursor, or a pointer to the last
    /// element skipped.
    skipped: usize,
    /// Either `NIL`, when there is no current skip cursor, or a pointer to the next
    /// element that can be received from the channel.
    cursor: usize,
    /// The wakers that are registered waiting on capacity if any.
    send_wakers: Option<Vec<Waker>>,
    /// The wakers that are registered waiting on data if any.
    receive_wakers: Option<Vec<Waker>>,
}

/// Data structure that contains the core of the channel including tracking of statistics and
/// node storage.
pub struct SeccCore<T: Sync + Send + Clone> {
    /// Capacity of the channel, which is the total number of items that can be stored. Note that
    /// there will be 2 additional nodes allocated because neither the queue nor pool may ever
    /// be empty.
    capacity: usize,
    /// The timeout used for polling the channel when waiting forever to send or recieve.
    poll_timeout: Duration,
    /// Storage of the nodes.
    nodes: Box<[SeccNode<T>]>,
    /// This is the lock that is used for channel operations as well as condvars for both
    /// has_capacity and has_messages in that order.
    lock: Arc<(Mutex<SeccPointers>, Condvar, Condvar)>,
    /// Count of the number of times receivers of this channel waited for messages.
    awaited_messages: AtomicUsize,
    /// Count of the number of times senders to the channel waited for capacity.
    awaited_capacity: AtomicUsize,
    /// Number of messages currently in the channel.
    pending: AtomicUsize,
    /// Number of messages in the channel that are available to be received. This will normally be
    /// the same as `pending` unless there is a skip cursor active; in which case it may be
    /// smaller than pending or even 0.
    receivable: AtomicUsize,
    /// Total number of messages that have been sent to the channel.
    sent: AtomicUsize,
    /// Total number of messages that have been received from the channel.
    received: AtomicUsize,
}

/// The future type used when asynchrnously sending to the channel.
pub struct SeccSendFuture<T: Sync + Send + Clone> {
    sender: SeccSender<T>,
    value: Option<T>,
}

impl<T: Sync + Send + Clone> SeccSendFuture<T> {
    /// Creates a new future with the given value on the given sender. When this future is complete
    /// then the value will have been sent to the channel or an unrecoverable error (not
    /// `SeccErrors::Full` will be returned. It is important to note that the value will be moved
    /// into the future and not returned to the creator.
    fn new(value: T, sender: SeccSender<T>) -> SeccSendFuture<T> {
        SeccSendFuture {
            sender: sender,
            value: Some(value),
        }
    }
}

impl<T: Sync + Send + Clone> Unpin for SeccSendFuture<T> {}

impl<T: Sync + Send + Clone> Future for SeccSendFuture<T> {
    type Output = Result<(), SeccErrors<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let value = self.value.take().unwrap();
        match self.sender.send_internal(value, Some(cx.waker().clone())) {
            result @ Ok(_) => Poll::Ready(result),
            Err(SeccErrors::Full(returned_value)) => {
                self.value = Some(returned_value);
                Poll::Pending
            }
            error @ Err(_) => Poll::Ready(error),
        }
    }
}

/// Sender side of the channel.
pub struct SeccSender<T: Sync + Send + Clone> {
    /// The core of the channel.
    core: Arc<SeccCore<T>>,
}

// Manual implementation necessary because of the following issue.
// https://github.com/rust-lang/rust/issues/26925
impl<T: Sync + Send + Clone> Clone for SeccSender<T> {
    fn clone(&self) -> Self {
        SeccSender {
            core: self.core.clone(),
        }
    }
}

impl<T: Sync + Send + Clone> SeccSender<T> {
    /// Sends a message, which will be moved into the channel. This function will either return
    /// an empty [`std::Result::Ok`] or an [`std::Result::Err`] containing the last message
    /// sent if something went wrong.
    pub fn send(&self, message: T) -> Result<(), SeccErrors<T>> {
        self.send_internal(message, None)
    }

    /// Internal implementation of send which will send a message to the channel if there is
    /// space available in the channel. This implementation takes an optional waker which will
    /// be registered if the channel is full.
    fn send_internal(&self, message: T, waker: Option<Waker>) -> Result<(), SeccErrors<T>> {
        // Check to see if anyone can receive from the channel. If this sender is the only thing
        // holding the core then the receiver is held by no one.
        // FIXME Needs testing
        if Arc::strong_count(&self.core) < 2 {
            return Err(SeccErrors::NoReceivers(message));
        }

        // Lock the channel to operate on it.
        let (ref mutex, _, has_data) = &*self.core.lock;
        let mut pointers = mutex.lock().unwrap();

        // Get a pointer to the current pool_head and see if we have space to send.
        let pool_head_ptr = &self.core.nodes[pointers.pool_head];
        let next_pool_head = pool_head_ptr.next.load(Ordering::SeqCst);
        if NIL == next_pool_head {
            if let Some(w) = waker {
                if let Some(ref mut wakers) = pointers.send_wakers {
                    wakers.push(w);
                } else {
                    pointers.send_wakers = Some(vec![w]);
                }
            }
            Err(SeccErrors::Full(message))
        } else {
            // We get the queue tail because the node from the pool will move here.
            let queue_tail_ptr = &self.core.nodes[pointers.queue_tail];

            // Add the message to the node, transferring ownership.
            unsafe {
                *queue_tail_ptr.cell.get() = Some(message);
            }

            // Update the pointers in the mutex.
            let old_pool_head = pointers.pool_head;
            pointers.queue_tail = pointers.pool_head;
            pointers.pool_head = next_pool_head;

            // The now filled node will get moved to the queue.
            pool_head_ptr.next.store(NIL, Ordering::SeqCst);
            queue_tail_ptr.next.store(old_pool_head, Ordering::SeqCst);

            // Take the receive wakers if any for iteration outside of the guard.
            let wakers_opt = pointers.receive_wakers.take();

            // Notify anyone that was waiting on the Condvar and we are done.
            has_data.notify_all();

            // Adjust receivable count.
            self.core.receivable.fetch_add(1, Ordering::SeqCst);

            // End of critical block so drop the lock.
            drop(pointers);

            // Adjust the channel metrics.
            self.core.sent.fetch_add(1, Ordering::SeqCst);
            self.core.pending.fetch_add(1, Ordering::SeqCst);

            // Call any potential wakers.
            if let Some(mut wakers) = wakers_opt {
                for waker in wakers.drain(..) {
                    waker.wake();
                }
            }

            Ok(())
        }
    }

    /// Returns a future for sending to the channel. The future will attempt to send to the channel
    /// on each poll and will be awoken when space opens up in the channel.
    pub fn async_send(&self, value: T) -> SeccSendFuture<T> {
        SeccSendFuture::new(value, self.clone())
    }

    /// Send to the channel, awaiting capacity if necessary up to a given timeout. This
    /// function is semantically identical to [`SeccSender::send`] but simply waits
    /// for there to be space in the channel before sending.
    pub fn send_await_timeout(
        &self,
        mut message: T,
        timeout: Duration,
    ) -> Result<(), SeccErrors<T>> {
        loop {
            match self.send(message) {
                Err(SeccErrors::Full(v)) => {
                    message = v;
                    // We will put a Condvar on the mutex to be notified if space opens up.
                    let (ref mutex, has_capacity, _) = &*self.core.lock;
                    let guard = mutex.lock().unwrap();

                    // Important that we drop the Condvar's guard to not deadlock channel.
                    let (_, result) = has_capacity.wait_timeout(guard, timeout).unwrap();
                    self.core.awaited_capacity.fetch_add(1, Ordering::SeqCst);
                    if result.timed_out() {
                        // Try one more time to send in case we missed a Condvar notification.
                        return self.send(message);
                    }
                }
                v => return v,
            }
        }
    }

    // Waits basically forever to send to the channel rechecking for capacity periodically. The
    // interval between checks is determined by the polling `Duration` passed to channel creation.
    pub fn send_await(&self, mut message: T) -> Result<(), SeccErrors<T>> {
        loop {
            match self.send_await_timeout(message, self.core.poll_timeout) {
                Err(SeccErrors::Full(v)) => {
                    message = v;
                }
                other => return other,
            }
        }
    }
}

impl<T: Sync + Send + Clone> SeccCoreOps<T> for SeccSender<T> {
    fn core(&self) -> &SeccCore<T> {
        &self.core
    }
}

impl<T: Sync + Send + Clone> fmt::Debug for SeccSender<T> {
    fn fmt(&self, formatter: &'_ mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "{}", self.debug_structure())
    }
}

unsafe impl<T: Send + Sync + Clone> Send for SeccSender<T> {}

unsafe impl<T: Send + Sync + Clone> Sync for SeccSender<T> {}

/// A stream that peeks at messages from the channel as they become available. It should be noted
/// that successive polling of this stream will produce the same value, since it is being peeked
/// and not removed, unless something pops the message off of the channel between peeks. A typical
/// use of this would be to peek at the message, decide what to do and then pop or skip the
/// message.
pub struct SeccPeekStream<T: Send + Sync + Clone> {
    /// This is the receiver that this stream will use for items coming off of the channel.
    receiver: SeccReceiver<T>,
}

impl<T: Send + Sync + Clone> Stream for SeccPeekStream<T> {
    type Item = Result<T, SeccErrors<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.peek_internal(Some(cx.waker().clone())) {
            result @ Ok(_) => Poll::Ready(Some(result)),
            Err(SeccErrors::Empty) => Poll::Pending,
            error @ Err(SeccErrors::EmptyNoSenders) => Poll::Ready(Some(error)),
            error @ Err(_) => Poll::Ready(Some(error)),
        }
    }
}

/// A stream that receives messages from the channel as they become available.
pub struct SeccReceiverStream<T: Send + Sync + Clone> {
    /// This is the receiver that this stream will use for items coming off of the channel.
    receiver: SeccReceiver<T>,
}

impl<T: Send + Sync + Clone> Stream for SeccReceiverStream<T> {
    type Item = Result<T, SeccErrors<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.receive_internal(Some(cx.waker().clone())) {
            result @ Ok(_) => Poll::Ready(Some(result)),
            Err(SeccErrors::Empty) => Poll::Pending,
            error @ Err(SeccErrors::EmptyNoSenders) => Poll::Ready(Some(error)),
            error @ Err(_) => Poll::Ready(Some(error)),
        }
    }
}

/// Receiver side of the channel.
pub struct SeccReceiver<T: Sync + Send + Clone> {
    /// The core of the channel.
    core: Arc<SeccCore<T>>,
}

// Manual implementation necessary because of the following issue.
// https://github.com/rust-lang/rust/issues/26925
impl<T: Sync + Send + Clone> Clone for SeccReceiver<T> {
    fn clone(&self) -> Self {
        SeccReceiver {
            core: self.core.clone(),
        }
    }
}

impl<T: Sync + Send + Clone> SeccReceiver<T> {
    /// Receives the next message that is receivable. This will either receive the message at
    /// the head of the channel or, in the case that there is a skip cursor active, the next
    /// receivable message will be in the node pointed to by the skip cursor. This means that it
    /// is possible that receive could return an [`SeccErrors::Empty`] when there are actually
    /// messages in the channel because there will be none readable until the skip is reset.
    pub fn receive(&self) -> Result<T, SeccErrors<T>> {
        self.receive_internal(None)
    }

    /// Internal function for receiving which will take an optional waker to be registered if there
    /// is no data currently to receive.
    pub fn receive_internal(&self, waker: Option<Waker>) -> Result<T, SeccErrors<T>> {
        let (ref mutex, has_capacity, _) = &*self.core.lock;
        let mut pointers = mutex.lock().unwrap();

        // Get a pointer to the queue_head or cursor and checks for anything receivable.
        let read_ptr = if pointers.cursor == NIL {
            &self.core.nodes[pointers.queue_head]
        } else {
            &self.core.nodes[pointers.cursor]
        };
        let next_read_pos = (*read_ptr).next.load(Ordering::SeqCst);
        if NIL == next_read_pos {
            // Check to see if anyone can receive from the channel. If this receiver is the only
            // thing holding the core then the sender is held by no one and no more messages can
            // enter the channel and an error will be returned.
            // FIXME Needs testing
            if Arc::strong_count(&self.core) < 2 {
                return Err(SeccErrors::EmptyNoSenders);
            } else {
                if let Some(w) = waker {
                    if let Some(ref mut wakers) = pointers.receive_wakers {
                        wakers.push(w);
                    } else {
                        pointers.receive_wakers = Some(vec![w]);
                    }
                }
                return Err(SeccErrors::Empty);
            }
        } else {
            // We can read something so we will pull the item out of the read pointer.
            let message: T = unsafe { (*(*read_ptr).cell.get()).take().unwrap() };

            // Now we have to manage either pulling a node out of the middle if there was a
            // cursor, or from the queue head if there was no cursor. Then we have to place
            // the released node on the pool tail.
            let pool_tail_ptr = &self.core.nodes[pointers.pool_tail];
            (*read_ptr).next.store(NIL, Ordering::SeqCst);

            let new_pool_tail = if pointers.cursor == NIL {
                // If we aren't using a cursor then the queue_head becomes the pool tail
                pointers.pool_tail = pointers.queue_head;
                let old_queue_head = pointers.queue_head;
                pointers.queue_head = next_read_pos;
                old_queue_head
            } else {
                // If the cursor is set we have to dequeue in the middle of the list and fix the
                // node chain and then move the node that the cursor was pointing at to the pool
                // tail. Note that the `skipped` pointer will never be `NIL` when the cursor is
                // not `NIL`. The `skipped` pointer is only ever set to a skipped node that
                // could be read and lags behind `cursor` by one node in the queue.
                let skipped_ptr = &self.core.nodes[pointers.skipped];
                ((*skipped_ptr).next).store(next_read_pos, Ordering::SeqCst);
                (*read_ptr).next.store(NIL, Ordering::SeqCst);
                pointers.pool_tail = pointers.cursor;
                let old_cursor = pointers.cursor;
                pointers.cursor = next_read_pos;
                old_cursor
            };

            (*pool_tail_ptr).next.store(new_pool_tail, Ordering::SeqCst);
            self.core.receivable.fetch_sub(1, Ordering::SeqCst);
            let wakers_opt = pointers.send_wakers.take();

            // End of critical block so drop the lock.
            drop(pointers);

            // Update the channel metrics.
            self.core.received.fetch_add(1, Ordering::SeqCst);
            self.core.pending.fetch_sub(1, Ordering::SeqCst);

            // Call and remove all wakers.
            if let Some(mut wakers) = wakers_opt {
                for waker in wakers.drain(..) {
                    waker.wake();
                }
            }

            // Notify anyone waiting on messages to be available.
            has_capacity.notify_all();

            // Return the message retrieved earlier.
            Ok(message)
        }
    }

    /// Peeks at the next receivable message in the channel and returns a `Clone` of the message.
    /// Note that the message isn't guaranteed to stay in the channel as a thread could pop the
    /// message off while another thread is looking at the message but the message won't change
    /// from the point of view of the peeking thread if it is popped off the channel.
    pub fn peek(&self) -> Result<T, SeccErrors<T>> {
        self.peek_internal(None)
    }

    /// An internal function that implements peeking at the queue head and registers an optional
    /// waker to be awoken if there is no data to be peeked at.
    fn peek_internal(&self, waker: Option<Waker>) -> Result<T, SeccErrors<T>> {
        let (ref mutex, _, _) = &*self.core.lock;
        let mut pointers = mutex.lock().unwrap();

        // Get a pointer to the queue_head or cursor and check for anything receivable.
        let read_ptr = if pointers.cursor == NIL {
            &self.core.nodes[pointers.queue_head]
        } else {
            &self.core.nodes[pointers.cursor]
        };
        let next_read_pos = (*read_ptr).next.load(Ordering::SeqCst);
        if NIL == next_read_pos {
            if Arc::strong_count(&self.core) < 2 {
                return Err(SeccErrors::EmptyNoSenders);
            } else {
                if let Some(w) = waker {
                    if let Some(ref mut wakers) = pointers.receive_wakers {
                        wakers.push(w);
                    } else {
                        pointers.receive_wakers = Some(vec![w]);
                    }
                }
                return Err(SeccErrors::Empty);
            }
        }

        // Extract the message and return a reference to it. If this panics then there
        // was somehow a receivable node with no message in it which should never happen.
        let message: T = unsafe {
            (*((*read_ptr).cell).get())
                .clone()
                .expect("secc::peek(): empty receivable node")
        };
        Ok(message)
    }

    /// Removes the next receivable message in the channel and abandons it or returns an error
    /// if the channel was empty.
    pub fn pop(&self) -> Result<(), SeccErrors<T>> {
        self.receive()?;
        Ok(())
    }

    /// Creates a futures stream for receiving elements from the channel. The result of each
    /// iteration is either an item from the channel or an error.
    pub fn receive_stream(&self) -> SeccReceiverStream<T> {
        SeccReceiverStream {
            receiver: self.clone(),
        }
    }

    /// Creates a futures stream for peeking at elements from the channel. The result of each
    /// iteration is either an item from the channel or an error. Note that this iterator does not
    /// change the channel so calling `next` will produce the same value unless something else has
    /// altered the channel by popping or receiving a message.
    pub fn peek_stream(&self) -> SeccPeekStream<T> {
        SeccPeekStream {
            receiver: self.clone(),
        }
    }

    /// A helper to call [`SeccReceiver::receive`] and await receivable messages until a message
    /// is available or the specified timeout has expired.
    pub fn receive_await_timeout(&self, timeout: Duration) -> Result<T, SeccErrors<T>> {
        loop {
            match self.receive() {
                Err(SeccErrors::Empty) => {
                    let (ref mutex, _, has_data) = &*self.core.lock;
                    let guard = mutex.lock().unwrap();
                    let (_, result) = has_data.wait_timeout(guard, timeout).unwrap();
                    self.core.awaited_capacity.fetch_add(1, Ordering::SeqCst);
                    if result.timed_out() {
                        // Try one more time in case we missed a Condvar notification.
                        return self.receive();
                    }
                }
                v => return v,
            }
        }
    }

    // Waits basically forever to receive from the channel rechecking for data periodically. The
    // interval between checks is determined by the polling `Duration` passed to channel creation.
    pub fn receive_await(&self) -> Result<T, SeccErrors<T>> {
        loop {
            match self.receive_await_timeout(self.core.poll_timeout) {
                Err(SeccErrors::Empty) => (),
                other => return other,
            }
        }
    }

    /// Skips the next message to be received from the channel. If the skip succeeds than the
    /// number of receivable messages will drop by one. Calling this function will either set up
    /// a skip `cursor` in the channel or move an existing skip `cursor`. To receive skipped
    /// messages the user will need to clear the skip cursor by calling the function `reset_skip`
    /// prior to calling `receive`.
    pub fn skip(&self) -> Result<(), SeccErrors<T>> {
        let (ref mutex, _, _) = &*self.core.lock;
        let mut pointers = mutex.lock().unwrap();

        let read_ptr = if pointers.cursor == NIL {
            &self.core.nodes[pointers.queue_head]
        } else {
            &self.core.nodes[pointers.cursor]
        };
        let next_read_pos = read_ptr.next.load(Ordering::SeqCst);

        // If there is a single node in the queue then there are no messages in the channel
        // and therefore nothing to skip so we just return an empty error.
        if NIL == next_read_pos {
            return Err(SeccErrors::Empty);
        }
        if pointers.cursor == NIL {
            // There is no current cursor so we need to establish one.
            pointers.skipped = pointers.queue_head;
            pointers.cursor = next_read_pos;
        } else {
            // There is a cursor already so make sure we increment `cursor` and `skipped`.
            pointers.skipped = pointers.cursor;
            pointers.cursor = next_read_pos;
        }
        self.core.receivable.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    /// Cancels skipping messages in the channel and resets the `skipped` and `cursor` pointers
    /// to `NIL` allowing previously skipped messages to be received. Note that calling this
    /// method on a channel with no skip cursor will do nothing.
    ///
    /// FIXME Create test to make sure that when the skip is reset receive wakers will be called. 
    pub fn reset_skip(&self) -> Result<(), SeccErrors<T>> {
        let (ref mutex, _, has_data) = &*self.core.lock;
        let mut pointers = mutex.lock().unwrap();

        if pointers.cursor != NIL {
            // We start from queue head and count to the cursor to get the new number of currently
            // receivable messages in the channel.
            let mut count: usize = 1; // Minimum number of skipped nodes.
            let mut next_ptr = self.core.nodes[pointers.queue_head]
                .next
                .load(Ordering::SeqCst);
            while next_ptr != pointers.cursor {
                count += 1;
                next_ptr = self.core.nodes[next_ptr].next.load(Ordering::SeqCst);
            }
            self.core.receivable.fetch_add(count, Ordering::SeqCst);
            pointers.cursor = NIL;
            pointers.skipped = NIL;
        }
        // Notify anyone waiting for receivable messages to be available.
        let wakers_opt = pointers.receive_wakers.take();

        // End of critical block so drop the lock.
        drop(pointers);

        // Call and remove all wakers.
        if let Some(mut wakers) = wakers_opt {
            for waker in wakers.drain(..) {
                waker.wake();
            }
        }

        // Notify condvar waiters.
        has_data.notify_all();

        Ok(())
    }

    /// Receive the message at the current cursor and then resets the skip cursor. If there
    /// is currently no skip cursor this is the same as calling [`receive`].
    pub fn receive_and_reset_skip(&self) -> Result<T, SeccErrors<T>> {
        let result = self.receive()?;
        self.reset_skip()?;
        Ok(result)
    }

    /// Pops the message at the current cursor and then resets the skip cursor. If there is
    /// currently no skip cursor this is the same as calling [`pop`].
    pub fn pop_and_reset_skip(&self) -> Result<(), SeccErrors<T>> {
        self.pop()?;
        self.reset_skip()
    }
}

impl<T: Sync + Send + Clone> SeccCoreOps<T> for SeccReceiver<T> {
    fn core(&self) -> &SeccCore<T> {
        &self.core
    }
}

/// This function will write a debug string for the `SeccReceiver`.
impl<T: Sync + Send + Clone> fmt::Debug for SeccReceiver<T> {
    fn fmt(&self, formatter: &'_ mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "{}", self.debug_structure())
    }
}

unsafe impl<T: Send + Sync + Clone> Send for SeccReceiver<T> {}

unsafe impl<T: Send + Sync + Clone> Sync for SeccReceiver<T> {}

/// Creates the sender and receiver sides of this channel and returns them as a tuple. The user
/// can pass both a channel `capacity` and a `poll` `Duration` which govern how often operations
/// that wait on the channel will poll.
pub fn create<T: Sync + Send + Clone>(
    capacity: u16,
    poll_timeout: Duration,
) -> (SeccSender<T>, SeccReceiver<T>) {
    if capacity < 1 {
        panic!("capacity cannot be smaller than 1");
    }

    // We add two to the allocated capacity to account for the mandatory two placeholder nodes
    // which guarantees that both queue and pool are never empty.
    let alloc_capacity = (capacity + 2) as usize;
    let mut nodes = Vec::<SeccNode<T>>::with_capacity(alloc_capacity);

    // The queue just gets one initial node with and the queue_tail is the same as the queue_head.
    nodes.push(SeccNode::<T>::new());
    let queue_head = nodes.len() - 1;
    let queue_tail = queue_head;

    // Allocate the tail in the pool of nodes that will be added to in order to form the pool.
    // Note that although this is expensive, it only has to be done once.
    nodes.push(SeccNode::<T>::new());
    let mut pool_head = nodes.len() - 1;
    let pool_tail = pool_head;

    // Allocate the rest of the pool setting the next pointers of each node to the previous node.
    for _ in 0..capacity {
        nodes.push(SeccNode::<T>::with_next(pool_head));
        pool_head = nodes.len() - 1;
    }

    // Package all of the pointers.
    let pointers = SeccPointers {
        queue_tail,
        pool_head,
        queue_head,
        pool_tail,
        skipped: NIL,
        cursor: NIL,
        send_wakers: None,
        receive_wakers: None,
    };

    // Create the channel structures.
    let core = Arc::new(SeccCore {
        capacity: capacity as usize,
        poll_timeout,
        nodes: nodes.into_boxed_slice(),
        lock: Arc::new((Mutex::new(pointers), Condvar::new(), Condvar::new())),
        awaited_messages: AtomicUsize::new(0),
        awaited_capacity: AtomicUsize::new(0),
        pending: AtomicUsize::new(0),
        receivable: AtomicUsize::new(0),
        sent: AtomicUsize::new(0),
        received: AtomicUsize::new(0),
    });

    // Return the resulting sender and receiver as a tuple.
    let sender = SeccSender { core: core.clone() };
    let receiver = SeccReceiver { core };

    (sender, receiver)
}

// --------------------- Test Cases ---------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::executor::ThreadPool;
    use futures::stream::StreamExt;
    use futures::task::SpawnExt;
    use std::thread;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// A macro to assert that pointers point to the right nodes.
    macro_rules! assert_pointer_nodes {
        (
            $sender:expr,
            $receiver:expr,
            $queue_head:expr,
            $queue_tail:expr,
            $pool_head:expr,
            $pool_tail:expr,
            $skipped:expr,
            $cursor:expr
        ) => {{
            let actual = debug_channel($sender.clone(), $receiver.clone());
            let (ref mutex, _, _) = &*$sender.core.lock;
            let pointers = mutex.lock().unwrap();

            assert_eq!(
                $queue_head, pointers.queue_head,
                " <== queue_head mismatch!\n Actual: {}\n",
                actual
            );
            assert_eq!(
                $queue_tail, pointers.queue_tail,
                "<== queue_tail mismatch\n Actual: {}\n",
                actual
            );
            assert_eq!(
                $pool_head, pointers.pool_head,
                "<== pool_head mismatch\n Actual: {}\n",
                actual
            );
            assert_eq!(
                $pool_tail, pointers.pool_tail,
                " <== pool_tail mismatch\n Actual: {}\n",
                actual
            );
            assert_eq!(
                $skipped, pointers.skipped,
                " <== skipped mismatch\n Actual: {}\n",
                actual
            );
            assert_eq!(
                $cursor, pointers.cursor,
                " <== cursor mismatch\n Actual: {}\n",
                actual
            );
        }};
    }

    /// Asserts that the given node in the channel has the expected next pointer.
    macro_rules! assert_node_next {
        ($pointers:expr, $node:expr, $next:expr) => {
            assert_eq!($pointers[$node].next.load(Ordering::Relaxed), $next,)
        };
    }

    /// Asserts that the given node in the channel has a next pointing to `NIL`.
    macro_rules! assert_node_next_nil {
        ($pointers:expr, $node:expr) => {
            assert_eq!($pointers[$node].next.load(Ordering::Relaxed), NIL,)
        };
    }

    /// Creates a debug string for debugging channel problems.
    pub fn debug_channel<T: Send + Sync + Clone>(
        sender: SeccSender<T>,
        receiver: SeccReceiver<T>,
    ) -> String {
        format!("{{ Sender: {:?}, Receiver: {:?} }}", sender, receiver)
    }

    /// Items used as messages by tests.
    #[derive(Debug, Eq, PartialEq, Clone)]
    enum Items {
        A,
        B,
        C,
        D,
        E,
        F,
    }

    /// Tests that if a message is popped after another thread peeks at the message that the
    /// message will be removed from the channel but wont change underneath the thread that
    /// peeked at the message.
    #[test]
    fn test_pop_after_peek() {
        use std::num::NonZeroU8;

        let value = NonZeroU8::new(5).unwrap();
        let (tx, rx) = create::<NonZeroU8>(1, Duration::from_millis(100));
        tx.send(value).unwrap();
        let item = rx.peek().unwrap();
        assert_eq!(value, item);
        // Popping shouldnt change the value.
        rx.pop().unwrap();
        assert_eq!(value, item);
    }

    /// Tests that the proper errors are returned if `peek` is called on an empty channel.
    #[test]
    fn test_peek_empty() {
        let (sender, receiver) = create::<Items>(5, Duration::from_millis(10));
        assert_eq!(Err(SeccErrors::Empty), receiver.peek());

        sender.send(Items::A).unwrap();
        receiver.pop().unwrap();
        assert_eq!(Err(SeccErrors::Empty), receiver.peek());
    }

    /// Tests that an error would be generated if another thread popped a message off the
    /// channel and the peeking thread tried to pop the same message in an empty channel.
    #[test]
    fn test_pop_while_peeking() {
        let (sender, receiver) = create::<Items>(5, Duration::from_millis(10));
        let peeked = Arc::new((Mutex::new(false), Condvar::new()));
        let popped = Arc::new((Mutex::new(false), Condvar::new()));

        let peeked_clone = peeked.clone();
        let popped_clone = popped.clone();
        let receiver_clone = receiver.clone();

        sender.send(Items::A).unwrap();

        let handle_peek = thread::spawn(move || {
            let (ref mutex1, ref cvar1) = &*peeked_clone;
            let mut ready = mutex1.lock().unwrap();
            *ready = true;
            let _item = receiver_clone.peek();
            cvar1.notify_all();
            drop(ready);

            // Wait for the pop to occur.
            let (ref mutex2, ref cvar2) = &*popped_clone;
            let mut done = mutex2.lock().unwrap();
            while !*done {
                done = cvar2.wait(done).unwrap();
            }
            // Pop after someone already did should be an error.
            assert_eq!(Err(SeccErrors::Empty), receiver_clone.pop());
        });

        let handle_pop = thread::spawn(move || {
            let (ref mutex1, ref cvar1) = &*peeked;
            let mut ready = mutex1.lock().unwrap();
            while !*ready {
                ready = cvar1.wait(ready).unwrap();
            }

            let (ref mutex2, ref cvar2) = &*popped;
            let mut done = mutex2.lock().unwrap();
            *done = true;
            receiver.pop().unwrap();
            cvar2.notify_all();
        });

        handle_pop.join().unwrap();
        handle_peek.join().unwrap();
    }

    /// Issue #4 Prevents #[derive(Clone)] from being used because of a rust bug that thinks
    /// it needs to clone the T type and required manual cloning. If not fixed this test
    /// wouldn't compile.
    #[test]
    fn test_clone_with_unclonable() {
        struct Unclonable {}

        let (sender, receiver) = create::<Arc<Unclonable>>(5, Duration::from_millis(10));
        let _s_clone = sender.clone();
        let _r_clone = receiver.clone();
    }

    /// This test checks the basic functionality of sending and receiving messages from the
    /// channel in a single thread. This is used to verify basic functionality.
    #[test]
    fn test_send_and_receive() {
        let channel = create::<Items>(5, Duration::from_millis(10));
        let (sender, receiver) = channel;

        // Fetch the pointers for easy checking of the nodes.
        let pointers = &sender.core.nodes;

        assert_eq!(7, pointers.len());
        assert_eq!(5, sender.core.capacity);
        assert_eq!(5, sender.capacity());
        assert_eq!(5, receiver.capacity());

        // Check the initial structure.
        assert_eq!(0, sender.pending());
        assert_eq!(0, sender.receivable());
        assert_eq!(0, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 0, 6, 1, NIL, NIL);

        // Check that sending a message to the channel removes pool head and appends to queue
        // tail and changes nothing else in the node structure.
        assert_eq!(Ok(()), sender.send(Items::A));
        assert_eq!(1, sender.pending());
        assert_eq!(1, sender.receivable());
        assert_eq!(1, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 6, 5, 1, NIL, NIL);

        assert_eq!(Ok(()), sender.send(Items::B));
        assert_eq!(2, sender.pending());
        assert_eq!(2, sender.receivable());
        assert_eq!(2, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 5, 4, 1, NIL, NIL);

        assert_eq!(Ok(()), sender.send(Items::C));
        assert_eq!(3, sender.pending());
        assert_eq!(3, sender.receivable());
        assert_eq!(3, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 4, 3, 1, NIL, NIL);

        assert_eq!(Ok(()), sender.send(Items::D));
        assert_eq!(4, sender.pending());
        assert_eq!(4, sender.receivable());
        assert_eq!(4, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 3, 2, 1, NIL, NIL);

        assert_eq!(Ok(()), sender.send(Items::E));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1, NIL, NIL);

        // Validate that we cannot fill the channel past its capacity and attempts do not
        // mangle the pointers in the channel.
        assert_eq!(Err(SeccErrors::Full(Items::F)), sender.send(Items::F));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());

        assert_eq!(Err(SeccErrors::Full(Items::F)), sender.send(Items::F));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());

        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1, NIL, NIL);

        // Peek at the first message in the channel which should change nothing.
        assert_eq!(Ok(Items::A), receiver.peek());
        assert_eq!(5, receiver.pending());
        assert_eq!(5, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(0, receiver.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1, NIL, NIL);

        // Validate that receiving from the channel performs the proper pointer operations.
        assert_eq!(Ok(Items::A), receiver.receive());
        assert_eq!(4, receiver.pending());
        assert_eq!(4, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_pointer_nodes!(sender, receiver, 6, 2, 1, 0, NIL, NIL);

        assert_eq!(Ok(Items::B), receiver.receive());
        assert_eq!(3, receiver.pending());
        assert_eq!(3, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(2, receiver.received());
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 5, 2, 1, 6, NIL, NIL);

        assert_eq!(Ok(Items::C), receiver.receive());
        assert_eq!(2, receiver.pending());
        assert_eq!(2, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(3, receiver.received());
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_pointer_nodes!(sender, receiver, 4, 2, 1, 5, NIL, NIL);

        assert_eq!(Ok(Items::D), receiver.receive());
        assert_eq!(1, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(4, receiver.received());
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_pointer_nodes!(sender, receiver, 3, 2, 1, 4, NIL, NIL);

        assert_eq!(Ok(Items::E), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL, NIL);

        // Validate that we cannot continue to receive from an empty channel and attempts
        // don't mangle the pointers.
        assert_eq!(Err(SeccErrors::Empty), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL, NIL);

        assert_eq!(Err(SeccErrors::Empty), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL, NIL);

        // Validate that after the channel is empty it can still be sent to and received from.
        assert_eq!(Ok(()), sender.send(Items::F));
        assert_eq!(1, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(6, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 1, 0, 3, NIL, NIL);

        assert_eq!(Ok(Items::F), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(6, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 1, 0, 2, NIL, NIL);

        // Skipping in empty queue should return empty and not mangle pointers.
        assert_eq!(Err(SeccErrors::Empty), receiver.skip());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(6, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 1, 0, 2, NIL, NIL);

        // Send another value to the channel so we can test skipping.
        assert_eq!(Ok(()), sender.send(Items::A));
        assert_eq!(1, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(7, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 0, 6, 2, NIL, NIL);

        // Skipping sets the skip cursor.
        assert_eq!(Ok(()), receiver.skip());
        assert_eq!(1, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(7, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 0, 6, 2, 1, 0);

        // A skip attempt should return empty and not change the pointers.
        assert_eq!(Err(SeccErrors::Empty), receiver.skip());
        assert_eq!(1, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(7, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 0, 6, 2, 1, 0);

        // Sending another item while skipping should work.
        assert_eq!(Ok(()), sender.send(Items::B));
        assert_eq!(2, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(8, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 6, 5, 2, 1, 0);

        // Peek will return a reference to cursor's item but not delete it.
        assert_eq!(Ok(Items::B), receiver.peek());
        assert_eq!(2, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(8, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 6, 5, 2, 1, 0);

        // Sending another item while skipping and after peeking should work but peek shouldn't
        // move to the new node.
        assert_eq!(Ok(()), sender.send(Items::C));
        assert_eq!(Ok(Items::B), receiver.peek());
        assert_eq!(3, receiver.pending());
        assert_eq!(2, receiver.receivable());
        assert_eq!(9, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 2, 1, 0);

        // Skip again and make sure pointers are right.
        assert_eq!(Ok(()), receiver.skip());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 2, 0, 6);

        // Receive at skip cursor and verify nodes move right.
        assert_eq!(Ok(Items::C), receiver.receive());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 6, 0, 5);

        // If we reset the skip then the cursor is cleared but the rest remains the same.
        assert_eq!(Ok(()), receiver.reset_skip());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 6, NIL, NIL);

        assert_eq!(Ok(()), receiver.skip());
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 6, 1, 0);

        assert_eq!(Ok(Items::B), receiver.receive_and_reset_skip());
        assert_node_next!(pointers, 1, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 6);
        assert_node_next!(pointers, 6, 0);
        assert_node_next_nil!(pointers, 0);
        assert_pointer_nodes!(sender, receiver, 1, 5, 4, 0, NIL, NIL);
    }

    /// Tests that the channel can send and receive messages with separate senders and
    /// receivers on different threads.
    #[test]
    fn test_single_producer_single_receiver() {
        let message_count = 200;
        let capacity = 32;
        let (sender, receiver) = create::<u32>(capacity, Duration::from_millis(20));

        let rx = thread::spawn(move || {
            let mut count = 0;
            while count < message_count {
                match receiver.receive_await_timeout(Duration::from_millis(20)) {
                    Ok(_v) => count += 1,
                    _ => (),
                };
            }
        });

        let tx = thread::spawn(move || {
            for i in 0..message_count {
                sender
                    .send_await_timeout(i, Duration::from_millis(20))
                    .unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });

        tx.join().unwrap();
        rx.join().unwrap();
    }

    /// Test that if a user attempts to receive before a message is sent, he will be forced
    /// to wait for the message.
    #[test]
    fn test_receive_before_send() {
        let (sender, receiver) = create::<u32>(5, Duration::from_millis(20));
        let receiver2 = receiver.clone();
        let mutex = Arc::new(Mutex::new(false));
        let rx_mutex = mutex.clone();

        let rx = thread::spawn(move || {
            let mut guard = rx_mutex.lock().unwrap();
            *guard = true;
            drop(guard);
            match receiver2.receive_await_timeout(Duration::from_millis(20)) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });

        // Keep trying to lock until the mutex is true meaning receive is ready.
        loop {
            let guard = mutex.lock().unwrap();
            if *guard == true {
                break;
            }
        }

        let tx = thread::spawn(move || {
            match sender.send_await_timeout(1, Duration::from_millis(20)) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });

        tx.join().unwrap();
        rx.join().unwrap();

        assert_eq!(1, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
    }

    /// Tests that triggering send and receive as close to at the same time as possible does
    /// not cause any race conditions.
    #[test]
    fn test_receive_concurrent_send() {
        let (sender, receiver) = create::<u32>(5, Duration::from_millis(20));
        let receiver2 = receiver.clone();
        let pair = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let rx_pair = pair.clone();
        let tx_pair = pair.clone();

        let rx = thread::spawn(move || {
            let mut guard = rx_pair.0.lock().unwrap();
            guard.0 = true;
            let c_guard = rx_pair.1.wait(guard).unwrap();
            drop(c_guard);
            match receiver2.receive_await_timeout(Duration::from_millis(20)) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });
        let tx = thread::spawn(move || {
            let mut guard = tx_pair.0.lock().unwrap();
            guard.1 = true;
            let c_guard = tx_pair.1.wait(guard).unwrap();
            drop(c_guard);
            match sender.send_await_timeout(1 as u32, Duration::from_millis(20)) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });

        // Wait until both threads are ready and waiting.
        loop {
            let guard = pair.0.lock().unwrap();
            if guard.0 && guard.1 {
                break;
            }
        }

        let guard = pair.0.lock().unwrap();
        pair.1.notify_all();
        drop(guard);

        tx.join().unwrap();
        rx.join().unwrap();

        assert_eq!(1, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
    }

    /// Creates a thread that will send a clone of the `message` passed until the `sender.send()`
    /// reaches the given count. The `pair` is used to trigger the thread to start when the
    /// Condvar notifies the thread.
    fn counted_sender<T: Sync + Send + Clone + 'static>(
        sender: SeccSender<T>,
        pair: Arc<(Mutex<bool>, Condvar)>,
        message: T,
        count: usize,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let (ref mutex, ref condvar) = &*pair;
            let mut started = mutex.lock().unwrap();
            while !*started {
                started = condvar.wait(started).unwrap();
            }
            drop(started);

            while sender.sent() < count {
                let _ = sender.send_await_timeout(message.clone(), Duration::from_millis(10));
            }
        })
    }

    /// Creates a thread that will receive a `message` until the `receiver.received()` reaches
    /// the given count. The `pair` is used to trigger the thread to start when the Condvar
    /// notifies the thread.
    fn counted_receiver<T: Sync + Send + Clone + 'static>(
        receiver: SeccReceiver<T>,
        pair: Arc<(Mutex<bool>, Condvar)>,
        count: usize,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let (ref mutex, ref condvar) = &*pair;
            let mut started = mutex.lock().unwrap();
            while !*started {
                started = condvar.wait(started).unwrap();
            }
            drop(started);

            while receiver.received() < count {
                let _ = receiver.receive_await_timeout(Duration::from_millis(10));
            }
        })
    }

    /// A helper for creating tests with multiple senders and receivers.
    fn multiple_thread_helper<T: Sync + Send + Clone + 'static>(
        channel_size: u16,
        receiver_count: u8,
        sender_count: u8,
        message_count: usize,
        time_limit: Duration,
        message: T,
    ) {
        let (sender, receiver) = create::<T>(channel_size, Duration::from_millis(1));
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let total_thread_count: usize = receiver_count as usize + sender_count as usize;
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(total_thread_count);

        for _ in 0..receiver_count {
            handles.push(counted_receiver(
                receiver.clone(),
                pair.clone(),
                message_count,
            ));
        }

        for _ in 0..sender_count {
            handles.push(counted_sender(
                sender.clone(),
                pair.clone(),
                message.clone(),
                message_count,
            ));
        }

        // We will wait a short time to make sure all threads are ready to go.
        thread::sleep(Duration::from_millis(10));

        // Notify the `Condvar`, triggering all threads to start but then drop the `Mutex` to
        // avoid any potential deadlock in the test.
        let (ref mutex, ref condvar) = &*pair;
        let mut started = mutex.lock().unwrap();
        *started = true;
        condvar.notify_all();
        drop(started);

        // Start a timer that will fail if the test takes too long. This could fail if there
        // is some sort of live lock or deadlock in the code.
        let start = Instant::now();
        while sender.sent() < message_count && receiver.received() < message_count {
            if Instant::elapsed(&start) > time_limit {
                panic!("Test took more than {:?} ms to run!", time_limit);
            }
        }

        // Wait for all of the thread handles to join before concluding the test.
        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Tests channel under multiple receivers and a single sender.
    #[test]
    fn test_multiple_receiver_single_sender() {
        multiple_thread_helper(10, 2, 1, 10_000, Duration::from_millis(1000), 7 as u32);
    }

    /// Tests channel under multiple senders and a single receiver.
    #[test]
    fn test_multiple_sender_single_receiver() {
        multiple_thread_helper(10, 1, 3, 10_000, Duration::from_millis(1000), 7 as u32);
    }

    /// Tests channel under multiple receivers and a multiple senders.
    #[test]
    fn test_multiple_receiver_multiple_sender() {
        let start = Instant::now();
        multiple_thread_helper(100, 3, 3, 100_000, Duration::from_millis(10000), 7 as u32);
        println!("Took {:?} to send 100k messages", Instant::elapsed(&start));
    }

    /// Tests that sending using a future to an empty channel works properly. 
    #[test]
    fn test_async_send_to_empty_channel() {
        let (sender, receiver) = create::<i32>(1, Duration::from_millis(1));

        // Sending to an empty channel should work.
        block_on(sender.async_send(17)).unwrap();
        thread::sleep(Duration::from_millis(2));

        let handle = thread::spawn(move || {
            match receiver.receive_await_timeout(Duration::from_millis(10)) {
                Ok(_) => (),
                Err(e) => panic!("Receive returned: {}", e),
            }
        });

        handle.join().unwrap();
    }

    /// Tests that sending using a future to an full channel works properly. 
    #[test]
    fn test_async_send_to_full_channel() {
        let pool = ThreadPool::new().unwrap();
        let (sender, receiver) = create::<i32>(1, Duration::from_millis(1));
        sender.send(5).unwrap();

        // Sending to a full channel should work after first message is received.
        pool.spawn_ok(async move {
            sender.async_send(17).await.unwrap();
        });

        let handle = thread::spawn(move || {
            match receiver.receive_await_timeout(Duration::from_millis(10)) {
                Ok(_) => (),
                Err(e) => panic!("Receive returned: {}", e),
            }
            // Receive the second message sent by future.
            match receiver.receive_await_timeout(Duration::from_millis(10)) {
                Ok(_) => (),
                Err(e) => panic!("Receive returned: {}", e),
            }
        });

        handle.join().unwrap();
    }

    /// Tests the functionality of a futures-based receive stream.
    #[test]
    fn test_async_receive_stream() {
        let mut pool = ThreadPool::new().unwrap();
        let (sender, receiver) = create::<i32>(5, Duration::from_millis(1));
        sender.send(3).unwrap();
        sender.send(5).unwrap();

        let handle = pool
            .spawn_with_handle(async move {
                let mut stream = receiver.receive_stream();
                assert_eq!(stream.next().await, Some(Ok(3)));
                assert_eq!(stream.next().await, Some(Ok(5)));
                assert_eq!(stream.next().await, Some(Ok(7)));
                assert_eq!(stream.next().await, Some(Ok(11)));
            })
            .unwrap();

        thread::sleep(Duration::from_millis(100));

        sender.send(7).unwrap();
        sender.send(11).unwrap();
        block_on(handle);
    }

    /// Tests the functionality of a futures-based peek stream.
    #[test]
    fn test_async_peek_stream() {
        let mut pool = ThreadPool::new().unwrap();
        let (sender, receiver) = create::<i32>(5, Duration::from_millis(1));
        sender.send(3).unwrap();
        sender.send(5).unwrap();

        let handle = pool
            .spawn_with_handle(async move {
                let mut stream = receiver.peek_stream();
                assert_eq!(stream.next().await, Some(Ok(3)));
                assert_eq!(stream.next().await, Some(Ok(3)));
                receiver.pop().unwrap();
                assert_eq!(stream.next().await, Some(Ok(5)));
                assert_eq!(stream.next().await, Some(Ok(5)));
                receiver.pop().unwrap();
                assert_eq!(stream.next().await, Some(Ok(7)));
                receiver.pop().unwrap();
                assert_eq!(stream.next().await, Some(Ok(11)));
            })
            .unwrap();

        thread::sleep(Duration::from_millis(100));

        sender.send(7).unwrap();
        sender.send(11).unwrap();
        block_on(handle);
    }
}
