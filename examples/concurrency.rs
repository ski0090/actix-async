/* Can be considered best practice for using this crate */
#![allow(incomplete_features)]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use actix_async::prelude::*;
use actix_rt::time::sleep;
use futures_intrusive::sync::LocalMutex;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;

// use smart pointers wrapping your actor state for mutation and/or share purpose.
struct MyActor {
    state_mut: RefCell<usize>,
    state_shared: Rc<usize>,
    state_mut_await: LocalMutex<usize>,
}
actor!(MyActor);

struct Msg;
message!(Msg, usize);

impl Handler<Msg> for MyActor {
    type Future<'a> = impl Future<Output = usize> + 'a;
    type FutureWait<'a> = Self::Future<'a>;

    fn handle<'act, 'ctx, 'res>(&'act self, _: Msg, _: &'ctx Context<Self>) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        async move {
            // RefCell can safely mutate actor state as long as RefMut is not held across await point.
            let mut state = self.state_mut.borrow_mut();

            *state += 1;

            // *. You must actively drop RefMut before await point. Otherwise the handle method
            // would  try to hold it for the entire scope. Leading to runtime borrow checker error.
            drop(state);

            sleep(Duration::from_millis(1)).await;

            // Rc can be cloned and used in spawned async tasks.
            let state = self.state_shared.clone();
            actix_rt::spawn(async move {
                sleep(Duration::from_millis(1)).await;
                drop(state);
            });

            // futures_intrusive::sync::LocalMutex is an async Mutex that is low cost and not
            // thread safe. It can hold mutable state across await point.
            //
            // *. This also applies to other async internal mutability primitives.
            // eg: tokio::sync::{Mutex, RwLock}
            let mut state = self.state_mut_await.lock().await;

            sleep(Duration::from_millis(1)).await;

            // We held the mutable state across await point. But it comes with a cost.
            // The actor would be blocked on this message as long as the MutexGuard is
            // held which means you lose concurrency completely in the whole process.
            //
            // So use async mutex as your last resort and try to avoid it whenever you can.
            *state += 1;

            *state
        }
    }

    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: Msg,
        ctx: &'ctx mut Context<Self>,
    ) -> Self::FutureWait<'res>
    where
        'act: 'res,
        'ctx: 'res,
    {
        // fall back to concurrent handle.
        self.handle(msg, ctx)
    }
}

#[actix_rt::main]
async fn main() {
    let act = MyActor {
        state_mut: RefCell::new(0),
        state_shared: Rc::new(0),
        state_mut_await: LocalMutex::new(0, false),
    };

    let addr = act.start();

    let mut fut = FuturesUnordered::new();

    for _ in 0..100 {
        fut.push(addr.send(Msg));
    }

    while let Some(Ok(res)) = fut.next().await {
        println!("result is {}", res);
    }

    let res = addr.stop(true).await;

    assert!(res.is_ok())
}
