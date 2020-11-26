use core::future::Future;
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use pin_project_lite::pin_project;

use crate::error::ActixAsyncError;
use crate::runtime::RuntimeService;
use crate::types::ActixResult;
use crate::util::channel::OneshotReceiver;

/// default timeout for sending message
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

pin_project! {
    /// Message request to actor with timeout setting.
    #[project = MessageRequestProj]
    pub enum MessageRequest<RT, Fut, Res>
    where
        RT: RuntimeService,
    {
        Request {
            #[pin]
            fut: Fut,
            rx: Option<OneshotReceiver<Res>>,
            timeout: RT::Sleep,
            timeout_response: Option<RT::Sleep>
        },
        Response {
            rx: OneshotReceiver<Res>,
            timeout_response: Option<RT::Sleep>
        }
    }
}

const TIMEOUT_CONFIGURABLE: &str = "Timeout is not configurable after Request Future is polled";

impl<RT, Fut, Res> MessageRequest<RT, Fut, Res>
where
    RT: RuntimeService,
{
    pub(crate) fn new(fut: Fut, rx: OneshotReceiver<Res>) -> Self {
        MessageRequest::Request {
            fut,
            rx: Some(rx),
            timeout: RT::sleep(DEFAULT_TIMEOUT),
            timeout_response: None,
        }
    }

    /// set the timeout duration for request.
    ///
    /// Default to 10 seconds.
    pub fn timeout(self, dur: Duration) -> Self {
        match self {
            MessageRequest::Request {
                fut,
                rx,
                timeout_response,
                ..
            } => MessageRequest::Request {
                fut,
                rx,
                timeout: RT::sleep(dur),
                timeout_response,
            },
            _ => unreachable!(TIMEOUT_CONFIGURABLE),
        }
    }

    /// set the timeout duration for response.(start from the message arrives at actor)
    ///
    /// Default to no timeout.
    pub fn timeout_response(self, dur: Duration) -> Self {
        match self {
            MessageRequest::Request {
                fut, rx, timeout, ..
            } => MessageRequest::Request {
                fut,
                rx,
                timeout,
                timeout_response: Some(RT::sleep(dur)),
            },
            _ => unreachable!(TIMEOUT_CONFIGURABLE),
        }
    }
}

impl<RT, Fut, Res> Future for MessageRequest<RT, Fut, Res>
where
    RT: RuntimeService,
    Fut: Future<Output = ActixResult<()>>,
{
    type Output = ActixResult<Res>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match self.as_mut().project() {
            MessageRequestProj::Request {
                fut,
                rx,
                timeout,
                timeout_response,
            } => match fut.poll(cx) {
                Poll::Ready(Ok(())) => {
                    let rx = rx.take().unwrap();
                    let timeout_response = timeout_response.take();

                    self.set(MessageRequest::Response {
                        rx,
                        timeout_response,
                    });
                    self.poll(cx)
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => match Pin::new(timeout).poll(cx) {
                    Poll::Ready(_) => Poll::Ready(Err(ActixAsyncError::SendTimeout)),
                    Poll::Pending => Poll::Pending,
                },
            },
            MessageRequestProj::Response {
                rx,
                timeout_response,
            } => match Pin::new(rx).poll(cx) {
                Poll::Ready(res) => Poll::Ready(Ok(res?)),
                Poll::Pending => {
                    if let Some(ref mut timeout) = timeout_response {
                        if Pin::new(timeout).poll(cx).is_ready() {
                            return Poll::Ready(Err(ActixAsyncError::ReceiveTimeout));
                        }
                    }
                    Poll::Pending
                }
            },
        }
    }
}
