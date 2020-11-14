use core::future::Future;
use core::pin::Pin;

use async_trait::async_trait;

use crate::actor::Actor;
use crate::context::Context;
use crate::message::{FunctionMessage, FunctionMutMessage, Message, MessageHandlerContainer};

#[async_trait(?Send)]
pub trait Handler<M>
where
    M: Message,
    Self: Actor,
{
    async fn handle(&self, msg: M, ctx: &Context<Self>) -> M::Result;

    // fall back to handle by default
    async fn handle_wait(&mut self, msg: M, ctx: &mut Context<Self>) -> M::Result {
        self.handle(msg, ctx).await
    }
}

#[async_trait(?Send)]
impl<A, F, R> Handler<FunctionMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a A, &'a Context<A>) -> Pin<Box<dyn Future<Output = R> + 'a>> + 'static,
    R: Send + 'static,
{
    async fn handle(&self, msg: FunctionMessage<F, R>, ctx: &Context<Self>) -> R {
        (msg.func)(self, ctx).await
    }
}

#[async_trait(?Send)]
impl<A, F, R> Handler<FunctionMutMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> Pin<Box<dyn Future<Output = R> + 'a>>
        + 'static,
    R: Send + 'static,
{
    async fn handle(&self, _: FunctionMutMessage<F, R>, _: &Context<Self>) -> R {
        unreachable!("Handler::handle can not be called on FunctionMutMessage");
    }

    async fn handle_wait(&mut self, msg: FunctionMutMessage<F, R>, ctx: &mut Context<Self>) -> R {
        (msg.func)(self, ctx).await
    }
}

#[async_trait(?Send)]
pub trait MessageHandler<A: Actor> {
    async fn handle(&mut self, act: &A, ctx: &Context<A>);

    async fn handle_wait(&mut self, act: &mut A, ctx: &mut Context<A>);
}

#[async_trait(?Send)]
impl<A, M> MessageHandler<A> for MessageHandlerContainer<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    async fn handle(&mut self, act: &A, ctx: &Context<A>) {
        let msg = self.msg.take().unwrap();

        match self.tx.take() {
            Some(tx) => {
                if !tx.is_closed() {
                    let res = act.handle(msg, ctx).await;
                    let _ = tx.send(res);
                }
            }
            None => {
                let _ = act.handle(msg, ctx).await;
            }
        }
    }

    async fn handle_wait(&mut self, act: &mut A, ctx: &mut Context<A>) {
        let msg = self.msg.take().unwrap();

        match self.tx.take() {
            Some(tx) => {
                if !tx.is_closed() {
                    let res = act.handle_wait(msg, ctx).await;
                    let _ = tx.send(res);
                }
            }
            None => {
                let _ = act.handle_wait(msg, ctx).await;
            }
        }
    }
}
