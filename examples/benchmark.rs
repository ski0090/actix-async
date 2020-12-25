// #![allow(incomplete_features)]
// #![feature(generic_associated_types)]
// #![feature(type_alias_impl_trait)]
//
// use std::time::{Duration, Instant};
//
// use futures_util::stream::FuturesUnordered;
// use futures_util::StreamExt;
//
// /*
//
//     A naive benchmark between actix and actix-async for exclusive message handling.
//     This example serve as a way to optimize actix-async crate.
//
//     It DOES NOT represent the real world performance of either crate.
//
//     Build with:
//
//     cargo build --example benchmark --release
//
//
//     Run with:
//
//     ./target/release/examples/benchmark
//
//     optional argument: --rounds <usize> --heap-alloc <bool>
//
// */
//
// pub struct ExclusiveMessage;
//
// pub struct ConcurrentMessage;
//
// mod actix_async_actor {
//     use std::future::Future;
//
//     pub use actix_async::prelude::*;
//     pub use actix_rt::Arbiter;
//     pub use futures_intrusive::sync::LocalMutex;
//     pub use tokio::fs::File;
//     pub use tokio::io::AsyncReadExt;
//
//     use super::*;
//
//     pub struct ActixAsyncActor {
//         pub file: LocalMutex<File>,
//         pub heap_alloc: bool,
//     }
//     actor!(ActixAsyncActor);
//
//     message!(ExclusiveMessage, ());
//
//     impl Handler<ExclusiveMessage> for ActixAsyncActor {
//         type Future<'a> = impl Future<Output = ()>;
//         type FutureWait<'a> = Self::Future<'a>;
//
//         fn handle<'a, 'c, 'r>(
//             &'a self,
//             _: ExclusiveMessage,
//             _: &'c Context<Self>,
//         ) -> Self::Future<'r>
//         where
//             'a: 'r,
//             'c: 'r,
//         {
//             async move {
//                 if self.heap_alloc {
//                     let mut buffer = Vec::with_capacity(100_0000);
//                     let _ = self.file.lock().await.read(&mut buffer).await.unwrap();
//                 } else {
//                     let mut buffer = [0u8; 2_048];
//                     let _ = self.file.lock().await.read(&mut buffer).await.unwrap();
//                 }
//             }
//         }
//
//         fn handle_wait<'a, 'c, 'r>(
//             &'a mut self,
//             msg: ExclusiveMessage,
//             ctx: &'c mut Context<Self>,
//         ) -> Self::Future<'r>
//         where
//             'a: 'r,
//             'c: 'r,
//         {
//             self.handle(msg, ctx)
//         }
//     }
//
//     message!(ConcurrentMessage, ());
//
//     impl Handler<ConcurrentMessage> for ActixAsyncActor {
//         type Future<'a> = impl Future<Output = ()> + 'a;
//         type FutureWait<'a> = impl Future<Output = ()> + 'a;
//
//         fn handle<'a, 'c, 'r>(
//             &'a self,
//             _: ConcurrentMessage,
//             _: &'c Context<Self>,
//         ) -> Self::Future<'r>
//         where
//             'a: 'r,
//             'c: 'r,
//         {
//             actix_rt::time::sleep(Duration::from_millis(1))
//         }
//
//         fn handle_wait<'a, 'c, 'r>(
//             &'a mut self,
//             _: ConcurrentMessage,
//             _: &'c mut Context<Self>,
//         ) -> Self::FutureWait<'r>
//         where
//             'a: 'r,
//             'c: 'r,
//         {
//             async { unimplemented!() }
//         }
//     }
// }
//
// mod actix_actor {
//     pub use actix::prelude::*;
//     pub use futures_intrusive::sync::LocalMutex;
//     pub use tokio02::fs::File;
//     pub use tokio02::io::AsyncReadExt;
//
//     use super::*;
//
//     pub struct ActixActor {
//         pub file: LocalMutex<File>,
//         pub heap_alloc: bool,
//     }
//
//     impl Actor for ActixActor {
//         type Context = Context<Self>;
//     }
//
//     impl Message for ExclusiveMessage {
//         type Result = ();
//     }
//
//     impl Handler<ExclusiveMessage> for ActixActor {
//         type Result = ResponseAsync<()>;
//
//         fn handle(&mut self, msg: ExclusiveMessage, ctx: &mut Context<Self>) -> Self::Result {
//             ResponseAsync::concurrent(self, ctx, |act, _| async move {
//                 let _msg = msg;
//                 if act.heap_alloc {
//                     let mut buffer = Vec::with_capacity(100_0000);
//                     let _ = act.file.lock().await.read(&mut buffer).await.unwrap();
//                 } else {
//                     let mut buffer = [0u8; 2_048];
//                     let _ = act.file.lock().await.read(&mut buffer).await.unwrap();
//                 }
//             })
//         }
//     }
//
//     impl Message for ConcurrentMessage {
//         type Result = ();
//     }
//
//     impl Handler<ConcurrentMessage> for ActixActor {
//         type Result = ResponseActFuture<Self, ()>;
//
//         fn handle(&mut self, _: ConcurrentMessage, _ctx: &mut Context<Self>) -> Self::Result {
//             Box::pin(actix::clock::delay_for(Duration::from_millis(1)).into_actor(self))
//         }
//     }
// }
//
// fn collect_arg(rounds: &mut usize, heap_alloc: &mut bool) -> String {
//     let mut iter = std::env::args().into_iter();
//
//     let file_path = std::env::current_dir()
//         .ok()
//         .and_then(|path| {
//             let path = path.to_str()?.to_owned();
//             Some(path + "/sample/sample.txt")
//         })
//         .unwrap_or_else(|| String::from("./sample/sample.txt"));
//
//     while let Some(arg) = iter.next() {
//         if arg.as_str() == "--rounds" {
//             if let Some(arg) = iter.next() {
//                 if let Ok(r) = arg.parse::<usize>() {
//                     *rounds = r;
//                 }
//             }
//         }
//         if arg.as_str() == "--heap-alloc" {
//             if let Some(arg) = iter.next() {
//                 if let Ok(use_heap) = arg.parse::<bool>() {
//                     *heap_alloc = use_heap;
//                 }
//             }
//         }
//     }
//
//     file_path
// }
//
// fn main() {
//     let mut rounds = 1000;
//     let mut heap_alloc = false;
//
//     let file_path = collect_arg(&mut rounds, &mut heap_alloc);
//
//     tokio_raw::benchmark(heap_alloc, file_path.clone(), rounds);
//
//     actix_rt::System::new("actix-async").block_on(async {
//         use actix_async_actor::*;
//         println!("starting benchmark actix-async");
//
//         let mut timing = Timing::new();
//         for _ in 0..10 {
//             let file_path = file_path.clone();
//             let addr = ActixAsyncActor::create_async(move |_| async move {
//                 let file = File::open(file_path.as_str()).await.unwrap();
//                 ActixAsyncActor {
//                     file: LocalMutex::new(file, false),
//                     heap_alloc,
//                 }
//             });
//
//             let mut exclusives = FuturesUnordered::new();
//             let mut concurrents = FuturesUnordered::new();
//
//             for _ in 0..rounds {
//                 exclusives.push(addr.send(ExclusiveMessage));
//                 concurrents.push(addr.send(ConcurrentMessage));
//             }
//
//             let start = Instant::now();
//             while exclusives.next().await.is_some() {}
//             timing.add_exclusive(Instant::now().duration_since(start));
//
//             let start = Instant::now();
//             while concurrents.next().await.is_some() {}
//             timing.add_concurrent(Instant::now().duration_since(start));
//         }
//
//         timing.print_res();
//     });
//
//     actix::System::new("actix").block_on(async move {
//         use actix_actor::*;
//         println!("starting benchmark actix");
//
//         let mut timing = Timing::new();
//
//         for _ in 0..10 {
//             let file = File::open(file_path.clone()).await.unwrap();
//             let heap_alloc = heap_alloc;
//             let addr = ActixActor::create(move |_| ActixActor {
//                 file: LocalMutex::new(file, false),
//                 heap_alloc,
//             });
//
//             let mut exclusives = FuturesUnordered::new();
//             let mut concurrents = FuturesUnordered::new();
//
//             for _ in 0..rounds {
//                 exclusives.push(addr.send(ExclusiveMessage));
//                 concurrents.push(addr.send(ConcurrentMessage));
//             }
//
//             let start = Instant::now();
//             while exclusives.next().await.is_some() {}
//             timing.add_exclusive(Instant::now().duration_since(start));
//
//             let start = Instant::now();
//             while concurrents.next().await.is_some() {}
//             timing.add_concurrent(Instant::now().duration_since(start));
//         }
//
//         timing.print_res();
//     });
// }
//
// mod tokio_raw {
//     use tokio::fs::File;
//     use tokio::io::AsyncReadExt;
//     use tokio::sync::{mpsc, oneshot};
//     use tokio::task;
//
//     use super::*;
//     use async_std::task::JoinHandle;
//     use futures_intrusive::sync::{GenericMutex, LocalMutex};
//     use futures_intrusive::NoopLock;
//
//     enum Msg {
//         Concurrent(ConcurrentMessage, oneshot::Sender<()>),
//         Exclusive(ExclusiveMessage, oneshot::Sender<()>),
//     }
//
//     pub fn benchmark(heap_alloc: bool, file_path: String, rounds: usize) {
//         let local = tokio::task::LocalSet::new();
//         let runtime = tokio::runtime::Builder::new_current_thread()
//             .enable_time()
//             .build()
//             .unwrap();
//
//         runtime.block_on(local.run_until(async {
//             println!("starting benchmark tokio raw channel");
//
//             let (tx, mut rx) = mpsc::channel(256);
//
//             task::spawn_local(async move {
//                 let file = LocalMutex::new(File::open(file_path.as_str()).await.unwrap(), false);
//
//                 // transmute is safe as we collect all spawn handles for tasks borrow file.
//                 // the async block only exit when all the handles are resolved.
//                 let file = unsafe {
//                     core::mem::transmute::<_, &'static GenericMutex<NoopLock, File>>(&file)
//                 };
//
//                 let mut handles = Vec::with_capacity(256);
//
//                 let mut exclusive: Option<JoinHandle<()>> = None;
//
//                 while let Some(msg) = rx.recv().await {
//                     // clean handles if it goes too big.
//                     if handles.len() == 4096 {
//                         for h in handles.iter_mut() {
//                             let _ = h.await;
//                         }
//                         handles = Vec::with_capacity(256);
//                     }
//                     match msg {
//                         Msg::Concurrent(_, tx) => {
//                             if let Some(h) = exclusive.as_mut() {
//                                 h.await;
//                                 exclusive = None;
//                             }
//                             handles.push(task::spawn_local(delay(tx)));
//                         }
//                         Msg::Exclusive(_, tx) => {
//                             if let Some(h) = exclusive.as_mut() {
//                                 h.await;
//                                 exclusive = None;
//                             }
//                             handles.push(task::spawn_local(read_file(heap_alloc, &file, tx)));
//                         }
//                     }
//                 }
//
//                 for h in handles {
//                     let _ = h.await;
//                 }
//             });
//
//             let mut timing = Timing::new();
//
//             for _ in 0..10 {
//                 let mut exclusives = FuturesUnordered::new();
//                 let mut concurrents = FuturesUnordered::new();
//
//                 for _ in 0..rounds {
//                     exclusives.push(async {
//                         let (o_tx, o_rx) = oneshot::channel();
//                         let _ = tx.send(Msg::Exclusive(ExclusiveMessage, o_tx)).await;
//                         o_rx.await
//                     });
//                     concurrents.push(async {
//                         let (o_tx, o_rx) = oneshot::channel();
//                         let _ = tx.send(Msg::Concurrent(ConcurrentMessage, o_tx)).await;
//                         o_rx.await
//                     });
//                 }
//
//                 let start = Instant::now();
//                 while exclusives.next().await.is_some() {}
//                 timing.add_exclusive(Instant::now().duration_since(start));
//
//                 let start = Instant::now();
//                 while concurrents.next().await.is_some() {}
//                 timing.add_concurrent(Instant::now().duration_since(start));
//             }
//
//             timing.print_res();
//         }));
//     }
//
//     async fn delay(tx: oneshot::Sender<()>) {
//         actix_rt::time::sleep(Duration::from_millis(1)).await;
//         let _ = tx.send(());
//     }
//
//     async fn read_file(heap_alloc: bool, file: &'static LocalMutex<File>, tx: oneshot::Sender<()>) {
//         if heap_alloc {
//             let mut buffer = Vec::with_capacity(100_0000);
//             let _ = file.lock().await.read(&mut buffer).await.unwrap();
//         } else {
//             let mut buffer = [0u8; 2_048];
//             let _ = file.lock().await.read(&mut buffer).await.unwrap();
//         }
//         let _ = tx.send(());
//     }
// }
//
// struct Timing {
//     exclusive: Vec<Duration>,
//     concurrent: Vec<Duration>,
// }
//
// impl Timing {
//     fn new() -> Self {
//         Self {
//             exclusive: Vec::with_capacity(10),
//             concurrent: Vec::with_capacity(10),
//         }
//     }
//
//     fn add_exclusive(&mut self, dur: Duration) {
//         self.exclusive.push(dur);
//     }
//
//     fn add_concurrent(&mut self, dur: Duration) {
//         self.concurrent.push(dur);
//     }
//
//     fn print_res(self) {
//         let dur = self
//             .exclusive
//             .into_iter()
//             .map(|dur| dur.as_nanos())
//             .sum::<u128>();
//         println!(
//             "average time for ExclusiveMessage: {:#?}",
//             Duration::from_nanos(dur as u64) / 10
//         );
//
//         let dur = self
//             .concurrent
//             .into_iter()
//             .map(|dur| dur.as_nanos())
//             .sum::<u128>();
//         println!(
//             "average time for ConcurrentMessage: {:#?}",
//             Duration::from_nanos(dur as u64) / 10
//         );
//     }
// }
fn main() {}
