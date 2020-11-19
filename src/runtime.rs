use core::future::Future;
use core::time::Duration;

/// Runtime trait for running actor on various runtimes.
/// # example:
/// ```rust
/// use std::future::Future;
/// use std::pin::Pin;
/// use std::time::Duration;
///
/// use actix_async::prelude::*;
///
/// // runtime type.
/// struct AsyncStdRuntime;
///
/// // runtime trait method would be called in actor.
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
/// // actor can run on target runtime.
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
/// // actor runs on default actix runtime(tokio current thread runtime)
/// struct TokioActor;
///
/// impl Actor for TokioActor {
///     type Runtime = ActixRuntime;
/// }
///
/// #[async_trait::async_trait(?Send)]
/// impl Handler<TestMessage> for TokioActor {
///     async fn handle(&self, _: TestMessage, _: &Context<Self>) -> usize {
///         251
///     }
/// }
///
/// #[async_std::main]
/// async fn main() {
///     // run actor in async-std runtime
///     let actor = AsyncStdActor;
///     let addr = actor.start();
///     let res = addr.send(TestMessage).await;
///     assert_eq!(996, res.unwrap());
///
///     // run actor in tokio runtime
///     std::thread::spawn(|| {
///         let local = tokio::task::LocalSet::new();
///         local.spawn_local(async {
///             let actor = TokioActor;
///             let addr = actor.start();
///             let res = addr.send(TestMessage).await;
///             assert_eq!(251, res.unwrap());
///         });
///         tokio::runtime::Builder::new_current_thread()
///             .enable_all()
///             .build()
///             .unwrap()
///             .block_on(local);
///     })
///     .join()
///     .unwrap();
/// }
/// ```
pub trait RuntimeService: Sized {
    type Sleep: Future<Output = ()> + Send + Unpin + 'static;

    fn spawn<F: Future<Output = ()> + 'static>(f: F);

    fn sleep(dur: Duration) -> Self::Sleep;
}

#[cfg(feature = "actix-rt")]
pub mod default_rt {
    use super::*;

    use tokio::runtime::Runtime as TokioRuntime;
    use tokio::task::{spawn_local, LocalSet};
    use tokio::time;

    /// default runtime(tokio current thread runtime).
    #[allow(dead_code)]
    pub struct ActixRuntime {
        rt: TokioRuntime,
        local: LocalSet,
    }

    impl RuntimeService for ActixRuntime {
        type Sleep = time::Sleep;

        #[inline]
        fn spawn<F: Future + 'static>(f: F) {
            spawn_local(f);
        }

        #[inline]
        fn sleep(dur: Duration) -> Self::Sleep {
            time::sleep(dur)
        }
    }
}
