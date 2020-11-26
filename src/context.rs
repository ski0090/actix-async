use core::cell::{Cell, RefCell};
use core::ops::Deref;
use core::pin::Pin;
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
use crate::util::channel::{OneshotSender, Receiver, TryRecvError};
use crate::util::futures::{cancelable, join, next, JoinFutures, LocalBoxedFuture, Stream};
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

    async fn try_handle_concurrent(
        &mut self,
        actor: &A,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut JoinFutures,
    ) {
        if !cache_ref.is_empty() {
            cache_ref.iter_mut().for_each(|m| {
                /*
                    SAFETY:
                    `JoinFutures` is a type alias for `Vec<LocalBoxedFuture<'static, ()>>`.

                    It can not tie to actor and context's lifetime. The reason is that it has no
                    idea the futures are all resolved in this scope and would assume the boxed
                    futures would live as long as the actor and context. Making it impossible to
                    mutably borrow them from this point forward.

                    All futures transmuted to static lifetime must resolved before exiting
                    try_handle_concurrent method.
                */
                fut.push(unsafe { core::mem::transmute(m.handle(actor, self)) });
            });

            // join would poll all futures and only resolve when JoinFutures is empty.
            join(fut).await;

            // clear the cache as they are all finished.
            cache_ref.clear();
        }
    }

    async fn handle_exclusive(
        &mut self,
        msg: MessageObject<A>,
        actor: &mut A,
        cache_mut: &mut Option<MessageObject<A>>,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut JoinFutures,
    ) {
        // put message in cache in case thread panic before it's handled
        *cache_mut = Some(msg);
        // try handle concurrent messages first.
        self.try_handle_concurrent(&*actor, cache_ref, fut).await;
        // pop the cache and handle
        cache_mut.take().unwrap().handle_wait(actor, self).await;
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

    // handle single message and return true if context is force stopping.
    async fn handle_message(
        &mut self,
        msg: ActorMessage<A>,
        actor: &mut A,
        cache_mut: &mut Option<MessageObject<A>>,
        cache_ref: &mut Vec<MessageObject<A>>,
        drop_notify: &mut Option<OneshotSender<()>>,
        fut: &mut JoinFutures,
    ) -> bool {
        match msg {
            ActorMessage::Ref(msg) => cache_ref.push(msg),
            ActorMessage::Mut(msg) => {
                self.handle_exclusive(msg, actor, cache_mut, cache_ref, fut)
                    .await
            }
            ActorMessage::DelayToken(token) => {
                let queue = self.delay_queue.get_mut();

                if queue.contains(token) {
                    let msg = queue.remove(token);
                    self.handle_queued_message(msg, actor, cache_mut, cache_ref, fut)
                        .await;
                }
            }
            ActorMessage::IntervalToken(token) => {
                let msg = match self.interval_queue.get_mut().get(token) {
                    Some(msg) => msg.clone_message(),
                    None => return false,
                };
                self.handle_queued_message(msg, actor, cache_mut, cache_ref, fut)
                    .await;
            }
            ActorMessage::DelayTokenCancel(token) => self.handle_delay_cancel(token),
            ActorMessage::IntervalTokenCancel(token) => self.handle_interval_cancel(token),
            ActorMessage::ActorState(state, notify) => {
                *drop_notify = notify;
                if state != ActorState::Running {
                    self.stop();
                };

                return state == ActorState::Stop;
            }
        };

        false
    }

    async fn handle_queued_message(
        &mut self,
        msg: ActorMessage<A>,
        actor: &mut A,
        cache_mut: &mut Option<MessageObject<A>>,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut JoinFutures,
    ) {
        match msg {
            ActorMessage::Ref(msg) => cache_ref.push(msg),
            ActorMessage::Mut(msg) => {
                self.handle_exclusive(msg, actor, cache_mut, cache_ref, fut)
                    .await
            }
            _ => unreachable!("Queued message can only be ActorMessage::Ref or ActorMessage::Mut"),
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
    cache_mut: Option<MessageObject<A>>,
    cache_ref: Vec<MessageObject<A>>,
    drop_notify: Option<OneshotSender<()>>,
}

impl<A: Actor> Drop for ContextWithActor<A> {
    fn drop(&mut self) {
        // // recovery from thread panic.
        // if self.ctx.as_ref().unwrap().state.get() == ActorState::Running {
        //     let mut ctx = core::mem::take(self);
        //     // some of the cached message object may gone. remove them.
        //     ctx.cache_ref.retain(|m| !m.is_taken());
        //
        //     A::spawn(async move {
        //         let _ = ctx.run().await;
        //     });
        // } else if let Some(tx) = self.drop_notify.take() {
        //     let _ = tx.send(());
        // }
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
        let actor = &mut self.actor;
        let ctx = &mut self.ctx;
        let cache_mut = &mut self.cache_mut;
        let cache_ref = &mut self.cache_ref;
        let drop_notify = &mut self.drop_notify;

        let fut = &mut JoinFutures::with_capacity(CHANNEL_CAP);

        // if there is cached message it must be dealt with
        ctx.try_handle_concurrent(&*actor, cache_ref, fut).await;

        if let Some(mut msg) = cache_mut.take() {
            msg.handle_wait(actor, ctx).await;
        }

        // batch receive new messages from channel.
        loop {
            match ctx.rx.try_recv() {
                Ok(msg) => {
                    let is_force_stop = ctx
                        .handle_message(msg, actor, cache_mut, cache_ref, drop_notify, fut)
                        .await;

                    if is_force_stop {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {
                    // channel is empty. try to handle concurrent messages from previous iters.
                    ctx.try_handle_concurrent(actor, cache_ref, fut).await;

                    // block the task and recv one message when channel is empty.
                    match ctx.rx.recv().await {
                        Ok(msg) => {
                            let is_force_stop = ctx
                                .handle_message(msg, actor, cache_mut, cache_ref, drop_notify, fut)
                                .await;

                            if is_force_stop {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                Err(TryRecvError::Closed) => {
                    // channel is closed. stop the context.
                    ctx.stop();
                    // try to handle concurrent messages from previous iters.
                    ctx.try_handle_concurrent(&*actor, cache_ref, fut).await;

                    break;
                }
            };
        }

        actor.on_stop(ctx).await;
    }
}
