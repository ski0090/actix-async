use core::future::Future;
use core::mem::transmute;

use alloc::boxed::Box;

use crate::actor::Actor;
use crate::context::Context;
use crate::message::{FunctionMessage, FunctionMutMessage, Message, MessageContainer};
use crate::util::channel::OneshotSender;
use crate::util::futures::LocalBoxFuture;

/// Trait define how actor handle a message.
/// # example:
/// ```rust:
/// #![allow(incomplete_features)]
/// #![feature(generic_associated_types, type_alias_impl_trait)]
///
/// use std::future::Future;
///
/// use actix_async::prelude::*;
///
/// struct TestActor;
/// actor!(TestActor);
///
/// struct TestMessage;
/// message!(TestMessage, ());
///
/// // impl boxed future manually without async_trait.
/// impl Handler<TestMessage> for TestActor {
///     type Future<'res> = impl Future<Output = ()> + 'res;
///     type FutureWait<'res> = impl Future<Output = ()> + 'res;
///     
///     fn handle<'act, 'ctx, 'res>(
///         &'act self,
///         _: TestMessage,
///         ctx: &'ctx Context<Self>
///     ) -> Self::Future<'res>
///     where
///         'act: 'res,
///         'ctx: 'res,
///     {
///         async move {
///             let _this = self;
///             let _ctx = ctx;
///             println!("hello from TestMessage2");
///         }
///     }
///
///     fn handle_wait<'act, 'ctx, 'res>(
///         &'act mut self,
///         _: TestMessage,
///         _: &'ctx mut Context<Self>
///     ) -> Self::FutureWait<'res>
///     where
///         'act: 'res,
///         'ctx: 'res
///     {
///         async { unimplemented!() }
///     }
/// }
/// ```
pub trait Handler<M>
where
    M: Message,
    Self: Actor,
{
    type Future<'res>: Future<Output = M::Result> + 'res;
    type FutureWait<'res>: Future<Output = M::Result> + 'res;

    /// concurrent handler. `Actor` and `Context` are borrowed immutably so it's safe to handle
    /// multiple messages at the same time.
    fn handle<'act, 'ctx, 'res>(&'act self, msg: M, ctx: &'ctx Context<Self>) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res;

    /// exclusive handler. `Actor` and `Context` are borrowed mutably so only one message can be
    /// handle at any given time. `Actor` would block on this method until it's finished.
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: M,
        ctx: &'ctx mut Context<Self>,
    ) -> Self::FutureWait<'res>
    where
        'act: 'res,
        'ctx: 'res;
}

impl<A, F, R> Handler<FunctionMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    type Future<'res> = impl Future<Output = R> + 'res;
    type FutureWait<'res> = impl Future<Output = R> + 'res;

    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: FunctionMessage<F, R>,
        ctx: &'ctx Context<Self>,
    ) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        (msg.func)(self, ctx)
    }

    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: FunctionMessage<F, R>,
        ctx: &'ctx mut Context<Self>,
    ) -> Self::FutureWait<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        // fall back to handle by default
        self.handle(msg, ctx)
    }
}

impl<A, F, R> Handler<FunctionMutMessage<F, R>> for A
where
    A: Actor,
    F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxFuture<'a, R> + 'static,
    R: Send + 'static,
{
    type Future<'res> = impl Future<Output = R> + 'res;
    type FutureWait<'res> = impl Future<Output = R> + 'res;

    fn handle<'act, 'ctx, 'res>(
        &'act self,
        _: FunctionMutMessage<F, R>,
        _: &'ctx Context<Self>,
    ) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        async { unimplemented!("Handler::handle can not be called on FunctionMutMessage") }
    }

    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: FunctionMutMessage<F, R>,
        ctx: &'ctx mut Context<Self>,
    ) -> Self::FutureWait<'res>
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
        ctx: &'ctx Context<A>,
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
        let act = unsafe { transmute::<_, &'static A>(act) };
        let ctx = unsafe { transmute::<_, &'static Context<A>>(ctx) };

        let fut = act.handle(msg, ctx);

        handle(tx, fut)
    }

    fn handle_wait<'msg, 'act, 'ctx>(
        &'msg mut self,
        act: &'act mut A,
        ctx: &'ctx mut Context<A>,
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
        let act = unsafe { transmute::<_, &'static mut A>(act) };
        let ctx = unsafe { transmute::<_, &'static mut Context<A>>(ctx) };

        let fut = act.handle_wait(msg, ctx);

        handle(tx, fut)
    }
}

#[inline(always)]
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
