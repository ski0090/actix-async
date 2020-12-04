//! actix API(mostly) with async/await friendly.
//!
//! # Example:
//! ```rust
//! #![allow(incomplete_features)]
//! #![feature(generic_associated_types, type_alias_impl_trait)]
//!
//! use std::future::Future;
//!
//! use actix_async::prelude::*;
//!
//! // actor type
//! struct TestActor;
//! // impl actor trait for actor type
//! actor!(TestActor);
//!
//! // message type
//! struct TestMessage;
//! // impl message trait for message type and define the result type.
//! message!(TestMessage, u32);
//!
//! // impl handler trait for message and actor types.
//! impl Handler<TestMessage> for TestActor {
//!     type Future<'res> = impl Future<Output = u32> + 'res;
//!     type FutureWait<'res> = impl Future<Output = u32> + 'res;
//!
//!     // concurrent message handler where actor state and context are borrowed immutably.
//!     fn handle<'act, 'ctx, 'res>(
//!         &'act self,
//!         msg: TestMessage,
//!         ctx: &'ctx Context<Self>
//!     ) -> Self::Future<'res>
//!     where
//!         'act: 'res,
//!         'ctx: 'res
//!     {
//!         async { 996 }
//!     }
//!
//!     // exclusive message handler where actor state and context are borrowed mutably.
//!     fn handle_wait<'act, 'ctx, 'res>(
//!         &'act mut self,
//!         msg: TestMessage,
//!         ctx: &'ctx mut Context<Self>
//!     ) -> Self::FutureWait<'res>
//!     where
//!         'act: 'res,
//!         'ctx: 'res
//!     {
//!         async { 251 }
//!     }
//! }
//!
//! #[actix_rt::main]
//! async fn main() {
//!     // construct actor
//!     let actor = TestActor;
//!
//!     // start actor and get address
//!     let address = actor.start();
//!
//!     // send concurrent message with address
//!     let res = address.send(TestMessage).await.unwrap();
//!
//!     // got result
//!     assert_eq!(996, res);
//!
//!     // send exclusive message with address
//!     let res = address.wait(TestMessage).await.unwrap();
//!
//!     // got result
//!     assert_eq!(251, res);
//! }
//! ```

#![no_std]
#![forbid(unused_imports, unused_mut, unused_variables)]
#![allow(incomplete_features)]
#![feature(generic_associated_types, type_alias_impl_trait)]

extern crate alloc;

mod actor;
mod handler;
mod macros;
mod message;
mod util;

pub mod address;
pub mod context;
pub mod error;
pub mod prelude {
    pub use crate::actor::Actor;
    pub use crate::context::Context;
    pub use crate::context::ContextJoinHandle;
    pub use crate::error::ActixAsyncError;
    pub use crate::handler::Handler;
    pub use crate::message;
    pub use crate::message::Message;
    pub use crate::runtime::RuntimeService;
    pub use crate::util::futures::LocalBoxFuture;
    #[cfg(feature = "actix-rt")]
    pub use {crate::actor, crate::runtime::default_rt::ActixRuntime};
}
pub mod request;
pub mod runtime;
#[cfg(feature = "actix-rt")]
pub mod supervisor;

#[cfg(doctest)]
doc_comment::doctest!("../README.md");

