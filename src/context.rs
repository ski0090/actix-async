use core::cell::{Cell, RefCell};
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::time::Duration;

use std::collections::VecDeque;

use futures_util::stream::{Stream, StreamExt};
use slab::Slab;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

use crate::actor::{Actor, ActorState, CHANNEL_CAP};
use crate::address::{Addr, WeakAddr};
use crate::handler::Handler;
use crate::message::{
    ActorMessage, FunctionMessage, FunctionMutMessage, IntervalMessage, Message, MessageObject,
};
use crate::runtime::RuntimeService;

pub struct Context<A: Actor> {
    state: Cell<ActorState>,
    queue: VecDeque<ActorMessage<A>>,
    interval_queue: RefCell<Slab<IntervalMessage<A>>>,
    delay_queue: RefCell<Slab<ActorMessage<A>>>,
    tx: WeakAddr<A>,
}

/// a join handle can be used to cancel a spawned async task like interval closure and stream
/// handler
pub struct ContextJoinHandle {
    handle: oneshot::Sender<()>,
}

impl ContextJoinHandle {
    pub fn cancel(self) {
        let _ = self.handle.send(());
    }
}

impl<A: Actor> Context<A> {
    pub(crate) fn new(tx: WeakAddr<A>) -> Self {
        Context {
            state: Cell::new(ActorState::Stop),
            queue: VecDeque::with_capacity(CHANNEL_CAP),
            interval_queue: RefCell::new(Slab::with_capacity(8)),
            delay_queue: RefCell::new(Slab::with_capacity(CHANNEL_CAP)),
            tx,
        }
    }

