use core::future::Future;
use core::ops::Deref;

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::actor::{Actor, ActorState};
use crate::context::Context;
use crate::error::ActixAsyncError;
use crate::handler::Handler;
use crate::message::{
    message_send_check, ActorMessage, FunctionMessage, FunctionMutMessage, Message, MessageObject,
};
use crate::request::MessageRequest;
use crate::types::LocalBoxedFuture;

pub struct Addr<A>(Arc<Sender<ActorMessage<A>>>);

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
    pub fn send<M>(
        &self,
        msg: M,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        M::Result,
    >
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._send(msg, ActorMessage::Ref)
    }

    /// send an exclusive message to actor. `Handler::handle_wait` will be called for exclusive
    /// message processing.
    /// If `Handler::handle_wait` is not override then it would use `Handler::handle` as fallback.
    pub fn wait<M>(
        &self,
        msg: M,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        M::Result,
    >
    where
        M: Message + Send,
        A: Handler<M>,
    {
        self._send(msg, ActorMessage::Mut)
    }

    /// send a concurrent closure to actor. `Handler::handle` will be called for concurrent message
    /// processing.
    /// closure must be `Send` bound.
    pub fn run<F, R>(
        &self,
        func: F,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        R,
    >
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, R> + Send + 'static,
        R: Send + 'static,
    {
        let msg = FunctionMessage::new(func);

        self._send(msg, ActorMessage::Ref)
    }

    /// send a exclusive closure to actor. `Handler::handle_wait` will be called for exclusive
    /// message processing.
    /// If `Handler::handle_wait` is not override then it would use `Handler::handle` as fallback.
    pub fn run_wait<F, R>(
        &self,
        func: F,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        R,
    >
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, R>
            + Send
            + 'static,
        R: Send + 'static,
    {
        let msg = FunctionMutMessage::new(func);

        self._send(msg, ActorMessage::Mut)
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
    pub fn stop(
        &self,
        graceful: bool,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        (),
    > {
        let state = if graceful {
            ActorState::StopGraceful
        } else {
            ActorState::Stop
        };

        let (tx, rx) = oneshot::channel();

        MessageRequest::new(
            self.deref().send(ActorMessage::ActorState(state, Some(tx))),
            rx,
        )
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

    /// weak version of `Recipient`.
    ///
    /// *. `RecipientWeak` will stay usable as long as the actor and it's `Addr` are alive.
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

    fn _send<M, F>(
        &self,
        msg: M,
        f: F,
    ) -> MessageRequest<
        A::Runtime,
        impl Future<Output = Result<(), SendError<ActorMessage<A>>>> + '_,
        M::Result,
    >
    where
        M: Message + Send,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + 'static,
    {
        let (tx, rx) = oneshot::channel();

        message_send_check::<M>();
        let msg = MessageObject::new(msg, Some(tx));

        MessageRequest::new(self.deref().send(f(msg)), rx)
    }

    fn _do_send<M, F>(&self, msg: M, f: F)
    where
        M: Message + Send,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + 'static,
    {
        let this = self.clone();
        A::spawn(async move {
            message_send_check::<M>();
            let msg = MessageObject::new(msg, None);
            let _ = this.deref().send(f(msg)).await;
        });
    }
}

pub struct WeakAddr<A>(Weak<Sender<ActorMessage<A>>>);

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
trait AddrHandler<M>
where
    M: Message + Send,
    Self: Send + Sync + 'static,
{
    async fn send(&self, msg: M) -> Result<M::Result, ActixAsyncError>;

    async fn wait(&self, msg: M) -> Result<M::Result, ActixAsyncError>;
}

#[async_trait(?Send)]
impl<A, M> AddrHandler<M> for Addr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    async fn send(&self, msg: M) -> Result<M::Result, ActixAsyncError> {
        Addr::send(self, msg).await
    }

    async fn wait(&self, msg: M) -> Result<M::Result, ActixAsyncError> {
        Addr::wait(self, msg).await
    }
}

#[async_trait(?Send)]
impl<A, M> AddrHandler<M> for WeakAddr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    async fn send(&self, msg: M) -> Result<M::Result, ActixAsyncError> {
        self.upgrade()
            .ok_or(ActixAsyncError::Closed)?
            .send(msg)
            .await
    }

    async fn wait(&self, msg: M) -> Result<M::Result, ActixAsyncError> {
        self.upgrade()
            .ok_or(ActixAsyncError::Closed)?
            .wait(msg)
            .await
    }
}

pub struct Recipient<M: Message + Send>(Box<dyn AddrHandler<M>>);

pub struct RecipientWeak<M: Message + Send>(Box<dyn AddrHandler<M>>);
