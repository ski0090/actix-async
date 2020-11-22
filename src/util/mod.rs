pub(crate) mod channel {
    pub(crate) use async_channel::{bounded as channel, Receiver, SendError, Sender, TryRecvError};
    pub(crate) use async_oneshot::{
        oneshot as oneshot_channel, Closed as OneshotRecvError, Receiver as OneshotReceiver,
        Sender as OneshotSender,
    };
}

pub(crate) mod futures;

pub(crate) mod smart_pointer {
    pub(crate) type RefCounter<T> = std::sync::Arc<T>;
    pub(crate) type WeakRefCounter<T> = std::sync::Weak<T>;
}
