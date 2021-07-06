use core::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    task::{Context as StdContext, Poll},
};

use alloc::{boxed::Box, vec::Vec};
use slab::Slab;

use super::actor::{Actor, ActorState};
use super::context::Context;
use super::handler::MessageHandler;
use super::message::{ActorMessage, FutureMessage, StreamMessage};
use super::util::{
    channel::Receiver,
    futures::{poll_fn, yield_now, LocalBoxFuture, Stream},
};
use super::waker::{ActorWaker, WakeQueue};

pub(crate) struct ContextInner<A: Actor> {
    pub(crate) state: Cell<ActorState>,
    pub(crate) future_cache: RefCell<Vec<FutureMessage<A>>>,
    pub(crate) stream_cache: RefCell<Vec<StreamMessage<A>>>,
    pub(crate) rx: RefCell<Receiver<ActorMessage<A>>>,
}

impl<A: Actor> ContextInner<A> {
    pub(crate) fn new(rx: Receiver<ActorMessage<A>>) -> Self {
        Self {
            state: Cell::new(ActorState::Stop),
            future_cache: RefCell::new(Vec::with_capacity(8)),
            stream_cache: RefCell::new(Vec::with_capacity(8)),
            rx: RefCell::new(rx),
        }
    }

    #[inline]
    pub(crate) fn as_ref(&self) -> Context<'_, A> {
        Context::new(self)
    }
}

pub struct ContextFuture<A: Actor> {
    act: A,
    ctx: ContextInner<A>,
    queue: WakeQueue,
}

impl<A: Actor> ContextFuture<A> {
    pub(crate) async fn start<F, Fut>(f: F, ctx: ContextInner<A>) -> Self
    where
        F: for<'c> FnOnce(Context<'c, A>) -> Fut + 'static,
        Fut: Future<Output = A>,
    {
        let act = f(ctx.as_ref()).await;

        Self {
            act,
            ctx,
            queue: WakeQueue::new(),
        }
    }
}

use tokio::select;

async fn poll_stream<A: Actor>(stream_cache: &RefCell<Vec<StreamMessage<A>>>) -> ActorMessage<A> {
    poll_fn(|cx| {
        let mut stream = stream_cache.borrow_mut();
        let mut i = 0;
        while i < stream.len() {
            match Pin::new(&mut stream[i]).poll_next(cx) {
                Poll::Ready(Some(msg)) => return Poll::Ready(msg),
                Poll::Ready(None) => {
                    stream.swap_remove(i);
                }
                Poll::Pending => i += 1,
            }
        }

        Poll::Pending
    })
    .await
}

async fn poll_future<A: Actor>(future_cache: &RefCell<Vec<FutureMessage<A>>>) -> ActorMessage<A> {
    poll_fn(|cx| {
        let mut cache = future_cache.borrow_mut();
        let mut i = 0;
        while i < cache.len() {
            match Pin::new(&mut cache[i]).poll(cx) {
                Poll::Ready(msg) => {
                    cache.swap_remove(i);

                    if let Some(msg) = msg {
                        return Poll::Ready(msg);
                    }

                    // Message is canceled by ContextJoinHandle. Ignore it.
                }
                Poll::Pending => i += 1,
            }
        }

        Poll::Pending
    })
    .await
}

struct TaskRef<'a> {
    task: Slab<LocalBoxFuture<'a, ()>>,
    queue: &'a WakeQueue,
}

impl<'a> TaskRef<'a> {
    #[inline(always)]
    fn new<A: Actor>(queue: &'a WakeQueue) -> Self {
        Self {
            task: Slab::with_capacity(A::size_hint()),
            queue,
        }
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.task.is_empty()
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.task.len()
    }

    fn add_task(&mut self, task: LocalBoxFuture<'a, ()>) {
        let idx = self.task.insert(task);
        self.queue.enqueue(idx);
    }

