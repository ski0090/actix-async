use core::future::Future;

use alloc::boxed::Box;

use crate::actor::Actor;
use crate::context::Context;
use crate::message::{FunctionMessage, FunctionMutMessage, Message, MessageHandlerContainer};
use crate::util::channel::OneshotSender;
use crate::util::futures::LocalBoxFuture;

/// Trait define how actor handle a message.
/// # example:
/// ```rust:
/// use actix_async::prelude::*;
///
/// struct TestActor;
/// actor!(TestActor);
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
///     fn handle<'a: 'r,'c: 'r, 'r>(&'a self, _: TestMessage2, ctx: &'c Context<Self>) -> LocalBoxFuture<'r, ()> {
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
    ) -> LocalBoxFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res;

    /// exclusive handler. `Actor` and `Context` are borrowed mutably so only one message can be
    /// handle at any given time. `Actor` would block on this method until it's finished.
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: M,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res,
    {
        // fall back to handle by default
        self.handle(msg, ctx)
    }
}

impl<A, F, R> Handler<FunctionMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: FunctionMessage<F, R>,
        ctx: &'ctx Context<Self>,
    ) -> LocalBoxFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
    {
        (msg.func)(self, ctx)
    }
}

impl<A, F, R> Handler<FunctionMutMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        _: FunctionMutMessage<F, R>,
        _: &'ctx Context<Self>,
    ) -> LocalBoxFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
    {
        unreachable!("Handler::handle can not be called on FunctionMutMessage");
    }

    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: FunctionMutMessage<F, R>,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxFuture<'res, R>
    where
        'act: 'res,
        'ctx: 'res,
    {
        (msg.func)(self, ctx)
    }
}

pub trait MessageHandler<A: Actor> {
    fn handle<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act A,
        ctx: &'ctx Context<A>,
    ) -> LocalBoxFuture<'static, ()>;

    fn handle_wait<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act mut A,
        ctx: &'ctx mut Context<A>,
    ) -> LocalBoxFuture<'static, ()>;
}

impl<A, M> MessageHandler<A> for MessageHandlerContainer<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn handle<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act A,
        ctx: &'ctx Context<A>,
    ) -> LocalBoxFuture<'static, ()> {
        let msg = self.msg.take().unwrap();
        let tx = self.tx.take();

        /*
            SAFETY:
            `MessageHandler::handle`can not tie to actor and context's lifetime. The reason is it
            would assume the boxed futures would live as long as the actor and context. Making it
            impossible to mutably borrow them from this point forward.

            futures transmuted to static lifetime in ContextWithActor.cache_ref must resolved
            before next ContextWithActor.cache_mut get polled
        */
        let act: &'static A = unsafe { core::mem::transmute(act) };
        let ctx: &'static Context<A> = unsafe { core::mem::transmute(ctx) };

        let fut = act.handle(msg, ctx);

        handle(tx, fut)
    }

    fn handle_wait<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act mut A,
        ctx: &'ctx mut Context<A>,
    ) -> LocalBoxFuture<'static, ()> {
        let msg = self.msg.take().unwrap();
        let tx = self.tx.take();

        /*
            SAFETY:
            `MessageHandler::handle_wait`can not tie to actor and context's lifetime. The reason is
            it would assume the boxed futures would live as long as the actor and context. Making it
            impossible to mutably borrow them again from this point forward.

            future transmute to static lifetime must be polled only when ContextWithActor.cache_ref
            is empty.
        */
        let act: &'static mut A = unsafe { core::mem::transmute(act) };
        let ctx: &'static mut Context<A> = unsafe { core::mem::transmute(ctx) };

        let fut = act.handle_wait(msg, ctx);

        handle(tx, fut)
    }
}

fn handle<F>(tx: Option<OneshotSender<F::Output>>, fut: F) -> LocalBoxFuture<'static, ()>
where
    F: Future + 'static,
{
    Box::pin(async move {
        match tx {
            Some(tx) => {
                if !tx.is_closed() {
                    let res = fut.await;
                    let _ = tx.send(res);
                }
            }
            None => {
                let _ = fut.await;
            }
        }
    })
}
