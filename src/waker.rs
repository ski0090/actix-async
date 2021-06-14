use core::{
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use alloc::{sync::Arc, task::Wake};

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

        let old = queue.enqueue(*idx);

        // If taks is already unqueued it means task is waked up by other caller.
        // skip the wake up and do nothing in this case.
        if old.is_queued(*idx) {
            waker.wake_by_ref();
        }
    }
}

#[derive(Clone)]
pub(crate) struct WakeQueue(Arc<AtomicUsize>);

impl WakeQueue {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    #[inline(always)]
    pub(crate) fn enqueue(&self, idx: usize) -> WakeQueueState {
        WakeQueueState(self.0.fetch_or(idx, Ordering::AcqRel))
    }

    #[inline(always)]
    pub(crate) fn load(&self) -> WakeQueueState {
        WakeQueueState(self.0.load(Ordering::Acquire))
    }

    #[inline(always)]
    pub(crate) fn store(&self, state: WakeQueueState) {
        self.0.store(state.0, Ordering::Release)
    }
}

pub(crate) struct WakeQueueState(usize);

impl Deref for WakeQueueState {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl WakeQueueState {
    #[inline(always)]
    pub(crate) fn is_queued(&self, idx: usize) -> bool {
        self.0 & idx == idx
    }

    #[inline(always)]
    pub(crate) fn dequeue(&mut self, idx: usize) {
        self.0 &= !idx;
    }
}
