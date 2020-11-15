use core::future::Future;
use core::pin::Pin;

pub type LocalBoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
