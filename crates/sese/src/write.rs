//! The deterministic writer.
//!
//! The writer appends to one `Vec<u8>`. Each message is written as its fields in
//! ascending index order, then its footer, then its trailer. A nested message is a
//! `u64` length and then the region of the child, so the parent writes the child in
//! place and patches the length when the child ends. Offsets are absolute.
//!
//! The canonical rules (see `SPEC.md`):
//!
//! - Fields are written in ascending index order. The footer has one entry for each
//!   index from 0 to the number of fields that `begin` gave. An index that is not
//!   written is [`ABSENT`].
//! - A `bytes` value with at least `align_min` bytes is aligned: zero bytes are added
//!   before its length so that its data starts at a multiple of `block_size`, and
//!   after its data up to the next multiple of `block_size`. No other padding exists.
//! - The elements of a variable-size list are written after its offset table, in order.
//!
//! With these rules the bytes are a function of the logical value, so equal values
//! have equal ids.

use crate::{Error, ABSENT, ALIGN_MIN, BLOCK_SIZE, MAGIC};

/// Options of the writer. The canonical encoding uses [`WriterOptions::default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterOptions {
    /// The block size for alignment.
    pub block_size: u64,
    /// The smallest `bytes` value that is aligned. `u64::MAX` turns alignment off.
    pub align_min: u64,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            block_size: BLOCK_SIZE as u64,
            align_min: ALIGN_MIN as u64,
        }
    }
}

/// What is open on the writer stack.
#[derive(Debug)]
enum Frame {
    /// A message: where its region starts, where its length is (if nested), its footer,
    /// and the next index that can be written.
    Message {
        len_at: Option<usize>,
        start: usize,
        footer: Vec<u64>,
        next: usize,
    },
    /// A variable-size list: where its offset table is, its size, and how many
    /// elements are written.
    List {
        table: usize,
        count: u64,
        written: u64,
    },
}

/// The deterministic writer. See the module docs.
#[derive(Debug)]
pub struct Writer {
    buf: Vec<u8>,
    opts: WriterOptions,
    stack: Vec<Frame>,
    done: bool,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new(WriterOptions::default())
    }
}

