//! SeSe over QUIC on the loopback: full transfers, partial dedup, resume, abort, and a
//! corrupted block that the receiver refuses.

use std::net::SocketAddr;
use std::path::PathBuf;

use quinn::Connection;
use sese::value::{encode, Value};
use sese::WriterOptions;
use sese_quic::*;

struct Pair {
    client: Connection,
    server: Connection,
    _eps: (quinn::Endpoint, quinn::Endpoint),
}

async fn pair() -> Pair {
    let t = TransportOptions::default();
    let (server_ep, cert) = server_endpoint("127.0.0.1:0".parse().unwrap(), &t).unwrap();
    let addr: SocketAddr = server_ep.local_addr().unwrap();
    let client_ep = client_endpoint("127.0.0.1:0".parse().unwrap(), cert, &t).unwrap();
    let accept = tokio::spawn({
        let ep = server_ep.clone();
        async move { ep.accept().await.unwrap().await.unwrap() }
    });
    let client = client_ep.connect(addr, "localhost").unwrap().await.unwrap();
    let server = accept.await.unwrap();
    Pair {
        client,
        server,
        _eps: (server_ep, client_ep),
    }
}

/// A SeSe file: some text, an incompressible 1 MiB file, a compressible 300 KiB file.
fn file(seed: u8, insert: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u32 ^ u32::from(seed);
    let mut random: Vec<u8> = (0..(1 << 20))
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    // An insertion in the middle (a changed version).
    let mid = random.len() / 2;
    random.splice(mid..mid, std::iter::repeat_n(0xAB, insert));
    let text: Vec<u8> = b"the same line again\n"
        .iter()
        .copied()
        .cycle()
        .take(300 * 1024)
        .collect();
    encode(
        &[
            Value::Str(format!("service version {seed}")),
            Value::Bytes(random),
            Value::Bytes(text),
            Value::ListStr(vec!["a".into(), "b".into()]),
        ],
        WriterOptions::default(),
    )
    .unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sese-quic-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("out.sese")
}

fn n_blocks(f: &[u8]) -> u64 {
    sese::blocks::block_count(f.len() as u64, sese::BLOCK_SIZE as u64)
}

async fn transfer(
    p: &Pair,
    f: &[u8],
    store: &dyn BlockStore,
    out: &std::path::Path,
    opts: SendOptions,
) -> (Result<Sent, Error>, Result<Received, Error>) {
    let f2 = f.to_vec();
    let client = p.client.clone();
    let send = tokio::spawn(async move { send(&client, &f2, &opts).await });
    let r = receive(&p.server, store, out, &ReceiveOptions::default()).await;
    (send.await.unwrap(), r)
}

#[tokio::test]
async fn full_transfer_uses_one_zstd_stream() {
    let p = pair().await;
    let f = file(1, 0);
    let out = tmp("full");
    let (s, r) = transfer(&p, &f, &MemStore::new(), &out, SendOptions::default()).await;
    let (s, r) = (s.unwrap(), r.unwrap());
    assert_eq!(s.mode, 1);
    assert_eq!(s.blocks_wanted, n_blocks(&f));
    assert_eq!(r.blocks_received, n_blocks(&f));
    assert_eq!(std::fs::read(&out).unwrap(), f);
    assert_eq!(r.id, sese::id(&f));
    // The compressible part shrinks: fewer bytes on the stream than in the file.
    assert!(
        s.stream_bytes < f.len() as u64,
        "{} < {}",
        s.stream_bytes,
        f.len()
    );
    // The footer came first, as the header: the receiver knows the root offsets.
    let root = sese::Message::root(&f).unwrap();
    assert_eq!(r.root_offsets.len() as u64, root.field_count());
    assert_eq!(r.root_offsets[1], root.offset(1).unwrap().unwrap());
}

#[tokio::test]
async fn full_transfer_in_blocks() {
    let p = pair().await;
    let f = file(2, 0);
    let out = tmp("blocks");
    let opts = SendOptions {
        zstd_whole: false,
        ..SendOptions::default()
    };
    let (s, r) = transfer(&p, &f, &MemStore::new(), &out, opts).await;
    assert_eq!(s.unwrap().mode, 0);
    assert_eq!(r.unwrap().blocks_received, n_blocks(&f));
    assert_eq!(std::fs::read(&out).unwrap(), f);
}

