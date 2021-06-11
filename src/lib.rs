//! actix API(mostly) with async/await friendly.
//!
//! # Example:
//! ```rust
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
//! #[async_trait::async_trait(?Send)]
//! impl Handler<TestMessage> for TestActor {
//!     // concurrent message handler where actor state and context are borrowed immutably.
//!     async fn handle(&self, _: TestMessage, _: &Context<Self>) -> u32 {
//!         996
//!     }
//!     
//!     // exclusive message handler where actor state and context are borrowed mutably.
//!     async fn handle_wait(&mut self, _: TestMessage, _: &mut Context<Self>) -> u32 {
//!         251
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     tokio::task::LocalSet::new().run_until(async {
//!         // construct actor
//!         let actor = TestActor;
//!
//!         // start actor and get address
//!         let address = actor.start();
//!
//!         // send concurrent message with address
//!         let res = address.send(TestMessage).await.unwrap();
//!
//!         // got result
//!         assert_eq!(996, res);
//!
//!         // send exclusive message with address
//!         let res = address.wait(TestMessage).await.unwrap();
//!
//!         // got result
//!         assert_eq!(251, res);
//!     })
//!     .await
//! }
//! ```

#![no_std]
#![forbid(unused_imports, unused_mut, unused_variables)]

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

    #[cfg(feature = "tokio-rt")]
    pub use crate::actor;

    #[cfg(feature = "tokio-rt")]
    pub use self::default_tokio_rt::TokioRuntime;

    #[cfg(feature = "tokio-rt")]
    mod default_tokio_rt {
        use super::RuntimeService;

        use core::{future::Future, time::Duration};

        pub struct TokioRuntime;

        impl RuntimeService for TokioRuntime {
            type Sleep = tokio::time::Sleep;

            fn spawn<F: Future<Output = ()> + 'static>(f: F) {
                tokio::task::spawn_local(f);
            }

            fn sleep(dur: Duration) -> Self::Sleep {
                tokio::time::sleep(dur)
            }
        }
    }
}
pub mod request;
pub mod runtime;

#[cfg(doctest)]
doc_comment::doctest!("../README.md");

#[cfg(test)]
mod test {
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context as StdContext, Poll};
    use core::time::Duration;

    use alloc::boxed::Box;
    use alloc::sync::Arc;

    use async_trait::async_trait;
    use tokio::task::LocalSet;
    use tokio::time::{interval, sleep, Interval};

    use crate as actix_async;
    use actix_async::prelude::*;

    #[tokio::test]
    async fn stop_graceful() {
        LocalSet::new()
            .run_until(async {
                let addr = TestActor::default().start();

                let res = addr.stop(true).await;
                assert!(res.is_ok());
                assert!(addr.send(TestMessage).await.is_err());
            })
            .await
    }

    #[tokio::test]
    async fn run_future() {
        LocalSet::new()
            .run_until(async {
                let addr = TestActor::default().start();

                let res = addr.run(|_act, _ctx| Box::pin(async move { 123 })).await;
                assert_eq!(123, res.unwrap());

                let res = addr
                    .run_wait(|_act, _ctx| Box::pin(async move { 321 }))
                    .await;
                assert_eq!(321, res.unwrap());
            })
            .await
    }

    #[tokio::test]
    async fn run_interval() {
        LocalSet::new()
            .run_until(async {
                let addr = TestActor::default().start();

                let (size, handle) = addr.send(TestIntervalMessage).await.unwrap();
                sleep(Duration::from_millis(1250)).await;
                handle.cancel();
                assert_eq!(size.load(Ordering::SeqCst), 2);

                let (size, handle) = addr.wait(TestIntervalMessage).await.unwrap();
                sleep(Duration::from_millis(1250)).await;
                handle.cancel();
                assert_eq!(size.load(Ordering::SeqCst), 2)
            })
            .await
    }

    #[tokio::test]
    async fn test_timeout() {
        LocalSet::new()
            .run_until(async {
                let addr = TestActor::default().start();

                let res = addr
                    .send(TestTimeoutMessage)
                    .timeout(Duration::from_secs(1))
                    .timeout_response(Duration::from_secs(1))
                    .await;

                assert_eq!(res, Err(ActixAsyncError::ReceiveTimeout));
            })
            .await
    }

    #[tokio::test]
    async fn test_recipient() {
        LocalSet::new()
            .run_until(async {
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
            })
            .await
    }

    #[tokio::test]
    async fn test_delay() {
        LocalSet::new()
            .run_until(async {
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
            })
            .await
    }

    #[tokio::test]
    async fn test_stream() {
        LocalSet::new()
            .run_until(async {
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
                        if this.interval.poll_tick(cx).is_pending() {
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
            })
            .await
    }
    //
    // #[tokio::test]
    // async fn test_panic_recovery() {
    //     let supervisor = Supervisor::new(1);
    //     let addr = supervisor.start_in_arbiter(1, |_| TestActor::default());
    //
    //     let _ = addr.send(TestPanicMsg).await;
    //     sleep(Duration::from_millis(1000)).await;
    //     let res = addr.send(TestMessage).await;
    //
    //     assert_eq!(996, res.unwrap());
    // }

    struct TestActor(usize);

    impl Default for TestActor {
        fn default() -> Self {
            Self(996)
        }
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type Runtime = TokioRuntime;

        async fn on_start(&mut self, _: &mut Context<Self>) {
            self.0 += 1;
            assert_eq!(997, self.0);
            self.0 -= 1;
        }
    }

    struct TestMessage;

    message!(TestMessage, usize);

    #[async_trait(?Send)]
    impl Handler<TestMessage> for TestActor {
        async fn handle(&self, _: TestMessage, _: &Context<Self>) -> usize {
            self.0
        }

        async fn handle_wait(&mut self, _: TestMessage, _: &mut Context<Self>) -> usize {
            251
        }
    }

    struct TestIntervalMessage;

    message!(TestIntervalMessage, (Arc<AtomicUsize>, ContextJoinHandle));

    #[async_trait(?Send)]
    impl Handler<TestIntervalMessage> for TestActor {
        async fn handle(
            &self,
            _: TestIntervalMessage,
            ctx: &Context<Self>,
        ) -> (Arc<AtomicUsize>, ContextJoinHandle) {
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

        async fn handle_wait(
            &mut self,
            _: TestIntervalMessage,
            ctx: &mut Context<Self>,
        ) -> (Arc<AtomicUsize>, ContextJoinHandle) {
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

    struct TestTimeoutMessage;

    message!(TestTimeoutMessage, ());

    #[async_trait(?Send)]
    impl Handler<TestTimeoutMessage> for TestActor {
        async fn handle(&self, _: TestTimeoutMessage, _: &Context<Self>) {
            sleep(Duration::from_secs(2)).await;
        }
    }

    struct TestDelayMessage;

    message!(TestDelayMessage, ContextJoinHandle);

    // impl Message for TestDelayMessage {
    //     type Result = ContextJoinHandle;
    // }

    #[async_trait(?Send)]
    impl Handler<TestDelayMessage> for TestActor {
        async fn handle(&self, _: TestDelayMessage, ctx: &Context<Self>) -> ContextJoinHandle {
            ctx.run_wait_later(Duration::from_millis(500), |act, _| {
                Box::pin(async move {
                    act.0 += 1;
                })
            })
        }
    }

    struct TestPanicMsg;

    message!(TestPanicMsg, ());

    #[async_trait(?Send)]
    impl Handler<TestPanicMsg> for TestActor {
        async fn handle(&self, _: TestPanicMsg, _: &Context<Self>) {
            panic!("This is a purpose panic to test actor recovery");
        }
    }
}
