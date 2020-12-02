#![allow(incomplete_features)]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

use std::future::Future;
use std::time::{Duration, Instant};

use actix_async::prelude::*;
use actix_async::supervisor::Supervisor;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;

/*
   actix-async does not provide blocking actor feature.

   This example shows how to use supervisor to run a pool of actor that run blocking jobs in async
   context.

   It has more overhead than pure blocking thread pool due to the cost of async runtime.
   In exchange the design of lib is simpler and have a good mix usage of blocking and async.
*/

struct BlockingActor;
actor!(BlockingActor);

struct Msg;
message!(Msg, ());

impl Handler<Msg> for BlockingActor {
    type Future<'res> = impl Future<Output = ()> + 'res;
    type FutureWait<'res> = impl Future<Output = ()> + 'res;

    fn handle<'act, 'ctx, 'res>(&'act self, _: Msg, _: &'ctx Context<Self>) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        async { unimplemented!() }
    }

    // since we are running pure blocking code. There is no point using concurrent handler
    // at all. use handle_wait would do just fine.
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        _: Msg,
        ctx: &'ctx mut Context<Self>,
    ) -> Self::FutureWait<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        async move {
            // use sleep to simulate heavy blocking computation.
            std::thread::sleep(Duration::from_millis(1));

            // sadly the code below would have no chance to run correctly.
            // due to long time of blocking of thread.
            let now = Instant::now();
            ctx.run_later(Duration::from_millis(1), move |_, _| {
                Box::pin(async move {
                    println!("delayed task took {:?} to run", now.elapsed());
                })
            });
        }
    }
}

#[actix_rt::main]
async fn main() {
    // construct a supervisor with 2 worker threads.
    // *. Supervisor runs on actix-runtime (uses tokio-runtime under the hood).
    let supervisor = Supervisor::new(2);

    // start 2 instance of BlockingActor in supervisor.
    let addr = supervisor.start_in_arbiter(2, |_| BlockingActor);

    // send 200 messages concurrently.
    let mut fut = FuturesUnordered::new();
    for _ in 0..200 {
        fut.push(addr.wait(Msg));
    }

    let now = Instant::now();
    while fut.next().await.is_some() {}

    // since we have 2 workers for 1 ms blocking job. the total time taken should be slightly above
    // 100 ms.
    assert!(now.elapsed() < Duration::from_millis(150));
    println!("took {:?} to finish", now.elapsed());
}
