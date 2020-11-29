use core::future::Future;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::task::{Context as StdContext, Poll};
use core::time::Duration;

use alloc::boxed::Box;

use crate::actor::{Actor, ActorState};
use crate::handler::{Handler, MessageHandler};
use crate::runtime::RuntimeService;
use crate::util::channel::{OneshotReceiver, OneshotSender};
use crate::util::futures::Stream;
use crate::util::smart_pointer::RefCounter;

pub trait Message: 'static {
    type Result: Send + 'static;
}

impl<M: Message> Message for RefCounter<M> {
    type Result = M::Result;
}

impl<M: Message> Message for Box<M> {
    type Result = M::Result;
}

pub(crate) struct FunctionMessage<F, R> {
    pub(crate) func: F,
    _res: PhantomData<R>,
}

impl<F: Clone, R> Clone for FunctionMessage<F, R> {
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
            _res: PhantomData,
        }
    }
}

impl<F, R> FunctionMessage<F, R> {
    pub(crate) fn new(func: F) -> Self {
        Self {
            func,
            _res: Default::default(),
        }
    }
}

impl<F, R> Message for FunctionMessage<F, R>
where
    F: 'static,
    R: Send + 'static,
{
    type Result = R;
}

pub(crate) struct FunctionMutMessage<F, R> {
    pub(crate) func: F,
    _res: PhantomData<R>,
}

impl<F: Clone, R> Clone for FunctionMutMessage<F, R> {
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
            _res: PhantomData,
        }
    }
}

impl<F, R> FunctionMutMessage<F, R> {
    pub(crate) fn new(func: F) -> Self {
        Self {
            func,
            _res: Default::default(),
        }
    }
}

impl<F, R> Message for FunctionMutMessage<F, R>
where
    F: 'static,
    R: Send + 'static,
{
    type Result = R;
}

// concrete type for dyn MessageHandler trait object that provide the message and the response
// channel.
pub(crate) struct MessageContainer<M: Message> {
    pub(crate) msg: Option<M>,
    pub(crate) tx: Option<OneshotSender<M::Result>>,
}

impl<M: Message> MessageContainer<M> {
    pub(crate) fn take(&mut self) -> (M, Option<OneshotSender<M::Result>>) {
        (self.msg.take().unwrap(), self.tx.take())
    }
}

// main type wrapper for all types of message goes to actor.
pub struct MessageObject<A>(Box<dyn MessageHandler<A>>);

/*
    SAFETY:
    Message object is construct from either `Context` or `Addr`.

    *. When it's constructed through `Addr`. The caller must make sure the `Message` type passed to
       `MessageObject::new` is `Send` bound as the object would possibly sent to another thread.
    *. When it's constructed through `Context`. The object remain on it's thread and never move to
       other threads so it's safe to bound to `Send` regardless.
*/
unsafe impl<A> Send for MessageObject<A> {}

pub(crate) fn message_send_check<M: Message + Send>() {}

impl<A> MessageObject<A> {
    pub(crate) fn new<M>(msg: M, tx: Option<OneshotSender<M::Result>>) -> MessageObject<A>
    where
        A: Actor + Handler<M>,
        M: Message,
    {
        MessageObject(Box::new(MessageContainer { msg: Some(msg), tx }))
    }
}

impl<A> Deref for MessageObject<A> {
    type Target = dyn MessageHandler<A>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<A> DerefMut for MessageObject<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

// intern type for cloning a MessageObject.
pub(crate) enum ActorMessageClone<A> {
    Ref(Box<dyn MessageObjectClone<A>>),
    Mut(Box<dyn MessageObjectClone<A>>),
}

// the clone would return ActorMessage directly.
impl<A: Actor> ActorMessageClone<A> {
    pub(crate) fn clone(&self) -> ActorMessage<A> {
        match self {
            Self::Ref(ref obj) => ActorMessage::Ref(obj.clone_object()),
            Self::Mut(ref obj) => ActorMessage::Mut(obj.clone_object()),
        }
    }
}

pub(crate) trait MessageObjectClone<A> {
    fn clone_object(&self) -> MessageObject<A>;
}

impl<A, M> MessageObjectClone<A> for M
where
    A: Actor + Handler<M>,
    M: Message + Sized + Clone + 'static,
{
    fn clone_object(&self) -> MessageObject<A> {
        MessageObject::new(self.clone(), None)
    }
}

// message would produced in the future passed to Context<Actor>.
pub(crate) struct FutureMessage<A: Actor> {
    delay: <A::Runtime as RuntimeService>::Sleep,
    handle: Option<OneshotReceiver<()>>,
    msg: Option<ActorMessage<A>>,
}

impl<A: Actor> FutureMessage<A> {
    pub(crate) fn new(dur: Duration, rx: OneshotReceiver<()>, msg: ActorMessage<A>) -> Self {
        Self {
            delay: A::sleep(dur),
            handle: Some(rx),
            msg: Some(msg),
        }
    }
}

impl<A: Actor> Future for FutureMessage<A> {
    type Output = Option<ActorMessage<A>>;

