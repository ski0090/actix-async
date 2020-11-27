use core::cell::{Cell, RefCell};
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::actor::{Actor, ActorState, CHANNEL_CAP};
use crate::address::Addr;
use crate::error::ActixAsyncError;
use crate::handler::Handler;
use crate::message::{
    ActorMessage, FunctionMessage, FunctionMutMessage, IntervalMessage, Message, MessageObject,
};
use crate::util::channel::{OneshotSender, Receiver};
use crate::util::futures::{cancelable, next, LocalBoxedFuture, Stream};
use crate::util::slab::Slab;

/// Context type of `Actor` type. Can be accessed within `Handler::handle` and
/// `Handler::handle_wait` method.
///
/// Used to mutate the state of actor and add additional tasks to actor.
pub struct Context<A> {
    state: Cell<ActorState>,
    interval_queue: RefCell<Slab<IntervalMessage<A>>>,
    delay_queue: RefCell<Slab<ActorMessage<A>>>,
    rx: Receiver<ActorMessage<A>>,
}

/// a join handle can be used to cancel a spawned async task like interval closure and stream
/// handler
pub struct ContextJoinHandle {
    handle: OneshotSender<()>,
}

impl ContextJoinHandle {
    /// cancel the task added to context associate to this handle. would consume self.
    pub fn cancel(self) {
        let _ = self.handle.send(());
    }
}

impl<A: Actor> Context<A> {
    pub(crate) fn new(rx: Receiver<ActorMessage<A>>) -> Self {
        Context {
            state: Cell::new(ActorState::Stop),
            interval_queue: RefCell::new(Slab::with_capacity(8)),
            delay_queue: RefCell::new(Slab::with_capacity(CHANNEL_CAP)),
            rx,
        }
    }

