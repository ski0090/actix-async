[actix](https://crates.io/crates/actix) API(mostly) with async/await friendly.

### Requirement:
- MSRV: rustc 1.50.0-nightly (1c389ffef 2020-11-24) and above.
- allow(incomplete_features) and feature(generic_associated_types, type_alias_impl_trait)

### Example:
```rust
// enable unstable features.
#![allow(incomplete_features)]
#![feature(generic_associated_types, type_alias_impl_trait)]

use std::future::Future;

use actix_async::prelude::*;

// actor type
struct TestActor;
// impl actor trait for actor type
actor!(TestActor);

// message type
struct TestMessage;
// impl message trait for message type and define the result type.
message!(TestMessage, u32);

// impl handler trait for message and actor types.
impl Handler<TestMessage> for TestActor {
    // generic associate type is needed to bind the 'res lifetime.
    // the output of opaque future must match the message! macro of Message type.
    type Future<'res> = impl Future<Output = u32> + 'res;
    type FutureWait<'res> = impl Future<Output = u32> + 'res;

    // concurrent message handler where actor state and context are borrowed immutably.
    fn handle<'act, 'ctx, 'res>(
        &'act self,
        msg: TestMessage,
        ctx: &'ctx Context<Self>
    ) -> Self::Future<'res>
    where
        'act: 'res,
        'ctx: 'res
    {
        // return async await block directly.
        // move keyword is needed if self and/or context is borrowed.
        async move {
            let _msg = msg;
            let _act = self;
            let _ctx = ctx;
            996 
        }
    }

    // exclusive message handler where actor state and context are borrowed mutably.
    fn handle_wait<'act, 'ctx, 'res>(
        &'act mut self,
        msg: TestMessage,
        ctx: &'ctx mut Context<Self>
    ) -> Self::FutureWait<'res>
    where
        'act: 'res,
        'ctx: 'res
    {
        async move {
            let _msg = msg;
            let _act = self;
            let _ctx = ctx;
            251 
        }
    }
}

#[actix_rt::main]
async fn main() {
    // construct actor
    let actor = TestActor;

    // start actor and get address
    let address = actor.start();

    // send concurrent message with address
    let res = address.send(TestMessage).await.unwrap();

    // got result
    assert_eq!(996, res);

    // send exclusive message with address
    let res = address.wait(TestMessage).await.unwrap();

    // got result
    assert_eq!(251, res);
}
```