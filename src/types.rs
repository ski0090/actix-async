use crate::error::ActixAsyncError;

pub(crate) type ActixResult<T> = Result<T, ActixAsyncError>;
