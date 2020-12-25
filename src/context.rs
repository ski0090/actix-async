use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::swap;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::read;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::actor::{Actor, ActorState};
use crate::address::Addr;
use crate::handler::{Handler, MessageHandler};
use crate::message::{
    ActorMessage, ActorMessageClone, FunctionMessage, FunctionMutMessage, FutureMessage,
    IntervalMessage, Message, StreamContainer, StreamMessage,
};
use crate::util::channel::{oneshot, OneshotReceiver, OneshotSender, Receiver};
use crate::util::futures::{LocalBoxFuture, Stream};

/// Context type of `Actor` type. Can be accessed within `Handler::handle` and
/// `Handler::handle_wait` method.
///
/// Used to mutate the state of actor and add additional tasks to actor.
pub struct Context<A: Actor> {
    state: Cell<ActorState>,
    future_message: RefCell<Vec<FutureMessage<A>>>,
    stream_message: RefCell<Vec<StreamMessage<A>>>,
    rx: Receiver<ActorMessage<A>>,
}

/// a join handle can be used to cancel a spawned async task like interval closure and stream
/// handler
pub struct ContextJoinHandle {
    handle: OneshotSender<()>,
}

impl ContextJoinHandle {
    /// Cancel the task associate to this handle.
    pub fn cancel(self) {
        let _ = self.handle.send(());
    }

    /// Check if the task associate with this handle is terminated.
    ///
    /// This happens when the task is finished or the thread task runs on is recovered from a
    /// panic.
    pub fn is_terminated(&self) -> bool {
        self.handle.is_closed()
    }
}

impl<A: Actor> Context<A> {
    pub(crate) fn new(rx: Receiver<ActorMessage<A>>) -> Self {
        Context {
            state: Cell::new(ActorState::Stop),
            future_message: RefCell::new(Vec::with_capacity(8)),
            stream_message: RefCell::new(Vec::with_capacity(8)),
            rx,
        }
    }

    /// run interval concurrent closure on context. `Handler::handle` will be called.
    pub fn run_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxFuture<'a, ()> + Clone + 'static,
    {
        self.interval(|rx| {
            let msg = FunctionMessage::new(f);
            IntervalMessage::new(dur, rx, ActorMessageClone::Ref(Box::new(msg)))
        })
    }

    /// run interval exclusive closure on context. `Handler::handle_wait` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxFuture<'a, ()>
            + Clone
            + 'static,
    {
        self.interval(|rx| {
            let msg = FunctionMutMessage::new(f);
            IntervalMessage::new(dur, rx, ActorMessageClone::Mut(Box::new(msg)))
        })
    }

    fn interval<F>(&self, f: F) -> ContextJoinHandle
    where
        F: FnOnce(OneshotReceiver<()>) -> IntervalMessage<A>,
    {
        let (handle, rx) = oneshot();

        let msg = f(rx);
        let msg = StreamMessage::new_interval(msg);

        self.stream_message.borrow_mut().push(msg);

        ContextJoinHandle { handle }
    }

    /// run concurrent closure on context after given duration. `Handler::handle` will be called.
    pub fn run_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxFuture<'a, ()> + 'static,
    {
        self.later(|rx| {
            let msg = FunctionMessage::<_, ()>::new(f);
            let msg = ActorMessage::new_ref(msg, None);
            FutureMessage::new(dur, rx, msg)
        })
    }

    /// run exclusive closure on context after given duration. `Handler::handle_wait` will be
    /// called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxFuture<'a, ()> + 'static,
    {
        self.later(|rx| {
            let msg = FunctionMutMessage::<_, ()>::new(f);
            let msg = ActorMessage::new_mut(msg, None);
            FutureMessage::new(dur, rx, msg)
        })
    }

    fn later<F>(&self, f: F) -> ContextJoinHandle
    where
        F: FnOnce(OneshotReceiver<()>) -> FutureMessage<A>,
    {
        let (handle, rx) = oneshot();
        self.future_message.borrow_mut().push(f(rx));
        ContextJoinHandle { handle }
    }

    /// stop the context. It would end the actor gracefully by close the channel draining all
    /// remaining messages.
    ///
    /// *. In the case of using `Supervisor`. This method would stop all actor instances at the
    /// same time
    pub fn stop(&self) {
        self.rx.close();
        self.state.set(ActorState::StopGraceful);
    }

