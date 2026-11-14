//! SeSe over QUIC.
//!
//! One transfer is one bidirectional QUIC stream, opened by the sender:
//!
//! ```text
//! sender -> receiver   HEADER  "SESQ" v1, id, file length, block size,
//!                              the tail of the file (root footer + trailer),
//!                              the BLAKE3 hash of each block
//! receiver -> sender   WANT    status, then a bitmap: bit k = 1 asks for block k
//! sender -> receiver   DATA    mode 1: one zstd stream of the whole file (all wanted)
//!                              mode 0: each wanted block, in order: codec, length, payload
//!                              then the sender finishes its direction
//! receiver -> sender   RESULT  0 = the file is complete and its BLAKE3 is the id
//!                              1 = refused, with a reason
//! ```
//!
//! The receiver takes the blocks that it already holds from its [`BlockStore`], checks
//! the hash of every block that arrives before it writes it, and checks the id of the
//! whole file at the end. A transfer whose connection is cut leaves `out.partial` with
//! the blocks that were verified; the next transfer to the same path counts them as held
//! and asks only for the rest (resume). See `SPEC.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sese::Hash;

/// The ALPN of the protocol.
pub const ALPN: &[u8] = b"sese/1";
/// The first bytes of a header.
pub const HEADER_MAGIC: &[u8; 4] = b"SESQ";
/// The version of the stream format.
pub const VERSION: u8 = 1;
/// The largest tail (root footer + trailer) that a receiver accepts.
pub const MAX_TAIL: u64 = 1 << 20;

/// Errors of a transfer.
#[derive(Debug)]
pub enum Error {
    /// A local file error.
    Io(std::io::Error),
    /// The QUIC connection or stream failed.
    Quic(String),
    /// The peer broke the stream format.
    Protocol(String),
    /// A block does not have the hash of the header.
    BlockHash {
        /// The block index.
        index: u64,
    },
    /// The whole file does not have the id of the header.
    Id,
    /// The receiver refused (reported to the sender).
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Quic(e) => write!(f, "quic: {e}"),
            Error::Protocol(e) => write!(f, "protocol: {e}"),
            Error::BlockHash { index } => write!(f, "block {index} does not match its hash"),
            Error::Id => write!(f, "the file does not match its id"),
            Error::Refused(e) => write!(f, "refused by the receiver: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

fn quic(e: impl std::fmt::Display) -> Error {
    Error::Quic(e.to_string())
}

fn proto(e: impl Into<String>) -> Error {
    Error::Protocol(e.into())
}

// ---------------------------------------------------------------------------------------
// Header

/// What the sender says first: the identity, the shape and the blocks of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// BLAKE3 of the whole file.
    pub id: Hash,
    /// Its length.
    pub file_len: u64,
    /// The block size.
    pub block_size: u32,
    /// The last bytes of the file, from the root footer to the end: the footer is the
    /// header (README: "QUIC uses the footer as a header").
    pub tail: Vec<u8>,
    /// The hash of each block.
    pub hashes: Vec<Hash>,
}

impl Header {
    /// The header of `file` with `block_size`.
    pub fn of(file: &[u8], block_size: u32) -> Result<Self, Error> {
        let root = sese::Message::root(file).map_err(|e| proto(format!("not a SeSe file: {e}")))?;
        let start = usize::try_from(root.footer_offset()).map_err(|_| proto("footer offset"))?;
        Ok(Header {
            id: sese::id(file),
            file_len: file.len() as u64,
            block_size,
            tail: file.get(start..).unwrap_or_default().to_vec(),
            hashes: sese::blocks::block_hashes(file, block_size as usize),
        })
    }

    /// The bytes of the header on the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(64 + self.tail.len() + self.hashes.len() * 32);
        v.extend_from_slice(HEADER_MAGIC);
        v.push(VERSION);
        v.extend_from_slice(&self.id);
        v.extend_from_slice(&self.file_len.to_le_bytes());
        v.extend_from_slice(&self.block_size.to_le_bytes());
        v.extend_from_slice(&(self.tail.len() as u32).to_le_bytes());
        v.extend_from_slice(&self.tail);
        v.extend_from_slice(&(self.hashes.len() as u64).to_le_bytes());
        for h in &self.hashes {
            v.extend_from_slice(h);
        }
        v
    }

