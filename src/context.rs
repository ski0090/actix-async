use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::actor::{Actor, ActorState, CHANNEL_CAP};
use crate::address::Addr;
use crate::handler::Handler;
use crate::message::{
    ActorMessage, ActorMessageClone, FunctionMessage, FunctionMutMessage, FutureMessage,
    IntervalMessage, Message, MessageObject, StreamContainer, StreamMessage,
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
            let msg = FunctionMessage::new(f);
            let msg = MessageObject::new(msg, None);
            FutureMessage::new(dur, rx, ActorMessage::Ref(msg))
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
            let msg = FunctionMutMessage::new(f);
            let msg = MessageObject::new(msg, None);
            FutureMessage::new(dur, rx, ActorMessage::Mut(msg))
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
    ///     async fn handle(&self, _: StreamMessage, _: &Context<Self>) {}
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
        self.stream(stream, ActorMessage::Ref)
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
        self.stream(stream, ActorMessage::Mut)
    }

    fn stream<S, F>(&self, stream: S, f: F) -> ContextJoinHandle
    where
        S: Stream + 'static,
        S::Item: Message + 'static,
        A: Handler<S::Item>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + Copy + 'static,
    {
        let (handle, rx) = oneshot();
        let stream = StreamContainer::new(stream, rx, f);
        let msg = StreamMessage::new_boxed(stream);
        self.stream_message.borrow_mut().push(msg);
        ContextJoinHandle { handle }
    }

    fn is_running(&self) -> bool {
        self.state.get() == ActorState::Running
    }
}

type Task = LocalBoxFuture<'static, ()>;

pub(crate) struct CacheRef(Vec<Task>);

impl CacheRef {
    fn new() -> Self {
        Self(Vec::with_capacity(CHANNEL_CAP))
    }