    /// get the address of actor from context.
    pub fn address(&self) -> Option<Addr<A>> {
        Addr::from_recv(&self.rx).ok()
    }

    /// add a stream to context. multiple stream can be added to one context.
    ///
    /// stream item will be treated as concurrent message and `Handler::handle` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    ///
    /// *. Stream would force closed when the actor is stopped. Either by dropping all `Addr` or
    /// calling `Addr::stop`
    ///
    /// # example:
    /// ```rust
    /// #![allow(incomplete_features)]
    /// #![feature(generic_associated_types, type_alias_impl_trait)]
    ///
    /// use std::future::Future;
    ///
    /// use actix_async::prelude::*;
    /// use futures_util::stream::once;
    ///
    /// struct StreamActor;
    /// actor!(StreamActor);
    ///
    /// struct StreamMessage;
    /// message!(StreamMessage, ());
    ///
    /// impl Handler<StreamMessage> for StreamActor {
    ///     type Future<'res> = impl Future<Output = ()>;
    ///     type FutureWait<'res> = impl Future<Output = ()>;
    ///
    ///     fn handle<'act, 'ctx, 'res>(
    ///         &'act self,
    ///         _: StreamMessage,
    ///         _: &'ctx Context<Self>
    ///     ) -> Self::Future<'res>
    ///     where
    ///         'act: 'res,
    ///         'ctx: 'res
    ///     {
    ///         async {
    ///             /*
    ///             The stream is owned by Context so there is no default way to return anything
    ///             from the handler.
    ///             A suggest way to return anything here is to use a channel sender or another
    ///             actor's Addr to StreamActor as it's state.
    ///             */
    ///         }
    ///     }
    ///
    ///     fn handle_wait<'act, 'ctx, 'res>(
    ///         &'act mut self,
    ///         _: StreamMessage,
    ///         _: &'ctx mut Context<Self>
    ///     ) -> Self::FutureWait<'res>
    ///     where
    ///         'act: 'res,
    ///         'ctx: 'res
    ///     {
    ///         async { unimplemented!() }
    ///     }
    /// }
    ///
    /// #[actix_rt::main]
    /// async fn main() {
    ///     let address = StreamActor::create(|ctx| {
    ///         ctx.add_stream(once(async { StreamMessage }));
    ///         StreamActor
    ///     });
    /// }
    /// ```
    pub fn add_stream<S>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream + 'static,
        S::Item: Message + 'static,
        A: Handler<S::Item>,
    {
        self.stream(stream, |item| ActorMessage::new_ref(item, None))
    }

    /// add a stream to context. multiple stream can be added to one context.
    ///
    /// stream item will be treated as exclusive message and `Handler::handle_wait` will be called.
    pub fn add_wait_stream<S>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream + 'static,
        S::Item: Message + 'static,
        A: Handler<S::Item>,
    {
        self.stream(stream, |item| ActorMessage::new_mut(item, None))
    }

    fn stream<S, F>(&self, stream: S, f: F) -> ContextJoinHandle
    where
        S: Stream + 'static,
        S::Item: Message + 'static,
        A: Handler<S::Item>,
        F: FnOnce(S::Item) -> ActorMessage<A> + Copy + 'static,
    {
        let (handle, rx) = oneshot();
        let stream = StreamContainer::new(stream, rx, f);
        let msg = StreamMessage::new_boxed(stream);
        self.stream_message.borrow_mut().push(msg);
        ContextJoinHandle { handle }
    }

    #[inline(always)]
    pub(crate) fn is_running(&self) -> bool {
        self.state.get() == ActorState::Running
    }

    fn set_running(&self) {
        self.state.set(ActorState::Running);
    }
}

type Task = LocalBoxFuture<'static, ()>;

pub(crate) struct CacheRef<A>(Vec<Task>, PhantomData<A>);