#[tokio::test]
async fn partial_dedup_sends_only_missing_blocks() {
    let p = pair().await;
    let v1 = file(3, 0);
    let v2 = file(3, 100); // 100 bytes inserted in the middle of the large file
    let held = tmp("dedup-v1");
    std::fs::write(&held, &v1).unwrap();
    let store = FileStore::open(&[held], sese::BLOCK_SIZE).unwrap();
    let have: std::collections::HashSet<_> = sese::blocks::block_hashes(&v1, sese::BLOCK_SIZE)
        .into_iter()
        .collect();
    let missing = sese::blocks::block_hashes(&v2, sese::BLOCK_SIZE)
        .iter()
        .filter(|h| !have.contains(*h))
        .count() as u64;
    assert!(
        missing > 0 && missing < n_blocks(&v2) / 2,
        "missing {missing} of {}",
        n_blocks(&v2)
    );
    let out = tmp("dedup");
    let (s, r) = transfer(&p, &v2, &store, &out, SendOptions::default()).await;
    let (s, r) = (s.unwrap(), r.unwrap());
    assert_eq!(s.mode, 0, "blocks, not the whole-file stream");
    assert_eq!(s.blocks_wanted, missing);
    assert_eq!(r.blocks_received, missing);
    assert_eq!(r.blocks_reused, n_blocks(&v2) - missing);
    assert_eq!(std::fs::read(&out).unwrap(), v2);
}

#[tokio::test]
async fn nothing_to_send_when_the_receiver_has_every_block() {
    let p = pair().await;
    let f = file(4, 0);
    let store = MemStore::new();
    for c in f.chunks(sese::BLOCK_SIZE) {
        store.put(&sese::blocks::block_hash(c), c);
    }
    let out = tmp("all");
    let (s, r) = transfer(&p, &f, &store, &out, SendOptions::default()).await;
    let s = s.unwrap();
    assert_eq!((s.mode, s.blocks_wanted), (2, 0));
    assert_eq!(r.unwrap().blocks_received, 0);
    assert_eq!(std::fs::read(&out).unwrap(), f);
}

async fn resume_case(zstd_whole: bool, after: u64) {
    let f = file(5, 0);
    let out = tmp(&format!("resume-{zstd_whole}"));
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(partial(&out));
    // First try: the sender cuts the connection after `after` blocks.
    let p = pair().await;
    let opts = SendOptions {
        zstd_whole,
        fault: Fault::CloseAfterBlocks(after),
        ..SendOptions::default()
    };
    let (s, r) = transfer(&p, &f, &MemStore::new(), &out, opts).await;
    assert!(s.is_err());
    assert!(matches!(r, Err(Error::Quic(_))), "{r:?}");
    assert!(!out.exists(), "no output after a failed transfer");
    let kept = std::fs::metadata(partial(&out)).unwrap().len() / sese::BLOCK_SIZE as u64;
    assert!(
        kept >= after.min(1),
        "verified blocks are kept in the .partial file: {kept}"
    );
    // Second try, new connection, empty store: only the rest is sent.
    let p = pair().await;
    let (s, r) = transfer(&p, &f, &MemStore::new(), &out, SendOptions::default()).await;
    let (s, r) = (s.unwrap(), r.unwrap());
    assert_eq!(s.blocks_wanted, n_blocks(&f) - kept);
    assert_eq!(r.blocks_reused, kept);
    assert_eq!(std::fs::read(&out).unwrap(), f);
    assert!(!partial(&out).exists());
}

#[tokio::test]
async fn resume_after_a_cut_in_block_mode() {
    resume_case(false, 20).await;
}

#[tokio::test]
async fn resume_after_a_cut_in_the_zstd_stream() {
    resume_case(true, 40).await;
}