    fn poll_unpin(&mut self, cx: &mut StdContext<'_>) {
        // poll concurrent messages
        let mut i = 0;
        while i < self.0.len() {
            match self.0[i].as_mut().poll(cx) {
                Poll::Ready(_) => {
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

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn add_concurrent<A: Actor>(&mut self, mut msg: MessageObject<A>, act: &A, ctx: &Context<A>) {
        self.0.push(msg.handle(act, ctx));
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

pub(crate) struct CacheMut(Option<Task>);

impl CacheMut {
    fn new() -> Self {
        Self(None)
    }

    fn get_mut(&mut self) -> Option<&mut Task> {
        self.0.as_mut()
    }

    fn clear(&mut self) {
        self.0 = None;
    }

    fn add_exclusive<A: Actor>(
        &mut self,
        mut msg: MessageObject<A>,
        act: &mut A,
        ctx: &mut Context<A>,
    ) {
        self.0 = Some(msg.handle_wait(act, ctx));
    }

    fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

pin_project_lite::pin_project! {
    #[project = ContextProj]
    pub(crate) enum ContextWithActor<A: Actor> {
        Starting {
            act: Option<A>,
            ctx: Option<Context<A>>,
            on_start: Option<Task>,
        },
        Running {
            act: Option<A>,
            ctx: Option<Context<A>>,
            cache_mut: CacheMut,
            cache_ref: CacheRef,
            future_cache: Vec<FutureMessage<A>>,
            stream_cache: Vec<StreamMessage<A>>,
            drop_notify: Option<OneshotSender<()>>,
        },
        Stopping {
            act: A,
            ctx: Context<A>,
            on_stop: Option<Task>,
            drop_notify: Option<OneshotSender<()>>,
        },
    }
}

impl<A: Actor> ContextWithActor<A> {
    pub(crate) fn new_starting(act: A, ctx: Context<A>) -> Self {
        Self::Starting {
            ctx: Some(ctx),
            act: Some(act),
            on_start: None,
        }
    }

    fn new_running(act: Option<A>, ctx: Option<Context<A>>) -> Self {
        Self::Running {
            act,
            ctx,
            cache_ref: CacheRef::new(),
            cache_mut: CacheMut::new(),
            future_cache: Vec::with_capacity(8),
            stream_cache: Vec::with_capacity(8),
            drop_notify: None,
        }
    }

    fn new_stopping(act: A, ctx: Context<A>, drop_notify: Option<OneshotSender<()>>) -> Self {
        Self::Stopping {
            act,
            ctx,
            on_stop: None,
            drop_notify,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        match self {
            ContextWithActor::Starting { ctx, .. } => {
                ctx.as_ref().map(|ctx| ctx.is_running()).unwrap_or(false)
            }
            ContextWithActor::Running { ctx, .. } => {
                ctx.as_ref().map(|ctx| ctx.is_running()).unwrap_or(false)
            }
            ContextWithActor::Stopping { ctx, .. } => ctx.is_running(),
        }
    }

    pub(crate) fn clear_cache(&mut self) {
        if let ContextWithActor::Running {
            cache_ref,
            cache_mut,
            stream_cache,
            ..
        } = self
        {
            cache_mut.clear();
            cache_ref.clear();
            stream_cache.clear();
        }
    }
}

impl<A: Actor> Future for ContextWithActor<A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match self.as_mut().project() {
            ContextProj::Starting { act, ctx, on_start } => match on_start {
                Some(fut) => match fut.as_mut().poll(cx) {
                    Poll::Ready(_) => {
                        *on_start = None;
                        let mut ctx = ctx.take();
                        let act = act.take();
                        ctx.as_mut().unwrap().state.set(ActorState::Running);

                        self.as_mut().set(ContextWithActor::new_running(act, ctx));
                        self.poll(cx)
                    }
                    Poll::Pending => Poll::Pending,
                },
                None => {
                    let act = act.as_mut().unwrap();
                    let ctx = ctx.as_mut().unwrap();

                    // SAFETY:
                    //
                    // Self reference is needed.
                    // on_start transmute to static lifetime must be resolved before dropping or
                    // move Context and Actor.
                    let fut = unsafe { core::mem::transmute(act.on_start(ctx)) };

                    *on_start = Some(fut);
                    self.poll(cx)
                }
            },
            ContextProj::Running {
                cache_ref,
                cache_mut,
                future_cache,
                stream_cache,
                act,
                ctx,
                drop_notify,
            } => {
                cache_ref.poll_unpin(cx);

                // try to poll exclusive message.
                if let Some(fut_mut) = cache_mut.get_mut() {
                    // still have concurrent messages. finish them.
                    if !cache_ref.is_empty() {
                        return Poll::Pending;
                    }

                    // poll exclusive message and remove it when success.
                    match fut_mut.as_mut().poll(cx) {
                        Poll::Ready(_) => cache_mut.clear(),
                        Poll::Pending => return Poll::Pending,
                    }
                }

                // flag indicate if return pending or poll an extra round.
                let mut new = false;

                // If context is stopped we stop dealing with delay and interval messages.
                if ctx.as_ref().unwrap().is_running() {
                    {
                        let ctx = ctx.as_mut().unwrap();
                        future_cache.merge(ctx);
                        stream_cache.merge(ctx);
                    }

                    // poll delay messages
                    let mut i = 0;
                    while i < future_cache.len() {
                        match Pin::new(&mut future_cache[i]).poll(cx) {
                            Poll::Ready(msg) => {
                                // SAFETY:
                                // Vec::swap_remove with no len check and drop of removed element in
                                // place.
                                // i is guaranteed to be smaller than delay_cache.len()
                                unsafe {
                                    future_cache.swap_remove_uncheck(i);
                                }

                                // msg.is_none() means it's canceled by ContextJoinHandle.
                                if let Some(msg) = msg {
                                    match msg {
                                        ActorMessage::Ref(msg) => {
                                            new = true;
                                            cache_ref.add_concurrent(
                                                msg,
                                                act.as_ref().unwrap(),
                                                ctx.as_ref().unwrap(),
                                            );
                                        }
                                        ActorMessage::Mut(msg) => {
                                            cache_mut.add_exclusive(
                                                msg,
                                                act.as_mut().unwrap(),
                                                ctx.as_mut().unwrap(),
                                            );
                                            return self.poll(cx);
                                        }
                                        _ => unreachable!(),
                                    }
                                }
                            }
                            Poll::Pending => i += 1,
                        }
                    }

                    // poll interval message.
                    let mut i = 0;
                    while i < stream_cache.len() {
                        match Pin::new(&mut stream_cache[i]).poll_next(cx) {
                            Poll::Ready(Some(msg)) => match msg {
                                ActorMessage::Ref(msg) => {
                                    new = true;
                                    cache_ref.add_concurrent(
                                        msg,
                                        act.as_ref().unwrap(),
                                        ctx.as_ref().unwrap(),
                                    );
                                }
                                ActorMessage::Mut(msg) => {
                                    cache_mut.add_exclusive(
                                        msg,
                                        act.as_mut().unwrap(),
                                        ctx.as_mut().unwrap(),
                                    );
                                    return self.poll(cx);
                                }
                                _ => unreachable!(),
                            },
                            // interval message is canceled by ContextJoinHandle
                            Poll::Ready(None) => {
                                // SAFETY:
                                // Vec::swap_remove with no len check and drop of removed element in
                                // place.
                                // i is guaranteed to be smaller than interval_cache.len()
                                unsafe {
                                    stream_cache.swap_remove_uncheck(i);
                                }
                            }
                            Poll::Pending => i += 1,
                        }
                    }
                }

                // actively drain receiver channel for incoming messages.
                loop {
                    match Pin::new(&mut ctx.as_mut().unwrap().rx).poll_next(cx) {
                        // new concurrent message. add it to cache_ref and continue.
                        Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                            new = true;
                            cache_ref.add_concurrent(
                                msg,
                                act.as_ref().unwrap(),
                                ctx.as_ref().unwrap(),
                            );
                        }
                        // new exclusive message. add it to cache_mut. No new messages should be accepted
                        // until this one is resolved.
                        Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                            cache_mut.add_exclusive(
                                msg,
                                act.as_mut().unwrap(),
                                ctx.as_mut().unwrap(),
                            );
                            return self.poll(cx);
                        }
                        Poll::Ready(Some(ActorMessage::ActorState(state, notify))) => {
                            *drop_notify = notify;

                            match state {
                                ActorState::StopGraceful => {
                                    ctx.as_mut().unwrap().stop();
                                }
                                ActorState::Stop => {
                                    let ctx = ctx.take().unwrap();
                                    ctx.stop();
                                    let act = act.take().unwrap();
                                    let drop_notify = drop_notify.take();
                                    self.as_mut().set(ContextWithActor::new_stopping(
                                        act,
                                        ctx,
                                        drop_notify,
                                    ));
                                    return self.poll(cx);
                                }
                                _ => {}
                            }
                        }
                        Poll::Ready(None) => {
                            ctx.as_mut().unwrap().stop();
                            return if new {
                                self.poll(cx)
                            } else if !cache_ref.is_empty() || cache_mut.is_some() {
                                Poll::Pending
                            } else {
                                let ctx = ctx.take().unwrap();
                                let act = act.take().unwrap();
                                let drop_notify = drop_notify.take();
                                self.as_mut().set(ContextWithActor::Stopping {
                                    ctx,
                                    act,
                                    on_stop: None,
                                    drop_notify,
                                });
                                self.poll(cx)
                            };
                        }
                        // if we have new concurrent messages then run an extra poll.
                        Poll::Pending => return if new { self.poll(cx) } else { Poll::Pending },
                    }
                }
            }
            ContextProj::Stopping {
                act,
                ctx,
                on_stop,
                drop_notify,
            } => match on_stop {
                Some(ref mut fut) => match fut.as_mut().poll(cx) {
                    Poll::Ready(_) => {
                        *on_stop = None;
                        if let Some(tx) = drop_notify.take() {
                            let _ = tx.send(());
                        }
                        Poll::Ready(())
                    }
                    Poll::Pending => Poll::Pending,
                },
                None => {
                    // SAFETY:
                    //
                    // Self reference is needed.
                    // on_stop transmute to static lifetime must be resolved before dropping or
                    // move Context and Actor.
                    let fut = unsafe { core::mem::transmute(act.on_stop(ctx)) };

                    *on_stop = Some(fut);
                    self.poll(cx)
                }
            },
        }
    }
}

// merge Context with ContextWithActor
trait MergeContext<A: Actor> {
    fn merge(&mut self, ctx: &mut Context<A>);
}

impl<A: Actor> MergeContext<A> for Vec<StreamMessage<A>> {
    fn merge(&mut self, ctx: &mut Context<A>) {
        let delay = ctx.stream_message.get_mut();
        while let Some(delay) = delay.pop() {
            self.push(delay);
        }
    }
}

impl<A: Actor> MergeContext<A> for Vec<FutureMessage<A>> {
    fn merge(&mut self, ctx: &mut Context<A>) {
        let delay = ctx.future_message.get_mut();
        while let Some(delay) = delay.pop() {
            self.push(delay);
        }
    }
}

// swap remove with index. do not check for overflow.
trait SwapRemoveUncheck {
    unsafe fn swap_remove_uncheck(&mut self, index: usize);
}

impl<T> SwapRemoveUncheck for Vec<T> {
    unsafe fn swap_remove_uncheck(&mut self, i: usize) {
        let len = self.len();
        let mut last = core::ptr::read(self.as_ptr().add(len - 1));
        let hole = self.as_mut_ptr().add(i);
        self.set_len(len - 1);
        core::mem::swap(&mut *hole, &mut last);
    }
}
