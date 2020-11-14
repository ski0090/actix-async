use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::actor::{Actor, ActorState};
use crate::context::Context;
use crate::handler::Handler;
use crate::message::{
    message_send_check, ActorMessage, FunctionMessage, FunctionMutMessage, Message, MessageObject,
};
use crate::runtime::RuntimeService;

pub struct Addr<A: Actor>(Arc<Sender<ActorMessage<A>>>);

impl<A: Actor> Clone for Addr<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A: Actor> Deref for Addr<A> {
    type Target = Sender<ActorMessage<A>>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<A: Actor> Addr<A> {
    /// send a concurrent message to actor. `Handler::handle` will be called for concurrent message
    /// processing.
    pub async fn send<M>(&self, msg: M) -> Result<M::Result, ()>
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._send(msg, ActorMessage::Ref).await
    }

    /// send an exclusive message to actor. `Handler::handle_wait` will be called for exclusive
    /// message processing.
    /// If `Handler::handle_wait` is not override then it would use `Handler::handle` as fallback.
    pub async fn wait<M>(&self, msg: M) -> Result<M::Result, ()>
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._send(msg, ActorMessage::Mut).await
    }

    /// send a concurrent closure to actor. `Handler::handle` will be called for concurrent message
    /// processing.
    /// closure must be `Send` bound.
    pub async fn run<F, R>(&self, func: F) -> Result<R, ()>
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> Pin<Box<dyn Future<Output = R> + 'a>>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let msg = FunctionMessage::new(func);

        self._send(msg, ActorMessage::Ref).await
    }

    /// send a exclusive closure to actor. `Handler::handle_wait` will be called for exclusive
    /// message processing.
    /// If `Handler::handle_wait` is not override then it would use `Handler::handle` as fallback.
    pub async fn run_wait<F, R>(&self, func: F) -> Result<R, ()>
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> Pin<Box<dyn Future<Output = R> + 'a>>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let msg = FunctionMutMessage::new(func);

        self._send(msg, ActorMessage::Mut).await
    }

    /// send a message to actor and ignore the result.
    pub fn do_send<M>(&self, msg: M)
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._do_send(msg, ActorMessage::Ref);
    }

    /// send a exclusive message to actor and ignore the result.
    pub fn do_wait<M>(&self, msg: M)
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._do_send(msg, ActorMessage::Mut);
    }

    /// stop actor
    pub async fn stop(&self, graceful: bool) -> Result<(), ()> {
        let state = if graceful {
            ActorState::StopGraceful
        } else {
            ActorState::Stop
        };

        let (tx, rx) = oneshot::channel();

        self.deref()
            .send(ActorMessage::ActorState(state, Some(tx)))
            .await
            .map_err(|_| ())?;

        rx.await.map_err(|_| ())
    }

    /// weak version of Addr that can be upgraded.
    pub fn downgrade(&self) -> WeakAddr<A> {
        WeakAddr(Arc::downgrade(&self.0))
    }

    /// Recipient bound to message type and not actor.
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        M: Message + Send,
        A: Handler<M>,
    {
        Recipient(Box::new(self.clone()))
    }

    /// weak version of Recipient.
    ///
    /// *. recipient weak will stay usable as long as the actor and it's `Addr` are alive.
    /// It DOES NOT care if a strong Recipient is alive or not.
    pub fn recipient_weak<M>(&self) -> RecipientWeak<M>
    where
        M: Message + Send,
        A: Handler<M>,
    {
        RecipientWeak(Box::new(self.downgrade()))
    }

    pub(crate) fn new(tx: Sender<ActorMessage<A>>) -> Self {
        Self(Arc::new(tx))
    }

    async fn _send<M, F>(&self, msg: M, f: F) -> Result<M::Result, ()>
    where
        M: Message + Send,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A>,
    {
        let (tx, rx) = oneshot::channel();

        message_send_check::<M>();
        let msg = MessageObject::new(msg, Some(tx));

        self.deref().send(f(msg)).await.map_err(|_| ())?;

        rx.await.map_err(|_| ())
    }

    fn _do_send<M, F>(&self, msg: M, f: F)
    where
        M: Message + Send,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + 'static,
    {
        let this = self.clone();
        A::Runtime::spawn(async move {
            message_send_check::<M>();
            let msg = MessageObject::new(msg, None);
            let _ = this.deref().send(f(msg)).await;
        });
    }
}

pub struct WeakAddr<A: Actor>(Weak<Sender<ActorMessage<A>>>);

impl<A: Actor> Clone for WeakAddr<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A: Actor> WeakAddr<A> {
    pub fn upgrade(&self) -> Option<Addr<A>> {
        self.0.upgrade().map(Addr)
    }
}

#[async_trait(?Send)]
trait AddrHandler<M>: Send + Sync + 'static
where
    M: Message + Send,
    Self: Send + Sync + 'static,
{
    async fn send(&self, msg: M) -> Result<M::Result, ()>;

    async fn wait(&self, msg: M) -> Result<M::Result, ()>;
}

#[async_trait(?Send)]
impl<A, M> AddrHandler<M> for Addr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    async fn send(&self, msg: M) -> Result<M::Result, ()> {
        Addr::send(self, msg).await
    }

    async fn wait(&self, msg: M) -> Result<M::Result, ()> {
        Addr::wait(self, msg).await
    }
}

#[async_trait(?Send)]
impl<A, M> AddrHandler<M> for WeakAddr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    async fn send(&self, msg: M) -> Result<M::Result, ()> {
        self.upgrade().ok_or(())?.send(msg).await
    }

    async fn wait(&self, msg: M) -> Result<M::Result, ()> {
        self.upgrade().ok_or(())?.wait(msg).await
    }
}

pub struct Recipient<M: Message + Send>(Box<dyn AddrHandler<M>>);

pub struct RecipientWeak<M: Message + Send>(Box<dyn AddrHandler<M>>);