impl<A> Deref for CacheRef<A> {
    type Target = Vec<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<A> DerefMut for CacheRef<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<A: Actor> CacheRef<A> {
    fn new() -> Self {
        Self(Vec::with_capacity(A::size_hint()), PhantomData)
    }

    #[inline(always)]
    fn add_concurrent(&mut self, mut msg: Box<dyn MessageHandler<A>>, cap: &Capsule<A>) {
        /*
            SAFETY:
            `MessageHandler::handle`can not tie to actor and context's lifetime.
            The reason is it would assume the boxed futures would live as long as the
            actor and context. Making it impossible to mutably borrow them again from
            this point forward.

            future transmute to static lifetime must be polled before next
            ContextWithActor.cache_mut is polled.
        */
        unsafe {
            let act = cap.act_static();
            let ctx = cap.ctx_static();

            self.push(msg.handle(act, ctx));
        }
    }

    #[inline(always)]
    fn clean(&mut self) {
        // reset to capacity if cache is empty
        if self.0.is_empty() {
            swap(&mut Vec::with_capacity(A::size_hint()), &mut self.0);
        }
    }
}

pub(crate) struct CacheMut(Option<Task>);

impl Deref for CacheMut {
    type Target = Option<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CacheMut {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl CacheMut {
    #[inline(always)]
    fn new() -> Self {
        Self(None)
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    #[inline(always)]
    fn add_exclusive<A: Actor>(
        &mut self,
        mut msg: Box<dyn MessageHandler<A>>,
        cap: &mut Capsule<A>,
    ) {
        /*
            SAFETY:
            `MessageHandler::handle_wait`can not tie to actor and context's lifetime.
            The reason is it would assume the boxed futures would live as long as the
            actor and context. Making it impossible to mutably borrow them again from
            this point forward.

            future transmute to static lifetime must be polled only when
            ContextWithActor.cache_ref is empty.
        */
        unsafe {
            let act = cap.act_mut_static();
            let ctx = cap.ctx_mut_static();
            self.0 = Some(msg.handle_wait(act, ctx));
        }
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct ContextFuture<A: Actor> {
        pub(crate) cap: Capsule<A>,
        pub(crate) cache_mut: CacheMut,
        pub(crate) cache_ref: CacheRef<A>,
        future_cache: Vec<FutureMessage<A>>,
        stream_cache: Vec<StreamMessage<A>>,
        drop_notify: Option<OneshotSender<()>>,
        state: ContextState,
        poll_task: Option<Task>,
        poll_concurrent: bool,
        #[pin]
        _pin: PhantomPinned,
    }
}

enum ContextState {
    Starting,
    Running,
    Stopping,
}

pub struct Capsule<A: Actor> {
    act: *mut A,
    ctx: *mut Context<A>,
}

impl<A: Actor> Drop for Capsule<A> {
    fn drop(&mut self) {
        // SAFETY:
        // convert back to box for freeing the memory.
        // at this point all futures referencing actor and context are resolved or dropped.
        unsafe {
            Box::from_raw(self.act);
            Box::from_raw(self.ctx);
        };
    }
}

impl<A: Actor> Capsule<A> {
    fn new(act: A, ctx: Context<A>) -> Self {
        let act = Box::into_raw(Box::new(act));
        let ctx = Box::into_raw(Box::new(ctx));
        Self { act, ctx }
    }

    #[inline(always)]
    pub(crate) fn ctx_mut(&mut self) -> &mut Context<A> {
        // SAFETY:
        // borrow self with mut so this is safe.
        unsafe { &mut *self.ctx }
    }

    #[inline(always)]
    pub(crate) fn ctx(&self) -> &Context<A> {
        // SAFETY:
        // borrow self so this is safe.
        unsafe { &*self.ctx }
    }

    // SAFETY:
    // caller must make sure the lifetime does not violate the borrow checker rule.
    #[inline(always)]
    unsafe fn act_mut_static(&mut self) -> &'static mut A {
        &mut *self.act
    }

    // SAFETY:
    // caller must make sure the lifetime does not violate the borrow checker rule.
    #[inline(always)]
    unsafe fn ctx_mut_static(&mut self) -> &'static mut Context<A> {
        &mut *self.ctx
    }

    // SAFETY:
    // caller must make sure the lifetime does not violate the borrow checker rule.
    #[inline(always)]
    unsafe fn act_static(&self) -> &'static A {
        &*self.act
    }

    // SAFETY:
    // caller must make sure the lifetime does not violate the borrow checker rule.
    #[inline(always)]
    unsafe fn ctx_static(&self) -> &'static Context<A> {
        &*self.ctx
    }
}

impl<A: Actor> ContextFuture<A> {
    #[inline(always)]
    pub(crate) fn new(act: A, ctx: Context<A>) -> Self {
        let cap = Capsule::new(act, ctx);
        Self {
            cap,
            cache_mut: CacheMut::new(),
            cache_ref: CacheRef::new(),
            future_cache: Vec::with_capacity(8),
            stream_cache: Vec::with_capacity(8),
            drop_notify: None,
            state: ContextState::Starting,
            poll_task: None,
            poll_concurrent: false,
            _pin: PhantomPinned,
        }
    }

