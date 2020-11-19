use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use crate::actor::{Actor, ActorState};
use crate::handler::{Handler, MessageHandler};
use crate::util::channel::OneshotSender;
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

pub(crate) enum IntervalMessage<A> {
    Ref(Box<dyn MessageObjectClone<A>>),
    Mut(Box<dyn MessageObjectClone<A>>),
}

impl<A: Actor> IntervalMessage<A> {
    pub(crate) fn clone_actor_message(&self) -> ActorMessage<A> {
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
        MessageObject(Box::new(MessageHandlerContainer { msg: Some(msg), tx }))
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

pub(crate) struct MessageHandlerContainer<M: Message> {
    pub(crate) msg: Option<M>,
    pub(crate) tx: Option<OneshotSender<M::Result>>,
}

pub enum ActorMessage<A> {
    Ref(MessageObject<A>),
    Mut(MessageObject<A>),
    ActorState(ActorState, Option<OneshotSender<()>>),
    DelayToken(usize),
    DelayTokenCancel(usize),
    IntervalToken(usize),
    IntervalTokenCancel(usize),
}
