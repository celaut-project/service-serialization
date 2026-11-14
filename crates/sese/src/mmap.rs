//! Map a SeSe file into memory, read only (feature `mmap`).
//!
//! The reader then reads the file with no copy. As with every mmap, the file must not
//! change while it is mapped: another process that writes it changes what the reader
//! sees. The reader still checks every bound, so it does not read outside of the map.

#![allow(unsafe_code)]

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

/// A file mapped read only.
#[derive(Debug)]
pub struct MappedFile {
    map: Option<Mmap>,
}

impl MappedFile {
    /// Map the file at `path`. An empty file is not mapped (mmap of 0 bytes fails on
    /// some systems) and reads as empty.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(MappedFile { map: None });
        }
        // SAFETY: the map is read only. The file must not be changed while it is
        // mapped (see the module docs); the reader checks all bounds.
        let map = unsafe { Mmap::map(&file)? };
        Ok(MappedFile { map: Some(map) })
    }

    /// The bytes of the file.
    pub fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }
}
