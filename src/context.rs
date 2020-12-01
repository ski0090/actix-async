use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::{swap, transmute};
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
    /// use actix_async::prelude::*;
    /// use futures_util::stream::once;
    ///
    /// struct StreamActor;
    /// actor!(StreamActor);
    ///
    /// struct StreamMessage;
    /// message!(StreamMessage, ());
    ///
    /// #[async_trait::async_trait(?Send)]
    /// impl Handler<StreamMessage> for StreamActor {
    ///     async fn handle(&self, _: StreamMessage, _: &Context<Self>) {
    ///     /*
    ///         The stream is owned by Context so there is no default way to return anything
    ///         from the handler.
    ///         A suggest way to return anything here is to use a channel sender or another
    ///         actor's Addr to StreamActor as it's state.
    ///     */
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

    pub(crate) fn is_running(&self) -> bool {
        self.state.get() == ActorState::Running
    }

    fn set_running(&self) {
        self.state.set(ActorState::Running);
    }
}

type Task = LocalBoxFuture<'static, ()>;

pub(crate) struct CacheRef<A>(pub(crate) Vec<Task>, PhantomData<A>);

impl<A: Actor> CacheRef<A> {
    fn new() -> Self {
        Self(Vec::with_capacity(A::size_hint()), PhantomData)
    }

    #[inline(always)]
    fn poll_unpin(&mut self, cx: &mut StdContext<'_>) {
        // poll concurrent messages
        let mut i = 0;
        while i < self.0.len() {
            match self.0[i].as_mut().poll(cx) {
                Poll::Ready(()) => {
                    // SAFETY:
                    // Vec::swap_remove with no len check and drop of removed element in place.
                    // i is guaranteed to be smaller than this.cache_ref.len()
                    unsafe {
                        self.0.swap_remove_uncheck(i);
                    }
                }
                Poll::Pending => i += 1,
            }
        }
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline(always)]
    fn add_concurrent(&mut self, mut msg: Box<dyn MessageHandler<A>>, act: &A, ctx: &Context<A>) {
        self.0.push(msg.handle(act, ctx));
    }
}

pub(crate) struct CacheMut(Option<Task>);

impl CacheMut {
    #[inline(always)]
    fn new() -> Self {
        Self(None)
    }

