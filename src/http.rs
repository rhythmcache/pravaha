use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

use crate::core::{File, FileSystem, FsError, Result};
use crate::plug::AsyncHttp;
use crate::plug::build_default_transport;

#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub chunk_size: u64,
    /// How many chunks ahead to speculatively prefetch during sequential reads.
    pub read_ahead_chunks: usize,
    /// Max parallel in-flight fetches across all operations on this file.
    pub max_parallel_fetches: usize,
    pub cache_max_entries: usize,
    pub cache_max_bytes: usize,
    pub retry_max_attempts: usize,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        let chunk_size = 256 * 1024;
        Self {
            chunk_size,
            read_ahead_chunks: 3,
            max_parallel_fetches: 4,
            cache_max_entries: 64,
            cache_max_bytes: 32 * 1024 * 1024,
            retry_max_attempts: 3,
            retry_base_delay: Duration::from_millis(50),
            retry_max_delay: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
        }
    }
}

fn retry_delay(base: Duration, max: Duration, attempt: usize) -> Duration {
    let mult = 1u32.checked_shl(attempt.min(20) as u32).unwrap_or(u32::MAX);
    let d = base.checked_mul(mult).unwrap_or(max);
    if d > max { max } else { d }
}

/// A chunk is identified by its aligned start offset.  End is always
/// `start + chunk_size - 1` (clamped by the server).
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
struct ChunkKey {
    url: Arc<str>,
    start: u64,
}

//  The DashMap stores InFlight entries.  Once the future resolves the data goes
//  into the LRU cache and the InFlight entry is removed.  Any number of waiters
//  can clone + await the same SharedFuture without duplicating the HTTP request.

type ChunkFuture = Shared<BoxFuture<'static, Result<Arc<[u8]>>>>;

struct LruCache {
    map: ahash::AHashMap<ChunkKey, Arc<[u8]>>,
    lru: VecDeque<ChunkKey>,
    max_entries: usize,
    max_bytes: usize,
    current_bytes: usize,
}

impl LruCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: ahash::AHashMap::new(),
            lru: VecDeque::new(),
            max_entries,
            max_bytes,
            current_bytes: 0,
        }
    }

    fn get(&mut self, key: &ChunkKey) -> Option<Arc<[u8]>> {
        if self.max_entries == 0 || self.max_bytes == 0 {
            return None;
        }
        let data = self.map.get(key)?.clone();
        // Move to front.
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_front(key.clone());
        Some(data)
    }

    fn insert(&mut self, key: ChunkKey, data: Arc<[u8]>) {
        if self.max_entries == 0 || self.max_bytes == 0 {
            return;
        }
        let size = data.len();
        if size > self.max_bytes {
            return;
        }

        if let Some(old) = self.map.remove(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(old.len());
            if let Some(pos) = self.lru.iter().position(|k| k == &key) {
                self.lru.remove(pos);
            }
        }

        self.current_bytes = self.current_bytes.saturating_add(size);
        self.map.insert(key.clone(), data);
        self.lru.push_front(key);

        while self.map.len() > self.max_entries || self.current_bytes > self.max_bytes {
            if let Some(evict) = self.lru.pop_back() {
                if let Some(d) = self.map.remove(&evict) {
                    self.current_bytes = self.current_bytes.saturating_sub(d.len());
                }
            } else {
                break;
            }
        }
    }
}

pub(crate) struct FetchEngine {
    transport: Arc<dyn AsyncHttp>,
    config: HttpConfig,
    /// In-flight futures, multiple readers share the same future.
    in_flight: Arc<DashMap<ChunkKey, ChunkFuture>>,
    /// Completed chunks.
    lru: Arc<std::sync::Mutex<LruCache>>,
    /// Bounds the number of simultaneous outgoing HTTP requests.
    semaphore: Arc<Semaphore>,
}

impl FetchEngine {
    fn new(transport: Arc<dyn AsyncHttp>, config: HttpConfig) -> Self {
        let sem = Arc::new(Semaphore::new(config.max_parallel_fetches));
        let lru = Arc::new(std::sync::Mutex::new(LruCache::new(
            config.cache_max_entries,
            config.cache_max_bytes,
        )));
        Self {
            transport,
            config,
            in_flight: Arc::new(DashMap::new()),
            lru,
            semaphore: sem,
        }
    }

