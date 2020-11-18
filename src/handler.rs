use crate::actor::Actor;
use crate::context::Context;
use crate::message::{FunctionMessage, FunctionMutMessage, Message, MessageHandlerContainer};
use crate::types::LocalBoxedFuture;

pub trait Handler<M>
where
    M: Message,
    Self: Actor,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: M,
        ctx: &'ctx Context<Self>,
    ) -> LocalBoxedFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res;

    // fall back to handle by default
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: M,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        self.handle(msg, ctx)
    }
}

impl<A, F, R> Handler<FunctionMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: FunctionMessage<F, R>,
        ctx: &'ctx Context<Self>,
    ) -> LocalBoxedFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        (msg.func)(self, ctx)
    }
}

impl<A, F, R> Handler<FunctionMutMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        _: FunctionMutMessage<F, R>,
        _: &'ctx Context<Self>,
    ) -> LocalBoxedFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        unreachable!("Handler::handle can not be called on FunctionMutMessage");
    }

    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: FunctionMutMessage<F, R>,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        (msg.func)(self, ctx)
    }
}

pub trait MessageHandler<A: Actor> {
    fn handle<'msg, 'act, 'ctx, 'res>(
        &'msg mut self,
        act: &'act A,
        ctx: &'ctx Context<A>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'msg: 'res,
        'act: 'res,
        'ctx: 'res,
        Self: 'res;

    fn handle_wait<'msg, 'act, 'ctx, 'res>(
        &'msg mut self,
        act: &'act mut A,
        ctx: &'ctx mut Context<A>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'msg: 'res,
        'act: 'res,
        'ctx: 'res,
        Self: 'res;

    fn is_taken(&self) -> bool;
}

impl<A, M> MessageHandler<A> for MessageHandlerContainer<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn handle<'msg, 'act, 'ctx, 'res>(
        &'msg mut self,
        act: &'act A,
        ctx: &'ctx Context<A>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'msg: 'res,
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        Box::pin(async move {
            if let Some(msg) = self.msg.take() {
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
        })
    }

    fn handle_wait<'msg, 'act, 'ctx, 'res>(
        &'msg mut self,
        act: &'act mut A,
        ctx: &'ctx mut Context<A>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'msg: 'res,
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        Box::pin(async move {
            if let Some(msg) = self.msg.take() {
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
        })
    }

    fn is_taken(&self) -> bool {
        self.msg.is_some()
    }
}