    #[inline(always)]
    fn poll_concurrent(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();

        // reset extra_poll
        *this.poll_concurrent = false;

        // poll concurrent messages
        let mut i = 0;
        while i < this.cache_ref.len() {
            match this.cache_ref[i].as_mut().poll(cx) {
                Poll::Ready(()) => {
                    // SAFETY:
                    // Vec::swap_remove with no len check and drop of removed element in place.
                    // i is guaranteed to be smaller than this.cache_ref.len()
                    unsafe {
                        this.cache_ref.swap_remove_uncheck(i);
                    }
                }
                Poll::Pending => i += 1,
            }
        }

        self.poll_exclusive(cx)
    }

    #[inline(always)]
    fn poll_exclusive(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();

        match this.poll_concurrent {
            false => {
                // try to poll exclusive message.
                match this.cache_mut.as_mut() {
                    // still have concurrent messages. finish them.
                    Some(_) if !this.cache_ref.is_empty() => return Poll::Pending,
                    // poll exclusive message and remove it when success.
                    Some(fut_mut) => match fut_mut.as_mut().poll(cx) {
                        Poll::Ready(_) => this.cache_mut.clear(),
                        Poll::Pending => return Poll::Pending,
                    },
                    None => {}
                };
                self.poll_future(cx)
            }
            true => self.poll_concurrent(cx),
        }
    }

    #[inline(always)]
    fn poll_future(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();

        // If context is stopped we stop dealing with future and stream messages.
        if this.cap.ctx_mut().is_running() {
            this.future_cache.merge(this.cap.ctx_mut());
            this.stream_cache.merge(this.cap.ctx_mut());

            // poll future messages
            let mut i = 0;
            while i < this.future_cache.len() {
                // SAFETY: FutureMessage are not moved until they are resolved.
                match unsafe { Pin::new_unchecked(&mut this.future_cache[i]) }.poll(cx) {
                    Poll::Ready(msg) => {
                        // SAFETY:
                        // Vec::swap_remove with no len check and drop of removed
                        // element in place.
                        // i is guaranteed to be smaller than future_cache.len()
                        unsafe {
                            this.future_cache.swap_remove_uncheck(i);
                        }

                        match msg {
                            Some(ActorMessage::Ref(msg)) => {
                                *this.poll_concurrent = true;
                                this.cache_ref.add_concurrent(msg, this.cap);
                            }
                            Some(ActorMessage::Mut(msg)) => {
                                this.cache_mut.add_exclusive(msg, this.cap);
                                return self.poll_exclusive(cx);
                            }
                            // Message is canceled by ContextJoinHandle. Ignore it.
                            None => {}
                            _ => unreachable!(),
                        }
                    }
                    Poll::Pending => i += 1,
                }
            }

            // poll stream message.
            let mut i = 0;
            while i < this.stream_cache.len() {
                match Pin::new(&mut this.stream_cache[i]).poll_next(cx) {
                    Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                        // when adding new concurrent message we always want an extra poll to register them.
                        *this.poll_concurrent = true;
                        this.cache_ref.add_concurrent(msg, this.cap);
                    }
                    Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                        this.cache_mut.add_exclusive(msg, this.cap);
                        return self.poll_exclusive(cx);
                    }
                    // stream is either canceled by ContextJoinHandle or finished.
                    Poll::Ready(None) => {
                        // SAFETY:
                        // Vec::swap_remove with no len check and drop of removed
                        // element in place.
                        // i is guaranteed to be smaller than stream_cache.len()
                        unsafe {
                            this.stream_cache.swap_remove_uncheck(i);
                        }
                    }
                    Poll::Pending => i += 1,
                    _ => unreachable!(),
                }
            }
        }

