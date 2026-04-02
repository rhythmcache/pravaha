//! # Pravaha
//!
//! Pravaha (pravaah -- "flow" in Sanskrit) lets you read files from HTTP(S) URLs
//! like they are regular files on disk.  It handles chunking, caching, retries,
//! parallel prefetching, and in-flight deduplication transparently.
//!
//! The **public API is fully synchronous** (`File` / `FileSystem` traits).
//! Internally it runs an async engine (Tokio) and bridges back to sync via
//! `Handle::block_on`.  This means:
//! - No `async` leaks into your code.
//! - Multiple concurrent `File` handles on the same `HttpFileSystem` share one
//!   connection pool and one chunk cache -- duplicate fetches are impossible.
//! - The engine can issue up to `max_parallel_fetches` HTTP requests at once
//!   (default 4), so sequential reads automatically pipeline chunks.
//!
//! ## Basic usage
//!
//! ```rust,no_run
//! use pravaha::{open, File, OpenMode};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut file = open("https://example.com/data.bin", OpenMode::Read)?;
//!
//! let mut buf = vec![0u8; 4096];
//! let n = file.read(&mut buf)?;
//!
//! file.seek(1_000_000)?;
//!
//! if let Some(size) = file.size() {
//!     println!("File size: {size} bytes");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Using with standard I/O libraries
//!
//! `open()` returns `Box<dyn File>`.  Wrap it in `FileAdapter` to get
//! `std::io::Read + Seek`:
//!
//! ```rust,ignore
//! use pravaha::{open, FileAdapter, OpenMode};
//! use zip::ZipArchive; // requires 'zip' crate
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = open("https://example.com/archive.zip", OpenMode::Read)?;
//! let mut archive = ZipArchive::new(FileAdapter::new(file))?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Tuning parallelism and cache
//!
//! ```rust,no_run
//! use pravaha::{HttpFileSystem, FileSystem, OpenMode};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let fs = HttpFileSystem::builder()
//!     .chunk_size(512 * 1024)             // 512 KB per chunk
//!     .read_ahead_chunks(4)               // prefetch 4 chunks ahead
//!     .max_parallel_fetches(8)            // up to 8 concurrent HTTP requests
//!     .cache_max_entries(128)
//!     .cache_max_bytes(128 * 1024 * 1024) // 128 MB cache
//!     .retry_max_attempts(5)
//!     .connect_timeout(Duration::from_secs(10))
//!     .read_timeout(Duration::from_secs(30))
//!     .build();
//!
//! let mut file = fs.open("https://example.com/large.bin", OpenMode::Read)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Architecture notes
//!
//! ```text
//! +-----------------------------------------------------+
//! |  Public sync API  (File / FileSystem traits)        |
//! |  HttpFile::read -> rt.block_on(engine.get_chunk())  |
//! +--------------------+--------------------------------+
//!                      |
//! +--------------------v--------------------------------+
//! |  FetchEngine  (per HttpFileSystem, shared via Arc)  |
//! |                                                     |
//! |  +---------------------------------------------+   |
//! |  | ChunkCache  DashMap<ChunkKey, SharedFuture>  |   |
//! |  |  * Missing  -> spawn new fetch future        |   |
//! |  |  * InFlight -> clone + await existing future |   |
//! |  |  * Ready    -> LRU hit, wrap in ready future |   |
//! |  +---------------------------------------------+   |
//! |                                                     |
//! |  Semaphore: caps concurrent HTTP requests           |
//! |  PrefetchPlanner: fires read_ahead_chunks futures   |
//! +--------------------+--------------------------------+
//!                      |
//! +--------------------v--------------------------------+
//! |  AsyncHttp transport (reqwest async / curl via     |
//! |  spawn_blocking)                                    |
//! |  * Retry with exponential backoff                  |
//! |  * Retry-After / 429 / 503 aware                   |
//! +-----------------------------------------------------+
//! ```
//!
//! ## Thread safety
//!
//! `HttpFileSystem` is `Send + Sync` and can be shared freely.
//! Individual `HttpFile` handles are `Send` (can be moved to another thread)
//! but are not `Sync` -- don't share one handle across threads simultaneously,
//! which is the standard Rust I/O pattern anyway.
//!
//! ## Memory budget
//!
//! With `max_parallel_fetches = N` and `chunk_size = C`, peak in-flight memory
//! is roughly `N x C` in addition to the completed LRU cache.  With defaults
//! (4 x 256 KB = 1 MB) this is negligible; tune conservatively for
//! memory-constrained environments.
//!
//! ## Feature flags
//!
//! - `curl` (default): use libcurl via `spawn_blocking`
//! - `reqwest`: use async reqwest (don't enable both)
//! - `capi`: build the C API

pub mod core;
pub mod http;
pub mod plug;

pub use core::*;
pub use http::*;
pub use plug::AsyncHttp;

#[cfg(feature = "capi")]
pub mod ffi;

use std::io::{self, Read, Seek, SeekFrom};

/// Adapts `Box<dyn File>` into `std::io::Read + Seek` for use with
/// third-party libraries (zip, image decoders, etc.).
pub struct FileAdapter {
    inner: Box<dyn File>,
}

impl FileAdapter {
    pub fn new(file: Box<dyn File>) -> Self {
        Self { inner: file }
    }
    pub fn into_inner(self) -> Box<dyn File> {
        self.inner
    }
}

impl From<Box<dyn File>> for FileAdapter {
    fn from(file: Box<dyn File>) -> Self {
        Self::new(file)
    }
}

impl Read for FileAdapter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).map_err(io::Error::other)
    }
}

pub(crate) fn resolve_seek(pos: SeekFrom, current: u64, size: Option<u64>) -> io::Result<u64> {
    match pos {
        SeekFrom::Start(o) => Ok(o),
        SeekFrom::Current(o) => {
            if o >= 0 {
                Ok(current.saturating_add(o as u64))
            } else {
                Ok(current.saturating_sub((-o) as u64))
            }
        }
        SeekFrom::End(o) => {
            let size = size.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Cannot seek from end without known file size",
                )
            })?;
            if o >= 0 {
                Ok(size.saturating_add(o as u64))
            } else {
                Ok(size.saturating_sub((-o) as u64))
            }
        }
    }
}

impl Seek for FileAdapter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = resolve_seek(pos, self.inner.tell(), self.inner.size())?;
        self.inner.seek(new_pos).map_err(io::Error::other)?;
        Ok(new_pos)
    }
}
