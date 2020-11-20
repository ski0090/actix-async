use std::time::{Duration, Instant};

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;

/*

    A naive benchmark between actix and actix-async for exclusive message handling.
    This example serve as a way to optimize actix-async crate.

    It DOES NOT represent the real world performance of either crate.

    Build with:

    cargo build --example benchmark --release


    Run with:

    ./target/release/examples/benchmark --target <actix-async or actix>.

    optional argument: --rounds <usize> --heap-alloc <bool>

*/

pub struct ExclusiveMessage;

pub struct ConcurrentMessage;

mod actix_async_actor {
    pub use actix_async::prelude::*;
    pub use actix_rt::Arbiter;
    pub use tokio::fs::File;
    pub use tokio::io::AsyncReadExt;

    use actix_async::{actor, message};

    use super::*;

    pub struct ActixAsyncActor {
        pub file: File,
        pub heap_alloc: bool,
    }
    actor!(ActixAsyncActor);

    message!(ExclusiveMessage, ());

    #[async_trait::async_trait(?Send)]
    impl Handler<ExclusiveMessage> for ActixAsyncActor {
        async fn handle(&self, _: ExclusiveMessage, _ctx: &Context<Self>) {}

        async fn handle_wait(&mut self, _: ExclusiveMessage, _ctx: &mut Context<Self>) {
            if self.heap_alloc {
                let mut buffer = Vec::with_capacity(100_0000);
                let _ = self.file.read(&mut buffer).await.unwrap();
            } else {
                let mut buffer = [0u8; 2_048];
                let _ = self.file.read(&mut buffer).await.unwrap();
            }
        }
    }
}

mod actix_actor {
    pub use std::cell::RefCell;
    pub use std::rc::Rc;

    pub use tokio02::fs::File;
    pub use tokio02::io::AsyncReadExt;

    use actix::prelude::*;

    use super::*;
    use std::ops::DerefMut;

    pub struct ActixActor {
        pub file: Rc<RefCell<File>>,
        pub heap_alloc: bool,
    }

    impl Actor for ActixActor {
        type Context = Context<Self>;
    }

    impl Message for ExclusiveMessage {
        type Result = ();
    }

    impl Handler<ExclusiveMessage> for ActixActor {
        type Result = AtomicResponse<Self, ()>;

        fn handle(&mut self, _: ExclusiveMessage, _ctx: &mut Context<Self>) -> Self::Result {
            let f = self.file.clone();
            let heap = self.heap_alloc;

            AtomicResponse::new(Box::pin(
                async move {
                    if heap {
                        let mut buffer = Vec::with_capacity(100_0000);
                        let _ = f.borrow_mut().deref_mut().read(&mut buffer).await.unwrap();
                    } else {
                        let mut buffer = [0u8; 2_048];
                        let _ = f.borrow_mut().deref_mut().read(&mut buffer).await.unwrap();
                    }
                }
                .into_actor(self),
            ))
        }
    }

    impl Message for ConcurrentMessage {
        type Result = ();
    }

    impl Handler<ConcurrentMessage> for ActixActor {
        type Result = ResponseActFuture<Self, ()>;

        fn handle(&mut self, _: ConcurrentMessage, _ctx: &mut Context<Self>) -> Self::Result {
            Box::pin(async {}.into_actor(self))
        }
    }
}

fn collect_arg(target: &mut String, rounds: &mut usize, heap_alloc: &mut bool) -> String {
    let mut iter = std::env::args().into_iter();

    let file_path = std::env::current_dir()
        .ok()
        .and_then(|path| {
            let path = path.to_str()?.to_owned();
            Some(path + "/sample/sample.txt")
        })
        .unwrap_or_else(|| String::from("./sample/sample.txt"));

    loop {
        if let Some(arg) = iter.next() {
            if arg.as_str() == "--target" {
                if let Some(arg) = iter.next() {
                    *target = arg;
                }
            }
            if arg.as_str() == "--rounds" {
                if let Some(arg) = iter.next() {
                    if let Ok(r) = arg.parse::<usize>() {
                        *rounds = r;
                    }
                }
            }
            if arg.as_str() == "--heap-alloc" {
                if let Some(arg) = iter.next() {
                    if let Ok(use_heap) = arg.parse::<bool>() {
                        *heap_alloc = use_heap;
                    }
                }
            }
            continue;
        }
        break;
    }

    file_path
}

fn main() {
    let num = num_cpus::get();
    let mut target = String::from("actix-async");
    let mut rounds = 1000;
    let mut heap_alloc = false;

    let file_path = collect_arg(&mut target, &mut rounds, &mut heap_alloc);

    match target.as_str() {
        "actix-async" => {
            use actix_async_actor::*;
            actix_rt::System::new("actix-async").block_on(async {
                println!("starting benchmark actix-async");

                let addrs = (0..num)
                    .map(|_| {
                        let arbiter = &Arbiter::new();
                        let file_path = file_path.clone();
                        ActixAsyncActor::start_async_in_arbiter(arbiter, move |_| async move {
                            let file = File::open(file_path.as_str()).await.unwrap();
                            ActixAsyncActor { file, heap_alloc }
                        })
                    })
                    .collect::<Vec<_>>();

                let mut exclusives = FuturesUnordered::new();
                let mut concurrents = FuturesUnordered::new();

                addrs.iter().for_each(|addr| {
                    for _ in 0..rounds {
                        exclusives.push(addr.wait(ExclusiveMessage));
                        concurrents.push(addr.send(ExclusiveMessage));
                    }
                });

                actix_rt::time::sleep(Duration::from_secs(1)).await;

                let start = Instant::now();
                while exclusives.next().await.is_some() {}
                println!(
                    "total time for ExclusiveMessage: {:#?}",
                    Instant::now().duration_since(start)
                );

                let start = Instant::now();
                while concurrents.next().await.is_some() {}
                println!(
                    "total time for ConcurrentMessage: {:#?}",
                    Instant::now().duration_since(start)
                );
            });
        }
        "actix" => {
            actix::System::new("actix").block_on(async move {
                println!("starting benchmark actix");

                use actix_actor::*;

                let mut exclusives = FuturesUnordered::new();
                let mut concurrents = FuturesUnordered::new();

                for _ in 0..num {
                    let file = File::open(file_path.clone()).await.unwrap();
                    let heap_alloc = heap_alloc;
                    let arb = actix::Arbiter::new();
                    use actix::Actor;
                    let addr = ActixActor::start_in_arbiter(&arb, move |_| ActixActor {
                        file: Rc::new(RefCell::new(file)),
                        heap_alloc,
                    });

                    for _ in 0..rounds {
                        exclusives.push(addr.send(ExclusiveMessage));
                        concurrents.push(addr.send(ConcurrentMessage));
                    }
                }

                let start = Instant::now();
                while exclusives.next().await.is_some() {}
                println!(
                    "total time for ExclusiveMessage: {:#?}",
                    Instant::now().duration_since(start)
                );

                let start = Instant::now();
                while concurrents.next().await.is_some() {}
                println!(
                    "total time for ConcurrentMessage: {:#?}",
                    Instant::now().duration_since(start)
                );
            });
        }
        _ => unimplemented!("unknown target"),
    }
}
