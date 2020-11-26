// Copy/paste from async-channel crate.
// The goal is to add weak sender and poll method for sender.

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{fence, AtomicUsize, Ordering};
use core::task::{Context as StdContext, Poll};
use core::usize;

use alloc::boxed::Box;
use alloc::vec::Vec;

use cache_padded::CachePadded;

use event_listener::{Event, EventListener};

use crate::error::ActixAsyncError;
use crate::types::ActixResult;
use crate::util::smart_pointer::{RefCounter, WeakRefCounter};

struct Channel<T> {
    queue: Bounded<T>,
    send_ops: Event,
    recv_ops: Event,
    stream_ops: Event,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Channel<T> {
    fn close(&self) -> bool {
        if self.queue.close() {
            // Notify all send operations.
            self.send_ops.notify(usize::MAX);

            // Notify all receive and stream operations.
            self.recv_ops.notify(usize::MAX);
            self.stream_ops.notify(usize::MAX);

            true
        } else {
            false
        }
    }
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    assert!(cap > 0, "capacity cannot be zero");

    let channel = RefCounter::new(Channel {
        queue: Bounded::new(cap),
        send_ops: Event::new(),
        recv_ops: Event::new(),
        stream_ops: Event::new(),
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });

    let s = Sender {
        channel: channel.clone(),
    };
    let r = Receiver { channel };
    (s, r)
}

pub struct Sender<T> {
    channel: RefCounter<Channel<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.channel.queue.push(msg).map(|()| {
            // Notify a single blocked receive operation. If the notified operation then
            // receives a message or gets canceled, it will notify another blocked receive
            // operation.
            self.channel.recv_ops.notify(1);

            // Notify all blocked streams.
            self.channel.stream_ops.notify(usize::MAX);
        })
    }

    pub fn send(&self, msg: T) -> SendFuture<'_, T> {
        SendFuture {
            sender: self,
            listener: None,
            msg: Some(msg),
        }
    }

    pub fn close(&self) -> bool {
        self.channel.close()
    }

    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender {
            channel: RefCounter::downgrade(&self.channel),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Decrement the sender count and close the channel if it drops down to zero.
        if self.channel.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.channel.close();
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sender {{ .. }}")
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        let count = self.channel.sender_count.fetch_add(1, Ordering::Relaxed);

        // Make sure the count never overflows, even if lots of sender clones are leaked.
        if count > usize::MAX / 2 {
            panic!("Sender count overflow");
        }

        Sender {
            channel: self.channel.clone(),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct SendFuture<'a, T> {
        sender: &'a Sender<T>,
        listener: Option<EventListener>,
        msg: Option<T>,
    }
}

