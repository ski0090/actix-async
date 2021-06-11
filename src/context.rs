use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::transmute;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
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
use crate::util::futures::{ready, LocalBoxFuture, Stream};

/// Context type of `Actor` type. Can be accessed within `Handler::handle` and
/// `Handler::handle_wait` method.
///
/// Used to mutate the state of actor and add additional tasks to actor.
pub struct Context<'a, A: Actor> {
    state: &'a Cell<ActorState>,
    future_cache: &'a RefCell<Vec<FutureMessage<A>>>,
    stream_cache: &'a RefCell<Vec<StreamMessage<A>>>,
    rx: &'a Receiver<ActorMessage<A>>,
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

impl<'c, A: Actor> Context<'c, A> {
    pub(crate) fn new(
        state: &'c Cell<ActorState>,
        future_cache: &'c RefCell<Vec<FutureMessage<A>>>,
        stream_cache: &'c RefCell<Vec<StreamMessage<A>>>,
        rx: &'c Receiver<ActorMessage<A>>,
    ) -> Self {
        Context {
            state,
            future_cache,
            stream_cache,
            rx,
        }
    }

    /// run interval concurrent closure on context. `Handler::handle` will be called.
    pub fn run_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, Context<'a, A>) -> LocalBoxFuture<'a, ()> + Clone + 'static,
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
        F: for<'a> FnOnce(&'a mut A, Context<'a, A>) -> LocalBoxFuture<'a, ()> + Clone + 'static,
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

        self.stream_cache.borrow_mut().push(msg);

        ContextJoinHandle { handle }
    }

    /// run concurrent closure on context after given duration. `Handler::handle` will be called.
    pub fn run_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, Context<'a, A>) -> LocalBoxFuture<'a, ()> + 'static,
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
        F: for<'a> FnOnce(&'a mut A, Context<'a, A>) -> LocalBoxFuture<'a, ()> + 'static,
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
        self.future_cache.borrow_mut().push(f(rx));
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
        Addr::from_recv(self.rx).ok()
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
    ///     async fn handle(&self, _: StreamMessage, _: Context<'_, Self>) {
    ///     /*
    ///         The stream is owned by Context so there is no default way to return anything
    ///         from the handler.
    ///         A suggest way to return anything here is to use a channel sender or another
    ///         actor's Addr to StreamActor as it's state.
    ///     */
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     tokio::task::LocalSet::new().run_until(async {
    ///         let address = StreamActor::create(|ctx| {
    ///             ctx.add_stream(once(async { StreamMessage }));
    ///             StreamActor
    ///         });
    ///     })
    ///     .await
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
        self.stream_cache.borrow_mut().push(msg);
        ContextJoinHandle { handle }
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
    fn add_concurrent(&mut self, mut msg: Box<dyn MessageHandler<A>>, act: &A, ctx: Context<'_, A>) {
        self.push(msg.handle(act, ctx));
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
        act: &mut A,
        ctx: Context<'_, A>,
    ) {
        self.0 = Some(msg.handle_wait(act, ctx));
    }
}

pub(crate) struct ContextFuture<A: Actor> {
    act: A,
    act_state: Cell<ActorState>,
    act_rx: Receiver<ActorMessage<A>>,
    pub(crate) cache_mut: CacheMut,
    pub(crate) cache_ref: CacheRef<A>,
    future_cache: RefCell<Vec<FutureMessage<A>>>,
    stream_cache: RefCell<Vec<StreamMessage<A>>>,
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
    pub(crate) fn new(
        act: A,
        act_state: Cell<ActorState>,
        act_rx: Receiver<ActorMessage<A>>,
        future_cache: RefCell<Vec<FutureMessage<A>>>,
        stream_cache: RefCell<Vec<StreamMessage<A>>>,
    ) -> Self {
        Self {
            act,
            act_state,
            act_rx,
            cache_mut: CacheMut::new(),
            cache_ref: CacheRef::new(),
            future_cache,
            stream_cache,
            drop_notify: None,
            state: ContextState::Starting,
            poll_task: None,
            extra_poll: false,
        }
    }

    #[inline(always)]
    fn merge(&mut self) {
    }

    #[inline(always)]
    fn add_exclusive(&mut self, msg: Box<dyn MessageHandler<A>>) {
        let ctx = Context::new(&self.act_state, &self.future_cache, &self.stream_cache, &self.act_rx);
        self.cache_mut
            .add_exclusive(msg, &mut self.act, ctx);
    }

    #[inline(always)]
    fn add_concurrent(&mut self, msg: Box<dyn MessageHandler<A>>) {
        // when adding new concurrent message we always want an extra poll to register them.
        self.extra_poll = true;
        let ctx = Context::new(&self.act_state, &self.future_cache, &self.stream_cache, &self.act_rx);
        self.cache_ref.add_concurrent(msg, &self.act, ctx);
    }

    #[inline(always)]
    fn have_cache(&self) -> bool {
        !self.cache_ref.is_empty() || self.cache_mut.is_some()
    }

    #[inline(always)]
    fn poll_running(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();

        // poll concurrent messages
        let mut i = 0;
        while i < this.cache_ref.len() {
            match this.cache_ref[i].as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.cache_ref.swap_remove(i);
                }
                Poll::Pending => i += 1,
            }
        }

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
        }

        // reset extra_poll
        this.extra_poll = false;

        // If context is stopped we stop dealing with future and stream messages.
        if this.act_state.get() == ActorState::Running {
            // poll future messages
            let mut i = 0;
            while i < this.future_cache.get_mut().len() {
                // SAFETY: FutureMessage never moved until they are resolved.
                match Pin::new(&mut this.future_cache.get_mut()[i]).poll(cx) {
                    Poll::Ready(msg) => {
                        this.future_cache.get_mut().swap_remove(i);

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
            while i < this.stream_cache.get_mut().len() {
                match Pin::new(&mut this.stream_cache.get_mut()[i]).poll_next(cx) {
                    Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                        this.add_concurrent(msg);
                    }
                    Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                        this.add_exclusive(msg);
                        return self.poll_running(cx);
                    }
                    // stream is either canceled by ContextJoinHandle or finished.
                    Poll::Ready(None) => {
                        this.stream_cache.get_mut().swap_remove(i);
                    }
                    Poll::Pending => i += 1,
                    _ => unreachable!(),
                }
            }
        }

        // actively drain receiver channel for incoming messages.
        loop {
            match Pin::new(&mut this.act_rx).poll_next(cx) {
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
                    this.act_rx.close();
                    this.act_state.set(ActorState::StopGraceful);
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
                    this.act_rx.close();
                    this.act_state.set(ActorState::StopGraceful);
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
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.poll_task = None;
                this.act_state.set(ActorState::Running);
                this.state = ContextState::Running;
                self.poll_running(cx)
            }
            None => {
                // SAFETY:
                // Self reference is needed.
                // on_start transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let ctx = Context::new(&this.act_state, &this.future_cache, &this.stream_cache, &this.act_rx);
                let task = unsafe { transmute(this.act.on_start(ctx)) };
                this.poll_task = Some(task);
                self.poll_start(cx)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.poll_task.as_mut() {
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.poll_task = None;
                Poll::Ready(())
            }
            None => {
                // SAFETY:
                // Self reference is needed.
                // on_stop transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let ctx = Context::new(&this.act_state, &this.future_cache, &this.stream_cache, &this.act_rx);
                let task = unsafe { transmute(this.act.on_stop(ctx)) };
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
