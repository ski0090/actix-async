use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::util::channel::{oneshot_channel, OneshotReceiver, OneshotSender};

pub(crate) fn cancelable_future<Fut, FutCancel>(
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
                Poll::Ready(Ok(())) => {
                    *this.canceled = true;
                    *this.rx = None;
                    this.on_cancel.poll(cx)
                }
                Poll::Ready(Err(_)) => {
                    *this.rx = None;
                    this.fut.poll(cx)
                }
                Poll::Pending => this.fut.poll(cx),
            },
            None if *this.canceled => this.on_cancel.poll(cx),
            None => this.fut.poll(cx),
        }
    }
}