    /// Read and check a header. `max_file_len` bounds what the receiver allocates.
    pub async fn read(recv: &mut RecvStream, max_file_len: u64) -> Result<Self, Error> {
        let mut fixed = [0u8; 4 + 1 + 32 + 8 + 4 + 4];
        read_exact(recv, &mut fixed).await?;
        if &fixed[..4] != HEADER_MAGIC {
            return Err(proto("not a SeSe transfer"));
        }
        if fixed[4] != VERSION {
            return Err(proto(format!("version {} is not supported", fixed[4])));
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&fixed[5..37]);
        let file_len = u64::from_le_bytes(fixed[37..45].try_into().map_err(|_| proto("len"))?);
        let block_size = u32::from_le_bytes(fixed[45..49].try_into().map_err(|_| proto("bs"))?);
        let tail_len = u64::from(u32::from_le_bytes(
            fixed[49..53].try_into().map_err(|_| proto("tail"))?,
        ));
        if !block_size.is_power_of_two() || !(4096..=(4 << 20)).contains(&block_size) {
            return Err(proto(format!("block size {block_size} is not supported")));
        }
        if file_len > max_file_len {
            return Err(proto(format!(
                "the file has {file_len} bytes, more than {max_file_len}"
            )));
        }
        if tail_len > MAX_TAIL || tail_len > file_len || tail_len < sese::TRAILER_LEN as u64 {
            return Err(proto("the tail length is not valid"));
        }
        let mut tail = vec![0u8; tail_len as usize];
        read_exact(recv, &mut tail).await?;
        // The tail must be a root footer and trailer: its footer offset is where it starts.
        let n = tail.len();
        let footer = u64::from_le_bytes(tail[n - 16..n - 8].try_into().map_err(|_| proto("tail"))?);
        let magic = u64::from_le_bytes(tail[n - 8..].try_into().map_err(|_| proto("tail"))?);
        if magic != sese::MAGIC || footer != file_len - tail_len || (tail_len - 16) % 8 != 0 {
            return Err(proto("the tail is not the root footer of the file"));
        }
        let mut count = [0u8; 8];
        read_exact(recv, &mut count).await?;
        let count = u64::from_le_bytes(count);
        if count != sese::blocks::block_count(file_len, u64::from(block_size)) {
            return Err(proto("the block count does not match the length"));
        }
        let mut raw = vec![0u8; (count as usize) * 32];
        read_exact(recv, &mut raw).await?;
        let hashes = raw.as_chunks::<32>().0.to_vec();
        Ok(Header {
            id,
            file_len,
            block_size,
            tail,
            hashes,
        })
    }

    /// The offsets of the root fields, read from the tail (no other byte is needed).
    pub fn root_offsets(&self) -> Vec<u64> {
        let n = self.tail.len().saturating_sub(16);
        self.tail[..n]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect()
    }

    fn block_len(&self, k: u64) -> usize {
        sese::blocks::block_range(self.file_len, u64::from(self.block_size), k)
            .map_or(0, |(s, e)| (e - s) as usize)
    }
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<(), Error> {
    recv.read_exact(buf).await.map_err(quic)
}

// ---------------------------------------------------------------------------------------
// Block stores

/// Blocks that a receiver already holds, by BLAKE3 hash.
pub trait BlockStore: Send + Sync {
    /// True if the store holds the block.
    fn has(&self, hash: &Hash) -> bool;
    /// The bytes of the block.
    fn get(&self, hash: &Hash) -> Option<Vec<u8>>;
    /// Keep a block that was received and checked.
    fn put(&self, hash: &Hash, data: &[u8]);
}

/// A store in memory.
#[derive(Default, Clone)]
pub struct MemStore(Arc<Mutex<HashMap<Hash, Vec<u8>>>>);

impl MemStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
    /// The number of blocks.
    pub fn len(&self) -> usize {
        self.0.lock().map(|m| m.len()).unwrap_or(0)
    }
    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlockStore for MemStore {
    fn has(&self, hash: &Hash) -> bool {
        self.0.lock().map(|m| m.contains_key(hash)).unwrap_or(false)
    }
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.0.lock().ok()?.get(hash).cloned()
    }
    fn put(&self, hash: &Hash, data: &[u8]) {
        if let Ok(mut m) = self.0.lock() {
            m.entry(*hash).or_insert_with(|| data.to_vec());
        }
    }
}

