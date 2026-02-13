use ahash::AHashMap as HashMap;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::core::{File, FileSystem, FsError, Result};
use crate::plug::{BlockingHttp, HttpResponse, build_default_transport};

fn empty_bytes() -> Arc<[u8]> {
    static EMPTY: OnceLock<Arc<[u8]>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::from(&[][..])).clone()
}

#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub chunk_size: u64,
    pub read_ahead: bool,
    pub read_ahead_trigger: u64,
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
            read_ahead: true,
            read_ahead_trigger: (chunk_size / 2).max(1),
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
    let shift = attempt.min(20);
    let mult = 1u32.checked_shl(shift as u32).unwrap_or(u32::MAX);
    let delay = base.checked_mul(mult).unwrap_or(max);
    if delay > max { max } else { delay }
}

#[derive(Clone, Hash, Eq, PartialEq, Debug)]
struct CacheKey {
    url: Arc<str>,
    start: u64,
    end: u64,
}

#[derive(Clone)]
struct CacheEntry {
    data: Arc<[u8]>,
    size: usize,
}

struct RangeCache {
    map: HashMap<CacheKey, CacheEntry>,
    lru: VecDeque<CacheKey>,
    max_entries: usize,
    max_bytes: usize,
    current_bytes: usize,
}

impl RangeCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            lru: VecDeque::new(),
            max_entries,
            max_bytes,
            current_bytes: 0,
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<Arc<[u8]>> {
        if self.max_entries == 0 || self.max_bytes == 0 {
            return None;
        }

        let entry = self.map.get(key)?.clone();
        self.touch_lru(key);
        Some(entry.data)
    }

    fn insert(&mut self, key: CacheKey, data: Arc<[u8]>) {
        if self.max_entries == 0 || self.max_bytes == 0 {
            return;
        }

        let size = data.len();
        if size > self.max_bytes {
            return;
        }

        if let Some(existing) = self.map.remove(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(existing.size);
            self.remove_lru(&key);
        }

        let entry = CacheEntry { data, size };

        self.current_bytes = self.current_bytes.saturating_add(size);
        self.map.insert(key.clone(), entry);
        self.lru.push_front(key);
        self.evict_to_limits();
    }

    fn touch_lru(&mut self, key: &CacheKey) {
        self.remove_lru(key);
        self.lru.push_front(key.clone());
    }

    fn remove_lru(&mut self, key: &CacheKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
    }

    fn evict_to_limits(&mut self) {
        while self.map.len() > self.max_entries || self.current_bytes > self.max_bytes {
            if let Some(key) = self.lru.pop_back() {
                if let Some(entry) = self.map.remove(&key) {
                    self.current_bytes = self.current_bytes.saturating_sub(entry.size);
                }
            } else {
                break;
            }
        }
    }
}

struct RangeBuffer {
    data: Arc<[u8]>,
    buffer_start: u64,
    buffer_end: u64,
}

impl RangeBuffer {
    fn new() -> Self {
        Self {
            data: empty_bytes(),
            buffer_start: 0,
            buffer_end: 0,
        }
    }

    fn set_data(&mut self, data: Arc<[u8]>, file_start: u64, file_end: u64) {
        self.data = data;
        self.buffer_start = file_start;
        self.buffer_end = file_end;
    }

    fn contains(&self, file_offset: u64) -> bool {
        file_offset >= self.buffer_start && file_offset < self.buffer_end
    }

    fn read(&self, out: &mut [u8], file_offset: u64) -> usize {
        if !self.contains(file_offset) {
            return 0;
        }

        let buffer_offset = (file_offset - self.buffer_start) as usize;
        let available = self.data.len() - buffer_offset;
        let to_copy = available.min(out.len());

        out[..to_copy].copy_from_slice(&self.data[buffer_offset..buffer_offset + to_copy]);
        to_copy
    }

    fn clear(&mut self) {
        self.data = empty_bytes();
        self.buffer_start = 0;
        self.buffer_end = 0;
    }

