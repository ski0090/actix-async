// Copy/paste from async-channel crate.
// The goal is to add weak sender and poll method for sender.

use core::cell::{Cell, UnsafeCell};
use core::fmt;
use core::future::Future;
use core::mem::{replace, ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering};
use core::task::{Context as StdContext, Poll, Waker};
use core::usize;

use alloc::boxed::Box;
use alloc::vec::Vec;

use cache_padded::CachePadded;
use spin::{Mutex, MutexGuard};

use crate::error::ActixAsyncError;
use crate::util::futures::Stream;
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
    listener: Option<EventListener>,
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

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
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
                        // SAFETY:
                        // have exclusive to slot
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
                        // SAFETY:
                        // have exclusive to slot.
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

    pub fn is_closed(&self) -> bool {
        self.tail.load(Ordering::SeqCst) & self.mark_bit != 0
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
            // SAFETY:
            // slot must be init as the value is inserted after head/tail moved successfully.
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

// Copy/paste from event-listener crate
// The goal is to remove blocking listener that would need std features.
struct Inner {
    notified: AtomicUsize,
    list: Mutex<List>,
    cache: UnsafeCell<Entry>,
}

impl Inner {
    fn lock(&self) -> ListGuard<'_> {
        ListGuard {
            inner: self,
            guard: self.list.lock(),
        }
    }

    #[inline(always)]
    fn cache_ptr(&self) -> NonNull<Entry> {
        unsafe { NonNull::new_unchecked(self.cache.get()) }
    }
}

pub struct Event {
    inner: AtomicPtr<Inner>,
}

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    #[inline]
    pub const fn new() -> Event {
        Event {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[cold]
    pub fn listen(&self) -> EventListener {
        let inner = self.inner();
        let listener = EventListener {
            inner: unsafe { RefCounter::clone(&ManuallyDrop::new(RefCounter::from_raw(inner))) },
            entry: Some(inner.lock().insert(inner.cache_ptr())),
        };

        // Make sure the listener is registered before whatever happens next.
        full_fence();
        listener
    }

    #[inline]
    pub fn notify(&self, n: usize) {
        // Make sure the notification comes after whatever triggered it.
        full_fence();

        if let Some(inner) = self.try_inner() {
            // Notify if there is at least one unnotified listener and the number of notified
            // listeners is less than `n`.
            if inner.notified.load(Ordering::Acquire) < n {
                inner.lock().notify(n);
            }
        }
    }

    #[inline]
    fn try_inner(&self) -> Option<&Inner> {
        let inner = self.inner.load(Ordering::Acquire);
        unsafe { inner.as_ref() }
    }

    fn inner(&self) -> &Inner {
        let mut inner = self.inner.load(Ordering::Acquire);

        // Initialize the state if this is its first use.
        if inner.is_null() {
            // Allocate on the heap.
            let new = RefCounter::new(Inner {
                notified: AtomicUsize::new(usize::MAX),
                list: Mutex::new(List {
                    head: None,
                    tail: None,
                    start: None,
                    len: 0,
                    notified: 0,
                    cache_used: false,
                }),
                cache: UnsafeCell::new(Entry {
                    state: Cell::new(State::Created),
                    prev: Cell::new(None),
                    next: Cell::new(None),
                }),
            });
            // Convert the heap-allocated state into a raw pointer.
            let new = RefCounter::into_raw(new) as *mut Inner;

            // Attempt to replace the null-pointer with the new state pointer.
            inner = self.inner.compare_and_swap(inner, new, Ordering::AcqRel);

            // Check if the old pointer value was indeed null.
            if inner.is_null() {
                // If yes, then use the new state pointer.
                inner = new;
            } else {
                // If not, that means a concurrent operation has initialized the state.
                // In that case, use the old pointer and deallocate the new one.
                unsafe {
                    drop(RefCounter::from_raw(new));
                }
            }
        }

        unsafe { &*inner }
    }
}

impl Drop for Event {
    #[inline]
    fn drop(&mut self) {
        let inner: *mut Inner = *self.inner.get_mut();

        // If the state pointer has been initialized, deallocate it.
        if !inner.is_null() {
            unsafe {
                drop(RefCounter::from_raw(inner));
            }
        }
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Event { .. }")
    }
}

impl Default for Event {
    fn default() -> Event {
        Event::new()
    }
}

pub struct EventListener {
    inner: RefCounter<Inner>,
    entry: Option<NonNull<Entry>>,
}

unsafe impl Send for EventListener {}
unsafe impl Sync for EventListener {}

impl fmt::Debug for EventListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("EventListener { .. }")
    }
}

impl Future for EventListener {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let mut list = self.inner.lock();

        let entry = match self.entry {
            None => unreachable!("cannot poll a completed `EventListener` future"),
            Some(entry) => entry,
        };
        let state = unsafe { &entry.as_ref().state };

        // Do a dummy replace operation in order to take out the state.
        match state.replace(State::Notified(false)) {
            State::Notified(_) => {
                // If this listener has been notified, remove it from the list and return.
                list.remove(entry, self.inner.cache_ptr());
                drop(list);
                self.entry = None;
                return Poll::Ready(());
            }
            State::Created => {
                // If the listener was just created, put it in the `Polling` state.
                state.set(State::Polling(cx.waker().clone()));
            }
            State::Polling(w) => {
                // If the listener was in the `Polling` state, update the waker.
                if w.will_wake(cx.waker()) {
                    state.set(State::Polling(w));
                } else {
                    state.set(State::Polling(cx.waker().clone()));
                }
            }
        }

