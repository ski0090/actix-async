[actix](https://crates.io/crates/actix) API(mostly) with async/await friendly.

### Requirement:
- MSRV: rustc 1.48 and above

### Example:
```rust
#![allow(incomplete_features)]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

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
        async { 996 }
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
        async { 251 }
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