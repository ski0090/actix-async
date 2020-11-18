use core::future::Future;
use core::pin::Pin;

use crate::error::ActixAsyncError;

pub type LocalBoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(crate) type ActixResult<T> = Result<T, ActixAsyncError>;
