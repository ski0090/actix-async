use core::{
    future::{ready, Future},
    time::Duration,
};

use alloc::boxed::Box;

use super::address::Addr;
use super::context::Context;
use super::context_future::ContextFuture;
use super::message::ActorMessage;
use super::runtime::RuntimeService;
use super::util::{
    channel::{channel, Receiver},
    futures::LocalBoxFuture,
};

const CHANNEL_CAP: usize = 256;

/// trait for stateful async actor.
pub trait Actor: Sized + 'static {
    /// actor is async and needs a runtime.
    type Runtime: RuntimeService;

    /// async hook before actor start to run.
    fn on_start<'act, 'ctx, 'res>(
        &'act mut self,
        ctx: Context<'ctx, Self>,
    ) -> LocalBoxFuture<'res, ()>
    where
        'act: 'res,
        'ctx: 'res,
    {
        Box::pin(async move {
            let _ = ctx;
        })
    }

    /// async hook before actor stops
    fn on_stop<'act, 'ctx, 'res>(
        &'act mut self,
        ctx: Context<'ctx, Self>,
    ) -> LocalBoxFuture<'res, ()>
    where
        'act: 'res,
        'ctx: 'res,
    {
        Box::pin(async move {
            let _ = ctx;
        })
    }

    /// start the actor on current thread and return it's address
    fn start(self) -> Addr<Self> {
        Self::create(|_| self)
    }

    /// create actor with closure
    fn create<F>(f: F) -> Addr<Self>
    where
        F: for<'c> FnOnce(Context<'c, Self>) -> Self + 'static,
    {
        Self::create_async(|ctx| ready(f(ctx)))
    }

    /// create actor with async closure
    /// # example:
    /// ```rust
    /// use std::time::Duration;
    ///
    /// use actix_async::prelude::*;
    /// use actix_async::{actor, message};
    /// use futures_util::FutureExt;
    ///
    /// struct TestActor;
    /// actor!(TestActor);
    ///
    /// impl TestActor {
    ///     async fn test(&mut self) -> usize {
    ///         996
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     tokio::task::LocalSet::new().run_until(async {
    ///         let addr = TestActor::create_async(|ctx| {
    ///             // *. notice context can not move to async block. so you have to use it from
    ///             // outside if you would.
    ///             let _ctx = ctx;
    ///             async {
    ///                 // run async code
    ///                 tokio::time::sleep(Duration::from_secs(1)).await;
    ///                 // return an instance of actor.
    ///                 TestActor
    ///             }
    ///         });
    ///
    ///         // run async closure with actor and it's context.
    ///         let res = addr.run_wait(|act, ctx| act.test().boxed_local()).await;
    ///         assert_eq!(996, res.unwrap());
    ///     })
    ///     .await
    /// }
    /// ```
    fn create_async<F, Fut>(f: F) -> Addr<Self>
    where
        F: for<'c> FnOnce(Context<'c, Self>) -> Fut + 'static,
        Fut: Future<Output = Self>,
    {
        let (tx, rx) = channel(Self::size_hint());

        let tx = Addr::new(tx);

        Self::_start(rx, f);

        tx
    }

    /// capacity of the actor's channel. Limit the max count of on flight messages.
    ///
    /// Default to `256`.
    fn size_hint() -> usize {
        CHANNEL_CAP
    }

    #[doc(hidden)]
    fn spawn<F: Future<Output = ()> + 'static>(f: F) {
        Self::Runtime::spawn(f)
    }

    #[doc(hidden)]
    fn sleep(dur: Duration) -> <Self::Runtime as RuntimeService>::Sleep {
        Self::Runtime::sleep(dur)
    }

    fn _start<F, Fut>(rx: Receiver<ActorMessage<Self>>, f: F)
    where
        F: for<'c> FnOnce(Context<'c, Self>) -> Fut + 'static,
        Fut: Future<Output = Self>,
    {
        Self::spawn(async move {
            use core::cell::{Cell, RefCell};

            use alloc::vec::Vec;

            let future_cache = RefCell::new(Vec::with_capacity(8));
            let stream_cache = RefCell::new(Vec::with_capacity(8));

            let act_state = Cell::new(ActorState::Stop);

            let ctx = Context::new(&act_state, &future_cache, &stream_cache, &rx);

            let actor = f(ctx).await;

            ContextFuture::new(actor, act_state, rx, future_cache, stream_cache).await;
        });
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum ActorState {
    Running,
    Stop,
    StopGraceful,
}
