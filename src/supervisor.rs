use core::future::{ready, Future};
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};

use alloc::vec::Vec;

use actix_rt::Arbiter;

use crate::actor::{Actor, CHANNEL_CAP};
use crate::address::Addr;
use crate::context::{Context, ContextWithActor};
use crate::util::channel::channel;

/// Supervisor can start multiple instance of same actor on multiple `actix_rt::Arbiter`.
/// Instances of same actor share the same `Addr` and use would steal work from it for.
///
/// *. `Supervisor` is enabled with `actix-rt` feature flag
pub struct Supervisor {
    arb: Vec<Arbiter>,
    next: usize,
}

impl Supervisor {
    /// Construct a new supervisor with the given worker count. Every worker would run on it's own
    /// thread and can have multiple actors run on it.
    ///
    /// *. It's suggested only one `Supervisor` instance is used for the entire program.
    pub fn new(workers: usize) -> Self {
        Self {
            arb: (0..workers).map(|_| Arbiter::new()).collect(),
            next: 0,
        }
    }

    /// Start multiple actor instance with `Supervisor`. These instances would start in
    /// `actix_rt::Arbiter` using a simple round robin schedule.
    ///
    /// #example:
    /// ```rust
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    /// use std::thread::ThreadId;
    /// use std::time::Duration;
    ///
    /// use actix_async::prelude::*;
    /// use actix_async::supervisor::Supervisor;
    /// use futures_util::stream::{FuturesUnordered, StreamExt};
    ///
    /// struct TestActor;
    /// actor!(TestActor);
    ///
    /// struct Msg;
    /// message!(Msg, ThreadId);
    ///
    /// #[async_trait::async_trait(?Send)]
    /// impl Handler<Msg> for TestActor {
    ///     async fn handle(&self, _: Msg, _: &Context<Self>) -> ThreadId {
    ///         unimplemented!()
    ///     }
    ///
    ///     async fn handle_wait(&mut self, _: Msg, _: &mut Context<Self>) -> ThreadId {
    ///         actix_rt::time::sleep(Duration::from_millis(1)).await;
    ///         // return the current thread id of actor.
    ///         std::thread::current().id()
    ///     }
    /// }
    ///
    /// #[actix_rt::main]
    /// async fn main() {
    ///     // start a supervisor with 4 worker arbiters.
    ///     let mut supervisor = Supervisor::new(4);
    ///
    ///     // start 4 TestActor instances in supervisor.
    ///     let addr = supervisor.start_in_arbiter(4, move |_| TestActor);
    ///
    ///     // construct multiple futures to TestActor's handler
    ///     let mut fut = FuturesUnordered::new();
    ///     for _ in 0..1000 {
    ///         fut.push(addr.wait(Msg));
    ///     }
    ///     
    ///     // collect the result and retain unique thread id.
    ///     let len = fut
    ///         .collect::<Vec<Result<ThreadId, ActixAsyncError>>>()
    ///         .await
    ///         .into_iter()
    ///         .fold(Vec::new(), |mut res, id| {
    ///             let id = id.unwrap();
    ///             if !res.contains(&id) {
    ///                 res.push(id);
    ///             }
    ///             res
    ///         })
    ///         .len();
    ///
    ///     // we should have at least 2-4 unique thread id here.
    ///     assert!(len > 1);
    /// }
    /// ```
    pub fn start_in_arbiter<A: Actor, F>(&mut self, count: usize, f: F) -> Addr<A>
    where
        F: FnOnce(&mut Context<A>) -> A + Copy + Send + Sync + 'static,
    {
        self.start_in_arbiter_async(count, move |ctx| ready(f(ctx)))
    }

    /// async version of start_in_arbiter.
    pub fn start_in_arbiter_async<A: Actor, F, Fut>(&mut self, count: usize, f: F) -> Addr<A>
    where
        F: FnOnce(&mut Context<A>) -> Fut + Copy + Send + Sync + 'static,
        Fut: Future<Output = A>,
    {
        let (tx, rx) = channel(CHANNEL_CAP);

        (0..count).for_each(|_| {
            let rx = rx.clone();
            let off = self.next % self.arb.len();

            self.arb[off].exec_fn(move || {
                A::spawn(async move {
                    let mut ctx = Context::new(rx);

                    let actor = f(&mut ctx).await;

                    SupervisorFut {
                        fut: Some(ContextWithActor::new(actor, ctx)),
                    }
                    .await;
                });
            });
            self.next += 1;
        });

        Addr::new(tx)
    }
}

struct SupervisorFut<A: Actor> {
    fut: Option<ContextWithActor<A>>,
}

impl<A: Actor> Default for SupervisorFut<A> {
    fn default() -> Self {
        Self { fut: None }
    }
}

impl<A: Actor> Drop for SupervisorFut<A> {
    fn drop(&mut self) {
        if !self.fut.as_ref().unwrap().is_stopped() {
            let mut fut = core::mem::take(self);

            // TODO: catch panic on task level and not clear all.
            fut.fut.as_mut().unwrap().clear_cache();

            A::spawn(fut);
        }
    }
}

impl<A: Actor> Future for SupervisorFut<A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match Pin::new(self.fut.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                if self.fut.as_ref().unwrap().is_stopped() {
                    Poll::Ready(())
                } else {
                    self.poll(cx)
                }
            }
        }
    }
}