    /// Returns a future that resolves to the chunk data.
    /// Deduplicates: if a fetch for this chunk is already running, returns
    /// the same shared future instead of starting a new request.
    fn get_chunk(&self, url: Arc<str>, start: u64) -> ChunkFuture {
        let key = ChunkKey {
            url: Arc::clone(&url),
            start,
        };

        // Fast path already in completed cache.
        if let Ok(mut lru) = self.lru.lock()
            && let Some(data) = lru.get(&key) {
                return futures::future::ready(Ok(data)).boxed().shared();
            }

        // In-flight deduplication.
        use dashmap::mapref::entry::Entry;

        
        match self.in_flight.entry(key.clone()) {
            Entry::Occupied(e) => e.get().clone(),
            Entry::Vacant(v) => {
                // Build the future only if we won the race.
                let transport = Arc::clone(&self.transport);
                let in_flight = Arc::clone(&self.in_flight);
                let lru = Arc::clone(&self.lru);
                let sem = Arc::clone(&self.semaphore);
                let config = self.config.clone();
                let key2 = key.clone();
                let chunk_size = self.config.chunk_size;
                let url2 = Arc::clone(&url);

                let fut: BoxFuture<'static, Result<Arc<[u8]>>> = Box::pin(async move {
                    // Acquire concurrency permit before touching the network.
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|_| FsError::Network("Semaphore closed".into()))?;

                    let range_end = start.saturating_add(chunk_size.saturating_sub(1));
                    let data =
                        fetch_with_retry(&transport, &url2, start, range_end, &config).await?;

                    if data.is_empty() && start > 0 {
                        // A 416 would have been caught by validate_range_response already.
                        // An empty body for a non-zero start is a protocol violation.
                        return Err(FsError::Protocol(format!(
                            "Server returned empty body for range {start}-{range_end}"
                        )));
                    }

                    #[cfg(debug_assertions)]
                    if !data.is_empty() && (data.len() as u64) < chunk_size && start > 0 {
                        eprintln!(
                            "[pravaha] short chunk at {start}: got {} bytes, expected {chunk_size}",
                            data.len()
                        );
                    }

                    let arc: Arc<[u8]> = data.into();

                    // Promote to completed cache and remove from in-flight.
                    if let Ok(mut lru) = lru.lock() {
                        lru.insert(key2.clone(), Arc::clone(&arc));
                    }
                    in_flight.remove(&key2);
                    Ok(arc)
                });

