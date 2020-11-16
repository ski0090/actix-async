use core::future::Future;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use tokio::sync::oneshot;

use crate::error::ActixAsyncError;
use crate::runtime::RuntimeService;
use crate::types::ActixResult;

/// default timeout for sending message
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

pin_project_lite::pin_project! {
    /// Request to actor with timeout setting.
    pub struct MessageRequest<RT, Fut, Res>
    where
        RT: RuntimeService,
    {
        #[pin]
        fut: Fut,
        sent: bool,
        rx: oneshot::Receiver<Res>,
        timeout: RT::Sleep,
    }
}

impl<RT: RuntimeService, Fut, Res> MessageRequest<RT, Fut, Res> {
    pub(crate) fn new(fut: Fut, rx: oneshot::Receiver<Res>) -> Self {
        Self {
            fut,
            sent: false,
            rx,
            timeout: RT::sleep(DEFAULT_TIMEOUT),
        }
    }

    /// set the timeout duration for request.
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = RT::sleep(dur);
        self
    }
}

impl<RT, Fut, Res> Future for MessageRequest<RT, Fut, Res>
where
    RT: RuntimeService,
    Fut: Future<Output = ActixResult<()>>,
{
    type Output = ActixResult<Res>;

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if !*this.sent {
            match this.fut.poll(cx) {
                Poll::Ready(Ok(())) => *this.sent = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    if Pin::new(this.timeout).poll(cx).is_ready() {
                        return Poll::Ready(Err(ActixAsyncError::Timeout));
                    }

                    return Poll::Pending;
                }
            }
        }

        match Pin::new(this.rx).poll(cx) {
            Poll::Ready(res) => Poll::Ready(Ok(res?)),
            Poll::Pending => {
                if Pin::new(this.timeout).poll(cx).is_ready() {
                    return Poll::Ready(Err(ActixAsyncError::Timeout));
                }

                Poll::Pending
            }
        }
    }
}