/// A store that is the SeSe files that the receiver already has: it maps each block
/// hash to a file and an offset, so the blocks are not copied anywhere; it reads a block
/// with `pread` when the transfer needs it. `put` keeps nothing: the received file is
/// itself the record (add it with [`FileStore::add`] for the next transfer).
#[derive(Default)]
pub struct FileStore {
    files: Vec<std::fs::File>,
    index: HashMap<Hash, (usize, u64, usize)>,
}

impl FileStore {
    /// Index `paths` with `block_size` (reads each file once, in blocks).
    pub fn open(paths: &[PathBuf], block_size: usize) -> std::io::Result<Self> {
        let mut s = FileStore::default();
        for p in paths {
            s.add(p, block_size)?;
        }
        Ok(s)
    }

    /// Index one more file. Only whole blocks of `block_size` and the last (shorter)
    /// block of the file are indexed.
    pub fn add(&mut self, path: &Path, block_size: usize) -> std::io::Result<()> {
        use std::io::Read;
        let i = self.files.len();
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; block_size];
        let mut at = 0u64;
        loop {
            let mut n = 0;
            while n < block_size {
                let r = f.read(&mut buf[n..])?;
                if r == 0 {
                    break;
                }
                n += r;
            }
            if n == 0 {
                break;
            }
            self.index
                .entry(sese::blocks::block_hash(&buf[..n]))
                .or_insert((i, at, n));
            at += n as u64;
            if n < block_size {
                break;
            }
        }
        self.files.push(f);
        Ok(())
    }

    /// The number of distinct blocks indexed.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True if no block is indexed.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

impl BlockStore for FileStore {
    fn has(&self, hash: &Hash) -> bool {
        self.index.contains_key(hash)
    }
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let (i, at, len) = self.index.get(hash)?;
        let mut v = vec![0u8; *len];
        self.files.get(*i)?.read_exact_at(&mut v, *at).ok()?;
        Some(v)
    }
    fn put(&self, _hash: &Hash, _data: &[u8]) {}
}

/// The store of the caller, and the blocks of a cut transfer (its `.partial` file).
struct WithResume<'a> {
    store: &'a dyn BlockStore,
    resume: FileStore,
}

impl BlockStore for WithResume<'_> {
    fn has(&self, h: &Hash) -> bool {
        self.store.has(h) || self.resume.has(h)
    }
    fn get(&self, h: &Hash) -> Option<Vec<u8>> {
        self.store.get(h).or_else(|| self.resume.get(h))
    }
    fn put(&self, h: &Hash, d: &[u8]) {
        self.store.put(h, d);
    }
}

// ---------------------------------------------------------------------------------------
// Sender

/// A fault to inject, for tests.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    /// No fault.
    #[default]
    None,
    /// Close the connection after this many blocks of data.
    CloseAfterBlocks(u64),
    /// Flip one byte of this block (mode 0, raw codec).
    CorruptBlock(u64),
}

/// Options of the sender.
#[derive(Clone, Debug)]
pub struct SendOptions {
    /// Send one zstd stream of the whole file when the receiver wants all blocks.
    pub zstd_whole: bool,
    /// Compress each block with zstd when that makes it smaller (mode 0).
    pub zstd_blocks: bool,
    /// The zstd level.
    pub level: i32,
    /// The block size.
    pub block_size: u32,
    #[doc(hidden)]
    pub fault: Fault,
}

impl Default for SendOptions {
    fn default() -> Self {
        SendOptions {
            zstd_whole: true,
            zstd_blocks: true,
            level: 3,
            block_size: sese::BLOCK_SIZE as u32,
            fault: Fault::None,
        }
    }
}

/// What a sender did.
#[derive(Clone, Debug, Default)]
pub struct Sent {
    /// 0 = blocks, 1 = zstd stream of the whole file, 2 = nothing (receiver had it).
    pub mode: u8,
    /// Blocks that the receiver asked for.
    pub blocks_wanted: u64,
    /// Blocks in the file.
    pub blocks_total: u64,
    /// Bytes written to the stream (header + data).
    pub stream_bytes: u64,
    /// Bytes of the header.
    pub header_bytes: u64,
    /// Time from the start to the result of the receiver.
    pub elapsed: Duration,
}

