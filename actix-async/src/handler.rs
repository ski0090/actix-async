use core::{future::Future, mem};

use alloc::boxed::Box;

use super::actor::Actor;
use super::context::Context;
use super::message::{FunctionMessage, FunctionMutMessage, Message, MessageContainer};
use super::util::{channel::OneshotSender, futures::LocalBoxFuture};

/// Trait define how actor handle a message.
/// # example:
/// ```rust:
/// use actix_async::prelude::*;
/// use actix_async::handler;
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
/// // use async method directly with the help of handler macro.
/// #[handler]
/// impl Handler<TestMessage> for TestActor {
///    async fn handle(&self, _: TestMessage,ctx: Context<'_, Self>) {
///         let _this = self;
///         let _ctx = ctx;
///         println!("hello from TestMessage");
///     }
/// }
///
/// // impl boxed future manually without handler macro.
/// impl Handler<TestMessage2> for TestActor {
///     fn handle<'a: 'r,'c: 'r, 'r>(&'a self, _: TestMessage2, ctx: Context<'c, Self>) -> LocalBoxFuture<'r, ()> {
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
        ctx: Context<'ctx, Self>,
    ) -> LocalBoxFuture<'res, M::Result>
    where
        'act: 'res,
        'ctx: 'res;

    /// exclusive handler. `Actor` and `Context` are borrowed mutably so only one message can be
    /// handle at any given time. `Actor` would block on this method until it's finished.
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: M,
        ctx: Context<'ctx, Self>,
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
    F: for<'a> FnOnce(&'a A, Context<'a, A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: FunctionMessage<F, R>,
        ctx: Context<'ctx, Self>,
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
    F: for<'a> FnOnce(&'a mut A, Context<'a, A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        _: FunctionMutMessage<F, R>,
        _: Context<'ctx, Self>,
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
        ctx: Context<'ctx, Self>,
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
        ctx: Context<'ctx, A>,
    ) -> LocalBoxFuture<'static, ()>;

    fn handle_wait<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act mut A,
        ctx: Context<'ctx, A>,
    ) -> LocalBoxFuture<'static, ()> {
        self.handle(act, ctx)
    }
}

impl<A, M> MessageHandler<A> for MessageContainer<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn handle<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act A,
        ctx: Context<'ctx, A>,
    ) -> LocalBoxFuture<'static, ()> {
        let (msg, tx) = self.take();

        /*
            SAFETY:
            `MessageHandler::handle`can not tie to actor and context's lifetime.
            The reason is it would assume the boxed futures would live as long as the
            actor and context. Making it impossible to mutably borrow them again from
            this point forward.

            future transmute to static lifetime must be polled before next
            ContextWithActor.cache_mut is polled.
        */
        let act = unsafe { mem::transmute::<_, &'static A>(act) };
        let ctx = unsafe { mem::transmute::<_, Context<'static, A>>(ctx) };

        let fut = act.handle(msg, ctx);

        handle(tx, fut)
    }

    fn handle_wait<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act mut A,
        ctx: Context<'ctx, A>,
    ) -> LocalBoxFuture<'static, ()> {
        let (msg, tx) = self.take();

        /*
            SAFETY:
            `MessageHandler::handle_wait`can not tie to actor and context's lifetime.
            The reason is it would assume the boxed futures would live as long as the
            actor and context. Making it impossible to mutably borrow them again from
            this point forward.

            future transmute to static lifetime must be polled only when
            ContextWithActor.cache_ref is empty.
        */
        let act = unsafe { mem::transmute::<_, &'static mut A>(act) };
        let ctx = unsafe { mem::transmute::<_, Context<'static, A>>(ctx) };

        let fut = act.handle_wait(msg, ctx);

        handle(tx, fut)
    }
}

#[inline]
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
