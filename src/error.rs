use core::fmt::{Debug, Display, Formatter, Result as FmtResult};

use crate::util::channel::{OneshotRecvError, SendError};

#[derive(PartialEq)]
pub enum ActixAsyncError {
    /// actor's channel is closed. happens when actor is shutdown.
    Closed,

    /// failed to get a response in time.
    /// (The message could have reached the actor without producing a response in time).
    Timeout,

    /// actor fail to return a response for given message. happens when actor is blocked or the
    /// thread it runs on panicked.
    Response,
}

impl<T> From<SendError<T>> for ActixAsyncError {
    fn from(_: SendError<T>) -> Self {
        ActixAsyncError::Closed
    }
}

impl From<OneshotRecvError> for ActixAsyncError {
    fn from(_: OneshotRecvError) -> Self {
        ActixAsyncError::Response
    }
}

impl Debug for ActixAsyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let mut fmt = f.debug_struct("ActixAsyncError");

        match self {
            ActixAsyncError::Closed => fmt
                .field("cause", &"Closed")
                .field("description", &"Actor is already closed"),
            ActixAsyncError::Timeout => fmt
                .field("cause", &"Timeout")
                .field("description", &"MessageRequest is timed out. (The message could have reached the actor without producing a response in time)"),
            ActixAsyncError::Response => fmt
                .field("cause", &"Response")
                .field("description", &"Failed to get response from Actor"),
        };

        fmt.finish()
    }
}

impl Display for ActixAsyncError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "({})", self)
    }
}

impl std::error::Error for ActixAsyncError {}
