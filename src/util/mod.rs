mod async_channel;
mod async_oneshot;

pub(crate) mod channel {
    pub(crate) use super::async_channel::{channel, Receiver, SendFuture, Sender, WeakSender};
    pub(crate) use super::async_oneshot::{oneshot, OneshotReceiver, OneshotSender};
}

pub(crate) mod futures;

pub(crate) mod smart_pointer {
    pub(crate) type RefCounter<T> = alloc::sync::Arc<T>;
    pub(crate) type WeakRefCounter<T> = alloc::sync::Weak<T>;

    pub(crate) type Lock<T> = spin::Mutex<T>;
    pub(crate) type LockGuard<'g, T> = spin::MutexGuard<'g, T>;
}
