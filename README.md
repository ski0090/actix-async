[actix](https://crates.io/crates/actix) API(mostly) with async/await friendly.

### Requirement:
- MSRV: rustc 1.51

### Example:
```rust
use actix_async::prelude::*;

// actor type
struct TestActor;
// impl actor trait for actor type
actor!(TestActor);

// message type
struct TestMessage;
// impl message trait for message type.
// the second argument is the result type of Message.
// it must match the return type of Handler<Message>::handle method.
message!(TestMessage, u32);

// impl handler trait for message and actor types.
#[actix_async::handler]
impl Handler<TestMessage> for TestActor {
    // concurrent message handler where actor state and context are borrowed immutably.
    async fn handle(&self, _: TestMessage, _: Context<'_, Self>) -> u32 {
        996
    }
    
    // exclusive message handler where actor state and context are borrowed mutably.
    async fn handle_wait(&mut self, _: TestMessage, _: Context<'_, Self>) -> u32 {
        251
    }
}

#[actix_async::main]
async fn main() {
    // construct actor
    let actor = TestActor;

    // start actor and get address
    let address = actor.start();

    // send concurrent message with address
    let res: Result<u32, ActixAsyncError> = address.send(TestMessage).await;

    // got result
    assert_eq!(996, res.unwrap());

    // send exclusive message with address
    let res = address.wait(TestMessage).await.unwrap();

    // got result
    assert_eq!(251, res);
}
```