/// Send `file` (a SeSe file, usually mapped) on a new stream of `conn`.
pub async fn send(conn: &Connection, file: &[u8], opts: &SendOptions) -> Result<Sent, Error> {
    let t = Instant::now();
    let header = Header::of(file, opts.block_size)?;
    let (mut tx, mut rx) = conn.open_bi().await.map_err(quic)?;
    let head = header.encode();
    tx.write_all(&head).await.map_err(quic)?;
    let n = header.hashes.len() as u64;
    // WANT
    let mut status = [0u8; 1];
    read_exact(&mut rx, &mut status).await?;
    if status[0] != 0 {
        return Err(Error::Refused(read_reason(&mut rx).await));
    }
    let mut bitmap = vec![0u8; n.div_ceil(8) as usize];
    read_exact(&mut rx, &mut bitmap).await?;
    let count = (0..n)
        .filter(|k| bitmap[(*k / 8) as usize] & (1 << (k % 8)) != 0)
        .count() as u64;
    let mut sent = Sent {
        blocks_wanted: count,
        blocks_total: n,
        header_bytes: head.len() as u64,
        stream_bytes: head.len() as u64,
        ..Sent::default()
    };
    match send_data(conn, &mut tx, file, opts, n, &bitmap, count, &mut sent).await {
        Ok(()) => {}
        Err(Error::Quic(e)) => {
            // The receiver may have stopped the stream with a reason: read it.
            let reason = tokio::time::timeout(Duration::from_secs(2), async {
                let mut r = [0u8; 1];
                match rx.read_exact(&mut r).await {
                    Ok(()) if r[0] != 0 => Some(read_reason(&mut rx).await),
                    _ => None,
                }
            })
            .await
            .ok()
            .flatten();
            return Err(reason.map_or(Error::Quic(e), Error::Refused));
        }
        Err(e) => return Err(e),
    }
    // RESULT
    let mut r = [0u8; 1];
    read_exact(&mut rx, &mut r).await?;
    if r[0] != 0 {
        return Err(Error::Refused(read_reason(&mut rx).await));
    }
    sent.elapsed = t.elapsed();
    Ok(sent)
}

#[allow(clippy::too_many_arguments)]
async fn send_data(
    conn: &Connection,
    tx: &mut SendStream,
    file: &[u8],
    opts: &SendOptions,
    n: u64,
    bitmap: &[u8],
    count: u64,
    sent: &mut Sent,
) -> Result<(), Error> {
    let wanted = |k: u64| bitmap[(k / 8) as usize] & (1 << (k % 8)) != 0;
    let bs = u64::from(opts.block_size);
    if count == n && n > 0 && opts.zstd_whole {
        sent.mode = 1;
        tx.write_all(&[1]).await.map_err(quic)?;
        sent.stream_bytes += 1 + send_zstd_whole(tx, conn, file, opts).await?;
    } else {
        sent.mode = if count == 0 { 2 } else { 0 };
        tx.write_all(&[0]).await.map_err(quic)?;
        sent.stream_bytes += 1;
        for (done, k) in (0..n).filter(|k| wanted(*k)).enumerate() {
            if opts.fault == Fault::CloseAfterBlocks(done as u64) {
                cut(conn).await;
                return Err(Error::Quic("closed by the fault".into()));
            }
            let (s, e) = sese::blocks::block_range(file.len() as u64, bs, k)
                .ok_or_else(|| proto("block"))?;
            let block = &file[s as usize..e as usize];
            let compressed = if opts.zstd_blocks {
                zstd::bulk::compress(block, opts.level)
                    .ok()
                    .filter(|c| c.len() < block.len())
            } else {
                None
            };
            let (codec, payload): (u8, std::borrow::Cow<[u8]>) = match compressed {
                Some(c) => (1, c.into()),
                None => (0, block.into()),
            };
            let mut payload = payload;
            if opts.fault == Fault::CorruptBlock(k) && codec == 0 {
                let mut p = payload.into_owned();
                p[0] ^= 0xff;
                payload = p.into();
            }
            let mut pre = [0u8; 5];
            pre[0] = codec;
            pre[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            tx.write_all(&pre).await.map_err(quic)?;
            tx.write_all(&payload).await.map_err(quic)?;
            sent.stream_bytes += 5 + payload.len() as u64;
        }
    }
    tx.finish().map_err(quic)?;
    Ok(())
}

/// The fault: let what was written arrive (a cut after the data, not before), then
/// close the connection with no goodbye on the stream.
async fn cut(conn: &Connection) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(0u32.into(), b"fault");
}

