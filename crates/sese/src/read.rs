//! The zero-copy reader.
//!
//! [`Message::root`] checks the trailer of the file. Each accessor checks the bounds of
//! what it reads, with checked arithmetic, against the body of its message (the bytes
//! before the footer). Nothing is copied: `bytes` and `string` values are slices of the
//! input. No input makes the reader panic or allocate in proportion to a length field.
//!
//! A field index past the end of the footer reads as absent, so a reader with a newer
//! schema reads a file from an older writer.

use crate::{u64_at, Error, ABSENT, MAGIC, TRAILER_LEN};

/// A message: a region of the file with its footer.
#[derive(Clone, Copy, Debug)]
pub struct Message<'a> {
    buf: &'a [u8],
    /// Where the region starts (absolute).
    start: u64,
    /// Where the footer starts (absolute). The body is `start..footer`.
    footer: u64,
    /// The number of footer entries.
    count: u64,
}

fn bounds(what: &'static str, offset: u64) -> Error {
    Error::OutOfBounds { what, offset }
}

impl<'a> Message<'a> {
    /// The root message: the whole of `buf`.
    pub fn root(buf: &'a [u8]) -> Result<Self, Error> {
        Self::region(buf, 0, buf.len() as u64)
    }

    /// The message in `start..end` of `buf`.
    pub fn region(buf: &'a [u8], start: u64, end: u64) -> Result<Self, Error> {
        let len = end
            .checked_sub(start)
            .ok_or(Error::TooShort { start, len: 0 })?;
        if end > buf.len() as u64 {
            return Err(bounds("a region", start));
        }
        if len < TRAILER_LEN as u64 {
            return Err(Error::TooShort { start, len });
        }
        let trailer = end - TRAILER_LEN as u64;
        if u64_at(buf, trailer + 8) != Some(MAGIC) {
            return Err(Error::BadMagic { end });
        }
        let footer = u64_at(buf, trailer).ok_or(Error::BadMagic { end })?;
        if footer < start || footer > trailer || !(trailer - footer).is_multiple_of(8) {
            return Err(Error::BadFooter { footer });
        }
        Ok(Message {
            buf,
            start,
            footer,
            count: (trailer - footer) / 8,
        })
    }

    /// The whole input that this message is in.
    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

    /// Where the region of this message starts.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Where its footer starts (the end of its body).
    pub fn footer_offset(&self) -> u64 {
        self.footer
    }

    /// Where the region ends (after the trailer).
    pub fn end(&self) -> u64 {
        self.footer + self.count * 8 + TRAILER_LEN as u64
    }

    /// The number of entries of the footer.
    pub fn field_count(&self) -> u64 {
        self.count
    }

    /// The offset of field `index`, or `None` if it is absent or past the footer. The
    /// offset is checked to be in the body.
    pub fn offset(&self, index: usize) -> Result<Option<u64>, Error> {
        let index = index as u64;
        if index >= self.count {
            return Ok(None);
        }
        let entry = self.footer + index * 8;
        let at = u64_at(self.buf, entry).ok_or(bounds("a footer entry", entry))?;
        if at == ABSENT {
            return Ok(None);
        }
        if at < self.start || at >= self.footer {
            return Err(bounds("a field offset", at));
        }
        Ok(Some(at))
    }

    /// `n` bytes at `at`, inside the body.
    fn span(&self, what: &'static str, at: u64, n: u64) -> Result<&'a [u8], Error> {
        span(self.buf, self.start, self.footer, what, at, n)
    }

    /// The bool of field `index`.
    pub fn bool(&self, index: usize) -> Result<Option<bool>, Error> {
        self.offset(index)?
            .map(|at| read_bool(self, at))
            .transpose()
    }

    /// The u64 of field `index`.
    pub fn u64(&self, index: usize) -> Result<Option<u64>, Error> {
        self.offset(index)?.map(|at| read_u64(self, at)).transpose()
    }

    /// The bytes of field `index` (a slice of the input).
    pub fn bytes(&self, index: usize) -> Result<Option<&'a [u8]>, Error> {
        self.offset(index)?
            .map(|at| read_bytes(self, at))
            .transpose()
    }

    /// The string of field `index` (a slice of the input).
    pub fn str(&self, index: usize) -> Result<Option<&'a str>, Error> {
        self.offset(index)?.map(|at| read_str(self, at)).transpose()
    }

    /// The nested message of field `index`.
    pub fn message(&self, index: usize) -> Result<Option<Message<'a>>, Error> {
        self.offset(index)?
            .map(|at| read_message(self, at))
            .transpose()
    }

    /// The list of field `index`.
    pub fn list(&self, index: usize) -> Result<Option<List<'a>>, Error> {
        self.offset(index)?
            .map(|at| read_list(self, at))
            .transpose()
    }

    /// The absolute range `start..end` of the data of the `bytes` field `index`, with no
    /// copy: for a caller that maps a value to blocks.
    pub fn bytes_range(&self, index: usize) -> Result<Option<(u64, u64)>, Error> {
        match self.offset(index)? {
            None => Ok(None),
            Some(at) => {
                let data = read_bytes(self, at)?;
                Ok(Some((at + 8, at + 8 + data.len() as u64)))
            }
        }
    }
}

