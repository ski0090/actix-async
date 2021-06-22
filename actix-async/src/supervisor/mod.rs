mod worker;

use core::{future::Future, mem, time::Duration};

use tokio::{runtime::Handle, select};

use super::actor::Actor;
use super::address::Addr;
use super::context::Context;
use super::context_future::{ContextFuture, ContextInner};
use super::util::{
    channel::{channel, Sender},
    futures::BoxFuture,
    smart_pointer::{Lock, RefCounter},
};

use self::worker::Worker;

#[derive(Clone, Debug)]
pub struct Supervisor {
    join_handles: RefCounter<Lock<Vec<Worker>>>,
    tx: Sender<BoxFuture<'static, ()>>,
    shutdown_timeout: Duration,
}

impl Supervisor {
    pub fn builder() -> SupervisorBuilder {
        SupervisorBuilder::new()
    }

    pub async fn stop(self) -> bool {
        if self.tx.close() {
            let handles = mem::take(&mut *self.join_handles.lock());

            if handles.is_empty() {
                false
            } else {
                let mut stop_handle =
                    tokio::task::spawn_blocking(|| handles.into_iter().for_each(|h| h.stop()));
                let timeout = Box::pin(tokio::time::sleep(self.shutdown_timeout));
                select! {
                    _ = &mut stop_handle => true,
                    _ = timeout => {
                        stop_handle.abort();
                        false
                    }
                }
            }
        } else {
            false
        }
    }

    pub async fn start<F, Fut, A>(&self, num: usize, func: F) -> Addr<A>
    where
        F: for<'c> Fn(Context<'c, A>) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = A> + 'static,
        A: Actor,
    {
        let (tx, rx) = channel(A::size_hint() * num);

        let addr = Addr::new(tx);

        for _ in 0..num {
            let rx = rx.clone();
            let func = func.clone();

            // TODO: handle error.
            let _ = self
                .tx
                .send(Box::pin(async move {
                    let ctx = ContextInner::new(rx);
                    tokio::task::spawn_local(async move {
                        let fut = ContextFuture::start(func, ctx).await;
                        fut.await
                    });
                }) as _)
                .await;
        }

        addr
    }
}

pub struct SupervisorBuilder {
    workers: usize,
    shutdown_timeout: Duration,
}

impl SupervisorBuilder {
    pub fn new() -> Self {
        SupervisorBuilder {
            workers: 4,
            shutdown_timeout: Duration::from_secs(30),
        }
    }

    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    pub fn shutdown_timeout(mut self, dur: Duration) -> Self {
        self.shutdown_timeout = dur;
        self
    }

    pub fn build(self) -> Supervisor {
        let (tx, rx) = channel(self.workers);

        let mut workers = Vec::new();

        for _ in 0..self.workers {
            let rx = rx.clone();
            let handle = Handle::current();

            let worker = Worker::new(rx, handle);

            workers.push(worker);
        }

        Supervisor {
            join_handles: RefCounter::new(Lock::new(workers)),
            tx,
            shutdown_timeout: self.shutdown_timeout,
        }
    }
}
