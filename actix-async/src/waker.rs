use core::{ops::Deref, task::Waker};

use alloc::{collections::VecDeque, task::Wake};

use super::util::smart_pointer::{Lock, RefCounter};

pub(crate) struct ActorWaker {
    queue: WakeQueue,
    idx: usize,
    waker: Waker,
}

impl ActorWaker {
    #[inline]
    pub(crate) fn new(queued: &WakeQueue, idx: usize, waker: &Waker) -> RefCounter<Self> {
        RefCounter::new(Self {
            queue: WakeQueue::clone(queued),
            idx,
            waker: Waker::clone(waker),
        })
    }
}

impl Wake for ActorWaker {
    fn wake(self: RefCounter<Self>) {
        // try to take ownership of actor waker. This would reduce the overhead
        // of task wake up if waker is not shared between multiple tasks.
        // (Which is a regular seen use case.)
        match RefCounter::try_unwrap(self) {
            Ok(ActorWaker { queue, idx, waker }) => {
                queue.enqueue(idx);
                waker.wake();
            }
            Err(this) => this.wake_by_ref(),
        }
    }

    fn wake_by_ref(self: &RefCounter<Self>) {
        let ActorWaker {
            ref queue,
            ref idx,
            ref waker,
        } = **self;

        queue.enqueue(*idx);

        waker.wake_by_ref();
    }
}

#[derive(Clone)]
pub(crate) struct WakeQueue(RefCounter<Lock<VecDeque<usize>>>);

impl Deref for WakeQueue {
    type Target = Lock<VecDeque<usize>>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl WakeQueue {
    #[inline]
    pub(crate) fn new() -> Self {
        Self(RefCounter::new(Lock::new(VecDeque::new())))
    }

    #[inline]
    pub(crate) fn enqueue(&self, idx: usize) {
        self.lock().push_back(idx);
    }
}