        self.poll_channel(cx)
    }

    #[inline(always)]
    fn poll_channel(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();

        // actively drain receiver channel for incoming messages.
        loop {
            match Pin::new(&mut this.cap.ctx_mut().rx).poll_next(cx) {
                // new concurrent message. add it to cache_ref and continue.
                Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                    *this.poll_concurrent = true;

                    this.cache_ref.add_concurrent(msg, this.cap);
                }
                // new exclusive message. add it to cache_mut. No new messages should
                // be accepted until this one is resolved.
                Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                    this.cache_mut.add_exclusive(msg, this.cap);
                    return self.poll_exclusive(cx);
                }
                // stopping messages received.
                Poll::Ready(Some(ActorMessage::State(state, notify))) => {
                    // a oneshot sender to to notify the caller shut down is complete.
                    *this.drop_notify = Some(notify);
                    // stop context which would close the channel.
                    this.cap.ctx_mut().stop();
                    // goes to stopping state if it's a force shut down.
                    // otherwise keep the loop until we drain the channel.
                    if let ActorState::Stop = state {
                        *this.state = ContextState::Stopping;
                        return self.poll_close(cx);
                    }
                }
                // channel is closed
                Poll::Ready(None) => {
                    // stop context just in case.
                    this.cap.ctx_mut().stop();
                    // have new concurrent message. poll another round.
                    return if *this.poll_concurrent {
                        self.poll_concurrent(cx)
                    // wait for unfinished messages to resolve.
                    } else if !this.cache_ref.is_empty() || this.cache_mut.is_some() {
                        Poll::Pending
                    } else {
                        // goes to stopping state.
                        *this.state = ContextState::Stopping;
                        self.poll_close(cx)
                    };
                }
                Poll::Pending => {
                    // have new concurrent message. poll another round.
                    return if *this.poll_concurrent {
                        self.poll_concurrent(cx)
                    } else {
                        // clear the cache ref vec if possible.
                        this.cache_ref.clean();
                        Poll::Pending
                    };
                }
            }
        }
    }

    fn poll_start(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();
        match this.poll_task.as_mut() {
            Some(task) => match task.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    *this.poll_task = None;
                    this.cap.ctx_mut().set_running();
                    *this.state = ContextState::Running;
                    self.poll_concurrent(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // SAFETY:
                // Self reference is needed.
                // Task would be destroyed if ContextFut goes out of scope.
                let task = unsafe {
                    this.cap
                        .act_mut_static()
                        .on_start(this.cap.ctx_mut_static())
                };
                *this.poll_task = Some(task);
                self.poll_start(cx)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().project();
        match this.poll_task.as_mut() {
            Some(task) => match task.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    *this.poll_task = None;
                    if let Some(tx) = this.drop_notify.take() {
                        let _ = tx.send(());
                    }
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // SAFETY:
                // Self reference is needed.
                // Task would be destroyed if ContextFut goes out of scope.
                let task = unsafe { this.cap.act_mut_static().on_stop(this.cap.ctx_mut_static()) };
                *this.poll_task = Some(task);
                self.poll_close(cx)
            }
        }
    }
}

impl<A: Actor> Future for ContextFuture<A> {
    type Output = ();

    #[inline(always)]
    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match self.as_mut().project().state {
            ContextState::Running => self.poll_concurrent(cx),
            ContextState::Starting => self.poll_start(cx),
            ContextState::Stopping => self.poll_close(cx),
        }
    }
}

// merge Context with ContextWithActor
trait MergeContext<A: Actor> {
    fn merge(&mut self, ctx: &mut Context<A>);
}

impl<A: Actor> MergeContext<A> for Vec<StreamMessage<A>> {
    #[inline(always)]
    fn merge(&mut self, ctx: &mut Context<A>) {
        let stream = ctx.stream_message.get_mut();
        self.extend(stream.drain(0..));
    }
}

impl<A: Actor> MergeContext<A> for Vec<FutureMessage<A>> {
    #[inline(always)]
    fn merge(&mut self, ctx: &mut Context<A>) {
        let future = ctx.future_message.get_mut();
        self.extend(future.drain(0..));
    }
}

// SAFETY:
// swap remove with index. do not check for overflow.
// caller is in charge of check the index to make sure it's smaller than then length.
trait SwapRemoveUncheck {
    unsafe fn swap_remove_uncheck(&mut self, index: usize);
}

impl<T> SwapRemoveUncheck for Vec<T> {
    #[inline(always)]
    unsafe fn swap_remove_uncheck(&mut self, i: usize) {
        let len = self.len();
        let mut last = read(self.as_ptr().add(len - 1));
        let hole = self.as_mut_ptr().add(i);
        self.set_len(len - 1);
        swap(&mut *hole, &mut last);
    }
}