                let shared = fut.shared();
                v.insert(shared.clone());
                shared
            }
        }
    }

    /// Kick off prefetch futures for the next `n` chunks without awaiting them.
    fn prefetch_ahead(&self, url: Arc<str>, from_offset: u64, n: usize) {
        for i in 0..n as u64 {
            let start = from_offset + i * self.config.chunk_size;
            // get_chunk registers the shared future in in_flight; we don't await.
            let fut = self.get_chunk(Arc::clone(&url), start);
            // Drive the future on the Tokio runtime without blocking the caller.
            tokio::spawn(async move {
                let _ = fut.await;
            });
        }
    }

    async fn content_length(&self, url: &str) -> Result<Option<u64>> {
        let mut attempt = 0;
        loop {
            match self.transport.get_content_length(url).await {
                Ok(v) => return Ok(v),
                Err(FsError::RateLimited { retry_after_secs }) => {
                    let wait = retry_after_secs.unwrap_or(5);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Err(FsError::Network(e)) if attempt < self.config.retry_max_attempts => {
                    let d = retry_delay(
                        self.config.retry_base_delay,
                        self.config.retry_max_delay,
                        attempt,
                    );
                    tokio::time::sleep(d).await;
                    attempt += 1;
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

async fn fetch_with_retry(
    transport: &Arc<dyn AsyncHttp>,
    url: &str,
    start: u64,
    end: u64,
    config: &HttpConfig,
) -> Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        match transport.get_range(url, start, end).await {
            Ok(resp) => return Ok(resp.data),
            Err(FsError::RateLimited { retry_after_secs }) => {
                let wait = retry_after_secs.unwrap_or(5);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                // Rate-limit waits don't count as retry attempts.
            }
            Err(FsError::Network(e)) if attempt < config.retry_max_attempts => {
                let d = retry_delay(config.retry_base_delay, config.retry_max_delay, attempt);
                tokio::time::sleep(d).await;
                attempt += 1;
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
}

fn block_sync<F, T>(rt: &tokio::runtime::Handle, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        // We are already inside a Tokio worker thread.
        // block_in_place suspends the current task and runs the future
        // synchronously on the same thread without blocking the scheduler.
        Ok(_) => tokio::task::block_in_place(|| rt.block_on(fut)),
        // No runtime on this thread – plain block_on is fine.
        Err(_) => rt.block_on(fut),
    }
}

pub struct HttpFile {
    url: Arc<str>,
    engine: Arc<FetchEngine>,
    rt: Handle,
    file_offset: u64,
    eof_reached: bool,
    closed: bool,
    /// Cached result of HEAD request.
    cached_size: Cell<Option<Option<u64>>>,
    /// Whether the last read was sequential (used for prefetch decisions).
    last_read_end: Option<u64>,
}

impl HttpFile {
    fn new(url: Arc<str>, engine: Arc<FetchEngine>, rt: Handle) -> Self {
        Self {
            url,
            engine,
            rt,
            file_offset: 0,
            eof_reached: false,
            closed: false,
            cached_size: Cell::new(None), // ← was: None
            last_read_end: None,
        }
    }

    fn chunk_start(&self, offset: u64) -> u64 {
        let cs = self.engine.config.chunk_size;
        (offset / cs) * cs
    }

    /// Blocking read of a single chunk via the shared async engine.
    fn fetch_chunk(&self, start: u64) -> Result<Arc<[u8]>> {
        let fut = self.engine.get_chunk(Arc::clone(&self.url), start);
        block_sync(&self.rt, fut)
    }

    fn fetch_size(&self) -> Option<u64> {
        if self.cached_size.get().is_none() {
            let size = block_sync(&self.rt, self.engine.content_length(self.url.as_ref()))
                .ok()
                .flatten();
            self.cached_size.set(Some(size));
        }
        self.cached_size.get().unwrap()
    }
}

impl File for HttpFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.closed {
            return Err(FsError::FileClosed);
        }

        let start_offset = self.file_offset;
        let mut total_read = 0;

        while total_read < buf.len() {
            if self.eof_reached {
                break;
            }

            let chunk_start = self.chunk_start(self.file_offset);
            let chunk = match self.fetch_chunk(chunk_start) {
                Ok(c) => c,
                Err(e) => {
                    return if total_read > 0 {
                        Ok(total_read)
                    } else {
                        Err(e)
                    };
                }
            };

            if chunk.is_empty() {
                self.eof_reached = true;
                break;
            }

            // Offset within this chunk.
            let inner = (self.file_offset - chunk_start) as usize;
            if inner >= chunk.len() {
                self.eof_reached = true;
                break;
            }

            let available = &chunk[inner..];
            let to_copy = available.len().min(buf.len() - total_read);
            buf[total_read..total_read + to_copy].copy_from_slice(&available[..to_copy]);

            total_read += to_copy;
            self.file_offset += to_copy as u64;

            // Check EOF against file size if known.
            if let Some(Some(size)) = self.cached_size.get()
                && self.file_offset >= size {
                    self.eof_reached = true;
                    break;
                }
        }

        if total_read > 0 {
            let sequential = self.last_read_end.is_none_or(|end| start_offset == end);
            self.last_read_end = Some(self.file_offset);

            if sequential {
                let next_chunk = self.chunk_start(self.file_offset);
                self.engine.prefetch_ahead(
                    Arc::clone(&self.url),
                    next_chunk,
                    self.engine.config.read_ahead_chunks,
                );
            }
        }

        Ok(total_read)
    }

    fn seek(&mut self, pos: u64) -> Result<()> {
        if self.closed {
            return Err(FsError::FileClosed);
        }
        self.file_offset = pos;
        self.eof_reached = false;
        self.last_read_end = None;
        Ok(())
    }

    fn tell(&self) -> u64 {
        self.file_offset
    }

    fn eof(&self) -> bool {
        self.eof_reached
    }

    fn size(&self) -> Option<u64> {
        if self.closed {
            return None;
        }
        self.fetch_size()
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

impl Read for HttpFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        File::read(self, buf).map_err(io::Error::other)
    }
}

impl Seek for HttpFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::Current(o) => {
                if o >= 0 {
                    self.file_offset.saturating_add(o as u64)
                } else {
                    self.file_offset.saturating_sub((-o) as u64)
                }
            }
            SeekFrom::End(o) => {
                let size = self.size().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Cannot seek from end without known file size",
                    )
                })?;
                if o >= 0 {
                    size.saturating_add(o as u64)
                } else {
                    size.saturating_sub((-o) as u64)
                }
            }
        };
        File::seek(self, new_pos).map_err(io::Error::other)?;
        Ok(new_pos)
    }
}

