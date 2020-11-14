use core::future::Future;
use core::time::Duration;

use tokio::runtime::Runtime as TokioRuntime;
use tokio::task::{spawn_local, LocalSet};
use tokio::time;

/// Runtime trait for running actor on various runtimes.
/// # example:
/// ```rust
/// use std::future::Future;
/// use std::pin::Pin;
/// use std::time::Duration;
///
/// use actix_async::prelude::*;
///
/// struct AsyncStdRuntime;
///
/// impl RuntimeService for AsyncStdRuntime {
///     type Sleep = Pin<Box<dyn Future<Output=()> + Send + 'static>>;
///
///     fn spawn<F: Future<Output = ()> + 'static>(f: F) {
///         async_std::task::spawn_local(f);
///     }
///
///     fn sleep(dur: Duration) -> Self::Sleep {
///         Box::pin(async move {
///             async_std::task::sleep(dur).await;
///         })
///     }
/// }
///
/// struct AsyncStdActor;
///
/// impl Actor for AsyncStdActor {
///     type Runtime = AsyncStdRuntime;
/// }
///
/// struct TestMessage;
///
/// impl Message for TestMessage {
///     type Result = usize;
/// }
///
/// #[async_trait::async_trait(?Send)]
/// impl Handler<TestMessage> for AsyncStdActor {
///     async fn handle(&self, _: TestMessage, _: &Context<Self>) -> usize {
///         996
///     }
/// }
///
/// #[async_std::main]
/// async fn main() {
///     let actor = AsyncStdActor;
///     let addr = actor.start();
///     let res = addr.send(TestMessage).await;
///
///     assert_eq!(996, res.unwrap());
/// }
/// ```
pub trait RuntimeService: Sized {
    type Sleep: Future<Output = ()> + Send + Unpin + 'static;

    fn spawn<F: Future<Output = ()> + 'static>(f: F);

    fn sleep(dur: Duration) -> Self::Sleep;
}

pub type ActixRuntime = (TokioRuntime, LocalSet);

impl RuntimeService for ActixRuntime {
    type Sleep = time::Sleep;

    #[inline]
    fn spawn<F: Future<Output = ()> + 'static>(f: F) {
        spawn_local(f);
    }

    #[inline]
    fn sleep(dur: Duration) -> Self::Sleep {
        time::sleep(dur)
    }
}
