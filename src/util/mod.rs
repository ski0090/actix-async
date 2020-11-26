mod async_channel;

pub(crate) mod channel {
    pub(crate) use super::async_channel::{
        bounded as channel, Receiver, SendFuture, Sender, TryRecvError, WeakSender,
    };

    use core::cell::UnsafeCell;
    use core::future::Future;
    use core::mem::MaybeUninit;
    use core::pin::Pin;
    use core::ptr::drop_in_place;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::Waker;
    use core::task::{Context as StdContext, Poll};

    use alloc::sync::Arc;

    use crate::error::ActixAsyncError;

    /*
       Copy/paste code from async-oneshot crate.
       The goal is to reduce one dependency(futures-micro) for not using wait method of
       async_oneshot::Sender
    */

    /// Create a new oneshot channel pair.
    pub fn oneshot<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
        let inner = Arc::new(Inner::new());
        let sender = OneshotSender::new(inner.clone());
        let receiver = OneshotReceiver::new(inner);
        (sender, receiver)
    }

    #[derive(Debug)]
    pub(crate) struct Inner<T> {
        // This one is easy.
        state: AtomicUsize,
        // This is where it all starts to go a bit wrong.
        value: UnsafeCell<MaybeUninit<T>>,
        // Yes, these are subtly different from the last just to confuse you.
        send: UnsafeCell<MaybeUninit<Waker>>,
        recv: UnsafeCell<MaybeUninit<Waker>>,
    }

    const CLOSED: usize = 0b1000;
    const SEND: usize = 0b0100;
    const RECV: usize = 0b0010;
    const READY: usize = 0b0001;

    impl<T> Inner<T> {
        #[inline(always)]
        pub(crate) fn new() -> Self {
            Inner {
                state: AtomicUsize::new(0),
                value: UnsafeCell::new(MaybeUninit::uninit()),
                send: UnsafeCell::new(MaybeUninit::uninit()),
                recv: UnsafeCell::new(MaybeUninit::uninit()),
            }
        }

        // Gets the current state
        #[inline(always)]
        pub(crate) fn state(&self) -> State {
            State(self.state.load(Ordering::Acquire))
        }

        // Gets the receiver's waker. You *must* check the state to ensure
        // it is set. This would be unsafe if it were public.
        #[inline(always)]
        pub(crate) fn recv(&self) -> &Waker {
            // MUST BE SET
            debug_assert!(self.state().recv());
            unsafe { &*(*self.recv.get()).as_ptr() }
        }

        // Sets the receiver's waker.
        #[inline(always)]
        pub(crate) fn set_recv(&self, waker: Waker) -> State {
            let recv = self.recv.get();
            unsafe { (*recv).as_mut_ptr().write(waker) } // !
            State(self.state.fetch_or(RECV, Ordering::AcqRel))
        }

        // Gets the sender's waker. You *must* check the state to ensure
        // it is set. This would be unsafe if it were public.
        #[inline(always)]
        pub(crate) fn send(&self) -> &Waker {
            debug_assert!(self.state().send());
            unsafe { &*(*self.send.get()).as_ptr() }
        }

        #[inline(always)]
        pub(crate) fn take_value(&self) -> T {
            // MUST BE SET
            debug_assert!(self.state().ready());
            unsafe { (*self.value.get()).as_ptr().read() }
        }

        #[inline(always)]
        pub(crate) fn set_value(&self, value: T) -> State {
            debug_assert!(!self.state().ready());
            let val = self.value.get();
            unsafe { (*val).as_mut_ptr().write(value) }
            State(self.state.fetch_or(READY, Ordering::AcqRel))
        }

        #[inline(always)]
        pub(crate) fn close(&self) -> State {
            State(self.state.fetch_or(CLOSED, Ordering::AcqRel))
        }
    }

    impl<T> Drop for Inner<T> {
        #[inline(always)]
        fn drop(&mut self) {
            let state = State(*self.state.get_mut());
            // Drop the wakers if they are present
            if state.recv() {
                unsafe {
                    drop_in_place((&mut *self.recv.get()).as_mut_ptr());
                }
            }
            if state.send() {
                unsafe {
                    drop_in_place((&mut *self.send.get()).as_mut_ptr());
                }
            }
        }
    }

    unsafe impl<T: Send> Send for Inner<T> {}
    unsafe impl<T: Sync> Sync for Inner<T> {}

    #[derive(Clone, Copy)]
    pub(crate) struct State(usize);

    impl State {
        #[inline(always)]
        pub(crate) fn closed(&self) -> bool {
            (self.0 & CLOSED) == CLOSED
        }
        #[inline(always)]
        pub(crate) fn ready(&self) -> bool {
            (self.0 & READY) == READY
        }
        #[inline(always)]
        pub(crate) fn send(&self) -> bool {
            (self.0 & SEND) == SEND
        }
        #[inline(always)]
        pub(crate) fn recv(&self) -> bool {
            (self.0 & RECV) == RECV
        }
    }

    /// The sending half of a oneshot channel.
    #[derive(Debug)]
    pub struct OneshotSender<T> {
        inner: Arc<Inner<T>>,
        done: bool,
    }

    impl<T> OneshotSender<T> {
        #[inline(always)]
        pub(crate) fn new(inner: Arc<Inner<T>>) -> Self {
            OneshotSender { inner, done: false }
        }

        /// true if the channel is closed
        #[inline(always)]
        pub fn is_closed(&self) -> bool {
            self.inner.state().closed()
        }

        /// Sends a message on the channel. Fails if the Receiver is dropped.
        #[inline]
        pub fn send(mut self, value: T) -> Result<(), ActixAsyncError> {
            self.done = true;
            let inner = &mut self.inner;
            let state = inner.set_value(value);
            if !state.closed() {
                if state.recv() {
                    inner.recv().wake_by_ref();
                    Ok(())
                } else {
                    Ok(())
                }
            } else {
                inner.take_value(); // force drop.
                Err(ActixAsyncError::Receiver)
            }
        }
    }

    impl<T> Drop for OneshotSender<T> {
        #[inline(always)]
        fn drop(&mut self) {
            if !self.done {
                let state = self.inner.state();
                if !state.closed() {
                    let old = self.inner.close();
                    if old.recv() {
                        self.inner.recv().wake_by_ref();
                    }
                }
            }
        }
    }

    /// The receiving half of a oneshot channel.
    #[derive(Debug)]
    pub struct OneshotReceiver<T> {
        inner: Arc<Inner<T>>,
        done: bool,
    }

    impl<T> OneshotReceiver<T> {
        #[inline(always)]
        pub(crate) fn new(inner: Arc<Inner<T>>) -> Self {
            OneshotReceiver { inner, done: false }
        }

        #[inline(always)]
        fn handle_state(&mut self, state: State) -> Poll<Result<T, ActixAsyncError>> {
            if state.ready() {
                Poll::Ready(Ok(self.inner.take_value()))
            } else if state.closed() {
                Poll::Ready(Err(ActixAsyncError::Receiver))
            } else {
                Poll::Pending
            }
            .map(|x| {
                self.done = true;
                x
            })
        }
    }

    impl<T> Future for OneshotReceiver<T> {
        type Output = Result<T, ActixAsyncError>;

        fn poll(self: Pin<&mut Self>, ctx: &mut StdContext) -> Poll<Result<T, ActixAsyncError>> {
            let this = Pin::into_inner(self);
            match this.handle_state(this.inner.state()) {
                Poll::Pending => {}
                x => return x,
            }
            let state = this.inner.set_recv(ctx.waker().clone());
            match this.handle_state(state) {
                Poll::Pending => {}
                x => return x,
            }
            if state.send() {
                this.inner.send().wake_by_ref();
            }
            Poll::Pending
        }
    }

    impl<T> Drop for OneshotReceiver<T> {
        #[inline(always)]
        fn drop(&mut self) {
            if !self.done {
                let state = self.inner.state();
                if !state.closed() && !state.ready() {
                    let old = self.inner.close();
                    if old.send() {
                        self.inner.send().wake_by_ref();
                    }
                }
            }
        }
    }
}

pub(crate) mod futures;

pub(crate) mod smart_pointer {
    pub(crate) type RefCounter<T> = alloc::sync::Arc<T>;
    pub(crate) type WeakRefCounter<T> = alloc::sync::Weak<T>;
}
