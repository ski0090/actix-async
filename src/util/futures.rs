use core::future::Future;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};

use alloc::boxed::Box;

pub(crate) use futures_core::Stream;

use crate::util::channel::{oneshot, OneshotReceiver, OneshotSender};

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(crate) fn cancelable<Fut, FutCancel>(
    fut: Fut,
    on_cancel: FutCancel,
) -> (CancelableFuture<Fut, FutCancel>, OneshotSender<()>) {
    let (tx, rx) = oneshot();

    let fut = CancelableFuture {
        rx: Some(rx),
        fut,
        on_cancel,
        canceled: false,
    };

    (fut, tx)
}

pin_project_lite::pin_project! {
    pub(crate) struct CancelableFuture<Fut, FutCancel> {
        #[pin]
        fut: Fut,
        #[pin]
        on_cancel: FutCancel,
        canceled: bool,
        rx: Option<OneshotReceiver<()>>
    }
}

impl<Fut, FutCancel> Future for CancelableFuture<Fut, FutCancel>
where
    Fut: Future<Output = ()>,
    FutCancel: Future<Output = ()>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.rx {
            Some(rx) => match Pin::new(rx).poll(cx) {
                Poll::Ready(res) => {
                    *this.rx = None;
                    match res {
                        Ok(()) => {
                            *this.canceled = true;
                            this.on_cancel.poll(cx)
                        }
                        Err(_) => this.fut.poll(cx),
                    }
                }
                Poll::Pending => this.fut.poll(cx),
            },
            None if *this.canceled => this.on_cancel.poll(cx),
            None => this.fut.poll(cx),
        }
    }
}

pub(crate) fn next<S>(stream: &mut S) -> Next<'_, S> {
    Next { stream }
}

pub(crate) struct Next<'a, S> {
    stream: &'a mut S,
}

impl<S> Future for Next<'_, S>
where
    S: Stream + Unpin,
{
    type Output = Option<S::Item>;

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().stream).poll_next(cx)
    }
}
