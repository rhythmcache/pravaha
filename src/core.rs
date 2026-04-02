use std::io;
use thiserror::Error;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Error, Debug, Clone)]
pub enum FsError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("File is closed")]
    FileClosed,

    #[error("Unsupported protocol: {0}")]
    UnsupportedProtocol(String),

    #[error("Rate limited: retry after {retry_after_secs:?}s")]
    RateLimited { retry_after_secs: Option<u64> },
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        FsError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, FsError>;

/// Abstract file interface  intentionally sync for public API stability.
pub trait File: Send + Sync {
    /// Read up to buf.len() bytes into buf.
    /// Returns number of bytes read (0 = EOF).
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Stateless read: does not modify cursor.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Seek to absolute position.
    fn seek(&mut self, pos: u64) -> Result<()>;

    /// Get current position.
    fn tell(&self) -> u64;

    /// Check if at end of file.
    fn eof(&self) -> bool;

    /// Get file size if available.
    fn size(&self) -> Option<u64> {
        None
    }

    /// Close the file (optional, called automatically on drop).
    fn close(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
}

pub trait FileSystem: Send + Sync {
    fn open(&self, path: &str, mode: OpenMode) -> Result<Box<dyn File + Send + Sync>>;
}

/// Create a filesystem for the given URL.
pub fn create(url: &str) -> Result<Box<dyn FileSystem>> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(Box::new(crate::http::HttpFileSystem::new()))
    } else {
        Err(FsError::UnsupportedProtocol(url.to_string()))
    }
}

/// Open a file directly.
pub fn open(url: &str, mode: OpenMode) -> Result<Box<dyn File + Send + Sync>> {
    let fs = create(url)?;
    fs.open(url, mode)
}
