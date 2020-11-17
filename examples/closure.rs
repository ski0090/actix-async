use std::time::Duration;

use actix_async::prelude::*;
use futures_util::FutureExt;

struct ClosureActor(usize);

// impl actor trait.
impl Actor for ClosureActor {
    type Runtime = ActixRuntime;
}

// impl methods that take in self and/or actor's context.
impl ClosureActor {
    async fn mutate_state(&mut self) -> usize {
        actix_rt::time::sleep(Duration::from_millis(1)).await;
        self.0 += 1;
        self.0
    }

    async fn access_context(&self, ctx: &Context<Self>) -> usize {
        actix_rt::time::sleep(Duration::from_millis(1)).await;
        ctx.stop();
        self.0 + 1
    }

    async fn capture_outer_var(&self, var: usize) -> usize {
        var * 2
    }
}

#[actix_rt::main]
async fn main() {
    let actor = ClosureActor(0);
    let addr = actor.start();

    // mutate actor state.
    let res = addr
        .run_wait(|act, _| act.mutate_state().boxed_local())
        .await;

    assert_eq!(1, res.unwrap());

    // capture var and move it into closure
    let var = 996usize;
    let res = addr
        .run(move |act, _| act.capture_outer_var(var).boxed_local())
        .await;

    assert_eq!(var * 2, res.unwrap());

    // access context.
    let res = addr
        .run(|act, ctx| act.access_context(ctx).boxed_local())
        .await;

    assert_eq!(2, res.unwrap());

    // actor already shut down with previous access.
    let res = addr
        .run_wait(|act, _| act.mutate_state().boxed_local())
        .await;

    assert_eq!(res, Err(ActixAsyncError::Closed));
}
