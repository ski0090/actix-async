use core::fmt::{Debug, Display, Formatter, Result as FmtResult};

use crate::util::channel::SendError;

#[derive(PartialEq)]
pub enum ActixAsyncError {
    /// actor's channel is closed. happens when actor is shutdown.
    Closed,

    /// failed to send message to actor in time.
    SendTimeout,

    /// failed to receive result from actor in time.
    ReceiveTimeout,

    /// fail to receive result for given message. happens when actor is blocked or the
    /// thread it runs on panicked.
    Receiver,
}

impl<T> From<SendError<T>> for ActixAsyncError {
    fn from(_: SendError<T>) -> Self {
        ActixAsyncError::Closed
    }
}

impl Debug for ActixAsyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let mut fmt = f.debug_struct("ActixAsyncError");

        match self {
            ActixAsyncError::Closed => fmt
                .field("cause", &"Closed")
                .field("description", &"Actor is already closed"),
            ActixAsyncError::SendTimeout => fmt.field("cause", &"SendTimeout").field(
                "description",
                &"MessageRequest is timed out. (Failed to send message to actor in time.)",
            ),
            ActixAsyncError::ReceiveTimeout => fmt.field("cause", &"ReceiveTimeout").field(
                "description",
                &"MessageRequest is timed out. (Failed to receive result from actor in time.)",
            ),
            ActixAsyncError::Receiver => fmt
                .field("cause", &"Receive")
                .field("description", &"Fail to receive result for given message."),
        };

        fmt.finish()
    }
}

impl Display for ActixAsyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "({})", self)
    }
}
