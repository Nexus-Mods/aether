//! A ring in a memory-mapped file, on any OS.
//!
//! Under Wine the director's ring lives in a real file that both sides map
//! (`docs/wire.md`, section 2.10). [`FileRing`] is the same arrangement for a
//! native tool: it maps that file and exposes it as an
//! [`aether_ipc::RingBacking`], so a `RingClient` or `RingServer` runs on it
//! exactly as on a Windows section. The director side calls
//! [`FileRing::create`], which creates and sizes the file; the client side
//! calls [`FileRing::open`], which maps an existing file at its own length.
//! Initialising the ring (`aether_ipc::ring::init`) stays with the creator.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use aether_ipc::{RingBacking, SharedSeg};

mod map;

/// A shared read/write mapping of a file, usable as a ring's backing.
pub struct FileRing {
    inner: map::Mapped,
}

impl FileRing {
    /// Create the file at `path`, size it to exactly `bytes` (zero-filled)
    /// and map it. The caller initialises the ring on it.
    ///
    /// A file already at `path` is removed and a new one created in its
    /// place, never truncated in place (`docs/wire.md` 2.10): a process that
    /// still maps the old file keeps its pages, where truncating them would
    /// fault it (SIGBUS on Unix; Windows refuses to truncate a mapped file).
    pub fn create(path: &Path, bytes: usize) -> io::Result<FileRing> {
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a file ring cannot be zero bytes",
            ));
        }
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(bytes as u64)?;
        Ok(FileRing {
            inner: map::Mapped::new(&file)?,
        })
    }

    /// Map the existing file at `path`, whose length is the ring's length.
    /// An empty file is refused.
    pub fn open(path: &Path) -> io::Result<FileRing> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if file.metadata()?.len() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the ring file is empty",
            ));
        }
        Ok(FileRing {
            inner: map::Mapped::new(&file)?,
        })
    }
}

impl RingBacking for FileRing {
    fn seg(&self) -> &SharedSeg {
        self.inner.seg()
    }
    fn len(&self) -> usize {
        self.inner.len()
    }
    fn as_mut_ptr(&self) -> *mut u8 {
        self.inner.as_mut_ptr()
    }
}
