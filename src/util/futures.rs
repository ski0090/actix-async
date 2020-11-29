use core::future::Future;
use core::pin::Pin;

use alloc::boxed::Box;

pub(crate) use futures_core::Stream;

pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
