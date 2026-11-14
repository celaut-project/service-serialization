//! Receive one Bee-RPC upload over gRPC, as a node receives a service package. Usage:
//! `bee-recv <listen addr> <out file> [block dir]`
//! The server parses the stream with the bee-rpc parser (blocks checked against their
//! ids), takes held blocks from the block dir (files named by their SHA3-256 id, as the
//! node's `__block__`) and asks the sender to skip them, writes the Service part to
//! `out`, answers one part, and exits after one call. Prints one JSON line.
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bee_rpc::buffer::Buffer;
use bee_rpc::frame::{serialize_part, BlockStore, ParseOptions, Parser, Part};
use bee_rpc::hash::HashAlgorithm;
use bee_rpc::skip::StreamControl;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::codegen::{http, Body, BoxFuture, Service, StdError};
use tonic::{Status, Streaming};

type Reply = Pin<Box<dyn Stream<Item = Result<Buffer, Status>> + Send>>;

struct DirStore(Option<PathBuf>);

impl BlockStore for DirStore {
    fn get(&self, id: &[u8]) -> Option<Vec<u8>> {
        let p = self.0.as_ref()?.join(hex::encode(id));
        if p.is_file() {
            std::fs::read(p).ok()
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct Server(Arc<Ctx>);

struct Ctx {
    out: PathBuf,
    store: Arc<DirStore>,
    done: mpsc::Sender<String>,
}

impl tonic::server::NamedService for Server {
    const NAME: &'static str = "bench.Bee";
}

struct Method(Arc<Ctx>);

impl tonic::server::StreamingService<Buffer> for Method {
    type Response = Buffer;
    type ResponseStream = Reply;
    type Future = BoxFuture<tonic::Response<Reply>, Status>;
    fn call(&mut self, request: tonic::Request<Streaming<Buffer>>) -> Self::Future {
        let ctx = self.0.clone();
        let mut input = request.into_inner();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel::<Result<Buffer, Status>>(64);
            tokio::spawn(async move {
                let t = Instant::now();
                let control = StreamControl::new(HashAlgorithm::Sha3_256);
                let mut parser = Parser::new(ParseOptions::default())
                    .with_store(ctx.store.clone())
                    .with_control(control.clone());
                let mut parts: Vec<Part> = Vec::new();
                let mut wire = 0u64;
                let mut err = None;
                let trace = std::env::var_os("BEE_TRACE").is_some();
                let mut n = 0u64;
                let mut skips = 0u64;
                let mut t_push = 0f64;
                let mut t_wait = 0f64;
                let mut tw = Instant::now();
                while let Some(b) = input.next().await {
                    t_wait += tw.elapsed().as_secs_f64();
                    n += 1;
                    if trace && n.is_multiple_of(200) {
                        eprintln!("{:8.3}s buffers {n} wire {wire} skips {skips} starts {} push {t_push:.3} wait {t_wait:.3}", t.elapsed().as_secs_f64(), parser.block_starts().len());
                    }
                    let b = match b {
                        Ok(b) => b,
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    };
                    wire += prost::Message::encoded_len(&b) as u64;
                    // Heavy pushes (hashing) run off the async threads, as bee-rpc's call does.
                    let tp = Instant::now();
                    let res = tokio::task::block_in_place(|| parser.push(b));
                    t_push += tp.elapsed().as_secs_f64();
                    match res {
                        Ok(p) => parts.extend(p),
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                    for s in control.take_outbound() {
                        skips += 1;
                        let _ = tx.send(Ok(s)).await;
                    }
                    // The package is the Metadata part and the Service part. With block skip
                    // the client keeps its direction open until the answer, so answer now.
                    if parts.iter().any(|p| p.index == 2) {
                        break;
                    }
                    tw = Instant::now();
                }
                if err.is_none() && !parts.iter().any(|p| p.index == 2) {
                    match parser.finish() {
                        Ok(p) => parts.extend(p),
                        Err(e) => err = Some(e.to_string()),
                    }
                }
                let held = parser.block_starts().len();
                let report = match err {
                    None => {
                        let svc = parts.iter().find(|p| p.index == 2);
                        let ok = svc.is_some();
                        if let Some(s) = svc {
                            std::fs::write(&ctx.out, &s.data).unwrap();
                        }
                        use sha3::Digest;
                        let id = svc
                            .map(|s| hex::encode(sha3::Sha3_256::digest(&s.data)))
                            .unwrap_or_default();
                        for b in serialize_part(&Part::new(1, id.clone().into_bytes())) {
                            let _ = tx.send(Ok(b)).await;
                        }
                        format!("{{\"side\":\"recv\",\"ok\":{ok},\"elapsed_s\":{:.3},\"wire_buffer_bytes\":{wire},\"blocks_seen\":{held},\"service_sha3\":\"{id}\"}}", t.elapsed().as_secs_f64())
                    }
                    Some(e) => format!("{{\"side\":\"recv\",\"ok\":false,\"error\":\"{e}\"}}"),
                };
                drop(tx);
                let _ = ctx.done.send(report).await;
            });
            Ok(tonic::Response::new(
                Box::pin(ReceiverStream::new(rx)) as Reply
            ))
        })
    }
}

impl<B> Service<http::Request<B>> for Server
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let ctx = self.0.clone();
        Box::pin(async move {
            let codec = tonic_prost::ProstCodec::<Buffer, Buffer>::default();
            let mut grpc = tonic::server::Grpc::new(codec).max_decoding_message_size(64 << 20);
            Ok(grpc.streaming(Method(ctx), request).await)
        })
    }
}

#[tokio::main]
async fn main() {
    let mem = sese_bench::MemPeak::start();
    let a: Vec<String> = std::env::args().collect();
    let listener = tokio::net::TcpListener::bind(&a[1]).await.unwrap();
    // HTTP/2 flow control, as in bee-send (BEE_WINDOW = large | adaptive | default).
    let window = std::env::var("BEE_WINDOW").unwrap_or_else(|_| "large".into());
    let (done_tx, mut done_rx) = mpsc::channel(1);
    let ctx = Arc::new(Ctx {
        out: PathBuf::from(&a[2]),
        store: Arc::new(DirStore(a.get(3).map(PathBuf::from))),
        done: done_tx,
    });
    tokio::spawn(
        // TCP_NODELAY as grpc-core (the node's gRPC) sets it; tonic's server default is
        // off, and then the small skip requests and window updates wait for Nagle.
        tonic::transport::Server::builder()
            .tcp_nodelay(true)
            .http2_adaptive_window(Some(window == "adaptive"))
            .initial_stream_window_size((window == "large").then_some(16u32 << 20))
            .initial_connection_window_size((window == "large").then_some(32u32 << 20))
            .add_service(Server(ctx))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let report = done_rx.recv().await.unwrap();
    // Let the answer leave before the process ends.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let report = report.trim_end_matches('}').to_string();
    println!("{report},{}}}", mem.json());
}
