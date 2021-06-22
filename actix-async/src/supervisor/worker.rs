use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use std::thread::JoinHandle;

use tokio::{runtime::Handle, task::LocalSet};

use crate::util::{
    channel::Receiver,
    futures::{BoxFuture, Stream},
};

/// Worker runs on a spawned thread and handle
#[derive(Debug)]
pub(super) struct Worker {
    join_handle: JoinHandle<()>,
}

impl Worker {
    pub(super) fn new(rx: Receiver<BoxFuture<'static, ()>>, tokio_handle: Handle) -> Self {
        let join_handle = std::thread::spawn(move || {
            tokio_handle.block_on(LocalSet::new().run_until(WorkerFuture { rx }))
        });

        Self { join_handle }
    }

    pub(super) fn stop(self) {
        self.join_handle.join().unwrap();
    }
}

struct WorkerFuture {
    rx: Receiver<BoxFuture<'static, ()>>,
}

impl Future for WorkerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match Pin::new(&mut this.rx).poll_next(cx) {
            Poll::Ready(Some(msg)) => {
                tokio::task::spawn_local(msg);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(None) => return Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}