    #[inline(always)]
    fn get_mut(&mut self) -> Option<&mut Task> {
        self.0.as_mut()
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    #[inline(always)]
    fn add_exclusive<A: Actor>(
        &mut self,
        mut msg: Box<dyn MessageHandler<A>>,
        act: &mut A,
        ctx: &mut Context<A>,
    ) {
        self.0 = Some(msg.handle_wait(act, ctx));
    }

    #[inline(always)]
    fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

pub(crate) struct ContextFuture<A: Actor> {
    act: A,
    pub(crate) ctx: Context<A>,
    pub(crate) cache_mut: CacheMut,
    pub(crate) cache_ref: CacheRef<A>,
    future_cache: Vec<FutureMessage<A>>,
    stream_cache: Vec<StreamMessage<A>>,
    drop_notify: Option<OneshotSender<()>>,
    state: ContextState,
    poll_task: Option<Task>,
    extra_poll: bool,
}

enum ContextState {
    Starting,
    Running,
    Stopping,
}

impl<A: Actor> Unpin for ContextFuture<A> {}

impl<A: Actor> Drop for ContextFuture<A> {
    fn drop(&mut self) {
        if let Some(tx) = self.drop_notify.take() {
            let _ = tx.send(());
        }
    }
}

impl<A: Actor> ContextFuture<A> {
    #[inline(always)]
    pub(crate) fn new(act: A, ctx: Context<A>) -> Self {
        Self {
            act,
            ctx,
            cache_mut: CacheMut::new(),
            cache_ref: CacheRef::new(),
            future_cache: Vec::with_capacity(8),
            stream_cache: Vec::with_capacity(8),
            drop_notify: None,
            state: ContextState::Starting,
            poll_task: None,
            extra_poll: false,
        }
    }

    #[inline(always)]
    fn merge(&mut self) {
        let ctx = &mut self.ctx;
        self.future_cache.merge(ctx);
        self.stream_cache.merge(ctx);
    }

    #[inline(always)]
    fn add_exclusive(&mut self, msg: Box<dyn MessageHandler<A>>) {
        self.cache_mut
            .add_exclusive(msg, &mut self.act, &mut self.ctx);
    }

    #[inline(always)]
    fn add_concurrent(&mut self, msg: Box<dyn MessageHandler<A>>) {
        // when adding new concurrent message we always want an extra poll to register them.
        self.extra_poll = true;
        self.cache_ref.add_concurrent(msg, &self.act, &self.ctx);
    }

    #[inline(always)]
    fn have_cache(&self) -> bool {
        !self.cache_ref.is_empty() || self.cache_mut.is_some()
    }

    #[inline(always)]
    fn poll_running(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();

        // poll concurrent messages
        this.cache_ref.poll_unpin(cx);

        // try to poll exclusive message.
        match this.cache_mut.get_mut() {
            // still have concurrent messages. finish them.
            Some(_) if !this.cache_ref.is_empty() => return Poll::Pending,
            // poll exclusive message and remove it when success.
            Some(fut_mut) => match fut_mut.as_mut().poll(cx) {
                Poll::Ready(_) => this.cache_mut.clear(),
                Poll::Pending => return Poll::Pending,
            },
            None => {}
        }

        // reset extra_poll
        this.extra_poll = false;

        // If context is stopped we stop dealing with future and stream messages.
        if this.ctx.is_running() {
            this.merge();

            // poll future messages
            let mut i = 0;
            while i < this.future_cache.len() {
                match Pin::new(&mut this.future_cache[i]).poll(cx) {
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
                                this.add_concurrent(msg);
                            }
                            Some(ActorMessage::Mut(msg)) => {
                                this.add_exclusive(msg);
                                return self.poll_running(cx);
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
                        this.add_concurrent(msg);
                    }
                    Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                        this.add_exclusive(msg);
                        return self.poll_running(cx);
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

        // actively drain receiver channel for incoming messages.
        loop {
            match Pin::new(&mut this.ctx.rx).poll_next(cx) {
                // new concurrent message. add it to cache_ref and continue.
                Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                    this.add_concurrent(msg);
                }
                // new exclusive message. add it to cache_mut. No new messages should
                // be accepted until this one is resolved.
                Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                    this.add_exclusive(msg);
                    return self.poll_running(cx);
                }
                // stopping messages received.
                Poll::Ready(Some(ActorMessage::State(state, notify))) => {
                    // a oneshot sender to to notify the caller shut down is complete.
                    this.drop_notify = Some(notify);
                    // stop context which would close the channel.
                    this.ctx.stop();
                    // goes to stopping state if it's a force shut down.
                    // otherwise keep the loop until we drain the channel.
                    if let ActorState::Stop = state {
                        this.state = ContextState::Stopping;
                        return self.poll_close(cx);
                    }
                }
                // channel is closed
                Poll::Ready(None) => {
                    // stop context just in case.
                    this.ctx.stop();
                    // have new concurrent message. poll another round.
                    return if this.extra_poll {
                        self.poll_running(cx)
                    // wait for unfinished messages to resolve.
                    } else if this.have_cache() {
                        Poll::Pending
                    } else {
                        // goes to stopping state.
                        this.state = ContextState::Stopping;
                        self.poll_close(cx)
                    };
                }
                Poll::Pending => {
                    // have new concurrent message. poll another round.
                    return if this.extra_poll {
                        self.poll_running(cx)
                    } else {
                        Poll::Pending
                    };
                }
            }
        }
    }

    fn poll_start(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.poll_task.as_mut() {
            Some(task) => match task.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    this.poll_task = None;
                    this.ctx.set_running();
                    this.state = ContextState::Running;
                    self.poll_running(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // SAFETY:
                // Self reference is needed.
                // on_start transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { transmute(this.act.on_start(&mut this.ctx)) };
                this.poll_task = Some(task);
                self.poll_start(cx)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.poll_task.as_mut() {
            Some(task) => match task.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    this.poll_task = None;
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // SAFETY:
                // Self reference is needed.
                // on_stop transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { transmute(this.act.on_stop(&mut this.ctx)) };
                this.poll_task = Some(task);
                self.poll_close(cx)
            }
        }
    }
}

impl<A: Actor> Future for ContextFuture<A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match self.as_mut().get_mut().state {
            ContextState::Running => self.poll_running(cx),
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
        while let Some(stream) = stream.pop() {
            self.push(stream);
        }
    }
}

impl<A: Actor> MergeContext<A> for Vec<FutureMessage<A>> {
    #[inline(always)]
    fn merge(&mut self, ctx: &mut Context<A>) {
        let future = ctx.future_message.get_mut();
        while let Some(future) = future.pop() {
            self.push(future);
        }
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
