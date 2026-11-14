//! SeSe (service serialization): the Flat-Footer format.
//!
//! A SeSe file is one message. A message is a region of bytes:
//!
//! ```text
//! [ value of field 0 ] [ value of field 1 ] ... (the body, in field order, with padding)
//! [ offset of field 0: u64 ] ... [ offset of field N-1: u64 ]   (the footer)
//! [ footer offset: u64 ] [ MAGIC: u64 ]                          (the trailer, 16 bytes)
//! ```
//!
//! All integers are little-endian. All offsets are absolute: they count from byte 0 of
//! the file, also in a nested message. A field that is not present has the offset
//! [`ABSENT`]. See `SPEC.md` at the root of the repository for the full rules.
//!
//! - [`read`]: a zero-copy reader over a `&[u8]` (an mmap or a `Vec`). It checks every
//!   bound and never panics on malformed input.
//! - [`write`]: the deterministic writer. One logical value has one encoding, so the
//!   [`id`] (BLAKE3 of the file) names the content.
//! - [`value`]: a value tree and a schema, for generic code and tests.
//! - [`blocks`]: the fixed-size blocks of a file and their hashes, for deduplication.

#![deny(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![deny(missing_docs)]

pub mod blocks;
pub mod read;
pub mod value;
pub mod write;

#[cfg(feature = "mmap")]
pub mod mmap;

pub use read::{List, Message};
pub use write::{Writer, WriterOptions};

/// The magic number, the last 8 bytes of a message: `0x46465F564D5F4D50` ("FF_VM_MP"),
/// written little-endian like every integer (so the bytes are `50 4D 5F 4D 56 5F 46 46`).
pub const MAGIC: u64 = 0x4646_5F56_4D5F_4D50;

/// The offset of a field that is not present.
pub const ABSENT: u64 = u64::MAX;

/// The size of the trailer: the footer offset and the magic number.
pub const TRAILER_LEN: usize = 16;

/// The block size of the canonical writer and of the transport: 16 KiB.
pub const BLOCK_SIZE: usize = 16 * 1024;

/// A `bytes` value of at least this many bytes is block-aligned by the canonical
/// writer: its data starts at a multiple of [`BLOCK_SIZE`], and zero padding fills
/// the rest of its last block. 64 KiB.
pub const ALIGN_MIN: usize = 64 * 1024;

/// The deepest nesting that the recursive helpers ([`value::decode`], the Celaut
/// mapping) follow. The lazy reader has no limit: it reads one level at a time.
pub const MAX_DEPTH: usize = 64;

/// A 32-byte BLAKE3 hash.
pub type Hash = [u8; 32];

/// The id of a SeSe file: BLAKE3 of all of its bytes.
pub fn id(file: &[u8]) -> Hash {
    *blake3::hash(file).as_bytes()
}

/// Errors of the reader and the writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A region is shorter than the trailer.
    TooShort {
        /// Where the region starts.
        start: u64,
        /// Its length.
        len: u64,
    },
    /// The last 8 bytes of a region are not [`MAGIC`].
    BadMagic {
        /// Where the region ends.
        end: u64,
    },
    /// The footer offset is outside of its region, or the footer is not a whole
    /// number of u64.
    BadFooter {
        /// The footer offset.
        footer: u64,
    },
    /// A field index is not in the footer.
    NoField {
        /// The index.
        index: usize,
    },
    /// A value (or its length, or a list element) is outside of the body of its message.
    OutOfBounds {
        /// What was read.
        what: &'static str,
        /// Where.
        offset: u64,
    },
    /// A bool byte that is not 0 or 1.
    BadBool {
        /// Where.
        offset: u64,
    },
    /// A string that is not UTF-8.
    BadUtf8 {
        /// Where.
        offset: u64,
    },
    /// A list element index is not below the count.
    NoElement {
        /// The index.
        index: u64,
    },
    /// Nesting deeper than [`MAX_DEPTH`].
    TooDeep,
    /// The value does not have the type that the schema gives.
    Type(String),
    /// The writer was used in a wrong order (for example a field index that is not
    /// greater than the last one).
    Usage(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TooShort { start, len } => {
                write!(
                    f,
                    "the region at {start} has {len} bytes, less than the trailer"
                )
            }
            Error::BadMagic { end } => write!(f, "no magic number before byte {end}"),
            Error::BadFooter { footer } => write!(f, "the footer offset {footer} is not valid"),
            Error::NoField { index } => write!(f, "field {index} is not in the footer"),
            Error::OutOfBounds { what, offset } => {
                write!(f, "{what} at {offset} is outside of its message")
            }
            Error::BadBool { offset } => write!(f, "the bool at {offset} is not 0 or 1"),
            Error::BadUtf8 { offset } => write!(f, "the string at {offset} is not UTF-8"),
            Error::NoElement { index } => write!(f, "list element {index} is not in the list"),
            Error::TooDeep => write!(f, "nesting deeper than {MAX_DEPTH}"),
            Error::Type(m) => write!(f, "type: {m}"),
            Error::Usage(m) => write!(f, "writer usage: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Read a little-endian u64 at `at` of `buf`, if the 8 bytes are there.
pub(crate) fn u64_at(buf: &[u8], at: u64) -> Option<u64> {
    let start = usize::try_from(at).ok()?;
    let end = start.checked_add(8)?;
    let bytes: [u8; 8] = buf.get(start..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}
