use core::task::Waker;

use alloc::{collections::LinkedList, sync::Arc, task::Wake};
use spin::Mutex;

pub(crate) struct ActorWaker {
    queue: WakeQueue,
    idx: usize,
    waker: Waker,
}

impl ActorWaker {
    #[inline]
    pub(crate) fn new(queued: &WakeQueue, idx: usize, waker: &Waker) -> Arc<Self> {
        Arc::new(Self {
            queue: WakeQueue::clone(queued),
            idx,
            waker: Waker::clone(waker),
        })
    }
}

impl Wake for ActorWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
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
pub(crate) struct WakeQueue(pub(crate) Arc<Mutex<LinkedList<usize>>>);

impl WakeQueue {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(LinkedList::new())))
    }

    #[inline(always)]
    pub(crate) fn enqueue(&self, idx: usize) {
        self.0.lock().push_back(idx);
    }
}
