//! Send one SeSe file over QUIC. Usage:
//! `sese-send <server addr> <cert path> <file.sese> [raw]` (`raw`: no zstd at all)
#[tokio::main]
async fn main() {
    let mem = sese_bench::MemPeak::start();
    let a: Vec<String> = std::env::args().collect();
    let t0 = std::time::Instant::now();
    let tr = sese_quic::TransportOptions {
        bbr: std::env::var_os("SESE_BBR").is_some(),
        ..Default::default()
    };
    let cert = rustls::pki_types::CertificateDer::from(std::fs::read(&a[2]).unwrap());
    let ep = sese_quic::client_endpoint("0.0.0.0:0".parse().unwrap(), cert, &tr).unwrap();
    let file = sese::mmap::MappedFile::open(&a[3]).unwrap();
    let raw = a.get(4).map(|s| s == "raw").unwrap_or(false);
    let opts = sese_quic::SendOptions {
        zstd_whole: !raw,
        zstd_blocks: !raw,
        ..Default::default()
    };
    let conn = ep
        .connect(a[1].parse().unwrap(), "localhost")
        .unwrap()
        .await
        .unwrap();
    let s = sese_quic::send(&conn, file.bytes(), &opts).await.unwrap();
    conn.close(0u32.into(), b"done");
    ep.wait_idle().await;
    println!(
        "{{\"side\":\"send\",\"ok\":true,\"elapsed_s\":{:.3},\"total_s\":{:.3},\"mode\":{},\"blocks_wanted\":{},\"blocks_total\":{},\"stream_bytes\":{},\"header_bytes\":{},{}}}",
        s.elapsed.as_secs_f64(), t0.elapsed().as_secs_f64(), s.mode, s.blocks_wanted, s.blocks_total, s.stream_bytes, s.header_bytes, mem.json()
    );
}
