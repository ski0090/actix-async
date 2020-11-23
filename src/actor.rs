use core::future::{ready, Future};
use core::time::Duration;

use alloc::boxed::Box;

use crate::address::{Addr, WeakAddr};
use crate::context::{Context, ContextWithActor};
use crate::message::ActorMessage;
use crate::runtime::RuntimeService;
use crate::util::channel::{channel, Receiver};
use crate::util::futures::LocalBoxedFuture;

pub(crate) const CHANNEL_CAP: usize = 256;

/// trait for stateful async actor.
pub trait Actor: Sized + 'static {
    /// actor is async and needs a runtime.
    type Runtime: RuntimeService;

    /// async hook before actor start to run.
    fn on_start<'act, 'ctx, 'res>(
        &'act mut self,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, ()>
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
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, ()>
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
        F: FnOnce(&mut Context<Self>) -> Self + 'static,
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
    /// #[actix_rt::main]
    /// async fn main() {
    ///     let addr = TestActor::create_async(|ctx| {
    ///         // *. notice context can not move to async block. so you have to use it from
    ///         // outside if you would.
    ///         let _ctx = ctx;
    ///         async {
    ///             // run async code
    ///             actix_rt::time::sleep(Duration::from_secs(1)).await;
    ///             // return an instance of actor.
    ///             TestActor
    ///         }   
    ///     });
    ///
    ///     // run async closure with actor and it's context.
    ///     let res = addr.run_wait(|act, ctx| act.test().boxed_local()).await;
    ///     assert_eq!(996, res.unwrap());
    /// }
    ///
    /// ```
    fn create_async<F, Fut>(f: F) -> Addr<Self>
    where
        F: FnOnce(&mut Context<Self>) -> Fut + 'static,
        Fut: Future<Output = Self>,
    {
        let (tx, rx) = channel(CHANNEL_CAP);

        let tx = Addr::new(tx);
        let weak_tx = Addr::downgrade(&tx);

        Self::_start(weak_tx, rx, f);

        tx
    }

    /// create actor with closure and start it in the given arbiter
    ///
    /// similar to how `Actor::create` work. The only difference as the creation would happen on
    /// the arbiter reference you passed in so the construct closure must be bound to `Send`
    #[cfg(feature = "actix-rt")]
    fn start_in_arbiter<F>(arb: &actix_rt::Arbiter, f: F) -> Addr<Self>
    where
        F: FnOnce(&mut Context<Self>) -> Self + Send + 'static,
    {
        Self::start_async_in_arbiter(arb, |ctx| ready(f(ctx)))
    }

    /// create actor with async closure and start it in the given arbiter
    ///
    /// similar to how `Actor::create_async` work. The only difference as the creation would happen
    /// on the arbiter reference you passed in so the construct async closure must be bound to
    /// `Send`
    #[cfg(feature = "actix-rt")]
    fn start_async_in_arbiter<F, Fut>(arb: &actix_rt::Arbiter, f: F) -> Addr<Self>
    where
        F: FnOnce(&mut Context<Self>) -> Fut + Send + 'static,
        Fut: Future<Output = Self>,
    {
        let (tx, rx) = channel(CHANNEL_CAP);

        let tx = Addr::new(tx);
        let weak_tx = Addr::downgrade(&tx);

        arb.exec_fn(move || {
            Self::_start(weak_tx, rx, f);
        });

        tx
    }

    fn spawn<F: Future<Output = ()> + 'static>(f: F) {
        Self::Runtime::spawn(f)
    }

    fn sleep(dur: Duration) -> <Self::Runtime as RuntimeService>::Sleep {
        Self::Runtime::sleep(dur)
    }

    fn _start<F, Fut>(tx: WeakAddr<Self>, rx: Receiver<ActorMessage<Self>>, f: F)
    where
        F: FnOnce(&mut Context<Self>) -> Fut + 'static,
        Fut: Future<Output = Self>,
    {
        Self::spawn(async move {
            let mut ctx = Context::new(tx, rx);

            let actor = f(&mut ctx).await;

            let mut ctx = ContextWithActor::new(actor, ctx);

            let _ = ctx.first_run().await;
        });
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ActorState {
    Running,
    Stop,
    StopGraceful,
}
