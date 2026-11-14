//! Receive one SeSe transfer over QUIC. Usage:
//! `sese-recv <listen addr> <cert out path> <out file> [held.sese ...]`
//! Writes the server certificate (DER) for the sender to pin, accepts one connection and
//! one transfer, prints one JSON line.
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let mem = sese_bench::MemPeak::start();
    let a: Vec<String> = std::env::args().collect();
    let t = sese_quic::TransportOptions {
        bbr: std::env::var_os("SESE_BBR").is_some(),
        ..Default::default()
    };
    let (ep, cert) = sese_quic::server_endpoint(a[1].parse().unwrap(), &t).unwrap();
    std::fs::write(&a[2], cert.as_ref()).unwrap();
    let held: Vec<PathBuf> = a[4..].iter().map(PathBuf::from).collect();
    let store = sese_quic::FileStore::open(&held, sese::BLOCK_SIZE).unwrap();
    let conn = ep.accept().await.unwrap().await.unwrap();
    let r = sese_quic::receive(
        &conn,
        &store,
        std::path::Path::new(&a[3]),
        &Default::default(),
    )
    .await;
    match r {
        Ok(r) => println!(
            "{{\"side\":\"recv\",\"ok\":true,\"elapsed_s\":{:.3},\"blocks_received\":{},\"blocks_reused\":{},\"data_bytes\":{},\"file_len\":{},{}}}",
            r.elapsed.as_secs_f64(), r.blocks_received, r.blocks_reused, r.data_bytes, r.file_len, mem.json()
        ),
        Err(e) => {
            println!("{{\"side\":\"recv\",\"ok\":false,\"error\":\"{e}\"}}");
            std::process::exit(1);
        }
    }
    conn.closed().await;
}
