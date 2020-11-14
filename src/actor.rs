use actix_rt::Arbiter;
use async_trait::async_trait;
use tokio::sync::mpsc::{channel, Receiver};

use crate::address::{Addr, WeakAddr};
use crate::context::{Context, ContextWithActor};
use crate::message::ActorMessage;
use crate::runtime::RuntimeService;

pub(crate) const CHANNEL_CAP: usize = 256;

#[async_trait(?Send)]
pub trait Actor: Sized + 'static {
    type Runtime: RuntimeService;

    // start the actor and return it's address
    fn start(self) -> Addr<Self> {
        Self::create(|_| self)
    }

    // create actor with closure
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

    // create actor with closure and start it in the given arbiter
    fn start_in_arbiter<F>(arb: &Arbiter, f: F) -> Addr<Self>
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

    // async hook before actor start to run.
    #[allow(unused_variables)]
    async fn on_start(&mut self, ctx: &mut Context<Self>) {}

    // async hook before actor stops
    #[allow(unused_variables)]
    async fn on_stop(&mut self, ctx: &mut Context<Self>) {}

    fn _start<F>(tx: WeakAddr<Self>, rx: Receiver<ActorMessage<Self>>, f: F)
    where
        F: FnOnce(&mut Context<Self>) -> Self,
    {
        let mut ctx = Context::new(tx);

        let actor = f(&mut ctx);

        let mut ctx = ContextWithActor::new(actor, rx, ctx);

        Self::Runtime::spawn(async move {
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