async fn send_zstd_whole(
    tx: &mut SendStream,
    conn: &Connection,
    file: &[u8],
    opts: &SendOptions,
) -> Result<u64, Error> {
    use zstd::stream::raw::{Encoder, Operation, OutBuffer};
    let mut enc = Encoder::new(opts.level)?;
    let _ = enc.set_pledged_src_size(Some(file.len() as u64));
    let mut out = vec![0u8; 1 << 20];
    let mut written = 0u64;
    let step = 1usize << 20;
    let bs = opts.block_size as usize;
    for (i, piece) in file.chunks(step).enumerate() {
        if let Fault::CloseAfterBlocks(b) = opts.fault {
            if (i * step) as u64 >= b * bs as u64 {
                cut(conn).await;
                return Err(Error::Quic("closed by the fault".into()));
            }
        }
        let mut input = zstd::stream::raw::InBuffer::around(piece);
        while input.pos < piece.len() {
            let mut o = OutBuffer::around(&mut out[..]);
            enc.run(&mut input, &mut o)?;
            let n = o.pos();
            tx.write_all(&out[..n]).await.map_err(quic)?;
            written += n as u64;
        }
    }
    loop {
        let mut o = OutBuffer::around(&mut out[..]);
        let left = enc.finish(&mut o, true)?;
        let n = o.pos();
        tx.write_all(&out[..n]).await.map_err(quic)?;
        written += n as u64;
        if left == 0 {
            break;
        }
    }
    Ok(written)
}

async fn read_reason(rx: &mut RecvStream) -> String {
    match rx.read_to_end(4096).await {
        Ok(b) => String::from_utf8_lossy(&b).to_string(),
        Err(e) => format!("(no reason: {e})"),
    }
}

// ---------------------------------------------------------------------------------------
// Receiver

/// Options of the receiver.
#[derive(Clone, Debug)]
pub struct ReceiveOptions {
    /// The largest file that it accepts.
    pub max_file_len: u64,
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        ReceiveOptions {
            max_file_len: 16 << 30,
        }
    }
}

/// What a receiver did.
#[derive(Clone, Debug, Default)]
pub struct Received {
    /// The header.
    pub id: Hash,
    /// File length.
    pub file_len: u64,
    /// Blocks taken from the store.
    pub blocks_reused: u64,
    /// Blocks received.
    pub blocks_received: u64,
    /// Bytes read from the stream after the header.
    pub data_bytes: u64,
    /// Offsets of the root fields, from the header.
    pub root_offsets: Vec<u64>,
    /// Time from the first byte to the end.
    pub elapsed: Duration,
    /// Where the file is.
    pub path: PathBuf,
}

/// Accept one transfer on `conn` and write the file to `out` (through `out.partial`).
pub async fn receive(
    conn: &Connection,
    store: &dyn BlockStore,
    out: &Path,
    opts: &ReceiveOptions,
) -> Result<Received, Error> {
    let (mut tx, mut rx) = conn.accept_bi().await.map_err(quic)?;
    let t = Instant::now();
    let header = match Header::read(&mut rx, opts.max_file_len).await {
        Ok(h) => h,
        Err(e) => {
            refuse(&mut tx, &e.to_string()).await;
            return Err(e);
        }
    };
    // A transfer to the same path that was cut left `out.partial`: its blocks were
    // verified before they were written, so they count as held.
    let resume_path = with_suffix(out, ".resume");
    let mut resume = FileStore::default();
    if partial(out).exists() {
        std::fs::rename(partial(out), &resume_path)?;
    }
    if resume_path.exists() {
        resume.add(&resume_path, header.block_size as usize)?;
    }
    let both = WithResume { store, resume };
    let result = receive_body(&mut tx, &mut rx, &header, &both, out).await;
    drop(both);
    let _ = std::fs::remove_file(&resume_path);
    match result {
        Ok(mut r) => {
            tx.write_all(&[0]).await.map_err(quic)?;
            tx.finish().map_err(quic)?;
            let _ = tx.stopped().await;
            r.elapsed = t.elapsed();
            r.root_offsets = header.root_offsets();
            Ok(r)
        }
        Err(e) => {
            // Keep the verified blocks of a cut connection for a resume; remove them
            // after any other error.
            if !matches!(e, Error::Quic(_)) {
                let _ = std::fs::remove_file(partial(out));
            }
            refuse(&mut tx, &e.to_string()).await;
            let _ = rx.stop(1u32.into());
            Err(e)
        }
    }
}

