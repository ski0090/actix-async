/// Copy/paste from async-channel crate.
///
/// The goal is to add weak sender and poll method for sender.
/// The channel has also been modified to have both unbounded and bounded behavior.
mod listener;
mod unbounded;

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{fence, AtomicUsize, Ordering};
use core::task::{Context, Poll};

use crate::error::ActixAsyncError;
use crate::util::{
    futures::Stream,
    smart_pointer::{RefCounter, WeakRefCounter},
};

use self::listener::{Event, EventListener};
use self::unbounded::Unbounded;

struct Channel<T> {
    queue: Unbounded<T>,
    in_queue: AtomicUsize,
    cap: usize,
    send_ops: Event,
    stream_ops: Event,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Channel<T> {
    fn close(&self) -> bool {
        if self.queue.close() {
            // Notify all send operations.
            self.send_ops.notify(usize::MAX);

            // Notify all stream operations.
            self.stream_ops.notify(usize::MAX);

            true
        } else {
            false
        }
    }

    fn avail(&self) -> bool {
        self.in_queue.load(Ordering::Acquire) < self.cap
    }

    /// return the remaining available count before increment.
    fn enqueue(&self) -> usize {
        self.cap - self.in_queue.fetch_add(1, Ordering::Release)
    }

    /// return if there is available count.
    fn dequeue(&self) -> bool {
        self.cap >= self.in_queue.fetch_sub(1, Ordering::Relaxed)
    }
}

pub fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    assert!(cap > 0, "capacity cannot be zero");

    let channel = RefCounter::new(Channel {
        queue: Unbounded::new(),
        cap,
        in_queue: AtomicUsize::new(0),
        send_ops: Event::new(),
        stream_ops: Event::new(),
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });

    let s = Sender {
        channel: channel.clone(),
    };
    let r = Receiver {
        channel,
        listener: None,
    };
    (s, r)
}

pub struct Sender<T> {
    channel: RefCounter<Channel<T>>,
}

impl<T> Sender<T> {
    pub fn do_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.channel.queue.push(msg).map(|()| {
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

    fn clone_sender(channel: &RefCounter<Channel<T>>) -> Self {
        let count = channel.sender_count.fetch_add(1, Ordering::Relaxed);

        // Make sure the count never overflows, even if lots of sender clones are leaked.
        if count > usize::MAX / 2 {
            panic!("Sender count overflow");
        }

        Sender {
            channel: RefCounter::clone(channel),
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
    fn clone(&self) -> Self {
        Self::clone_sender(&self.channel)
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
    type Output = Result<(), ActixAsyncError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let mut msg = this.msg.take().unwrap();

        loop {
            if this.sender.channel.avail() {
                match this.sender.do_send(msg) {
                    Ok(()) => {
                        // If the capacity is larger than 1, notify another blocked send operation.
                        match this.sender.channel.enqueue() {
                            1 => {}
                            _ => this.sender.channel.send_ops.notify(1),
                        }
                        return Poll::Ready(Ok(()));
                    }
                    Err(TrySendError::Closed(_)) => {
                        return Poll::Ready(Err(ActixAsyncError::Closed))
                    }
                    Err(TrySendError::Full(m)) => msg = m,
                }
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
    listener: Option<EventListener>,
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.channel.queue.pop().map(|msg| {
            if self.channel.dequeue() {
                // Notify a single blocked send operation. If the notified operation then sends a
                // message or gets canceled, it will notify another blocked send operation.
                self.channel.send_ops.notify(1);
            }

            msg
        })
    }

    pub fn as_sender(&self) -> Option<Sender<T>> {
        if self.channel.queue.is_closed() {
            None
        } else {
            Some(Sender::clone_sender(&self.channel))
        }
    }

    pub fn close(&self) -> bool {
        self.channel.close()
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // If this stream is listening for events, first wait for a notification.
            if let Some(listener) = self.listener.as_mut() {
                futures_core::ready!(Pin::new(listener).poll(cx));
                self.listener = None;
            }

            loop {
                // Attempt to receive a message.
                match self.try_recv() {
                    Ok(msg) => {
                        // The stream is not blocked on an event - drop the listener.
                        self.listener = None;
                        return Poll::Ready(Some(msg));
                    }
                    Err(TryRecvError::Closed) => {
                        // The stream is not blocked on an event - drop the listener.
                        self.listener = None;
                        return Poll::Ready(None);
                    }
                    Err(TryRecvError::Empty) => {}
                }

                // Receiving failed - now start listening for notifications or wait for one.
                match self.listener.as_mut() {
                    None => {
                        // Create a listener and try sending the message again.
                        self.listener = Some(self.channel.stream_ops.listen());
                    }
                    Some(_) => {
                        // Go back to the outer loop to poll the listener.
                        break;
                    }
                }
            }
        }
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
            listener: None,
        }
    }
}

pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

pub enum TryRecvError {
    Empty,
    Closed,
}

#[inline]
fn full_fence() {
    if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        // HACK(stjepang): On x86 architectures there are two different ways of executing
        // a `SeqCst` fence.
        //
        // 1. `atomic::fence(SeqCst)`, which compiles into a `mfence` instruction.
        // 2. `_.compare_exchange(_, _, SeqCst)`, which compiles into a `lock cmpxchg` instruction.
        //
        // Both instructions have the effect of a full barrier, but empirical benchmarks have shown
        // that the second one is sometimes a bit faster.
        //
        // The ideal solution here would be to use inline assembly, but we're instead creating a
        // temporary atomic variable and compare-and-exchanging its value. No sane compiler to
        // x86 platforms is going to optimize this away.
        let a = AtomicUsize::new(0);
        let _ = a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
    } else {
        fence(Ordering::SeqCst);
    }
}
