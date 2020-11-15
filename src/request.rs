use core::future::Future;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use tokio::sync::mpsc::error::SendError;
use tokio::sync::oneshot;

use crate::error::ActixAsyncError;
use crate::runtime::RuntimeService;

/// default timeout for sending message
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Request to actor with timeout setting.
#[pin_project::pin_project]
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

impl<RT: RuntimeService, Fut, Res> MessageRequest<RT, Fut, Res> {
    pub(crate) fn new(fut: Fut, rx: oneshot::Receiver<Res>) -> Self {
        Self {
            fut,
            sent: false,
            rx,
            timeout: RT::sleep(DEFAULT_TIMEOUT),
        }
    }

    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = RT::sleep(dur);
        self
    }
}

impl<RT, Fut, Res, T> Future for MessageRequest<RT, Fut, Res>
where
    RT: RuntimeService,
    Fut: Future<Output = Result<(), SendError<T>>>,
{
    type Output = Result<Res, ActixAsyncError>;

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if !*this.sent {
            match this.fut.poll(cx) {
                Poll::Ready(Ok(())) => *this.sent = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
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