fn with_suffix(out: &Path, suffix: &str) -> PathBuf {
    let mut p = out.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// Where a transfer to `out` writes until the file is complete and verified. After a cut
/// connection it stays, and the next transfer to `out` resumes from it.
pub fn partial(out: &Path) -> PathBuf {
    with_suffix(out, ".partial")
}

async fn refuse(tx: &mut SendStream, reason: &str) {
    let _ = tx.write_all(&[1]).await;
    let _ = tx.write_all(reason.as_bytes()).await;
    let _ = tx.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), tx.stopped()).await;
}

async fn receive_body(
    tx: &mut SendStream,
    rx: &mut RecvStream,
    header: &Header,
    store: &dyn BlockStore,
    out: &Path,
) -> Result<Received, Error> {
    let n = header.hashes.len() as u64;
    let mut bitmap = vec![0u8; n.div_ceil(8) as usize];
    let mut reused = 0u64;
    for (k, h) in header.hashes.iter().enumerate() {
        if store.has(h) {
            reused += 1;
        } else {
            bitmap[k / 8] |= 1 << (k % 8);
        }
    }
    tx.write_all(&[0]).await.map_err(quic)?;
    tx.write_all(&bitmap).await.map_err(quic)?;
    let mut mode = [0u8; 1];
    read_exact(rx, &mut mode).await?;
    let file = std::fs::File::create(partial(out))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let mut hasher = blake3::Hasher::new();
    let mut r = Received {
        id: header.id,
        file_len: header.file_len,
        blocks_reused: reused,
        path: out.to_path_buf(),
        data_bytes: 1,
        ..Received::default()
    };
    match mode[0] {
        1 => {
            if reused != 0 {
                return Err(proto(
                    "a zstd stream of the whole file when blocks are held",
                ));
            }
            receive_zstd_whole(rx, header, store, &mut w, &mut hasher, &mut r).await?;
        }
        0 => {
            for k in 0..n {
                let h = &header.hashes[k as usize];
                let len = header.block_len(k);
                let block = if bitmap[(k / 8) as usize] & (1 << (k % 8)) == 0 {
                    store.get(h).ok_or_else(|| proto("a held block is gone"))?
                } else {
                    let mut pre = [0u8; 5];
                    read_exact(rx, &mut pre).await?;
                    let plen =
                        u32::from_le_bytes(pre[1..].try_into().map_err(|_| proto("len"))?) as usize;
                    if plen > len + len / 2 + 1024 {
                        return Err(proto(format!(
                            "block {k}: payload of {plen} bytes is too long"
                        )));
                    }
                    let mut payload = vec![0u8; plen];
                    read_exact(rx, &mut payload).await?;
                    r.data_bytes += 5 + plen as u64;
                    let data = match pre[0] {
                        0 => payload,
                        1 => zstd::bulk::decompress(&payload, len)
                            .map_err(|e| proto(format!("block {k}: {e}")))?,
                        c => return Err(proto(format!("codec {c}"))),
                    };
                    if data.len() != len || sese::blocks::block_hash(&data) != *h {
                        return Err(Error::BlockHash { index: k });
                    }
                    store.put(h, &data);
                    r.blocks_received += 1;
                    data
                };
                hasher.update(&block);
                w.write_all(&block)?;
            }
            // Nothing may follow the last block.
            let rest = rx
                .read_to_end(1)
                .await
                .map_err(|e| proto(format!("after the last block: {e}")))?;
            if !rest.is_empty() {
                return Err(proto("data after the last block"));
            }
        }
        m => return Err(proto(format!("mode {m}"))),
    }
    w.flush()?;
    drop(w);
    if *hasher.finalize().as_bytes() != header.id {
        return Err(Error::Id);
    }
    std::fs::rename(partial(out), out)?;
    Ok(r)
}

