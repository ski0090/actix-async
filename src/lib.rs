//! actix API(mostly) with async/await friendly.
//!
//! # Example:
//! ```rust
//! use actix_async::prelude::*;
//!
//! // actor type
//! struct TestActor;
//!
//! // impl actor trait for actor type
//! impl Actor for TestActor {
//!     type Runtime = ActixRuntime;
//! }
//!
//! // message type
//! struct TestMessage;
//!
//! // impl message trait for message type.
//! impl Message for TestMessage {
//!     type Result = u32;
//! }
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

mod actor;
mod handler;
mod message;
mod types;
mod util;

pub mod address;
pub mod context;
pub mod error;
pub mod prelude {
    pub use crate::actor::Actor;
    pub use crate::address::AddrHandler;
    pub use crate::context::Context;
    pub use crate::error::ActixAsyncError;
    pub use crate::handler::Handler;
    pub use crate::message::Message;
    #[cfg(feature = "actix-rt")]
    pub use crate::runtime::default_rt::ActixRuntime;
    pub use crate::runtime::RuntimeService;
    pub use crate::util::futures::LocalBoxedFuture;
}
pub mod request;
pub mod runtime;

#[cfg(test)]
mod test {
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context as StdContext, Poll};
    use core::time::Duration;

    use std::sync::Arc;

    use actix_rt::time::{interval, sleep, Interval};
    use actix_rt::Arbiter;
    use async_trait::async_trait;

    use super::context::ContextJoinHandle;
    use super::message;
    use super::prelude::*;

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

        let _ = addr.stop(true).await;
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
        sleep(Duration::from_millis(1600)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 3);

        let (size, handle) = addr.wait(TestIntervalMessage).await.unwrap();
        sleep(Duration::from_millis(1600)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 3)
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

        sleep(Duration::from_millis(400)).await;
        let res = addr.send(TestMessage).await.unwrap();
        assert_eq!(996, res);

        sleep(Duration::from_millis(200)).await;
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
                            interval: interval(Duration::from_millis(400)),
                            state,
                        })
                    })
                }
            })
            .await
            .unwrap();

        sleep(Duration::from_millis(1600)).await;
        handle.cancel();
        sleep(Duration::from_millis(800)).await;
        assert_eq!(4, state.load(Ordering::SeqCst));
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
            <TestActor as Actor>::Runtime::sleep(Duration::from_secs(2)).await;
        }
    }

    struct TestDelayMessage;

    impl Message for TestDelayMessage {
        type Result = ContextJoinHandle;
    }

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
}
