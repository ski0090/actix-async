use core::future::{ready, Future};
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context as StdContext, Poll};

use alloc::vec::Vec;

use actix_rt::Arbiter;

use crate::actor::Actor;
use crate::address::Addr;
use crate::context::{Context, ContextFuture};
use crate::util::channel::channel;
use crate::util::smart_pointer::RefCounter;

/// Supervisor can start multiple instance of same actor on multiple `actix_rt::Arbiter`.
/// Instances of same actor share the same `Addr` and steal work from a mpmc bounded channel.
///
/// It also guards the actor's drop and partially recover them from panic.
/// (Due to no task level panic catch. Cache futures/streams in `Context` will be cleared after
/// recovery)
///
/// *. `Supervisor` is enabled with `actix-rt` feature flag
pub struct Supervisor {
    arb: Vec<Arbiter>,
    next: RefCounter<AtomicUsize>,
}

impl Clone for Supervisor {
    fn clone(&self) -> Self {
        Self {
            arb: self.arb.to_vec(),
            next: self.next.clone(),
        }
    }
}

impl Supervisor {
    /// Construct a new supervisor with the given worker count. Every worker would run on it's
    /// own thread and can have multiple actors run on it.
    ///
    /// *. It's suggested only one `Supervisor` instance is used for the entire program.
    pub fn new(workers: usize) -> Self {
        Self {
            arb: (0..workers).map(|_| Arbiter::new()).collect(),
            next: RefCounter::new(AtomicUsize::new(0)),
        }
    }

    /// Start multiple actor instance with `Supervisor`. These instances would start in
    /// `actix_rt::Arbiter` using a simple round robin schedule.
    ///
    /// # example:
    /// ```rust
    /// #![allow(incomplete_features)]
    /// #![feature(generic_associated_types)]
    /// #![feature(type_alias_impl_trait)]
    ///
    /// use std::future::Future;
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
    /// impl Handler<Msg> for TestActor {
    ///     type Future<'res> = impl Future<Output = ThreadId>;
    ///     type FutureWait<'res> = impl Future<Output = ThreadId>;
    ///
    ///     fn handle<'act, 'ctx, 'res>(
    ///         &'act self,
    ///         _: Msg,
    ///         _: &'ctx Context<Self>
    ///     ) -> Self::Future<'res>
    ///     where
    ///         'act: 'res,
    ///         'ctx: 'res,
    ///     {
    ///         async { unimplemented!() }
    ///     }
    ///
    ///     fn handle_wait<'act, 'ctx, 'res>(
    ///         &mut self,
    ///         _: Msg,
    ///         _: &mut Context<Self>
    ///     ) -> Self::FutureWait<'res>
    ///     where
    ///         'act: 'res,
    ///         'ctx: 'res,
    ///     {
    ///         async {
    ///            actix_rt::time::sleep(Duration::from_millis(1)).await;
    ///            // return the current thread id of actor.
    ///            std::thread::current().id()
    ///         }
    ///     }
    /// }
    ///
    /// #[actix_rt::main]
    /// async fn main() {
    ///     // start a supervisor with 4 worker arbiters.
    ///     let supervisor = Supervisor::new(4);
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
    ///
    ///     // supervisor is thread safe and cloneable.
    ///     let s = supervisor.clone();
    ///     std::thread::spawn(move || {
    ///         let s = s;
    ///     });
    /// }
    /// ```
    pub fn start_in_arbiter<A: Actor, F>(&self, count: usize, f: F) -> Addr<A>
    where
        F: FnOnce(&mut Context<A>) -> A + Copy + Send + Sync + 'static,
    {
        self.start_in_arbiter_async(count, move |ctx| ready(f(ctx)))
    }

    /// async version of start_in_arbiter.
    pub fn start_in_arbiter_async<A: Actor, F, Fut>(&self, count: usize, f: F) -> Addr<A>
    where
        F: FnOnce(&mut Context<A>) -> Fut + Copy + Send + Sync + 'static,
        Fut: Future<Output = A>,
    {
        let (tx, rx) = channel(A::size_hint() * count);

        (0..count).for_each(|_| {
            let rx = rx.clone();
            let off = self.next.fetch_add(1, Ordering::Relaxed) % self.arb.len();

            self.arb[off].exec_fn(move || {
                A::spawn(async move {
                    let mut ctx = Context::new(rx);

                    let act = f(&mut ctx).await;

                    SupervisorFut {
                        fut: Some(ContextFuture::new(act, ctx)),
                    }
                    .await;
                });
            });
        });

        Addr::new(tx)
    }
}

struct SupervisorFut<A: Actor> {
    fut: Option<ContextFuture<A>>,
}

impl<A: Actor> Default for SupervisorFut<A> {
    fn default() -> Self {
        Self { fut: None }
    }
}

impl<A: Actor> Drop for SupervisorFut<A> {
    fn drop(&mut self) {
        if self.fut.as_ref().unwrap().is_running() {
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
        // SAFETY:
        // ContextFuture<A> is only PhantomPinned to make sure it's not moved.
        // Supervisor does not move it until it goes into Drop phase.
        let fut = unsafe {
            let fut = self.as_mut().get_unchecked_mut().fut.as_mut().unwrap();
            Pin::new_unchecked(fut)
        };

        match fut.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) if self.fut.as_ref().unwrap().is_running() => self.poll(cx),
            Poll::Ready(()) => Poll::Ready(()),
        }
    }
}

impl<A: Actor> ContextFuture<A> {
    pub(crate) fn is_running(&self) -> bool {
        self.cap.ctx().is_running()
    }

    #[cold]
    fn clear_cache(&mut self) {
        // SAFETY:
        // CacheRef and CacheMut have reference of Context's act and ctx field.
        // It's important to drop all of them when recovering from panic.
        self.cache_mut.clear();
        self.cache_ref.clear();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn supervisor_bound() {
        fn send_sync_clone<T: Send + Sync + Clone + 'static>() {};

        send_sync_clone::<Supervisor>();
    }
}
