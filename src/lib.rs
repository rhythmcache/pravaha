//! # Pravaha
//!
//! Pravaha (प्रवाह - "flow" in Sanskrit) lets you read files from HTTP(S) URLs like they're
//! regular files on disk. It handles all the messy details: chunking, caching, retries, and
//! prefetching.
//!
//! Right now it supports HTTP and HTTPS. More protocols might come later.
//!
//! ## Basic usage
//!
//! ```rust
//! use pravaha::{open, File};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut file = open("https://example.com/data.bin", "r")?;
//!
//! let mut buffer = vec![0u8; 1024];
//! let bytes_read = file.read(&mut buffer)?;
//!
//! file.seek(1000)?;
//!
//! if let Some(size) = file.size() {
//!     println!("File size: {} bytes", size);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## What it does
//!
//! When you read from a remote file, it:
//! - Fetches data in configurable chunks (default 256KB)
//! - Caches chunks you've already read (in case you seek backwards)
//! - Prefetches the next chunk in the background for sequential reads
//! - Retries failed requests with exponential backoff
//! - Concrete file implementations (`HttpFile`) implement `std::io::Read` and `Seek` traits
//!
//! ## Using with standard I/O libraries
//!
//! The `open()` function returns `Box<dyn File>`, which uses the `File` trait from this crate.
//! To use with libraries that require `std::io::Read` and `Seek`, wrap it in `FileAdapter`:
//!
//! ```rust
//! use pravaha::{open, FileAdapter};
//! use zip::ZipArchive;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = open("https://example.com/archive.zip", "r")?;
//! let mut archive = ZipArchive::new(FileAdapter::new(file))?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Tuning the behavior
//!
//! If the defaults don't work for you:
//!
//! ```rust
//! use pravaha::{HttpFileSystem, FileSystem};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let fs = HttpFileSystem::builder()
//!     .chunk_size(512 * 1024)           // fetch 512KB at a time
//!     .read_ahead(true)                  // prefetch next chunk automatically
//!     .cache_max_entries(128)            // remember up to 128 chunks
//!     .cache_max_bytes(64 * 1024 * 1024) // use max 64MB for cache
//!     .retry_max_attempts(5)             // try 5 times before giving up
//!     .connect_timeout(Duration::from_secs(10))
//!     .read_timeout(Duration::from_secs(30))
//!     .build();
//!
//! let mut file = fs.open("https://example.com/large-file.bin", "r")?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Using it from C
//!
//! Build with `--features capi` to get C bindings:
//!
//! ```c
//! #include <pravaha.h>
//!
//! PravahaFile* file = pravaha_open_url("https://example.com/data.bin", "r");
//! if (!file) {
//!     fprintf(stderr, "Error: %s\n", pravaha_last_error());
//!     return 1;
//! }
//!
//! char buffer[1024];
//! ssize_t bytes_read = pravaha_read(file, buffer, sizeof(buffer));
//!
//! pravaha_file_close(file);
//! ```
//!
//! ## Some things to know
//!
//! Bigger chunks (256KB to 1MB) work better if you're reading files sequentially on a fast
//! network. Smaller chunks are fine for random access or slower connections.
//!
//! The cache helps if you're seeking around in the same area of a file. If you're just reading
//! sequentially once, you might want to reduce cache size or disable it entirely.
//!
//! Read-ahead prefetching makes sequential reads faster but wastes bandwidth if you're jumping
//! around randomly. It automatically turns off when it detects non-sequential access.
//!
//! The library needs servers to support HTTP Range requests (most do). If a server returns 200
//! instead of 206 for a range request, you'll get an error.
//!
//! The `size()` method returns `None` for streams without a known content-length or when the
//! server doesn't provide this information. You can still read from such files, but you won't
//! know their size in advance and cannot seek from the end.
//!
//! ## Errors
//!
//! You'll get different errors for different problems:
//! - Network errors: can't connect, timeout, connection dropped
//! - Protocol errors: server doesn't support ranges, returned wrong data
//! - IO errors: standard Rust IO problems
//! - File closed: you tried to use a file after closing it
//! - Unsupported protocol: right now this means you tried something other than http/https
//!
//! ## Thread safety
//!
//! The FileSystem can be shared between threads safely. Individual File handles can be sent to
//! other threads but shouldn't be used from multiple threads at once (which is the recommended Rust
//! IO pattern anyway ig :(
//!
//! ## Feature flags
//!
//! - `curl` (default): use libcurl for HTTP
//! - `reqwest`: use reqwest instead of curl (don't enable both)
//! - `capi`: build the C API

pub mod core;
pub mod http;
pub mod plug;

pub use core::*;
pub use http::*;
pub use plug::*;

#[cfg(feature = "capi")]
pub mod ffi;

use std::io::{self, Read, Seek, SeekFrom};

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

impl Seek for FileAdapter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                let current = self.inner.tell();
                if offset >= 0 {
                    current.saturating_add(offset as u64)
                } else {
                    current.saturating_sub((-offset) as u64)
                }
            }
            SeekFrom::End(offset) => {
                let size = self.inner.size().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Cannot seek from end without known file size",
                    )
                })?;

                if offset >= 0 {
                    size.saturating_add(offset as u64)
                } else {
                    size.saturating_sub((-offset) as u64)
                }
            }
        };

        self.inner.seek(new_pos).map_err(io::Error::other)?;
        Ok(new_pos)
    }
}
