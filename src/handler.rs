use crate::actor::Actor;
use crate::context::Context;
use crate::message::{FunctionMessage, FunctionMutMessage, Message, MessageHandlerContainer};
use crate::util::futures::LocalBoxedFuture;

/// Trait define how actor handle a message.
/// # example:
/// ```rust:
/// use actix_async::prelude::*;
/// use actix_async::message;
///
/// struct TestActor;
///
/// impl Actor for TestActor {
///     type Runtime = ActixRuntime;
/// }
///
/// struct TestMessage;
/// message!(TestMessage, ());
///
/// struct TestMessage2;
/// message!(TestMessage2, ());
///
/// // use async method directly with the help of async_trait crate.
/// #[async_trait::async_trait(?Send)]
/// impl Handler<TestMessage> for TestActor {
///    async fn handle(&self, _: TestMessage,ctx: &Context<Self>) {
///         let _this = self;
///         let _ctx = ctx;
///         println!("hello from TestMessage");
///     }
/// }
///
/// // impl boxed future manually without async_trait.
/// impl Handler<TestMessage2> for TestActor {
///     fn handle<'a: 'r,'c: 'r, 'r>(&'a self, _: TestMessage2, ctx: &'c Context<Self>) -> LocalBoxedFuture<'r, ()> {
///         Box::pin(async move {
///             let _this = self;
///             let _ctx = ctx;
///             println!("hello from TestMessage2");
///         })
///     }
/// }
/// ```
pub trait Handler<M>
where
    M: Message,
    Self: Actor,
{
    /// concurrent handler. `Actor` and `Context` are borrowed immutably so it's safe to handle
    /// multiple messages at the same time.
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: M,
        ctx: &'ctx Context<Self>,
    ) -> LocalBoxedFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res;

    /// exclusive handler. `Actor` and `Context` are borrowed mutably so only one message can be
    /// handle at any given time. `Actor` would block on this method until it's finished.
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
        // fall back to handle by default
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
