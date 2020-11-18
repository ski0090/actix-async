use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

pub(crate) use futures_core::stream::Stream;

use crate::util::channel::{oneshot_channel, OneshotReceiver, OneshotSender};

pub type JoinedFutures = Vec<LocalBoxedFuture<'static, ()>>;
pub type LocalBoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(crate) fn cancelable<Fut, FutCancel>(
    fut: Fut,
    on_cancel: FutCancel,
) -> (CancelableFuture<Fut, FutCancel>, OneshotSender<()>) {
    let (tx, rx) = oneshot_channel();

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

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().stream).poll_next(cx)
    }
}

pub(crate) fn join<'a>(fut: &'a mut Vec<LocalBoxedFuture<'static, ()>>) -> Join<'a> {
    Join { fut }
}

pub(crate) struct Join<'a> {
    fut: &'a mut Vec<LocalBoxedFuture<'static, ()>>,
}

impl Future for Join<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        for i in 0..this.fut.len() {
            if this.fut[i].as_mut().poll(cx).is_ready() {
                this.fut.swap_remove(i);
            }
        }

        if this.fut.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
