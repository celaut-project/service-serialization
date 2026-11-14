//! Send a .celaut.bee package over gRPC with bee-rpc (`call_buffers`, block skip on), as a
//! client uploads a service to a node. Usage: `bee-send <http://addr:port> <file.celaut.bee>`
use std::fs::File;
use std::io::BufReader;
use std::time::Instant;

use bee_rpc::grpc::{call_buffers, BufferStream, CallOptions};
use tokio_stream::wrappers::ReceiverStream;

#[tokio::main]
async fn main() {
    let mem = sese_bench::MemPeak::start();
    let a: Vec<String> = std::env::args().collect();
    let t0 = Instant::now();
    // HTTP/2 flow control: BEE_WINDOW = large (16 MiB stream / 32 MiB connection, the
    // default here), adaptive (hyper's BDP probing), or default (hyper's static defaults).
    let window = std::env::var("BEE_WINDOW").unwrap_or_else(|_| "large".into());
    let channel = tonic::transport::Endpoint::from_shared(a[1].clone())
        .unwrap()
        .http2_adaptive_window(window == "adaptive")
        .initial_stream_window_size((window == "large").then_some(16u32 << 20))
        .initial_connection_window_size((window == "large").then_some(32u32 << 20))
        .tcp_nodelay(true)
        .connect()
        .await
        .unwrap();
    let path = a[2].clone();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    // Read the records on a blocking thread: the file is never all in memory.
    std::thread::spawn(move || {
        for b in bee_rpc::bee_file::read_bee(BufReader::with_capacity(
            1 << 20,
            File::open(path).unwrap(),
        )) {
            if tx.blocking_send(b).is_err() {
                break;
            }
        }
    });
    let input: BufferStream = Box::pin(ReceiverStream::new(rx));
    let t = Instant::now();
    let parts = call_buffers(
        channel,
        "/bench.Bee/Upload",
        input,
        CallOptions {
            block_skip: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let answer = parts
        .first()
        .map(|p| String::from_utf8_lossy(&p.data).to_string())
        .unwrap_or_default();
    println!("{{\"side\":\"send\",\"ok\":true,\"elapsed_s\":{:.3},\"total_s\":{:.3},\"service_sha3\":\"{answer}\",{}}}", t.elapsed().as_secs_f64(), t0.elapsed().as_secs_f64(), mem.json());
}
