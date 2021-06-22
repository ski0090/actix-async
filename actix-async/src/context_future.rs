use core::{
    cell::{Cell, RefCell},
    future::Future,
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
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
    channel::{OneshotSender, Receiver},
    futures::{ready, LocalBoxFuture, Stream},
};
use super::waker::{ActorWaker, WakeQueue};

type Task = LocalBoxFuture<'static, ()>;

pub(crate) struct TaskRef<A>(Slab<Task>, PhantomData<A>);

impl<A> Deref for TaskRef<A> {
    type Target = Slab<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<A> DerefMut for TaskRef<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<A: Actor> TaskRef<A> {
    fn new() -> Self {
        Self(Slab::new(), PhantomData)
    }

    #[inline(always)]
    fn add_task(&mut self, task: Task) -> usize {
        self.insert(task)
    }
}

pub(crate) struct TaskMut(Option<Task>);

impl Deref for TaskMut {
    type Target = Option<Task>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TaskMut {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl TaskMut {
    #[inline(always)]
    fn new() -> Self {
        Self(None)
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    #[inline(always)]
    fn add_task(&mut self, task: Task) {
        self.0 = Some(task);
    }
}

pub struct ContextFuture<A: Actor> {
    act: A,
    ctx: ContextInner<A>,
    queue: WakeQueue,
    pub(crate) task_mut: TaskMut,
    pub(crate) task_ref: TaskRef<A>,
    drop_notify: Option<OneshotSender<()>>,
    state: ContextState,
    extra_poll: bool,
}

pub(crate) struct ContextInner<A: Actor> {
    pub(crate) state: Cell<ActorState>,
    pub(crate) future_cache: RefCell<Vec<FutureMessage<A>>>,
    pub(crate) stream_cache: RefCell<Vec<StreamMessage<A>>>,
    pub(crate) rx: Receiver<ActorMessage<A>>,
}

impl<A: Actor> ContextInner<A> {
    pub(crate) fn new(rx: Receiver<ActorMessage<A>>) -> Self {
        Self {
            state: Cell::new(ActorState::Stop),
            future_cache: RefCell::new(Vec::with_capacity(8)),
            stream_cache: RefCell::new(Vec::with_capacity(8)),
            rx,
        }
    }

    #[inline(always)]
    pub(crate) fn as_ref(&self) -> Context<'_, A> {
        Context::new(self)
    }
}

enum ContextState {
    Starting,
    Running,
    Stopping,
}

impl<A: Actor> Unpin for ContextFuture<A> {}

impl<A: Actor> Drop for ContextFuture<A> {
    fn drop(&mut self) {
        if let Some(tx) = self.drop_notify.take() {
            let _ = tx.send(());
        }
    }
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
            task_mut: TaskMut::new(),
            task_ref: TaskRef::new(),
            drop_notify: None,
            state: ContextState::Starting,
            extra_poll: false,
        }
    }

    #[inline(always)]
    fn add_task_mut(&mut self, mut msg: Box<dyn MessageHandler<A>>) {
        let ctx = self.ctx.as_ref();
        let task = msg.handle_wait(&mut self.act, ctx);
        self.task_mut.add_task(task);
    }

    #[inline(always)]
    fn add_task_ref(&mut self, mut msg: Box<dyn MessageHandler<A>>) {
        // when adding new concurrent message we always want an extra poll to register them.
        self.extra_poll = true;
        let ctx = self.ctx.as_ref();
        let task = msg.handle(&self.act, ctx);
        let idx = self.task_ref.add_task(task);
        self.queue.enqueue(idx);
    }

    #[inline(always)]
    fn have_task(&self) -> bool {
        !self.task_ref.is_empty() || self.task_mut.is_some()
    }

    #[inline(always)]
    fn can_add_task(&self) -> bool {
        let len = self.task_ref.len();
        if self.task_mut.is_some() {
            len + 1 < A::size_hint()
        } else {
            len < A::size_hint()
        }
    }

