//! Fixed-size blocks of a file, for deduplication and transport.
//!
//! Block `k` of a file is the bytes `k * block_size .. min((k + 1) * block_size, len)`.
//! Its hash is BLAKE3 of those bytes. The transport does not parse the file to find
//! blocks. The canonical writer aligns large `bytes` values to blocks, so the blocks of
//! a large file of the service do not change when other parts of the service change.

use crate::Hash;

/// The number of blocks of a file of `len` bytes.
pub fn block_count(len: u64, block_size: u64) -> u64 {
    if block_size == 0 {
        return 0;
    }
    len.div_ceil(block_size)
}

/// The byte range of block `k`.
pub fn block_range(len: u64, block_size: u64, k: u64) -> Option<(u64, u64)> {
    let start = k.checked_mul(block_size)?;
    if start >= len {
        return None;
    }
    Some((start, start.saturating_add(block_size).min(len)))
}

/// The hash of each block of `file`.
pub fn block_hashes(file: &[u8], block_size: usize) -> Vec<Hash> {
    if block_size == 0 {
        return Vec::new();
    }
    file.chunks(block_size)
        .map(|c| *blake3::hash(c).as_bytes())
        .collect()
}

/// The hash of one block.
pub fn block_hash(block: &[u8]) -> Hash {
    *blake3::hash(block).as_bytes()
}