impl Drop for HttpFile {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct HttpFileSystem {
    engine: Arc<FetchEngine>,
    rt: tokio::runtime::Runtime,
}

impl HttpFileSystem {
    pub fn new() -> Self {
        HttpFileSystemBuilder::new().build()
    }

    pub fn builder() -> HttpFileSystemBuilder {
        HttpFileSystemBuilder::new()
    }
}

impl Default for HttpFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for HttpFileSystem {
    fn open(&self, url: &str, mode: &str) -> Result<Box<dyn File>> {
        if mode != "r" && mode != "rb" {
            return Err(FsError::Io(
                "Only read mode ('r' or 'rb') is supported".into(),
            ));
        }
        Ok(Box::new(HttpFile::new(
            Arc::from(url),
            Arc::clone(&self.engine),
            self.rt.handle().clone(),
        )))
    }
}

pub struct HttpFileSystemBuilder {
    config: HttpConfig,
    transport: Option<Arc<dyn AsyncHttp>>,
}

impl Default for HttpFileSystemBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFileSystemBuilder {
    pub fn new() -> Self {
        Self {
            config: HttpConfig::default(),
            transport: None,
        }
    }

    pub fn transport(mut self, t: Arc<dyn AsyncHttp>) -> Self {
        self.transport = Some(t);
        self
    }

    pub fn chunk_size(mut self, v: u64) -> Self {
        self.config.chunk_size = v.max(1);
        self
    }

    pub fn read_ahead_chunks(mut self, n: usize) -> Self {
        self.config.read_ahead_chunks = n;
        self
    }

    pub fn max_parallel_fetches(mut self, n: usize) -> Self {
        self.config.max_parallel_fetches = n.max(1);
        self
    }

    pub fn cache_max_entries(mut self, v: usize) -> Self {
        self.config.cache_max_entries = v;
        self
    }

    pub fn cache_max_bytes(mut self, v: usize) -> Self {
        self.config.cache_max_bytes = v;
        self
    }

    pub fn retry_max_attempts(mut self, v: usize) -> Self {
        self.config.retry_max_attempts = v;
        self
    }

    pub fn retry_base_delay(mut self, v: Duration) -> Self {
        self.config.retry_base_delay = v;
        self
    }

    pub fn retry_max_delay(mut self, v: Duration) -> Self {
        self.config.retry_max_delay = v;
        self
    }

    pub fn connect_timeout(mut self, v: Duration) -> Self {
        self.config.connect_timeout = v;
        self
    }

    pub fn read_timeout(mut self, v: Duration) -> Self {
        self.config.read_timeout = v;
        self
    }

    pub fn idle_timeout(mut self, v: Duration) -> Self {
        self.config.idle_timeout = v;
        self
    }

    pub fn build(self) -> HttpFileSystem {
        let transport = self
            .transport
            .unwrap_or_else(|| build_default_transport(&self.config));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("pravaha-io")
            .build()
            .expect("Failed to build Tokio runtime");
        let engine = Arc::new(FetchEngine::new(transport, self.config));
        HttpFileSystem { engine, rt }
    }
}
