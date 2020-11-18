use core::future::Future;
use core::time::Duration;

use crate::address::{Addr, WeakAddr};
use crate::context::{Context, ContextWithActor};
use crate::message::ActorMessage;
use crate::runtime::RuntimeService;
use crate::util::channel::{channel, Receiver};
use crate::util::futures::LocalBoxedFuture;

pub(crate) const CHANNEL_CAP: usize = 256;

pub trait Actor: Sized + 'static {
    type Runtime: RuntimeService;

    /// async hook before actor start to run.
    #[allow(unused_variables)]
    fn on_start<'act, 'ctx, 'res>(
        &'act mut self,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        Box::pin(async {})
    }

    /// async hook before actor stops
    #[allow(unused_variables)]
    fn on_stop<'act, 'ctx, 'res>(
        &'act mut self,
        ctx: &'ctx mut Context<Self>,
    ) -> LocalBoxedFuture<'res, ()>
    where
        'act: 'res,
        'ctx: 'res,
        Self: 'res,
    {
        Box::pin(async {})
    }

    /// start the actor on current thread and return it's address
    fn start(self) -> Addr<Self> {
        Self::create(|_| self)
    }

    /// create actor with closure
    fn create<F>(f: F) -> Addr<Self>
    where
        F: FnOnce(&mut Context<Self>) -> Self,
    {
        let (tx, rx) = channel(CHANNEL_CAP);

        let tx = Addr::new(tx);
        let weak_tx = Addr::downgrade(&tx);

        Self::_start(weak_tx, rx, f);

        tx
    }

    /// create actor with closure and start it in the given arbiter
    #[cfg(feature = "actix-rt")]
    fn start_in_arbiter<F>(arb: &actix_rt::Arbiter, f: F) -> Addr<Self>
    where
        F: FnOnce(&mut Context<Self>) -> Self + Send + 'static,
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

    fn _start<F>(tx: WeakAddr<Self>, rx: Receiver<ActorMessage<Self>>, f: F)
    where
        F: FnOnce(&mut Context<Self>) -> Self,
    {
        let mut ctx = Context::new(tx, rx);

        let actor = f(&mut ctx);

        let mut ctx = ContextWithActor::new(actor, ctx);

        Self::spawn(async move {
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
