# pravaha

**Pravaha** (प्रवाह - "flow" in Sanskrit) lets you read files from HTTP(S) URLs as if they were local files. It is a **read-only** library - write operations are not supported.

```rust
use pravaha::{open, File};

let mut file = open("https://example.com/data.bin", "r")?;
let mut buf = vec![0u8; 1024];
file.read(&mut buf)?;
file.seek(500)?;
```

## Blocking behaviour

All operations are synchronous from the caller's perspective. Internally Pravaha uses asynchronous I/O (Tokio), but the public API blocks the calling thread until data is available. No `async`/`await` leaks into your code.

## Installation

```toml
[dependencies]
pravaha = "0.1.1"
```

Default HTTP backend is `curl`. To use `reqwest` instead:

```toml
[dependencies]
pravaha = { version = "0.1.0", default-features = false, features = ["reqwest"] }
```

## Feature flags

| Feature   | Default | Description                                               |
|-----------|---------|-----------------------------------------------------------|
| `curl`    | ✓       | libcurl backend (blocking, executed via `spawn_blocking`) |
| `reqwest` |         | async reqwest backend (don't enable both)                 |
| `capi`    |         | C ABI bindings + header generation                        |

## Usage

Useful when you have a large remote file and only need parts of it — or want to stream it incrementally without downloading everything first.

### Basic

```rust
use pravaha::{open, File};

let mut file = open("https://example.com/file.bin", "r")?; // returns Result<Box<dyn File>>
let mut buf = vec![0u8; 4096];
let n = file.read(&mut buf)?;

// Seeking may trigger additional HTTP range requests
// and is not constant-time like local file I/O.
file.seek(1_000_000)?;

if let Some(size) = file.size() {
    println!("File is {} bytes", size);
}
```

### Custom configuration

```rust
use pravaha::{HttpFileSystem, FileSystem};
use std::time::Duration;

let fs = HttpFileSystem::builder()
    .chunk_size(1024 * 1024)            // 1 MB chunks
    .read_ahead_chunks(4)               // prefetch 4 chunks ahead on sequential reads
    .max_parallel_fetches(8)            // up to 8 concurrent HTTP requests
    .cache_max_entries(128)
    .cache_max_bytes(128 * 1024 * 1024) // 128 MB LRU cache
    .retry_max_attempts(5)
    .connect_timeout(Duration::from_secs(10))
    .read_timeout(Duration::from_secs(30))
    .build();

let mut file = fs.open("https://example.com/big-file.bin", "r")?;
```

### Using with standard I/O libraries

Wrap in `FileAdapter` to get `std::io::Read + Seek` for third-party crates:

```rust
use pravaha::{open, FileAdapter};
use zip::ZipArchive;

let file = open("https://example.com/archive.zip", "r")?;
let mut archive = ZipArchive::new(FileAdapter::new(file))?;
```

## C API

Build with `--features capi` to generate C bindings and a pkg-config file. All C API calls are blocking, matching the Rust API behaviour.

```c
#include "pravaha.h"

pravaha_file_t* file = pravaha_open_url("https://example.com/data.bin", "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
    return 1;
}

char buf[4096];
ssize_t n = pravaha_read(file, buf, sizeof(buf));

uint64_t pos;
pravaha_tell(file, &pos);

uint64_t size;
int has_size;
pravaha_size(file, &size, &has_size);

pravaha_file_close(file);
```

For the full C API reference see [docs/c.md](docs/c.md).

## How it works

- Fetches data in configurable chunks (default 256 KB)
- LRU cache for completed chunks (default 32 MB / 64 entries)
- Speculative prefetch based on access pattern - triggered on sequential reads, skipped when reads are non-sequential
- In-flight deduplication - concurrent reads on the same chunk share one HTTP request
- Exponential backoff retry on network errors; `Retry-After`-aware for 429/503 responses
- `HttpFile` implements `std::io::Read` and `Seek` directly

### Architecture

```
┌──────────────────────────────────────────────┐
│  Public sync API  (File / FileSystem traits) │
│  HttpFile::read -> block_sync(engine.get_chunk)│
└──────────────────┬───────────────────────────┘
                   │
┌──────────────────V───────────────────────────┐
│  FetchEngine  (shared via Arc per FileSystem) │
│                                              │
│  DashMap<ChunkKey, SharedFuture>             │
│    Missing -> spawn new fetch                │
│    InFlight -> clone + await existing future  │
│    Ready    -> LRU hit, wrap in ready future  │
│                                              │
│  Semaphore: caps concurrent HTTP requests    │
└──────────────────┬───────────────────────────┘
                   │
┌──────────────────V───────────────────────────┐
│  AsyncHttp transport (curl / reqwest)        │
│  Retry · exponential backoff · 429/503 aware │
└──────────────────────────────────────────────┘
```

## Requirements

- The server must support HTTP Range requests (RFC 7233). Pravaha returns a `Protocol` error if the server responds with `200 OK` instead of `206 Partial Content`.
- Performance depends on server behaviour. Some servers throttle or limit parallel range requests, which reduces the benefit of concurrency.

## Thread safety

- `HttpFileSystem` is `Send + Sync` - share one instance freely across threads.
- `HttpFile` handles are `Send` - move to another thread safely.
- `HttpFile` handles are not `Sync` - don't use one handle from multiple threads simultaneously.

## Memory budget

With defaults: `max_parallel_fetches (4) x chunk_size (256 KB) = 1 MB` peak in-flight, plus up to 32 MB completed LRU cache. Tune conservatively for memory-constrained environments.

## License

Apache License 2.0 - see [LICENSE](./LICENSE)