#[cfg(test)]
mod test {
    use core::future::Ready;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context as StdContext, Poll};
    use core::time::Duration;

    use alloc::boxed::Box;
    use alloc::sync::Arc;

    use actix_rt::time::{interval, sleep, Interval};
    use actix_rt::Arbiter;
    use async_trait::async_trait;

    use crate as actix_async;
    use actix_async::prelude::*;
    use actix_async::supervisor::Supervisor;
    use core::future::Future;

    #[actix_rt::test]
    async fn start_in_arbiter() {
        let arb = Arbiter::new();
        let addr = TestActor::start_in_arbiter(&arb, |_ctx| TestActor::default());

        let res = addr.send(TestMessage).await;
        assert_eq!(996, res.unwrap());

        let res = addr.wait(TestMessage).await;
        assert_eq!(251, res.unwrap());
    }

    #[actix_rt::test]
    async fn stop_graceful() {
        let addr = TestActor::default().start();

        let res = addr.stop(true).await;
        assert!(res.is_ok());
        assert!(addr.send(TestMessage).await.is_err());
    }

    #[actix_rt::test]
    async fn run_future() {
        let addr = TestActor::default().start();

        let res = addr.run(|_act, _ctx| Box::pin(async move { 123 })).await;
        assert_eq!(123, res.unwrap());

        let res = addr
            .run_wait(|_act, _ctx| Box::pin(async move { 321 }))
            .await;
        assert_eq!(321, res.unwrap());
    }

    #[actix_rt::test]
    async fn run_interval() {
        let addr = TestActor::default().start();

        let (size, handle) = addr.send(TestIntervalMessage).await.unwrap();
        sleep(Duration::from_millis(1250)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 2);

        let (size, handle) = addr.wait(TestIntervalMessage).await.unwrap();
        sleep(Duration::from_millis(1250)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 2)
    }

    #[actix_rt::test]
    async fn test_timeout() {
        let addr = TestActor::default().start();

        let res = addr
            .send(TestTimeoutMessage)
            .timeout(Duration::from_secs(1))
            .timeout_response(Duration::from_secs(1))
            .await;

        assert_eq!(res, Err(ActixAsyncError::ReceiveTimeout));
    }

    #[actix_rt::test]
    async fn test_recipient() {
        let addr = TestActor::default().start();

        let re = addr.recipient::<TestMessage>();

        let res = re.send(TestMessage).await;
        assert_eq!(996, res.unwrap());

        let res = re.wait(TestMessage).await;
        assert_eq!(251, res.unwrap());

        drop(re);

        let re = addr.recipient_weak::<TestMessage>();

        let res = re.send(TestMessage).await;
        assert_eq!(996, res.unwrap());

        let res = re.wait(TestMessage).await;
        assert_eq!(251, res.unwrap());

        drop(addr);

        let res = re.send(TestMessage).await;

        assert_eq!(res, Err(ActixAsyncError::Closed));
    }

    #[actix_rt::test]
    async fn test_delay() {
        let addr = TestActor::default().start();

        let res = addr.send(TestDelayMessage).await.unwrap();
        drop(res);

        sleep(Duration::from_millis(300)).await;
        let res = addr.send(TestMessage).await.unwrap();
        assert_eq!(996, res);

        sleep(Duration::from_millis(300)).await;
        let res = addr.send(TestMessage).await.unwrap();
        assert_eq!(997, res);

        let res = addr.send(TestDelayMessage).await.unwrap();

        sleep(Duration::from_millis(400)).await;
        res.cancel();

        sleep(Duration::from_millis(300)).await;
        let res = addr.send(TestMessage).await.unwrap();
        assert_eq!(997, res);
    }

    #[actix_rt::test]
    async fn test_stream() {
        let addr = TestActor::default().start();

        struct TestStream {
            interval: Interval,
            state: Arc<AtomicUsize>,
        }

        impl futures_util::stream::Stream for TestStream {
            type Item = TestMessage;

            fn poll_next(
                self: Pin<&mut Self>,
                cx: &mut StdContext<'_>,
            ) -> Poll<Option<Self::Item>> {
                let this = self.get_mut();
                if Pin::new(&mut this.interval).poll_next(cx).is_pending() {
                    return Poll::Pending;
                }
                this.state.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Some(TestMessage))
            }
        }

        let state = Arc::new(AtomicUsize::new(0));

        let handle = addr
            .run_wait({
                let state = state.clone();
                move |_, ctx| {
                    Box::pin(async move {
                        ctx.add_stream(TestStream {
                            interval: interval(Duration::from_millis(500)),
                            state,
                        })
                    })
                }
            })
            .await
            .unwrap();

        sleep(Duration::from_millis(750)).await;
        handle.cancel();
        sleep(Duration::from_millis(1000)).await;
        assert_eq!(2, state.load(Ordering::SeqCst));
    }

    #[actix_rt::test]
    async fn test_panic_recovery() {
        let supervisor = Supervisor::new(1);
        let addr = supervisor.start_in_arbiter(1, |_| TestActor::default());

        let _ = addr.send(TestPanicMsg).await;
        sleep(Duration::from_millis(1000)).await;
        let res = addr.send(TestMessage).await;

        assert_eq!(996, res.unwrap());
    }

    struct TestActor(usize);

    impl Default for TestActor {
        fn default() -> Self {
            Self(996)
        }
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type Runtime = ActixRuntime;

        async fn on_start(&mut self, _: &mut Context<Self>) {
            self.0 += 1;
            assert_eq!(997, self.0);
            self.0 -= 1;
        }
    }

    struct TestMessage;

    message!(TestMessage, usize);

    impl Handler<TestMessage> for TestActor {
        type Future<'res> = impl Future<Output = usize> + 'res;
        type FutureWait<'res> = impl Future<Output = usize> + 'res;

        fn handle<'act, 'ctx, 'res>(
            &'act self,
            _: TestMessage,
            _: &'ctx Context<Self>,
        ) -> Self::Future<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            async move { self.0 }
        }

        fn handle_wait<'act, 'ctx, 'res>(
            &'act mut self,
            _: TestMessage,
            _: &'ctx mut Context<Self>,
        ) -> Self::FutureWait<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            async { 251 }
        }
    }

    struct TestIntervalMessage;

    message!(TestIntervalMessage, (Arc<AtomicUsize>, ContextJoinHandle));

    impl Handler<TestIntervalMessage> for TestActor {
        type Future<'res> = impl Future<Output = (Arc<AtomicUsize>, ContextJoinHandle)> + 'res;
        type FutureWait<'res> = impl Future<Output = (Arc<AtomicUsize>, ContextJoinHandle)> + 'res;

        fn handle<'act, 'ctx, 'res>(
            &'act self,
            _: TestIntervalMessage,
            ctx: &'ctx Context<Self>,
        ) -> Self::Future<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            async move {
                let size = Arc::new(AtomicUsize::new(0));
                let handle = ctx.run_interval(Duration::from_millis(500), {
                    let size = size.clone();
                    move |_, _| {
                        Box::pin(async move {
                            size.fetch_add(1, Ordering::SeqCst);
                        })
                    }
                });

                (size, handle)
            }
        }

        fn handle_wait<'act, 'ctx, 'res>(
            &'act mut self,
            _: TestIntervalMessage,
            ctx: &'ctx mut Context<Self>,
        ) -> Self::FutureWait<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            async move {
                let size = Arc::new(AtomicUsize::new(0));
                let handle = ctx.run_wait_interval(Duration::from_millis(500), {
                    let size = size.clone();
                    move |_, _| {
                        Box::pin(async move {
                            size.fetch_add(1, Ordering::SeqCst);
                        })
                    }
                });

                (size, handle)
            }
        }
    }

    struct TestTimeoutMessage;

    message!(TestTimeoutMessage, ());

    impl Handler<TestTimeoutMessage> for TestActor {
        type Future<'res> = impl Future<Output = ()> + 'res;
        type FutureWait<'res> = Ready<()>;

        fn handle<'act, 'ctx, 'res>(
            &'act self,
            _: TestTimeoutMessage,
            _: &'ctx Context<Self>,
        ) -> Self::Future<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            sleep(Duration::from_secs(2))
        }

        fn handle_wait<'act, 'ctx, 'res>(
            &'act mut self,
            _: TestTimeoutMessage,
            _: &'ctx mut Context<Self>,
        ) -> Self::FutureWait<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            unimplemented!()
        }
    }

    struct TestDelayMessage;

    impl Message for TestDelayMessage {
        type Result = ContextJoinHandle;
    }

    impl Handler<TestDelayMessage> for TestActor {
        type Future<'res> = impl Future<Output = ContextJoinHandle> + 'res;
        type FutureWait<'res> = Ready<ContextJoinHandle>;

        fn handle<'act, 'ctx, 'res>(
            &'act self,
            _: TestDelayMessage,
            ctx: &'ctx Context<Self>,
        ) -> Self::Future<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            async move {
                ctx.run_wait_later(Duration::from_millis(500), |act, _| {
                    Box::pin(async move {
                        act.0 += 1;
                    })
                })
            }
        }

        fn handle_wait<'act, 'ctx, 'res>(
            &'act mut self,
            _: TestDelayMessage,
            _: &'ctx mut Context<Self>,
        ) -> Self::FutureWait<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            unimplemented!()
        }
    }

    struct TestPanicMsg;

    message!(TestPanicMsg, ());

    impl Handler<TestPanicMsg> for TestActor {
        type Future<'res> = Ready<()>;
        type FutureWait<'res> = Ready<()>;

        fn handle<'act, 'ctx, 'res>(
            &'act self,
            _: TestPanicMsg,
            _: &'ctx Context<Self>,
        ) -> Self::Future<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            panic!("This is a purpose panic to test actor recovery")
        }

        fn handle_wait<'act, 'ctx, 'res>(
            &'act mut self,
            _: TestPanicMsg,
            _: &'ctx mut Context<Self>,
        ) -> Self::FutureWait<'res>
        where
            'act: 'res,
            'ctx: 'res,
        {
            panic!("This is a purpose panic to test actor recovery")
        }
    }
}