    fn poll(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(h) = this.handle.as_mut() {
            match Pin::new(h).poll(cx) {
                // handle canceled. resolve with nothing.
                Poll::Ready(Ok(())) => return Poll::Ready(None),
                // handle dropped. the task is now detached.
                Poll::Ready(Err(_)) => this.handle = None,
                Poll::Pending => {}
            }
        }

        match Pin::new(&mut this.delay).poll(cx) {
            Poll::Ready(_) => Poll::Ready(Some(this.msg.take().unwrap())),
            Poll::Pending => Poll::Pending,
        }
    }
}

// interval message passed to Context<Actor>.
pub(crate) struct IntervalMessage<A: Actor> {
    dur: Duration,
    delay: <A::Runtime as RuntimeService>::Sleep,
    handle: Option<OneshotReceiver<()>>,
    msg: ActorMessageClone<A>,
}

impl<A: Actor> IntervalMessage<A> {
    pub(crate) fn new(dur: Duration, rx: OneshotReceiver<()>, msg: ActorMessageClone<A>) -> Self {
        Self {
            dur,
            delay: A::sleep(dur),
            handle: Some(rx),
            msg,
        }
    }
}

impl<A: Actor> Stream for IntervalMessage<A> {
    type Item = ActorMessage<A>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(h) = this.handle.as_mut() {
            match Pin::new(h).poll(cx) {
                // handle canceled. resolve with nothing.
                Poll::Ready(Ok(())) => return Poll::Ready(None),
                // handle dropped. the task is now detached.
                Poll::Ready(Err(_)) => this.handle = None,
                Poll::Pending => {}
            }
        }

        match Pin::new(&mut this.delay).poll(cx) {
            Poll::Ready(_) => {
                this.delay = A::sleep(this.dur);
                // wake self one more time to register the new sleep.
                cx.waker().wake_by_ref();
                Poll::Ready(Some(this.msg.clone()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(crate) enum StreamMessage<A: Actor> {
    Interval(IntervalMessage<A>),
    Boxed(Pin<Box<dyn Stream<Item = ActorMessage<A>>>>),
}

impl<A: Actor> StreamMessage<A> {
    pub(crate) fn new_interval(msg: IntervalMessage<A>) -> Self {
        Self::Interval(msg)
    }

    pub(crate) fn new_boxed<S>(stream: S) -> Self
    where
        S: Stream<Item = ActorMessage<A>> + 'static,
    {
        Self::Boxed(Box::pin(stream))
    }
}

impl<A: Actor> Stream for StreamMessage<A> {
    type Item = ActorMessage<A>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            StreamMessage::Interval(stream) => Pin::new(stream).poll_next(cx),
            StreamMessage::Boxed(stream) => stream.as_mut().poll_next(cx),
        }
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct StreamContainer<A, S, F> {
        #[pin]
        stream: S,
        handle: Option<OneshotReceiver<()>>,
        _act: PhantomData<A>,
        f: F
    }
}

impl<A, S, F> StreamContainer<A, S, F> {
    pub(crate) fn new(stream: S, handle: OneshotReceiver<()>, f: F) -> Self {
        Self {
            stream,
            handle: Some(handle),
            _act: PhantomData,
            f,
        }
    }
}

impl<A, S, F> Stream for StreamContainer<A, S, F>
where
    A: Actor + Handler<S::Item>,
    S: Stream,
    S::Item: Message,
    F: FnOnce(MessageObject<A>) -> ActorMessage<A> + Copy,
{
    type Item = ActorMessage<A>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        if let Some(h) = this.handle.as_mut() {
            match Pin::new(h).poll(cx) {
                // handle canceled. resolve with nothing.
                Poll::Ready(Ok(())) => return Poll::Ready(None),
                // handle dropped. the task is now detached.
                Poll::Ready(Err(_)) => *this.handle = None,
                Poll::Pending => {}
            }
        }

        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                let msg = MessageObject::new(item, None);
                let msg = (this.f)(msg);
                Poll::Ready(Some(msg))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// main type of message goes through actor's channel.
pub enum ActorMessage<A> {
    Ref(MessageObject<A>),
    Mut(MessageObject<A>),
    ActorState(ActorState, Option<OneshotSender<()>>),
}