async fn receive_zstd_whole(
    rx: &mut RecvStream,
    header: &Header,
    store: &dyn BlockStore,
    w: &mut BufWriter<std::fs::File>,
    hasher: &mut blake3::Hasher,
    r: &mut Received,
) -> Result<(), Error> {
    use zstd::stream::raw::{DParameter, Decoder, InBuffer, Operation, OutBuffer};
    let mut dec = Decoder::new()?;
    dec.set_parameter(DParameter::WindowLogMax(24))?;
    let n = header.hashes.len() as u64;
    let mut k = 0u64;
    let mut block = vec![0u8; header.block_size as usize];
    let mut filled = 0usize;
    let mut eof = false;
    while !eof {
        let chunk = rx.read_chunk(1 << 20, true).await.map_err(quic)?;
        let bytes = match chunk {
            Some(c) => c.bytes,
            None => {
                eof = true;
                bytes::Bytes::new()
            }
        };
        r.data_bytes += bytes.len() as u64;
        let mut input = InBuffer::around(&bytes[..]);
        loop {
            let before = input.pos;
            if k >= n {
                // The file is complete: any more output is an error.
                let mut extra = [0u8; 1];
                let produced = {
                    let mut o = OutBuffer::around(&mut extra[..]);
                    dec.run(&mut input, &mut o)
                        .map_err(|e| proto(format!("zstd: {e}")))?;
                    o.pos()
                };
                if produced != 0 {
                    return Err(proto("more data than the file length"));
                }
                if input.pos == before {
                    break;
                }
                continue;
            }
            let len = header.block_len(k);
            let produced = {
                let mut o = OutBuffer::around(&mut block[filled..len]);
                dec.run(&mut input, &mut o)
                    .map_err(|e| proto(format!("zstd: {e}")))?;
                o.pos()
            };
            filled += produced;
            if filled == len {
                let h = &header.hashes[k as usize];
                let data = &block[..len];
                if sese::blocks::block_hash(data) != *h {
                    return Err(Error::BlockHash { index: k });
                }
                store.put(h, data);
                hasher.update(data);
                w.write_all(data)?;
                k += 1;
                filled = 0;
                r.blocks_received += 1;
                continue;
            }
            if produced == 0 && input.pos == before {
                break;
            }
        }
    }
    if k != n {
        return Err(proto(format!("the stream ended after {k} of {n} blocks")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Endpoints

/// QUIC transport settings.
#[derive(Clone, Debug)]
pub struct TransportOptions {
    /// BBR instead of the default congestion controller (CUBIC).
    pub bbr: bool,
    /// The receive window of a stream and of the connection, bytes.
    pub window: u64,
}

impl Default for TransportOptions {
    fn default() -> Self {
        TransportOptions {
            bbr: false,
            window: 16 << 20,
        }
    }
}

fn transport(t: &TransportOptions) -> Arc<quinn::TransportConfig> {
    let mut c = quinn::TransportConfig::default();
    let w = quinn::VarInt::from_u64(t.window).unwrap_or(quinn::VarInt::MAX);
    c.stream_receive_window(w);
    c.receive_window(w);
    c.send_window(t.window);
    c.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
        60_000,
    ))));
    if t.bbr {
        c.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }
    Arc::new(c)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server endpoint on `addr` with a new self-signed certificate for "localhost".
/// Gives the certificate, for the client to pin.
pub fn server_endpoint(
    addr: SocketAddr,
    t: &TransportOptions,
) -> Result<(Endpoint, CertificateDer<'static>), Error> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .map_err(|e| proto(e.to_string()))?;
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(quic)?
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .map_err(quic)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(quic)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    sc.transport_config(transport(t));
    let ep = Endpoint::server(sc, addr)?;
    Ok((ep, der))
}

/// A client endpoint that trusts only `cert` (the server's, pinned).
pub fn client_endpoint(
    bind: SocketAddr,
    cert: CertificateDer<'static>,
    t: &TransportOptions,
) -> Result<Endpoint, Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).map_err(quic)?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(quic)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(quic)?;
    let mut cc = quinn::ClientConfig::new(Arc::new(crypto));
    cc.transport_config(transport(t));
    let mut ep = Endpoint::client(bind)?;
    ep.set_default_client_config(cc);
    Ok(ep)
}