    /// run interval concurrent closure on context. `Handler::handle` will be called.
    pub fn run_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> Pin<Box<dyn Future<Output = ()> + 'a>>
            + Clone
            + 'static,
    {
        let msg = FunctionMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Ref(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run interval exclusive closure on context. `Handler::handle_wait` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> Pin<Box<dyn Future<Output = ()> + 'a>>
            + Clone
            + 'static,
    {
        let msg = FunctionMutMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Mut(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run concurrent closure on context after given duration. `Handler::handle` will be called.
    pub fn run_later<F>(&self, dur: Duration, f: F)
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> Pin<Box<dyn Future<Output = ()> + 'a>>
            + 'static,
    {
        let msg = FunctionMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Ref(msg));
    }

    /// run exclusive closure on context after given duration. `Handler::handle_wait` will be
    /// called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_later<F>(&self, dur: Duration, f: F)
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> Pin<Box<dyn Future<Output = ()> + 'a>>
            + 'static,
    {
        let msg = FunctionMutMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Mut(msg));
    }

    /// stop the context. It would end the actor gracefully by draining all remaining message in
    /// queue.
    ///
    /// *. It DOES NOT drain the channel.
    pub fn stop(&self) {
        self.state.set(ActorState::StopGraceful);
    }

    /// get the address of actor from context.
    pub fn address(&self) -> Option<Addr<A>> {
        self.tx.upgrade()
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
    ///
    /// impl Actor for StreamActor {
    ///     type Runtime = ActixRuntime;
    /// }
    ///
    /// struct StreamMessage;
    ///
    /// impl Message for StreamMessage {
    ///     type Result = ();
    /// }
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
    pub fn add_stream<S, M>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
    {
        self.stream(stream, ActorMessage::Ref)
    }

    /// add a stream to context. multiple stream can be added to one context.
    ///
    /// stream item will be treated as exclusve message and `Handler::handle_wait` will be called.
    pub fn add_wait_stream<S, M>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
    {
        self.stream(stream, ActorMessage::Mut)
    }

    fn stream<S, M, F>(&self, stream: S, f: F) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + Copy + 'static,
    {
        let tx = self.tx.clone();
        let (tx2, mut rx2) = oneshot::channel();

        A::Runtime::spawn(async move {
            tokio::pin!(stream);
            loop {
                tokio::select! {
                    rx2 = (&mut rx2) => {
                        if rx2.is_ok() {
                            return;
                        }
                    }
                    item = stream.next() => {
                        if let Some(msg) = item {
                            if let Some(tx) = tx.upgrade() {
                                let msg = MessageObject::new(msg, None);
                                if tx.deref().send(f(msg)).await.is_ok() {
                                    continue;
                                }
                            }
                        }
                        return;
                    }
                }
            }
        });

        ContextJoinHandle { handle: tx2 }
    }

    fn interval(&self, dur: Duration, msg: IntervalMessage<A>) -> ContextJoinHandle {
        let token = self.interval_queue.borrow_mut().insert(msg);

        let tx = self.tx.clone();
        let (tx2, mut rx2) = oneshot::channel();

        A::Runtime::spawn(async move {
            let mut sleep = A::Runtime::sleep(dur);
            loop {
                tokio::select! {
                    rx2 = (&mut rx2) => {
                        if rx2.is_ok() {
                            if let Some(tx) = tx.upgrade() {
                                let _ = tx.deref().send(ActorMessage::IntervalTokenCancel(token)).await;
                            }
                            return;
                        }
                    }
                    _ = (&mut sleep) => {
                        match tx.upgrade() {
                            Some(tx) if tx.deref().send(ActorMessage::IntervalToken(token)).await.is_ok() => {
                                sleep = A::Runtime::sleep(dur);
                                continue;
                            },
                            _ => return
                        }
                    }
                }
            }
        });

        ContextJoinHandle { handle: tx2 }
    }

    fn later(&self, dur: Duration, msg: ActorMessage<A>) {
        let token = self.delay_queue.borrow_mut().insert(msg);
        let tx = self.tx.clone();
        A::Runtime::spawn(async move {
            A::Runtime::sleep(dur).await;
            if let Some(tx) = tx.upgrade() {
                let _ = tx.deref().send(ActorMessage::DelayToken(token)).await;
            }
        })
    }

    // return true to notify the outer loop to continue
    fn add_message(
        &mut self,
        msg: ActorMessage<A>,
        drop_notify: &mut Option<oneshot::Sender<()>>,
    ) -> bool {
        match msg {
            ActorMessage::ActorState(state, notify) => {
                let should_continue =
                    state == ActorState::Stop || state == ActorState::StopGraceful;
                self.state.set(state);
                *drop_notify = notify;
                return should_continue;
            }
            _ => self.queue.push_back(msg),
        }
        false
    }

    async fn handle_concurrent(&self, actor: &A, cache_ref: &mut Vec<MessageObject<A>>) {
        if !cache_ref.is_empty() {
            let map = cache_ref.iter_mut().map(|m| async move {
                m.handle(actor, self).await;
                // set message object to finish state.
                m.set_finished();
            });
            let _ = futures_util::future::join_all(map).await;

            // clear the cache as they are all finished.
            cache_ref.clear();
        }
    }

    fn handle_delay(&mut self, token: usize) {
        let msg = self.delay_queue.borrow_mut().remove(token);
        self.queue.push_front(msg);
    }

    fn handle_interval(&mut self, token: usize) {
        if let Some(msg) = self.interval_queue.borrow().get(token) {
            self.queue.push_front(msg.clone_actor_message());
        }
    }

    fn handle_interval_cancel(&self, token: usize) {
        self.interval_queue.borrow_mut().remove(token);
    }
}

pub(crate) struct ContextWithActor<A: Actor> {
    ctx: Option<Context<A>>,
    actor: Option<A>,
    rx: Option<Receiver<ActorMessage<A>>>,
    cache_mut: Option<MessageObject<A>>,
    cache_ref: Vec<MessageObject<A>>,
    drop_notify: Option<oneshot::Sender<()>>,
}

impl<A: Actor> Default for ContextWithActor<A> {
    fn default() -> Self {
        Self {
            ctx: None,
            actor: None,
            rx: None,
            cache_mut: None,
            cache_ref: Vec::new(),
            drop_notify: None,
        }
    }
}

impl<A: Actor> Drop for ContextWithActor<A> {
    fn drop(&mut self) {
        if std::thread::panicking() && self.ctx.as_ref().unwrap().state.get() == ActorState::Running
        {
            let mut ctx = std::mem::take(self);
            // some of the cached message object may finished already. remove them.
            ctx.cache_ref.retain(|m| !m.is_finished());

            A::Runtime::spawn(async move {
                let _ = ctx.run().await;
            });
        } else if let Some(tx) = self.drop_notify.take() {
            let _ = tx.send(());
        }
    }
}

impl<A: Actor> ContextWithActor<A> {
    pub(crate) fn new(actor: A, rx: Receiver<ActorMessage<A>>, ctx: Context<A>) -> Self {
        Self {
            actor: Some(actor),
            rx: Some(rx),
            ctx: Some(ctx),
            cache_mut: None,
            cache_ref: Vec::with_capacity(CHANNEL_CAP),
            drop_notify: None,
        }
    }

    pub(crate) async fn first_run(&mut self) {
        let actor = self.actor.as_mut().unwrap();
        let ctx = self.ctx.as_mut().unwrap();

        actor.on_start(ctx).await;
        ctx.state.set(ActorState::Running);

        self.run().await;
    }

    async fn run(&mut self) {
        let actor = self.actor.as_mut().unwrap();
        let ctx = self.ctx.as_mut().unwrap();
        let recv = self.rx.as_mut().unwrap();
        let cache_mut = &mut self.cache_mut;
        let cache_ref = &mut self.cache_ref;
        let drop_notify = &mut self.drop_notify;

        // if there is cached message it must be dealt with
        ctx.handle_concurrent(&*actor, cache_ref).await;

        if let Some(mut msg) = cache_mut.take() {
            msg.handle_wait(actor, ctx).await;
        }

        'ctx: loop {
            if ctx.state.get() == ActorState::Stop {
                break 'ctx;
            }

            'msg: loop {
                match ctx.queue.pop_front() {
                    // have exclusive messages.
                    Some(ActorMessage::Mut(msg)) => {
                        // put message in cache in case thread panic before it's handled
                        *cache_mut = Some(msg);
                        // try handle concurrent messages first.
                        ctx.handle_concurrent(&*actor, cache_ref).await;

                        // pop the cache and handle
                        let mut msg = cache_mut.take().unwrap();
                        msg.handle_wait(actor, ctx).await;
                    }
                    // have concurrent message.
                    Some(ActorMessage::Ref(msg)) => cache_ref.push(msg),
                    Some(ActorMessage::DelayToken(token)) => ctx.handle_delay(token),
                    Some(ActorMessage::IntervalToken(token)) => ctx.handle_interval(token),
                    Some(ActorMessage::IntervalTokenCancel(token)) => {
                        ctx.handle_interval_cancel(token)
                    }
                    None => {
                        // try handle concurrent messages before break.
                        ctx.handle_concurrent(&*actor, cache_ref).await;
                        break 'msg;
                    }
                    _ => unreachable!("Wrong variant of ActorMessage added to context queue."),
                }
            }

            if ctx.state.get() == ActorState::StopGraceful {
                break 'ctx;
            }

            // batch receive new messages.
            're: loop {
                match recv.try_recv() {
                    Ok(msg) => {
                        if ctx.add_message(msg, drop_notify) {
                            continue 'ctx;
                        }
                    }
                    Err(TryRecvError::Empty) => break 're,
                    Err(TryRecvError::Closed) => ctx.state.set(ActorState::StopGraceful),
                }
            }

            if ctx.queue.is_empty() {
                match recv.recv().await {
                    Some(msg) => {
                        ctx.add_message(msg, drop_notify);
                    }
                    None => ctx.state.set(ActorState::StopGraceful),
                }
            }
        }

        actor.on_stop(ctx).await;
    }
}