        Poll::Pending
    }
}

impl Drop for EventListener {
    fn drop(&mut self) {
        // If this listener has never picked up a notification...
        if let Some(entry) = self.entry.take() {
            let mut list = self.inner.lock();

            // But if a notification was delivered to it...
            if let State::Notified(additional) = list.remove(entry, self.inner.cache_ptr()) {
                // Then pass it on to another active listener.
                if additional {
                    list.notify_additional(1);
                } else {
                    list.notify(1);
                }
            }
        }
    }
}

struct ListGuard<'a> {
    inner: &'a Inner,
    guard: MutexGuard<'a, List>,
}

impl Drop for ListGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        let list = &mut **self;

        // Update the atomic `notified` counter.
        let notified = if list.notified < list.len {
            list.notified
        } else {
            usize::MAX
        };
        self.inner.notified.store(notified, Ordering::Release);
    }
}

impl Deref for ListGuard<'_> {
    type Target = List;

    #[inline]
    fn deref(&self) -> &List {
        &*self.guard
    }
}

impl DerefMut for ListGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut List {
        &mut *self.guard
    }
}

enum State {
    Created,
    Notified(bool),
    Polling(Waker),
}

impl State {
    #[inline]
    fn is_notified(&self) -> bool {
        match self {
            State::Notified(_) => true,
            State::Created | State::Polling(_) => false,
        }
    }
}

struct Entry {
    state: Cell<State>,
    prev: Cell<Option<NonNull<Entry>>>,
    next: Cell<Option<NonNull<Entry>>>,
}

struct List {
    head: Option<NonNull<Entry>>,
    tail: Option<NonNull<Entry>>,
    start: Option<NonNull<Entry>>,
    len: usize,
    notified: usize,
    cache_used: bool,
}

impl List {
    fn insert(&mut self, cache: NonNull<Entry>) -> NonNull<Entry> {
        unsafe {
            let entry = Entry {
                state: Cell::new(State::Created),
                prev: Cell::new(self.tail),
                next: Cell::new(None),
            };

            let entry = if self.cache_used {
                // Allocate an entry that is going to become the new tail.
                NonNull::new_unchecked(Box::into_raw(Box::new(entry)))
            } else {
                // No need to allocate - we can use the cached entry.
                self.cache_used = true;
                cache.as_ptr().write(entry);
                cache
            };

            // Replace the tail with the new entry.
            match replace(&mut self.tail, Some(entry)) {
                None => self.head = Some(entry),
                Some(t) => t.as_ref().next.set(Some(entry)),
            }

            // If there were no unnotified entries, this one is the first now.
            if self.start.is_none() {
                self.start = self.tail;
            }

            // Bump the entry count.
            self.len += 1;

            entry
        }
    }

    fn remove(&mut self, entry: NonNull<Entry>, cache: NonNull<Entry>) -> State {
        unsafe {
            let prev = entry.as_ref().prev.get();
            let next = entry.as_ref().next.get();

            // Unlink from the previous entry.
            match prev {
                None => self.head = next,
                Some(p) => p.as_ref().next.set(next),
            }

            // Unlink from the next entry.
            match next {
                None => self.tail = prev,
                Some(n) => n.as_ref().prev.set(prev),
            }

            // If this was the first unnotified entry, move the pointer to the next one.
            if self.start == Some(entry) {
                self.start = next;
            }

            // Extract the state.
            let state = if ptr::eq(entry.as_ptr(), cache.as_ptr()) {
                // Free the cached entry.
                self.cache_used = false;
                entry.as_ref().state.replace(State::Created)
            } else {
                // Deallocate the entry.
                Box::from_raw(entry.as_ptr()).state.into_inner()
            };

            // Update the counters.
            if state.is_notified() {
                self.notified -= 1;
            }
            self.len -= 1;

            state
        }
    }

    #[cold]
    fn notify(&mut self, mut n: usize) {
        if n <= self.notified {
            return;
        }
        n -= self.notified;

        while n > 0 {
            n -= 1;

            // Notify the first un notified entry.
            match self.start {
                None => break,
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = unsafe { e.as_ref() };
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    match e.state.replace(State::Notified(false)) {
                        State::Notified(_) => {}
                        State::Created => {}
                        State::Polling(w) => w.wake(),
                    }

                    // Update the counter.
                    self.notified += 1;
                }
            }
        }
    }

    #[cold]
    fn notify_additional(&mut self, mut n: usize) {
        while n > 0 {
            n -= 1;

            // Notify the first un notified entry.
            match self.start {
                None => break,
                Some(e) => {
                    // Get the entry and move the pointer forward.
                    let e = unsafe { e.as_ref() };
                    self.start = e.next.get();

                    // Set the state of this entry to `Notified` and notify.
                    match e.state.replace(State::Notified(true)) {
                        State::Notified(_) => {}
                        State::Created => {}
                        State::Polling(w) => w.wake(),
                    }

                    // Update the counter.
                    self.notified += 1;
                }
            }
        }
    }
}