    fn end(&self) -> u64 {
        self.buffer_end
    }
}

struct PrefetchState {
    range_start: u64,
    range_end: u64,
    rx: mpsc::Receiver<Result<HttpResponse>>,
}

pub struct HttpFile {
    url: Arc<str>,
    transport: Arc<dyn BlockingHttp>,
    config: HttpConfig,
    cache: Arc<Mutex<RangeCache>>,
    buffer: RangeBuffer,
    file_offset: u64,
    eof_reached: bool,
    closed: bool,
    cached_size: Cell<Option<Option<u64>>>,
    prefetch: Option<PrefetchState>,
    last_read_end: Option<u64>,
}

impl HttpFile {
    const MAX_REFILL_ATTEMPTS: usize = 3;

    fn new(
        url: Arc<str>,
        transport: Arc<dyn BlockingHttp>,
        config: HttpConfig,
        cache: Arc<Mutex<RangeCache>>,
    ) -> Self {
        Self {
            url,
            transport,
            config,
            cache,
            buffer: RangeBuffer::new(),
            file_offset: 0,
            eof_reached: false,
            closed: false,
            cached_size: Cell::new(None),
            prefetch: None,
            last_read_end: None,
        }
    }

    fn get_content_length_with_retry(&self) -> Result<Option<u64>> {
        let mut attempt = 0;
        loop {
            match self.transport.get_content_length(&self.url) {
                Ok(v) => return Ok(v),
                Err(FsError::Network(err)) => {
                    if attempt >= self.config.retry_max_attempts {
                        return Err(FsError::Network(err));
                    }
                }
                Err(e) => return Err(e),
            }

            let delay = retry_delay(
                self.config.retry_base_delay,
                self.config.retry_max_delay,
                attempt,
            );
            thread::sleep(delay);
            attempt += 1;
        }
    }

    fn get_range_with_retry(&self, start: u64, end: u64) -> Result<HttpResponse> {
        let mut attempt = 0;
        loop {
            match self.transport.get_range(&self.url, start, end) {
                Ok(v) => return Ok(v),
                Err(FsError::Network(err)) => {
                    if attempt >= self.config.retry_max_attempts {
                        return Err(FsError::Network(err));
                    }
                }
                Err(e) => return Err(e),
            }

            let delay = retry_delay(
                self.config.retry_base_delay,
                self.config.retry_max_delay,
                attempt,
            );
            thread::sleep(delay);
            attempt += 1;
        }
    }

    fn try_cache_lookup(&self, range_start: u64, range_end: u64) -> Option<Arc<[u8]>> {
        let key = CacheKey {
            url: Arc::clone(&self.url),
            start: range_start,
            end: range_end,
        };

        let mut cache = self.cache.lock().ok()?;
        cache.get(&key)
    }