    /// run interval concurrent closure on context. `Handler::handle` will be called.
    pub fn run_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, ()> + Clone + 'static,
    {
        let msg = FunctionMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Ref(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run interval exclusive closure on context. `Handler::handle_wait` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, ()>
            + Clone
            + 'static,
    {
        let msg = FunctionMutMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Mut(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run concurrent closure on context after given duration. `Handler::handle` will be called.
    pub fn run_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, ()> + 'static,
    {
        let msg = FunctionMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Ref(msg))
    }

    /// run exclusive closure on context after given duration. `Handler::handle_wait` will be
    /// called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, ()> + 'static,
    {
        let msg = FunctionMutMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Mut(msg))
    }

    /// stop the context. It would end the actor gracefully by close the channel draining all
    /// remaining messages.
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
    /// stream item will be treated as exclusve message and `Handler::handle_wait` will be called.
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
        let rx = self.rx.clone();

        let fut = async move {
            let mut stream = stream;
            // SAFETY:
            // stream is owned by async task and never moved. The loop would borrow pinned stream
            // with `Next`.
            let mut stream = unsafe { Pin::new_unchecked(&mut stream) };
            while let Some(msg) = next(&mut stream).await {
                let msg = MessageObject::new(msg, None);
                let msg = f(msg);
                if Self::send_with_rx(&rx, msg).await.is_err() {
                    return;
                }
            }
        };

        let (fut, handle) = cancelable(fut, async {});

        A::spawn(fut);

        ContextJoinHandle { handle }
    }

    fn interval(&self, dur: Duration, msg: IntervalMessage<A>) -> ContextJoinHandle {
        let token = self.interval_queue.borrow_mut().insert(msg);

        let rx = self.rx.clone();
        let rx1 = self.rx.clone();

        let fut = async move {
            loop {
                A::sleep(dur).await;
                if Self::send_with_rx(&rx, ActorMessage::IntervalToken(token))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        };

        let on_cancel = async move {
            let _ = Self::send_with_rx(&rx1, ActorMessage::IntervalTokenCancel(token)).await;
        };

        let (fut, handle) = cancelable(fut, on_cancel);

        A::spawn(fut);

        ContextJoinHandle { handle }
    }

    fn later(&self, dur: Duration, msg: ActorMessage<A>) -> ContextJoinHandle {
        let token = self.delay_queue.borrow_mut().insert(msg);

        let rx = self.rx.clone();
        let rx1 = self.rx.clone();

        let fut = async move {
            A::sleep(dur).await;
            let _ = Self::send_with_rx(&rx, ActorMessage::DelayToken(token)).await;
        };

        let on_cancel = async move {
            let _ = Self::send_with_rx(&rx1, ActorMessage::DelayTokenCancel(token)).await;
        };

        let (fut, handle) = cancelable(fut, on_cancel);

        A::spawn(fut);

        ContextJoinHandle { handle }
    }

    fn handle_delay_cancel(&mut self, token: usize) {
        let queue = self.delay_queue.get_mut();
        if queue.contains(token) {
            queue.remove(token);
        }
    }

    fn handle_interval_cancel(&mut self, token: usize) {
        let queue = self.interval_queue.get_mut();
        if queue.contains(token) {
            queue.remove(token);
        }
    }

    async fn send_with_rx(
        rx: &Receiver<ActorMessage<A>>,
        msg: ActorMessage<A>,
    ) -> Result<(), ActixAsyncError> {
        Addr::from_recv(rx)?.deref().send(msg).await
    }
}

pub(crate) struct ContextWithActor<A: Actor> {
    ctx: Context<A>,
    actor: A,
    cache_mut: Option<LocalBoxedFuture<'static, ()>>,
    cache_ref: Vec<LocalBoxedFuture<'static, ()>>,
    drop_notify: Option<OneshotSender<()>>,
}

impl<A: Actor> Unpin for ContextWithActor<A> {}

impl<A: Actor> Drop for ContextWithActor<A> {
    fn drop(&mut self) {
        if let Some(tx) = self.drop_notify.take() {
            let _ = tx.send(());
        }
    }
}

impl<A: Actor> ContextWithActor<A> {
    pub(crate) fn new(actor: A, ctx: Context<A>) -> Self {
        Self {
            actor,
            ctx,
            cache_mut: None,
            cache_ref: Vec::with_capacity(CHANNEL_CAP),
            drop_notify: None,
        }
    }

    pub(crate) async fn first_run(&mut self) {
        let actor = &mut self.actor;
        let ctx = &mut self.ctx;

        actor.on_start(ctx).await;
        ctx.state.set(ActorState::Running);

        self.run().await;
    }

    async fn run(&mut self) {
        (&mut *self).await;

        {
            let actor = &mut self.actor;
            let ctx = &mut self.ctx;
            actor.on_stop(ctx).await;
        }
    }

    fn add_concurrent_msg(&mut self, mut msg: MessageObject<A>) {
        self.cache_ref.push(msg.handle(&self.actor, &self.ctx));
    }

    fn add_exclusive_msg(&mut self, mut msg: MessageObject<A>) {
        self.cache_mut = Some(msg.handle_wait(&mut self.actor, &mut self.ctx));
    }
}

impl<A: Actor> Future for ContextWithActor<A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        // poll concurrent messages
        let mut i = 0;
        while i < this.cache_ref.len() {
            if this.cache_ref[i].as_mut().poll(cx).is_ready() {
                // SAFETY:
                // Vec::swap_remove with no len check and drop of removed element in place.
                // i is guaranteed to be smaller than this.fut.len()
                unsafe {
                    let len = this.cache_ref.len();
                    let mut last = core::ptr::read(this.cache_ref.as_ptr().add(len - 1));
                    let hole = this.cache_ref.as_mut_ptr().add(i);
                    this.cache_ref.set_len(len - 1);
                    core::mem::swap(&mut *hole, &mut last);
                }
            } else {
                i += 1;
            }
        }

        // try to poll exclusive message.
        if let Some(fut_mut) = this.cache_mut.as_mut() {
            // still have concurrent messages. finish them.
            if !this.cache_ref.is_empty() {
                return Poll::Pending;
            }

            // poll exclusive message and remove it when success.
            match fut_mut.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    this.cache_mut = None;
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // no cache message at this point. can stop gracefully.
        match this.ctx.state.get() {
            ActorState::Running => {}
            ActorState::StopGraceful if !this.cache_ref.is_empty() || this.cache_mut.is_some() => {
                return Poll::Pending;
            }
            ActorState::StopGraceful | ActorState::Stop => return Poll::Ready(()),
        }

        // flag indicate if return pending all poll an extra round.
        let mut new = false;

        // actively drain receiver channel for incoming messages.
        loop {
            match Pin::new(&mut this.ctx.rx).poll_next(cx) {
                // new concurrent message. add it to CacheRef and fut.
                Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                    new = true;
                    this.add_concurrent_msg(msg)
                }
                // new exclusive message. add it to CacheMut. No new messages should be accepted
                // until this one is resolved.
                Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                    this.add_exclusive_msg(msg);
                    return self.poll(cx);
                }
                // context_delay queue is treat the same as receiver.
                // add new concurrent messages or return with new exclusive.
                Poll::Ready(Some(ActorMessage::DelayToken(token))) => {
                    let queue = this.ctx.delay_queue.get_mut();

                    if queue.contains(token) {
                        match queue.remove(token) {
                            ActorMessage::Ref(msg) => {
                                new = true;
                                this.add_concurrent_msg(msg)
                            }
                            ActorMessage::Mut(msg) => {
                                this.add_exclusive_msg(msg);
                                return self.poll(cx);
                            }
                            _ => {}
                        }
                    }
                }
                // similar to delay_queue.
                Poll::Ready(Some(ActorMessage::IntervalToken(token))) => {
                    let msg = match this.ctx.interval_queue.get_mut().get(token) {
                        Some(msg) => msg.clone_message(),
                        None => continue,
                    };
                    match msg {
                        ActorMessage::Ref(msg) => {
                            new = true;
                            this.add_concurrent_msg(msg)
                        }
                        ActorMessage::Mut(msg) => {
                            this.add_exclusive_msg(msg);
                            return self.poll(cx);
                        }
                        _ => {}
                    }
                }
                Poll::Ready(Some(ActorMessage::DelayTokenCancel(token))) => {
                    this.ctx.handle_delay_cancel(token)
                }
                Poll::Ready(Some(ActorMessage::IntervalTokenCancel(token))) => {
                    this.ctx.handle_interval_cancel(token)
                }
                Poll::Ready(Some(ActorMessage::ActorState(state, notify))) => {
                    this.drop_notify = notify;
                    match state {
                        ActorState::StopGraceful => {
                            this.ctx.stop();
                        }
                        ActorState::Stop => {
                            this.ctx.stop();
                            return Poll::Ready(());
                        }
                        _ => (),
                    }
                }
                Poll::Ready(None) => {
                    this.ctx.stop();
                    return self.poll(cx);
                }
                // if we have new concurrent messages then run an extra poll.
                Poll::Pending => return if new { self.poll(cx) } else { Poll::Pending },
            }
        }
    }
}