    #[inline(always)]
    fn poll_running(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();

        // poll concurrent messages and collect task index that is ready.

        // only try to get the lock. When lock is held by others it means they are about to wake up
        // this actor future and it would be scheduled to wake up again.
        let len = this.task_ref.len();
        let mut polled = 0;

        while let Some(idx) = this.queue.try_lock().and_then(|mut l| l.pop_front()) {
            if let Some(task) = this.task_ref.get_mut(idx) {
                // construct actor waker from the waker actor received.
                let waker = ActorWaker::new(&this.queue, idx, cx.waker()).into();
                let cx = &mut StdContext::from_waker(&waker);
                // prepare to remove the resolved tasks.
                if task.as_mut().poll(cx).is_ready() {
                    this.task_ref.remove(idx);
                }
            }
            polled += 1;
            // TODO: there is a race condition happening so a hard break is scheduled.
            // tokio task budget could be the cause of this but it's not possible to force
            // an unconstrained task for generic runtime.
            if polled == len {
                cx.waker().wake_by_ref();
                break;
            }
        }

        // try to poll exclusive message.
        match this.task_mut.as_mut() {
            // still have concurrent messages. finish them.
            Some(_) if !this.task_ref.is_empty() => return Poll::Pending,
            // poll exclusive message and remove it when success.
            Some(fut_mut) => {
                ready!(fut_mut.as_mut().poll(cx));
                this.task_mut.clear();
            }
            None => {}
        }

        // reset extra_poll
        this.extra_poll = false;

        // If context is stopped we stop dealing with future and stream messages.
        if this.ctx.state.get() == ActorState::Running {
            // poll future messages
            let mut i = 0;
            while i < this.ctx.future_cache.get_mut().len() {
                let cache = this.ctx.future_cache.get_mut();
                match Pin::new(&mut cache[i]).poll(cx) {
                    Poll::Ready(msg) => {
                        cache.swap_remove(i);

                        match msg {
                            Some(ActorMessage::Ref(msg)) => {
                                this.add_task_ref(msg);
                            }
                            Some(ActorMessage::Mut(msg)) => {
                                this.add_task_mut(msg);
                                return self.poll_running(cx);
                            }
                            // Message is canceled by ContextJoinHandle. Ignore it.
                            None => {}
                            _ => unreachable!(),
                        }
                    }
                    Poll::Pending => i += 1,
                }
            }

            // poll stream message.
            let mut i = 0;
            while i < this.ctx.stream_cache.get_mut().len() {
                let mut polled = 0;

                'stream: while let Poll::Ready(res) =
                    Pin::new(&mut this.ctx.stream_cache.get_mut()[i]).poll_next(cx)
                {
                    polled += 1;
                    match res {
                        Some(ActorMessage::Ref(msg)) => {
                            this.add_task_ref(msg);
                        }
                        Some(ActorMessage::Mut(msg)) => {
                            this.add_task_mut(msg);
                            return self.poll_running(cx);
                        }
                        // stream is either canceled by ContextJoinHandle or finished.
                        None => {
                            this.ctx.stream_cache.get_mut().swap_remove(i);
                            break 'stream;
                        }
                        _ => unreachable!(),
                    }

                    // force to yield when having 16 consecutive successful poll.
                    if polled == 16 {
                        // set extra poll flag to true when force yield happens.
                        this.extra_poll = true;
                        break 'stream;
                    }
                }

                i += 1;
            }
        }

        // Actively drain receiver channel for incoming messages.
        // Do not poll on receiver when actor is at full capacity.
        while this.can_add_task() {
            match Pin::new(&mut this.ctx.rx).poll_next(cx) {
                // new concurrent message. add it to cache_ref and continue.
                Poll::Ready(Some(ActorMessage::Ref(msg))) => {
                    this.add_task_ref(msg);
                }
                // new exclusive message. add it to cache_mut. No new messages should
                // be accepted until this one is resolved.
                Poll::Ready(Some(ActorMessage::Mut(msg))) => {
                    this.add_task_mut(msg);
                    return self.poll_running(cx);
                }
                // stopping messages received.
                Poll::Ready(Some(ActorMessage::State(state, notify))) => {
                    // a one shot sender to to notify the caller shut down is complete.
                    this.drop_notify = Some(notify);
                    // close the channel.
                    this.ctx.rx.close();

                    match state {
                        ActorState::Stop => {
                            // goes to stopping state if it's a force shut down.
                            // otherwise keep the loop until we drain the channel.
                            this.state = ContextState::Stopping;
                            return self.poll_close(cx);
                        }
                        ActorState::StopGraceful => this.ctx.state.set(ActorState::StopGraceful),
                        ActorState::Running => {
                            unreachable!("Running state must not be sent through ActorMessage")
                        }
                    }
                }
                // channel is closed
                Poll::Ready(None) => {
                    return match this.ctx.state.replace(ActorState::StopGraceful) {
                        ActorState::StopGraceful | ActorState::Running => {
                            if this.extra_poll {
                                // have new concurrent message. poll another round.
                                self.poll_running(cx)
                            } else if this.have_task() {
                                // wait for unfinished messages to resolve.
                                Poll::Pending
                            } else {
                                // goes to stopping state.
                                this.state = ContextState::Stopping;
                                self.poll_close(cx)
                            }
                        }
                        ActorState::Stop => {
                            this.state = ContextState::Stopping;
                            self.poll_close(cx)
                        }
                    };
                }
                Poll::Pending => break,
            }
        }

        // have new concurrent message. poll another round.
        if this.extra_poll {
            self.poll_running(cx)
        } else {
            Poll::Pending
        }
    }

    fn poll_start(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.task_mut.as_mut() {
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.task_mut.clear();
                this.ctx.state.set(ActorState::Running);
                this.state = ContextState::Running;
                self.poll_running(cx)
            }
            None => {
                let ctx = this.ctx.as_ref();

                // SAFETY:
                // Self reference is needed.
                // on_start transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { mem::transmute(this.act.on_start(ctx)) };

                this.task_mut.add_task(task);

                self.poll_start(cx)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<()> {
        let this = self.as_mut().get_mut();
        match this.task_mut.as_mut() {
            Some(task) => {
                ready!(task.as_mut().poll(cx));
                this.task_mut.clear();
                Poll::Ready(())
            }
            None => {
                let ctx = this.ctx.as_ref();

                // SAFETY:
                // Self reference is needed.
                // on_stop transmute to static lifetime must be resolved before dropping
                // or move Context and Actor.
                let task = unsafe { mem::transmute(this.act.on_stop(ctx)) };

                this.task_mut.add_task(task);

                self.poll_close(cx)
            }
        }
    }
}

impl<A: Actor> Future for ContextFuture<A> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        match self.as_mut().get_mut().state {
            ContextState::Running => self.poll_running(cx),
            ContextState::Starting => self.poll_start(cx),
            ContextState::Stopping => self.poll_close(cx),
        }
    }
}