    fn store_cache(&self, range_start: u64, range_end: u64, data: Arc<[u8]>) {
        let key = CacheKey {
            url: Arc::clone(&self.url),
            start: range_start,
            end: range_end,
        };

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, data);
        }
    }

    fn take_prefetch_if_match(
        &mut self,
        range_start: u64,
        range_end: u64,
    ) -> Option<Result<HttpResponse>> {
        let prefetch = self.prefetch.take()?;
        if prefetch.range_start == range_start && prefetch.range_end == range_end {
            return Some(
                prefetch
                    .rx
                    .recv()
                    .unwrap_or_else(|_| Err(FsError::Network("Prefetch thread canceled".into()))),
            );
        }

        self.prefetch = Some(prefetch);
        None
    }

    fn maybe_prefetch_next(&mut self) {
        if !self.config.read_ahead || self.eof_reached {
            return;
        }

        let buffer_end = self.buffer.end();
        if buffer_end <= self.file_offset {
            return;
        }

        let remaining = buffer_end - self.file_offset;
        if remaining > self.config.read_ahead_trigger {
            return;
        }

        let next_start = buffer_end;
        let next_end = next_start.saturating_add(self.config.chunk_size.saturating_sub(1));

        if let Some(prefetch) = &self.prefetch
            && prefetch.range_start == next_start
            && prefetch.range_end == next_end
        {
            return;
        }

        if self.try_cache_lookup(next_start, next_end).is_some() {
            return;
        }

        let transport = Arc::clone(&self.transport);
        let url = self.url.clone();
        let config = self.config.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut attempt = 0;
            let result = loop {
                match transport.get_range(&url, next_start, next_end) {
                    Ok(v) => break Ok(v),
                    Err(FsError::Network(err)) => {
                        if attempt >= config.retry_max_attempts {
                            break Err(FsError::Network(err));
                        }
                    }
                    Err(e) => break Err(e),
                }

                let delay = retry_delay(config.retry_base_delay, config.retry_max_delay, attempt);
                thread::sleep(delay);
                attempt += 1;
            };
            let _ = tx.send(result);
        });

        self.prefetch = Some(PrefetchState {
            range_start: next_start,
            range_end: next_end,
            rx,
        });
    }

    fn refill_buffer(&mut self) -> Result<()> {
        let range_start = self.file_offset;
        let range_end = self
            .file_offset
            .saturating_add(self.config.chunk_size.saturating_sub(1));

        let old_buffer_end = self.buffer.end();
        let expected_size = range_end - range_start + 1;

        let file_size = if self.cached_size.get().is_none() {
            let sz = self.get_content_length_with_retry().ok().flatten();
            self.cached_size.set(Some(sz));
            sz
        } else {
            self.cached_size.get().unwrap()
        };

        if let Some(data) = self.try_cache_lookup(range_start, range_end) {
            let actual_size = data.len() as u64;
            if actual_size == 0 {
                self.eof_reached = true;
                self.buffer.clear();
                return Ok(());
            }
            let actual_end = range_start + actual_size;

            let reached_eof = if let Some(size) = file_size {
                actual_end >= size
            } else {
                actual_size < expected_size && old_buffer_end > 0
            };

            if reached_eof {
                self.eof_reached = true;
            }

            self.buffer.set_data(data, range_start, actual_end);
            return Ok(());
        }

        let response = match self.take_prefetch_if_match(range_start, range_end) {
            Some(result) => result?,
            None => self.get_range_with_retry(range_start, range_end)?,
        };

        if response.data.is_empty() {
            self.eof_reached = true;
            self.buffer.clear();
            return Ok(());
        }

        let actual_size = response.data.len() as u64;
        let actual_end = range_start + actual_size;

        let reached_eof = if let Some(size) = file_size {
            actual_end >= size
        } else {
            actual_size < expected_size && old_buffer_end > 0
        };

        if reached_eof {
            self.eof_reached = true;
        }

        let data: Arc<[u8]> = response.data.into();
        self.store_cache(range_start, range_end, Arc::clone(&data));
        self.buffer.set_data(data, range_start, actual_end);

        if self.buffer.end() <= old_buffer_end && old_buffer_end > 0 {
            return Err(FsError::Protocol("Buffer refill did not advance".into()));
        }

        Ok(())
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
        let mut refill_attempts = 0;

        while total_read < buf.len() {
            if !self.buffer.contains(self.file_offset) {
                if self.eof_reached {
                    break;
                }

                self.refill_buffer()?;
                refill_attempts += 1;

                if self.eof_reached && !self.buffer.contains(self.file_offset) {
                    break;
                }

                if refill_attempts > Self::MAX_REFILL_ATTEMPTS {
                    return Err(FsError::Protocol(
                        "Too many refill attempts without progress".into(),
                    ));
                }

                continue;
            }

            refill_attempts = 0;
            let bytes_read = self.buffer.read(&mut buf[total_read..], self.file_offset);

            if bytes_read == 0 {
                return Err(FsError::Protocol(
                    "Internal error: buffer contains offset but read returned 0".into(),
                ));
            }

            total_read += bytes_read;
            self.file_offset += bytes_read as u64;

            if self.eof_reached && !self.buffer.contains(self.file_offset) {
                break;
            }
        }

        if total_read > 0 {
            let sequential = match self.last_read_end {
                Some(prev_end) => start_offset == prev_end,
                None => true,
            };

            self.last_read_end = Some(start_offset + total_read as u64);

            if sequential {
                self.maybe_prefetch_next();
            } else {
                self.prefetch = None;
            }
        }

        Ok(total_read)
    }

    fn seek(&mut self, pos: u64) -> Result<()> {
        if self.closed {
            return Err(FsError::FileClosed);
        }

        if pos < self.file_offset || !self.buffer.contains(pos) {
            self.buffer.clear();
        }

        self.file_offset = pos;
        self.eof_reached = false;
        self.prefetch = None;
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

        if self.cached_size.get().is_none() {
            let size = self.get_content_length_with_retry().ok().flatten();
            self.cached_size.set(Some(size));
        }

        self.cached_size.get().unwrap()
    }

    fn close(&mut self) {
        if !self.closed {
            self.buffer.clear();
            self.prefetch = None;
            self.closed = true;
        }
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
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                if offset >= 0 {
                    self.file_offset.saturating_add(offset as u64)
                } else {
                    self.file_offset.saturating_sub((-offset) as u64)
                }
            }
            SeekFrom::End(offset) => {
                let size = self.size().ok_or_else(|| {
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
    transport: Arc<dyn BlockingHttp>,
    config: HttpConfig,
    cache: Arc<Mutex<RangeCache>>,
}

pub struct HttpFileSystemBuilder {
    config: HttpConfig,
    transport: Option<Arc<dyn BlockingHttp>>,
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

    pub fn transport(mut self, transport: Arc<dyn BlockingHttp>) -> Self {
        self.transport = Some(transport);
        self
    }

    pub fn chunk_size(mut self, chunk_size: u64) -> Self {
        self.config.chunk_size = chunk_size.max(1);
        self
    }

    pub fn read_ahead(mut self, enabled: bool) -> Self {
        self.config.read_ahead = enabled;
        self
    }

    pub fn read_ahead_trigger(mut self, trigger: u64) -> Self {
        self.config.read_ahead_trigger = trigger.max(1);
        self
    }

    pub fn cache_max_entries(mut self, max_entries: usize) -> Self {
        self.config.cache_max_entries = max_entries;
        self
    }

    pub fn cache_max_bytes(mut self, max_bytes: usize) -> Self {
        self.config.cache_max_bytes = max_bytes;
        self
    }

    pub fn retry_max_attempts(mut self, attempts: usize) -> Self {
        self.config.retry_max_attempts = attempts;
        self
    }

    pub fn retry_base_delay(mut self, delay: Duration) -> Self {
        self.config.retry_base_delay = delay;
        self
    }

    pub fn retry_max_delay(mut self, delay: Duration) -> Self {
        self.config.retry_max_delay = delay;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout = timeout;
        self
    }

    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.config.read_timeout = timeout;
        self
    }

    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.config.idle_timeout = timeout;
        self
    }

    pub fn build(self) -> HttpFileSystem {
        let transport = self
            .transport
            .unwrap_or_else(|| build_default_transport(&self.config));

        HttpFileSystem {
            transport,
            config: self.config.clone(),
            cache: Arc::new(Mutex::new(RangeCache::new(
                self.config.cache_max_entries,
                self.config.cache_max_bytes,
            ))),
        }
    }
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
            return Err(FsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Only read mode ('r' or 'rb') is supported",
            )));
        }

        Ok(Box::new(HttpFile::new(
            Arc::from(url),
            Arc::clone(&self.transport),
            self.config.clone(),
            Arc::clone(&self.cache),
        )))
    }
}
