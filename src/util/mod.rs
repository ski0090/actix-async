mod async_channel;
mod async_oneshot;
pub(crate) mod slab;

pub(crate) mod channel {
    pub(crate) use super::async_channel::{
        bounded as channel, Receiver, SendFuture, Sender, WeakSender,
    };
    pub(crate) use super::async_oneshot::{oneshot, OneshotReceiver, OneshotSender};
}

pub(crate) mod futures;

pub(crate) mod smart_pointer {
    pub(crate) type RefCounter<T> = alloc::sync::Arc<T>;
    pub(crate) type WeakRefCounter<T> = alloc::sync::Weak<T>;
}
