/*
   This example has to be start with tokio-uring feature
   .e.g:

   cargo run --example io-uring --features tokio-uring
*/

use std::io;

use actix_async::prelude::*;
use tokio_uring::fs::File;

struct TokioUringActor;
actor!(TokioUringActor);

struct TestMessage;
message!(TestMessage, io::Result<String>);

#[actix_async::handler]
impl Handler<TestMessage> for TokioUringActor {
    async fn handle(&self, _: TestMessage, _: Context<'_, Self>) -> io::Result<String> {
        let path = "test.txt";

        let file = File::create(path).await?;

        let buf = b"Hello World!";
        let _ = file.write_at(&buf[..], 0).await;

        file.sync_all().await?;
        file.close().await?;

        let file = File::open(path).await?;

        let buf = Vec::with_capacity(12);
        let (res, buf) = file.read_at(buf, 0).await;
        let n = res?;
        let res = std::str::from_utf8(&buf[..n]).unwrap().to_string();

        file.close().await?;

        std::fs::remove_file(path)?;

        Ok(res)
    }
}

fn main() -> io::Result<()> {
    tokio_uring::start(async {
        let addr = TokioUringActor.start();

        let res = addr.send(TestMessage).await.unwrap()?;
        assert_eq!(res, "Hello World!");
        Ok(())
    })
}
