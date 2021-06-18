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
use slab::Slab;

use crate::actor::{Actor, ActorState};
use crate::address::Addr;
use crate::handler::{Handler, MessageHandler};
use crate::message::{
    ActorMessage, ActorMessageClone, FunctionMessage, FunctionMutMessage, FutureMessage,
    IntervalMessage, Message, StreamContainer, StreamMessage,
};
use crate::util::channel::{oneshot, OneshotReceiver, OneshotSender, Receiver};
use crate::util::futures::{ready, LocalBoxFuture, Stream};
use crate::waker::{ActorWaker, WakeQueue};

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

pub(crate) struct TaskRef<A>(Slab<Task>, PhantomData<A>);

impl<A> Deref for TaskRef<A> {
    type Target = Slab<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<A> DerefMut for TaskRef<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<A: Actor> TaskRef<A> {
    fn new() -> Self {
        Self(Slab::with_capacity(A::size_hint()), PhantomData)
    }

    #[inline(always)]
    fn add_task(&mut self, task: Task) -> usize {
        self.insert(task)
    }
}

pub(crate) struct TaskMut(Option<Task>);

impl Deref for TaskMut {
    type Target = Option<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TaskMut {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl TaskMut {
    #[inline(always)]
    fn new() -> Self {
        Self(None)
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    #[inline(always)]
    fn add_task(&mut self, task: Task) {
        self.0 = Some(task);
    }
}

pub(crate) struct ContextFuture<A: Actor> {
    act: A,
    act_state: Cell<ActorState>,
    act_rx: Receiver<ActorMessage<A>>,
    queue: WakeQueue,
    pub(crate) cache_mut: TaskMut,
    pub(crate) cache_ref: TaskRef<A>,
    future_cache: RefCell<Vec<FutureMessage<A>>>,
    stream_cache: RefCell<Vec<StreamMessage<A>>>,
    drop_notify: Option<OneshotSender<()>>,
    state: ContextState,
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
            queue: WakeQueue::new(),
            cache_mut: TaskMut::new(),
            cache_ref: TaskRef::new(),
            future_cache,
            stream_cache,
            drop_notify: None,
            state: ContextState::Starting,
            extra_poll: false,
        }
    }

    #[inline(always)]
    fn add_exclusive(&mut self, mut msg: Box<dyn MessageHandler<A>>) {
        let ctx = Context::new(
            &self.act_state,
            &self.future_cache,
            &self.stream_cache,
            &self.act_rx,
        );
        let task = msg.handle_wait(&mut self.act, ctx);
        self.cache_mut.add_task(task);
    }

    #[inline(always)]
    fn add_concurrent(&mut self, mut msg: Box<dyn MessageHandler<A>>) {
        // when adding new concurrent message we always want an extra poll to register them.
        self.extra_poll = true;
        let ctx = Context::new(
            &self.act_state,
            &self.future_cache,
            &self.stream_cache,
            &self.act_rx,
        );
        let task = msg.handle(&self.act, ctx);
        let idx = self.cache_ref.add_task(task);
        self.queue.enqueue(idx);
    }

    #[inline(always)]
    fn have_cache(&self) -> bool {
        !self.cache_ref.is_empty() || self.cache_mut.is_some()
    }

    #[inline(always)]
    fn poll_running(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();

        // poll concurrent messages and collect task index that is ready.

        // only try to get the lock. When lock is held by others it means they are about to wake up
        // this actor future and it would be scheduled to wake up again.
        let len = this.cache_ref.len();
        let mut polled = 0;
        while let Some(idx) = this.queue.try_lock().and_then(|mut l| l.pop_front()) {
            if let Some(task) = this.cache_ref.get_mut(idx) {
                // construct actor waker from the waker actor received.
                let waker = ActorWaker::new(&this.queue, idx, cx.waker()).into();
                let cx = &mut StdContext::from_waker(&waker);
                // prepare to remove the resolved tasks.
                if task.as_mut().poll(cx).is_ready() {
                    this.cache_ref.remove(idx);
                }
            }
            polled += 1;
            // TODO: there is a race condition happening so a hard break is scheduled.
            // investigate the source.
            if polled == len {
                cx.waker().wake_by_ref();
                break;
            }
        }

        // try to poll exclusive message.
        match this.cache_mut.as_mut() {
            // still have concurrent messages. finish them.
            Some(_) if !this.cache_ref.is_empty() => return Poll::Pending,
            // poll exclusive message and remove it when success.
            Some(fut_mut) => {
                ready!(fut_mut.as_mut().poll(cx));
                this.cache_mut.clear();
            }
            None => {}
        }

        // reset extra_poll
        this.extra_poll = false;

        // If context is stopped we stop dealing with future and stream messages.
        if this.act_state.get() == ActorState::Running {
            // poll future messages
            let mut i = 0;
            while i < this.future_cache.get_mut().len() {
                let cache = this.future_cache.get_mut();
                match Pin::new(&mut cache[i]).poll(cx) {
                    Poll::Ready(msg) => {
                        cache.swap_remove(i);

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
            let mut extra_wake = false;
            while i < this.stream_cache.get_mut().len() {
                let mut polled = 0;

                'stream: while let Poll::Ready(res) =
                    Pin::new(&mut this.stream_cache.get_mut()[i]).poll_next(cx)
                {
                    polled += 1;
                    match res {
                        Some(ActorMessage::Ref(msg)) => {
                            this.add_concurrent(msg);
                        }
                        Some(ActorMessage::Mut(msg)) => {
                            this.add_exclusive(msg);
                            return self.poll_running(cx);
                        }
                        // stream is either canceled by ContextJoinHandle or finished.
                        None => {
                            this.stream_cache.get_mut().swap_remove(i);
                            break 'stream;
                        }
                        _ => unreachable!(),
                    }

                    // force to yield when having 16 consecutive successful poll.
                    if polled == 16 {
                        // set flag to true when force yield happens.
                        // this is to reduce the overhead of multiple streams that enter
                        // this branch and all call for wake up.
                        extra_wake = true;
                        break 'stream;
                    }
                }

                i += 1;
            }

            if extra_wake {
                cx.waker().wake_by_ref();
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
        match this.cache_mut.as_mut() {
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.cache_mut.clear();
                this.act_state.set(ActorState::Running);
                this.state = ContextState::Running;
                self.poll_running(cx)
            }
            None => {
                let ctx = Context::new(
                    &this.act_state,
                    &this.future_cache,
                    &this.stream_cache,
                    &this.act_rx,
                );

                // SAFETY:
                // Self reference is needed.
                // on_start transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { transmute(this.act.on_start(ctx)) };

                this.cache_mut.add_task(task);

                self.poll_start(cx)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.cache_mut.as_mut() {
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.cache_mut.clear();
                Poll::Ready(())
            }
            None => {
                let ctx = Context::new(
                    &this.act_state,
                    &this.future_cache,
                    &this.stream_cache,
                    &this.act_rx,
                );

                // SAFETY:
                // Self reference is needed.
                // on_stop transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { transmute(this.act.on_stop(ctx)) };

                this.cache_mut.add_task(task);

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
