pub(crate) mod channel {
    pub(crate) use tokio::sync::mpsc::error::{SendError, TryRecvError};
    pub(crate) use tokio::sync::mpsc::{channel, Receiver, Sender};
    pub(crate) use tokio::sync::oneshot::error::RecvError as OneshotRecvError;
    pub(crate) use tokio::sync::oneshot::{
        channel as oneshot_channel, Receiver as OneshotReceiver, Sender as OneshotSender,
    };
}

pub(crate) mod futures;

pub(crate) mod smart_pointer {
    pub(crate) type RefCounter<T> = std::sync::Arc<T>;
    pub(crate) type WeakRefCounter<T> = std::sync::Weak<T>;
}
