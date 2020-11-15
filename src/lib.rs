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
//!     // construct message
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

pub mod address;
pub mod context;
pub mod error;
pub mod prelude {
    pub use crate::actor::Actor;
    pub use crate::context::Context;
    pub use crate::error::ActixAsyncError;
    pub use crate::handler::Handler;
    pub use crate::message::Message;
    pub use crate::runtime::{ActixRuntime, RuntimeService};
    pub use crate::types::LocalBoxedFuture;
}
pub mod request;
pub mod runtime;

#[cfg(test)]
mod test {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::time::Duration;

    use std::sync::Arc;

    use actix_rt::Arbiter;
    use async_trait::async_trait;

    use super::prelude::*;
    use crate::context::ContextJoinHandle;

    #[actix_rt::test]
    async fn start_in_arbiter() {
        let arb = Arbiter::new();
        let addr = TestActor::start_in_arbiter(&arb, |_ctx| TestActor);

        let res = addr.send(TestMessage).await;
        assert_eq!(996, res.unwrap());

        let res = addr.wait(TestMessage).await;
        assert_eq!(251, res.unwrap());
    }

    #[actix_rt::test]
    async fn stop_graceful() {
        let actor = TestActor;
        let addr = actor.start();

        let _ = addr.stop(true).await;
        assert!(addr.send(TestMessage).await.is_err());
    }

    #[actix_rt::test]
    async fn run_future() {
        let actor = TestActor;
        let addr = actor.start();

        let res = addr.run(|_act, _ctx| Box::pin(async move { 123 })).await;
        assert_eq!(123, res.unwrap());

        let res = addr
            .run_wait(|_act, _ctx| Box::pin(async move { 321 }))
            .await;
        assert_eq!(321, res.unwrap());
    }

    #[actix_rt::test]
    async fn run_interval() {
        let actor = TestActor;
        let addr = actor.start();

        let (size, handle) = addr.send(TestIntervalMessage).await.unwrap();
        actix_rt::time::sleep(Duration::from_millis(1600)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 3);

        let (size, handle) = addr.wait(TestIntervalMessage).await.unwrap();
        actix_rt::time::sleep(Duration::from_millis(1600)).await;
        handle.cancel();
        assert_eq!(size.load(Ordering::SeqCst), 3)
    }

    #[actix_rt::test]
    async fn test_timeout() {
        let actor = TestActor;
        let addr = actor.start();

        let res = addr
            .send(TestTimeoutMessage)
            .timeout(Duration::from_secs(1))
            .await;

        assert!(res.is_err());
    }

    struct TestActor;

    impl Actor for TestActor {
        type Runtime = ActixRuntime;
    }

    struct TestMessage;

    impl Message for TestMessage {
        type Result = u32;
    }

    #[async_trait(?Send)]
    impl Handler<TestMessage> for TestActor {
        async fn handle(&self, _: TestMessage, _: &Context<Self>) -> u32 {
            996
        }

        async fn handle_wait(&mut self, _: TestMessage, _: &mut Context<Self>) -> u32 {
            251
        }
    }

    struct TestIntervalMessage;

    impl Message for TestIntervalMessage {
        type Result = (Arc<AtomicUsize>, ContextJoinHandle);
    }

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

    impl Message for TestTimeoutMessage {
        type Result = ();
    }

    #[async_trait(?Send)]
    impl Handler<TestTimeoutMessage> for TestActor {
        async fn handle(&self, _: TestTimeoutMessage, _: &Context<Self>) {
            <TestActor as Actor>::Runtime::sleep(Duration::from_secs(2)).await;
        }
    }
}