fn patch(buf: &mut [u8], at: usize, v: u64) -> Result<(), Error> {
    buf.get_mut(at..at + 8)
        .ok_or(Error::Usage("patch outside of the buffer"))?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

impl Writer {
    /// A writer with `opts`.
    pub fn new(opts: WriterOptions) -> Self {
        Writer {
            buf: Vec::new(),
            opts,
            stack: Vec::new(),
            done: false,
        }
    }

    /// A writer that reserves `capacity` bytes.
    pub fn with_capacity(opts: WriterOptions, capacity: usize) -> Self {
        let mut w = Self::new(opts);
        w.buf.reserve(capacity);
        w
    }

    /// The number of bytes written so far.
    pub fn position(&self) -> u64 {
        self.buf.len() as u64
    }

    /// Start the root message with `fields` footer entries.
    pub fn begin(&mut self, fields: usize) -> Result<(), Error> {
        if !self.stack.is_empty() || self.done {
            return Err(Error::Usage("begin: the root is already open or written"));
        }
        self.stack.push(Frame::Message {
            len_at: None,
            start: self.buf.len(),
            footer: vec![ABSENT; fields],
            next: 0,
        });
        Ok(())
    }

    /// Record that a value starts here: for field `index` of the open message, or for
    /// the next element of the open list.
    fn slot(&mut self, index: Option<usize>, pos: usize) -> Result<(), Error> {
        let table_entry = match (self.stack.last_mut(), index) {
            (Some(Frame::Message { footer, next, .. }), Some(i)) => {
                if i < *next {
                    return Err(Error::Usage("field indices must be ascending"));
                }
                let entry = footer
                    .get_mut(i)
                    .ok_or(Error::Usage("field index past the footer"))?;
                *entry = pos as u64;
                *next = i + 1;
                None
            }
            (
                Some(Frame::List {
                    table,
                    count,
                    written,
                }),
                None,
            ) => {
                if *written >= *count {
                    return Err(Error::Usage("more list elements than the count"));
                }
                let at = *table + (*written as usize) * 8;
                *written += 1;
                Some(at)
            }
            (Some(Frame::Message { .. }), None) => {
                return Err(Error::Usage("a message field needs an index"))
            }
            (Some(Frame::List { .. }), Some(_)) => {
                return Err(Error::Usage("a list element has no index"))
            }
            (None, _) => return Err(Error::Usage("nothing is open")),
        };
        if let Some(at) = table_entry {
            patch(&mut self.buf, at, pos as u64)?;
        }
        Ok(())
    }

    fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Field `index` (or the next list element if `index` is `None`) as a bool.
    pub fn bool(&mut self, index: usize, v: bool) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.buf.push(u8::from(v));
        Ok(())
    }

    /// Field `index` as a u64.
    pub fn u64(&mut self, index: usize, v: u64) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.put_u64(v);
        Ok(())
    }

    /// Field `index` as bytes (aligned if long enough).
    pub fn bytes(&mut self, index: usize, v: &[u8]) -> Result<(), Error> {
        self.bytes_at(Some(index), v)
    }

    /// Field `index` as a string.
    pub fn str(&mut self, index: usize, v: &str) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.put_u64(v.len() as u64);
        self.buf.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn bytes_at(&mut self, index: Option<usize>, v: &[u8]) -> Result<(), Error> {
        let aligned = v.len() as u64 >= self.opts.align_min && self.opts.block_size > 0;
        if aligned {
            let b = self.opts.block_size;
            let data_at = self.position() + 8;
            let lead = (b - data_at % b) % b;
            self.buf.resize(self.buf.len() + lead as usize, 0);
        }
        self.slot(index, self.buf.len())?;
        self.put_u64(v.len() as u64);
        self.buf.extend_from_slice(v);
        if aligned {
            let b = self.opts.block_size;
            let tail = (b - self.position() % b) % b;
            self.buf.resize(self.buf.len() + tail as usize, 0);
        }
        Ok(())
    }

    /// Field `index` as a list of u64.
    pub fn list_u64(&mut self, index: usize, v: &[u64]) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.put_u64(v.len() as u64);
        for x in v {
            self.put_u64(*x);
        }
        Ok(())
    }

    /// Field `index` as a list of bool.
    pub fn list_bool(&mut self, index: usize, v: &[bool]) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.put_u64(v.len() as u64);
        self.buf.extend(v.iter().map(|b| u8::from(*b)));
        Ok(())
    }

    /// Start field `index` as a nested message with `fields` footer entries. End it with
    /// [`Writer::end`].
    pub fn begin_message(&mut self, index: usize, fields: usize) -> Result<(), Error> {
        self.begin_message_at(Some(index), fields)
    }

    fn begin_message_at(&mut self, index: Option<usize>, fields: usize) -> Result<(), Error> {
        let len_at = self.buf.len();
        self.slot(index, len_at)?;
        self.put_u64(0);
        self.stack.push(Frame::Message {
            len_at: Some(len_at),
            start: self.buf.len(),
            footer: vec![ABSENT; fields],
            next: 0,
        });
        Ok(())
    }

    /// Start field `index` as a list of `count` variable-size elements. Write them with
    /// the `element_*` calls, then call [`Writer::end`].
    pub fn begin_list(&mut self, index: usize, count: u64) -> Result<(), Error> {
        self.slot(Some(index), self.buf.len())?;
        self.put_u64(count);
        let table = self.buf.len();
        let size = usize::try_from(count)
            .ok()
            .and_then(|c| c.checked_mul(8))
            .ok_or(Error::Usage("list too long"))?;
        self.buf.resize(table + size, 0);
        self.stack.push(Frame::List {
            table,
            count,
            written: 0,
        });
        Ok(())
    }

    /// The next element of the open list, as bytes.
    pub fn element_bytes(&mut self, v: &[u8]) -> Result<(), Error> {
        self.bytes_at(None, v)
    }

    /// The next element of the open list, as a string.
    pub fn element_str(&mut self, v: &str) -> Result<(), Error> {
        self.slot(None, self.buf.len())?;
        self.put_u64(v.len() as u64);
        self.buf.extend_from_slice(v.as_bytes());
        Ok(())
    }

    /// Start the next element of the open list as a message. End it with [`Writer::end`].
    pub fn begin_element_message(&mut self, fields: usize) -> Result<(), Error> {
        self.begin_message_at(None, fields)
    }

    /// End the open message (write its footer and trailer, patch its length) or the
    /// open list (check that all elements are written).
    pub fn end(&mut self) -> Result<(), Error> {
        match self.stack.pop() {
            Some(Frame::Message {
                len_at,
                start,
                footer,
                ..
            }) => {
                let footer_at = self.buf.len() as u64;
                for o in &footer {
                    self.put_u64(*o);
                }
                self.put_u64(footer_at);
                self.put_u64(MAGIC);
                if let Some(at) = len_at {
                    let len = (self.buf.len() - start) as u64;
                    patch(&mut self.buf, at, len)?;
                } else {
                    self.done = true;
                }
                Ok(())
            }
            Some(Frame::List { count, written, .. }) => {
                if written != count {
                    return Err(Error::Usage("fewer list elements than the count"));
                }
                Ok(())
            }
            None => Err(Error::Usage("end: nothing is open")),
        }
    }

    /// The bytes of the file. The root must be ended.
    pub fn finish(self) -> Result<Vec<u8>, Error> {
        if !self.done || !self.stack.is_empty() {
            return Err(Error::Usage("finish: the root is not ended"));
        }
        Ok(self.buf)
    }
}