#[tokio::test]
async fn corrupted_block_is_refused() {
    let p = pair().await;
    let f = file(6, 0);
    let store = MemStore::new();
    let out = tmp("corrupt");
    let opts = SendOptions {
        zstd_whole: false,
        zstd_blocks: false,
        fault: Fault::CorruptBlock(3),
        ..SendOptions::default()
    };
    let (s, r) = transfer(&p, &f, &store, &out, opts).await;
    assert!(matches!(r, Err(Error::BlockHash { index: 3 })), "{r:?}");
    match s {
        Err(Error::Refused(reason)) => assert!(reason.contains("block 3"), "{reason}"),
        other => panic!("sender: {other:?}"),
    }
    assert!(!out.exists());
    assert!(!PathBuf::from(format!("{}.partial", out.display())).exists());
    // Blocks before the bad one were verified and kept; the bad one was not.
    assert_eq!(store.len(), 3);
}

/// A sender that lies about the id: every block matches its hash, the whole does not.
#[tokio::test]
async fn wrong_id_is_refused() {
    let p = pair().await;
    let f = file(7, 0);
    let mut h = Header::of(&f, sese::BLOCK_SIZE as u32).unwrap();
    h.id = [0u8; 32];
    let out = tmp("wrongid");
    let client = p.client.clone();
    let f2 = f.clone();
    let sender = tokio::spawn(async move {
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        tx.write_all(&h.encode()).await.unwrap();
        let mut want = vec![0u8; 1 + h.hashes.len().div_ceil(8)];
        rx.read_exact(&mut want).await.unwrap();
        tx.write_all(&[0]).await.unwrap();
        for c in f2.chunks(sese::BLOCK_SIZE) {
            tx.write_all(&[0]).await.unwrap();
            tx.write_all(&(c.len() as u32).to_le_bytes()).await.unwrap();
            tx.write_all(c).await.unwrap();
        }
        tx.finish().unwrap();
        rx.read_to_end(1 << 16).await.unwrap()
    });
    let r = receive(
        &p.server,
        &MemStore::new(),
        &out,
        &ReceiveOptions::default(),
    )
    .await;
    assert!(matches!(r, Err(Error::Id)), "{r:?}");
    let answer = sender.await.unwrap();
    assert_eq!(answer[0], 1);
    assert!(!out.exists());
}

#[tokio::test]
async fn malformed_header_is_refused() {
    let p = pair().await;
    let client = p.client.clone();
    let sender = tokio::spawn(async move {
        let (mut tx, mut rx) = client.open_bi().await.unwrap();
        tx.write_all(b"NOPE and some more bytes than the fixed part of a header..")
            .await
            .unwrap();
        tx.finish().unwrap();
        rx.read_to_end(1 << 16).await.unwrap()
    });
    let r = receive(
        &p.server,
        &MemStore::new(),
        &tmp("bad"),
        &ReceiveOptions::default(),
    )
    .await;
    assert!(matches!(r, Err(Error::Protocol(_))));
    let answer = sender.await.unwrap();
    assert_eq!(answer[0], 1);
    assert!(String::from_utf8_lossy(&answer[1..]).contains("not a SeSe transfer"));
}

#[tokio::test]
async fn receiver_refuses_a_file_that_is_too_large() {
    let p = pair().await;
    let f = file(8, 0);
    let client = p.client.clone();
    let f2 = f.clone();
    let send = tokio::spawn(async move { send(&client, &f2, &SendOptions::default()).await });
    let r = receive(
        &p.server,
        &MemStore::new(),
        &tmp("big"),
        &ReceiveOptions { max_file_len: 1000 },
    )
    .await;
    assert!(r.is_err());
    match send.await.unwrap() {
        Err(Error::Refused(reason)) => assert!(reason.contains("more than 1000"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn header_round_trip_and_tail() {
    let f = file(9, 0);
    let h = Header::of(&f, sese::BLOCK_SIZE as u32).unwrap();
    let root = sese::Message::root(&f).unwrap();
    assert_eq!(h.tail.len() as u64, f.len() as u64 - root.footer_offset());
    assert_eq!(
        h.root_offsets(),
        (0..4)
            .map(|i| root.offset(i).unwrap().unwrap())
            .collect::<Vec<_>>()
    );
    let enc = h.encode();
    assert_eq!(&enc[..4], HEADER_MAGIC);
    assert_eq!(
        enc.len(),
        4 + 1 + 32 + 8 + 4 + 4 + h.tail.len() + 8 + 32 * h.hashes.len()
    );
}