fn span<'a>(
    buf: &'a [u8],
    lo: u64,
    hi: u64,
    what: &'static str,
    at: u64,
    n: u64,
) -> Result<&'a [u8], Error> {
    let end = at.checked_add(n).ok_or(bounds(what, at))?;
    if at < lo || end > hi {
        return Err(bounds(what, at));
    }
    let (a, e) = (
        usize::try_from(at).map_err(|_| bounds(what, at))?,
        usize::try_from(end).map_err(|_| bounds(what, at))?,
    );
    buf.get(a..e).ok_or(bounds(what, at))
}

fn read_u64(m: &Message<'_>, at: u64) -> Result<u64, Error> {
    let b = m.span("a u64", at, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Ok(u64::from_le_bytes(a))
}

fn read_bool(m: &Message<'_>, at: u64) -> Result<bool, Error> {
    match m.span("a bool", at, 1)?.first() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(Error::BadBool { offset: at }),
    }
}

fn read_bytes<'a>(m: &Message<'a>, at: u64) -> Result<&'a [u8], Error> {
    let len = read_u64(m, at)?;
    m.span("bytes", at + 8, len)
}

fn read_str<'a>(m: &Message<'a>, at: u64) -> Result<&'a str, Error> {
    std::str::from_utf8(read_bytes(m, at)?).map_err(|_| Error::BadUtf8 { offset: at })
}

fn read_message<'a>(m: &Message<'a>, at: u64) -> Result<Message<'a>, Error> {
    let len = read_u64(m, at)?;
    let start = at + 8;
    let end = start.checked_add(len).ok_or(bounds("a message", at))?;
    if end > m.footer {
        return Err(bounds("a message", at));
    }
    Message::region(m.buf, start, end)
}

fn read_list<'a>(m: &Message<'a>, at: u64) -> Result<List<'a>, Error> {
    let count = read_u64(m, at)?;
    Ok(List { msg: *m, at, count })
}

/// A list: a u64 count, then the elements. Elements of a fixed size (bool: 1 byte,
/// u64: 8 bytes) are inline. Elements of a variable size (bytes, string, message) are
/// a table of `count` absolute u64 offsets; each offset points to the element, in the
/// body of the same message. The list does not know its element type: the schema does.
#[derive(Clone, Copy, Debug)]
pub struct List<'a> {
    msg: Message<'a>,
    /// The offset of the count.
    at: u64,
    count: u64,
}

impl<'a> List<'a> {
    /// The number of elements.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The `count * size` bytes after the count, checked once.
    fn inline(&self, size: u64) -> Result<&'a [u8], Error> {
        let n = self
            .count
            .checked_mul(size)
            .ok_or(bounds("a list", self.at))?;
        self.msg.span("a list", self.at + 8, n)
    }

    /// The elements as u64. Checks the whole extent first.
    pub fn u64s(&self) -> Result<impl ExactSizeIterator<Item = u64> + 'a, Error> {
        Ok(self.inline(8)?.chunks_exact(8).map(|c| {
            let mut a = [0u8; 8];
            a.copy_from_slice(c);
            u64::from_le_bytes(a)
        }))
    }

    /// The elements as bool. Checks the extent and every byte first.
    pub fn bools(&self) -> Result<Vec<bool>, Error> {
        let raw = self.inline(1)?;
        raw.iter()
            .enumerate()
            .map(|(k, b)| match b {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(Error::BadBool {
                    offset: self.at + 8 + k as u64,
                }),
            })
            .collect()
    }

    /// The offset of variable-size element `k`, checked to be in the body.
    pub fn element_offset(&self, k: u64) -> Result<u64, Error> {
        if k >= self.count {
            return Err(Error::NoElement { index: k });
        }
        let entry = k
            .checked_mul(8)
            .and_then(|o| o.checked_add(self.at + 8))
            .ok_or(bounds("a list offset", self.at))?;
        let b = self.msg.span("a list offset", entry, 8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        let off = u64::from_le_bytes(a);
        if off < self.msg.start || off >= self.msg.footer {
            return Err(bounds("a list element", off));
        }
        Ok(off)
    }

    /// Check that the offset table of a variable-size list fits in the body. Call it
    /// before you reserve memory for `count` elements.
    pub fn check_offsets(&self) -> Result<(), Error> {
        self.inline(8).map(|_| ())
    }

    /// Element `k` as bytes.
    pub fn bytes(&self, k: u64) -> Result<&'a [u8], Error> {
        read_bytes(&self.msg, self.element_offset(k)?)
    }

    /// Element `k` as a string.
    pub fn str(&self, k: u64) -> Result<&'a str, Error> {
        read_str(&self.msg, self.element_offset(k)?)
    }

    /// Element `k` as a message.
    pub fn message(&self, k: u64) -> Result<Message<'a>, Error> {
        read_message(&self.msg, self.element_offset(k)?)
    }
}
