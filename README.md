[actix](https://crates.io/crates/actix) API(mostly) with async/await friendly.

### Example:
```rust
use actix_async::prelude::*;

// actor type
struct TestActor;

// impl actor trait for actor type
impl Actor for TestActor {
    type Runtime = ActixRuntime;
}

// message type
struct TestMessage;

// impl message trait for message type.
impl Message for TestMessage {
    type Result = u32;
}

// impl handler trait for message and actor types.
#[async_trait::async_trait(?Send)]
impl Handler<TestMessage> for TestActor {
    // concurrent message handler where actor state and context are borrowed immutably.
    async fn handle(&self, _: TestMessage, _: &Context<Self>) -> u32 {
        996
    }
    
    // exclusive message handler where actor state and context are borrowed mutably.
    async fn handle_wait(&mut self, _: TestMessage, _: &mut Context<Self>) -> u32 {
        251
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