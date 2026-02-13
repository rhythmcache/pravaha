use std::io;

use thiserror::Error;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Error, Debug)]
pub enum FsError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("File is closed")]
    FileClosed,

    #[error("Unsupported protocol: {0}")]
    UnsupportedProtocol(String),
}

pub type Result<T> = std::result::Result<T, FsError>;

/// Abstract file interface
pub trait File: Send {
    /// Read up to buf.len() bytes into buf.
    /// Returns number of bytes read (0 = EOF).
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Seek to absolute position.
    fn seek(&mut self, pos: u64) -> Result<()>;

    /// Get current position.
    fn tell(&self) -> u64;

    /// Check if at end of file.
    fn eof(&self) -> bool;

    /// Get file size if available.
    /// Returns None for streams, pipes, or chunked responses.
    fn size(&self) -> Option<u64> {
        None
    }

    /// Close the file (optional, called automatically on drop).
    fn close(&mut self) {}
}

pub trait FileSystem: Send + Sync {
    fn open(&self, path: &str, mode: &str) -> Result<Box<dyn File>>;
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
pub fn open(url: &str, mode: &str) -> Result<Box<dyn File>> {
    let fs = create(url)?;
    fs.open(url, mode)
}
