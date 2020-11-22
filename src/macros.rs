#[macro_export]
#[cfg(feature = "actix-rt")]
macro_rules! actor {
    ($ty: ty) => {
        impl Actor for $ty {
            type Runtime = actix_async::prelude::ActixRuntime;
        }
    };
}

#[macro_export]
macro_rules! message {
    ($ty: ty, $res: ty) => {
        impl actix_async::prelude::Message for $ty {
            type Result = $res;
        }
    };
}
