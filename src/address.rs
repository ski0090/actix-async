use core::future::Future;
use core::ops::Deref;

use crate::actor::{Actor, ActorState};
use crate::context::Context;
use crate::error::ActixAsyncError;
use crate::handler::Handler;
use crate::message::{
    message_send_check, ActorMessage, FunctionMessage, FunctionMutMessage, Message, MessageObject,
};
use crate::request::MessageRequest;
use crate::runtime::RuntimeService;
use crate::types::ActixResult;
use crate::util::channel::{oneshot_channel, Sender};
use crate::util::futures::LocalBoxedFuture;
use crate::util::smart_pointer::{RefCounter, WeakRefCounter};

pub struct Addr<A>(RefCounter<Sender<ActorMessage<A>>>);

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
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, M::Result>
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
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, M::Result>
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
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, R>
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
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, R>
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
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, ()> {
        let state = if graceful {
            ActorState::StopGraceful
        } else {
            ActorState::Stop
        };

        let (tx, rx) = oneshot_channel();

        MessageRequest::new(
            async move {
                self.deref()
                    .send(ActorMessage::ActorState(state, Some(tx)))
                    .await?;
                Ok(())
            },
            rx,
        )
    }

    /// weak version of Addr that can be upgraded.
    pub fn downgrade(&self) -> WeakAddr<A> {
        WeakAddr(RefCounter::downgrade(&self.0))
    }

    /// Recipient bound to message type and not actor.
    pub fn recipient<M>(&self) -> Recipient<A::Runtime, M>
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
    pub fn recipient_weak<M>(&self) -> RecipientWeak<A::Runtime, M>
    where
        M: Message + Send,
        A: Handler<M>,
    {
        RecipientWeak(Box::new(self.downgrade()))
    }

    pub(crate) fn new(tx: Sender<ActorMessage<A>>) -> Self {
        Self(RefCounter::new(tx))
    }

    fn _send<M, F>(
        &self,
        msg: M,
        f: F,
    ) -> MessageRequest<A::Runtime, impl Future<Output = ActixResult<()>> + '_, M::Result>
    where
        M: Message + Send,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + 'static,
    {
        send_with_async_closure(msg, |obj| async move {
            self.deref().send(f(obj)).await?;
            Ok(())
        })
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

fn send_with_async_closure<A, M, F, Fut>(msg: M, f: F) -> MessageRequest<A::Runtime, Fut, M::Result>
where
    M: Message + Send,
    A: Handler<M>,
    F: FnOnce(MessageObject<A>) -> Fut,
{
    let (tx, rx) = oneshot_channel();

    message_send_check::<M>();
    let msg = MessageObject::new(msg, Some(tx));
    MessageRequest::new(f(msg), rx)
}

/// weak version `Addr`. Can upgrade to `Addr` when at least one instance of `Addr` is still in
/// scope.
pub struct WeakAddr<A>(WeakRefCounter<Sender<ActorMessage<A>>>);

impl<A> Clone for WeakAddr<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A: Actor> WeakAddr<A> {
    pub fn upgrade(&self) -> Option<Addr<A>> {
        self.0.upgrade().map(Addr)
    }

    pub(crate) async fn _send(&self, msg: ActorMessage<A>) -> ActixResult<()> {
        self.upgrade()
            .ok_or(ActixAsyncError::Closed)?
            .deref()
            .send(msg)
            .await?;

        Ok(())
    }
}

pub trait AddrHandler<RT, M>
where
    RT: RuntimeService,
    M: Message + Send,
    Self: Send + Sync + 'static,
{
    fn send(&self, msg: M) -> MessageRequest<RT, LocalBoxedFuture<'_, ActixResult<()>>, M::Result>;

    fn wait(&self, msg: M) -> MessageRequest<RT, LocalBoxedFuture<'_, ActixResult<()>>, M::Result>;

    fn do_send(&self, msg: M);

    fn do_wait(&self, msg: M);
}

impl<A, M> AddrHandler<A::Runtime, M> for Addr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    fn send(
        &self,
        msg: M,
    ) -> MessageRequest<A::Runtime, LocalBoxedFuture<'_, ActixResult<()>>, M::Result> {
        send_with_async_closure::<_, _, _, LocalBoxedFuture<'_, ActixResult<()>>>(msg, |obj| {
            Box::pin(async move {
                self.deref().send(ActorMessage::Ref(obj)).await?;
                Ok(())
            })
        })
    }

    fn wait(
        &self,
        msg: M,
    ) -> MessageRequest<A::Runtime, LocalBoxedFuture<'_, ActixResult<()>>, M::Result> {
        send_with_async_closure::<_, _, _, LocalBoxedFuture<'_, ActixResult<()>>>(msg, |obj| {
            Box::pin(async move {
                self.deref().send(ActorMessage::Mut(obj)).await?;
                Ok(())
            })
        })
    }

    fn do_send(&self, msg: M) {
        Addr::do_send(self, msg);
    }

    fn do_wait(&self, msg: M) {
        Addr::do_wait(self, msg);
    }
}

impl<A, M> AddrHandler<A::Runtime, M> for WeakAddr<A>
where
    A: Actor + Handler<M>,
    M: Message + Send,
{
    fn send(
        &self,
        msg: M,
    ) -> MessageRequest<A::Runtime, LocalBoxedFuture<'_, ActixResult<()>>, M::Result> {
        send_with_async_closure::<_, _, _, LocalBoxedFuture<'_, ActixResult<()>>>(msg, |obj| {
            Box::pin(self._send(ActorMessage::Ref(obj)))
        })
    }

    fn wait(
        &self,
        msg: M,
    ) -> MessageRequest<A::Runtime, LocalBoxedFuture<'_, ActixResult<()>>, M::Result> {
        send_with_async_closure::<_, _, _, LocalBoxedFuture<'_, ActixResult<()>>>(msg, |obj| {
            Box::pin(self._send(ActorMessage::Mut(obj)))
        })
    }

    /// `AddrHandler::do_send` will panic if the `Addr` for `RecipientWeak` is gone.
    fn do_send(&self, msg: M) {
        let addr = &self.upgrade().unwrap();
        Addr::do_send(addr, msg);
    }

    /// `AddrHandler::do_wait` will panic if the `Addr` for `RecipientWeak` is gone.
    fn do_wait(&self, msg: M) {
        let addr = &self.upgrade().unwrap();
        Addr::do_wait(addr, msg);
    }
}

pub struct Recipient<RT, M: Message + Send>(Box<dyn AddrHandler<RT, M>>);

impl<RT, M: Message + Send> Deref for Recipient<RT, M> {
    type Target = dyn AddrHandler<RT, M>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

pub struct RecipientWeak<RT, M: Message + Send>(Box<dyn AddrHandler<RT, M>>);

impl<RT, M: Message + Send> Deref for RecipientWeak<RT, M> {
    type Target = dyn AddrHandler<RT, M>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