impl<T> Future for SendFuture<'_, T> {
    type Output = ActixResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let mut msg = this.msg.take().unwrap();

        loop {
            match this.sender.try_send(msg) {
                Ok(()) => {
                    // If the capacity is larger than 1, notify another blocked send operation.
                    match this.sender.channel.queue.capacity() {
                        1 => {}
                        _ => this.sender.channel.send_ops.notify(1),
                    }
                    return Poll::Ready(Ok(()));
                }
                Err(TrySendError::Closed(_)) => return Poll::Ready(Err(ActixAsyncError::Closed)),
                Err(TrySendError::Full(m)) => msg = m,
            }

            // Sending failed - now start listening for notifications or wait for one.
            match this.listener.as_mut() {
                None => {
                    // Start listening and then try receiving again.
                    *this.listener = Some(this.sender.channel.send_ops.listen());
                }
                Some(l) => {
                    // Wait for a notification.
                    match Pin::new(l).poll(cx) {
                        Poll::Ready(_) => {
                            this.listener.take();
                            continue;
                        }
                        Poll::Pending => {
                            *this.msg = Some(msg);
                            return Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}

pub struct WeakSender<T> {
    channel: WeakRefCounter<Channel<T>>,
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        Self {
            channel: WeakRefCounter::clone(&self.channel),
        }
    }
}

impl<T> WeakSender<T> {
    pub fn upgrade(&self) -> Option<Sender<T>> {
        WeakRefCounter::upgrade(&self.channel).map(|channel| {
            channel.sender_count.fetch_add(1, Ordering::Relaxed);
            Sender { channel }
        })
    }
}

pub struct Receiver<T> {
    channel: RefCounter<Channel<T>>,
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.channel.queue.pop().map(|msg| {
            // Notify a single blocked send operation. If the notified operation then sends a
            // message or gets canceled, it will notify another blocked send operation.
            self.channel.send_ops.notify(1);

            msg
        })
    }

    pub async fn recv(&self) -> Result<T, RecvError> {
        let mut listener = None;

        loop {
            // Attempt to receive a message.
            match self.try_recv() {
                Ok(msg) => {
                    // If the capacity is larger than 1, notify another blocked receive operation.
                    // There is no need to notify stream operations because all of them get
                    // notified every time a message is sent into the channel.
                    match self.channel.queue.capacity() {
                        1 => {}
                        _ => self.channel.recv_ops.notify(1),
                    }
                    return Ok(msg);
                }
                Err(TryRecvError::Closed) => return Err(RecvError),
                Err(TryRecvError::Empty) => {}
            }

            // Receiving failed - now start listening for notifications or wait for one.
            match listener.take() {
                None => {
                    // Start listening and then try receiving again.
                    listener = Some(self.channel.recv_ops.listen());
                }
                Some(l) => {
                    // Wait for a notification.
                    l.await;
                }
            }
        }
    }

    pub fn close(&self) -> bool {
        self.channel.close()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Decrement the receiver count and close the channel if it drops down to zero.
        if self.channel.receiver_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.channel.close();
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Receiver {{ .. }}")
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        let count = self.channel.receiver_count.fetch_add(1, Ordering::Relaxed);

        // Make sure the count never overflows, even if lots of receiver clones are leaked.
        if count > usize::MAX / 2 {
            panic!("Receiver count overflow");
        }

        Receiver {
            channel: self.channel.clone(),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SendError(..)")
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending into a closed channel")
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TrySendError::Full(..) => write!(f, "Full(..)"),
            TrySendError::Closed(..) => write!(f, "Closed(..)"),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TrySendError::Full(..) => write!(f, "sending into a full channel"),
            TrySendError::Closed(..) => write!(f, "sending into a closed channel"),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "receiving from an empty and closed channel")
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TryRecvError {
    Empty,
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TryRecvError::Empty => write!(f, "receiving from an empty channel"),
            TryRecvError::Closed => write!(f, "receiving from an empty and closed channel"),
        }
    }
}

// Copy/paste from concurrent-queue crate.
// The goal is to use only bounded channel and remove std::thread::yield_now.

struct Slot<T> {
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

pub struct Bounded<T> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    buffer: Box<[Slot<T>]>,
    one_lap: usize,
    mark_bit: usize,
}

unsafe impl<T: Send> Send for Bounded<T> {}
unsafe impl<T: Send> Sync for Bounded<T> {}

impl<T> Bounded<T> {
    pub fn new(cap: usize) -> Bounded<T> {
        assert!(cap > 0, "capacity must be positive");

        // Head is initialized to `{ lap: 0, mark: 0, index: 0 }`.
        let head = 0;
        // Tail is initialized to `{ lap: 0, mark: 0, index: 0 }`.
        let tail = 0;

        // Allocate a buffer of `cap` slots initialized with stamps.
        let mut buffer = Vec::with_capacity(cap);
        for i in 0..cap {
            // Set the stamp to `{ lap: 0, mark: 0, index: i }`.
            buffer.push(Slot {
                stamp: AtomicUsize::new(i),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }

        // Compute constants `mark_bit` and `one_lap`.
        let mark_bit = (cap + 1).next_power_of_two();
        let one_lap = mark_bit * 2;

        Bounded {
            buffer: buffer.into(),
            one_lap,
            mark_bit,
            head: CachePadded::new(AtomicUsize::new(head)),
            tail: CachePadded::new(AtomicUsize::new(tail)),
        }
    }

    pub fn push(&self, value: T) -> Result<(), TrySendError<T>> {
        let mut tail = self.tail.load(Ordering::Relaxed);

        loop {
            // Check if the queue is closed.
            if tail & self.mark_bit != 0 {
                return Err(TrySendError::Closed(value));
            }

            // Deconstruct the tail.
            let index = tail & (self.mark_bit - 1);
            let lap = tail & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = &self.buffer[index];
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the tail and the stamp match, we may attempt to push.
            if tail == stamp {
                let new_tail = if index + 1 < self.buffer.len() {
                    // Same lap, incremented index.
                    // Set to `{ lap: lap, mark: 0, index: index + 1 }`.
                    tail + 1
                } else {
                    // One lap forward, index wraps around to zero.
                    // Set to `{ lap: lap.wrapping_add(1), mark: 0, index: 0 }`.
                    lap.wrapping_add(self.one_lap)
                };

                // Try moving the tail.
                match self.tail.compare_exchange_weak(
                    tail,
                    new_tail,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Write the value into the slot and update the stamp.
                        unsafe {
                            slot.value.get().write(MaybeUninit::new(value));
                        }
                        slot.stamp.store(tail + 1, Ordering::Release);
                        return Ok(());
                    }
                    Err(t) => {
                        tail = t;
                    }
                }
            } else if stamp.wrapping_add(self.one_lap) == tail + 1 {
                full_fence();
                let head = self.head.load(Ordering::Relaxed);

                // If the head lags one lap behind the tail as well...
                if head.wrapping_add(self.one_lap) == tail {
                    // ...then the queue is full.
                    return Err(TrySendError::Full(value));
                }

                tail = self.tail.load(Ordering::Relaxed);
            } else {
                // Yield because we need to wait for the stamp to get updated.
                // std::thread::yield_now();
                tail = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    pub fn pop(&self) -> Result<T, TryRecvError> {
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            // Deconstruct the head.
            let index = head & (self.mark_bit - 1);
            let lap = head & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = &self.buffer[index];
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the the stamp is ahead of the head by 1, we may attempt to pop.
            if head + 1 == stamp {
                let new = if index + 1 < self.buffer.len() {
                    // Same lap, incremented index.
                    // Set to `{ lap: lap, mark: 0, index: index + 1 }`.
                    head + 1
                } else {
                    // One lap forward, index wraps around to zero.
                    // Set to `{ lap: lap.wrapping_add(1), mark: 0, index: 0 }`.
                    lap.wrapping_add(self.one_lap)
                };

                // Try moving the head.
                match self.head.compare_exchange_weak(
                    head,
                    new,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Read the value from the slot and update the stamp.
                        let value = unsafe { slot.value.get().read().assume_init() };
                        slot.stamp
                            .store(head.wrapping_add(self.one_lap), Ordering::Release);
                        return Ok(value);
                    }
                    Err(h) => {
                        head = h;
                    }
                }
            } else if stamp == head {
                full_fence();
                let tail = self.tail.load(Ordering::Relaxed);

                // If the tail equals the head, that means the queue is empty.
                if (tail & !self.mark_bit) == head {
                    // Check if the queue is closed.
                    return if tail & self.mark_bit != 0 {
                        Err(TryRecvError::Closed)
                    } else {
                        Err(TryRecvError::Empty)
                    };
                }

                head = self.head.load(Ordering::Relaxed);
            } else {
                // Yield because we need to wait for the stamp to get updated.
                // std::thread::yield_now();
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }

    pub fn len(&self) -> usize {
        loop {
            // Load the tail, then load the head.
            let tail = self.tail.load(Ordering::SeqCst);
            let head = self.head.load(Ordering::SeqCst);

            // If the tail didn't change, we've got consistent values to work with.
            if self.tail.load(Ordering::SeqCst) == tail {
                let hix = head & (self.mark_bit - 1);
                let tix = tail & (self.mark_bit - 1);

                return if hix < tix {
                    tix - hix
                } else if hix > tix {
                    self.buffer.len() - hix + tix
                } else if (tail & !self.mark_bit) == head {
                    0
                } else {
                    self.buffer.len()
                };
            }
        }
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    pub fn close(&self) -> bool {
        let tail = self.tail.fetch_or(self.mark_bit, Ordering::SeqCst);
        tail & self.mark_bit == 0
    }
}

impl<T> Drop for Bounded<T> {
    fn drop(&mut self) {
        // Get the index of the head.
        let hix = self.head.load(Ordering::Relaxed) & (self.mark_bit - 1);

        // Loop over all slots that hold a value and drop them.
        for i in 0..self.len() {
            // Compute the index of the next slot holding a value.
            let index = if hix + i < self.buffer.len() {
                hix + i
            } else {
                hix + i - self.buffer.len()
            };

            // Drop the value in the slot.
            let slot = &self.buffer[index];
            unsafe {
                let value = slot.value.get().read().assume_init();
                drop(value);
            }
        }
    }
}

#[inline]
fn full_fence() {
    if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        // HACK(stjepang): On x86 architectures there are two different ways of executing
        // a `SeqCst` fence.
        //
        // 1. `atomic::fence(SeqCst)`, which compiles into a `mfence` instruction.
        // 2. `_.compare_and_swap(_, _, SeqCst)`, which compiles into a `lock cmpxchg` instruction.
        //
        // Both instructions have the effect of a full barrier, but empirical benchmarks have shown
        // that the second one is sometimes a bit faster.
        //
        // The ideal solution here would be to use inline assembly, but we're instead creating a
        // temporary atomic variable and compare-and-exchanging its value. No sane compiler to
        // x86 platforms is going to optimize this away.
        let a = AtomicUsize::new(0);
        a.compare_and_swap(0, 1, Ordering::SeqCst);
    } else {
        fence(Ordering::SeqCst);
    }
}