    async fn poll_task(&mut self) {
        let task_ref = &mut self.task;
        let queue = &self.queue;
        poll_fn(|cx| {
            let len = task_ref.len();
            let mut polled = 0;

            while let Some(idx) = queue.try_lock().and_then(|mut l| l.pop_front()) {
                if let Some(task) = task_ref.get_mut(idx) {
                    // construct actor waker from the waker actor received.
                    let waker = ActorWaker::new(queue, idx, cx.waker()).into();
                    let cx = &mut StdContext::from_waker(&waker);
                    // prepare to remove the resolved tasks.
                    if task.as_mut().poll(cx).is_ready() {
                        task_ref.remove(idx);
                    }
                }
                polled += 1;

                // TODO: there is a race condition happening so a hard break is scheduled.
                // tokio task budget could be the cause of this but it's not possible to force
                // an unconstrained task for generic runtime.
                if polled == len {
                    return Poll::Ready(());
                }
            }

            Poll::Pending
        })
        .await
    }

    #[inline(never)]
    async fn graceful_resolve(&mut self) {
        while !self.is_empty() {
            self.poll_task().await;
            yield_now().await;
        }
    }
}

struct TaskMut<A: Actor>(Option<Box<dyn MessageHandler<A> + Send>>);

impl<A: Actor> TaskMut<A> {
    fn new() -> Self {
        Self(None)
    }

    #[inline(always)]
    fn add_task(&mut self, msg: Box<dyn MessageHandler<A> + Send>) {
        self.0 = Some(msg);
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    #[inline(always)]
    fn take(&mut self) -> Option<Box<dyn MessageHandler<A> + Send>> {
        self.0.take()
    }
}

impl<A: Actor> ContextFuture<A> {
    pub async fn run(mut self) {
        let ContextFuture {
            ctx,
            queue,
            ref mut act,
            ..
        } = self;

        act.on_start(ctx.as_ref()).await;
        ctx.state.set(ActorState::Running);

        let mut notify = None;

        let task_mut = &mut TaskMut::new();

        'task: loop {
            match task_mut.take() {
                Some(mut msg) => msg.handle_wait(act, ctx.as_ref()).await,
                None => {
                    let task_ref = &mut TaskRef::new::<A>(&queue);

                    loop {
                        match ctx.state.get() {
                            ActorState::StopGraceful => {
                                task_ref.graceful_resolve().await;
                                break 'task;
                            }
                            ActorState::Stop => break 'task,
                            ActorState::Running if !task_mut.is_empty() && task_ref.is_empty() => {
                                continue 'task
                            }
                            _ => {}
                        }

                        select! {
                            biased;
                            msg = poll_fn(|cx| Pin::new(&mut *ctx.rx.borrow_mut()).poll_next(cx)), if task_mut.is_empty() && task_ref.len() < A::size_hint() => {
                                match msg {
                                    Some(ActorMessage::Ref(mut msg)) => {
                                        let task = msg.handle(act, ctx.as_ref());
                                        task_ref.add_task(task);
                                    },
                                    Some(ActorMessage::Mut(msg)) => task_mut.add_task(msg),
                                    Some(ActorMessage::State(state, tx)) => {
                                        ctx.state.set(state);
                                        notify = Some(tx);
                                    }
                                    None => ctx.state.set(ActorState::Stop),
                                };
                            },
                            // see comment in poll_ref function.
                            // when a hard break happen yield to executor.
                            _ = task_ref.poll_task(), if !task_ref.is_empty() => yield_now().await,
                            msg = poll_stream(&ctx.stream_cache), if task_mut.is_empty() => {
                                match msg {
                                    ActorMessage::Ref(mut msg) => {
                                        let task = msg.handle(act, ctx.as_ref());
                                        task_ref.add_task(task);
                                    },
                                    ActorMessage::Mut(msg) => task_mut.add_task(msg),
                                    _ => unreachable!()
                                }
                            },
                            msg = poll_future(&ctx.future_cache), if task_mut.is_empty() => {
                                match msg {
                                    ActorMessage::Ref(mut msg) => {
                                        let task = msg.handle(act, ctx.as_ref());
                                        task_ref.add_task(task);
                                    },
                                    ActorMessage::Mut(msg) => task_mut.add_task(msg),
                                    _ => unreachable!()
                                }
                            },
                        }
                    }
                }
            }
        }

        if ctx.state.get() == ActorState::StopGraceful {
            if let Some(mut msg) = task_mut.take() {
                msg.handle_wait(act, ctx.as_ref()).await
            }
        }

        act.on_stop(ctx.as_ref()).await;

        if let Some(notify) = notify {
            let _ = notify.send(());
        }
    }
}
