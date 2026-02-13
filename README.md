# pravaha

**Pravaha** (प्रवाह - "flow" in Sanskrit) lets you read files from HTTP(S) URLs as if they were local files.

```rust
use pravaha::open;

let mut file = open("https://example.com/data.bin", "r")?;
let mut buffer = vec![0u8; 1024];
file.read(&mut buffer)?;
file.seek(500)?;
```

## Installation

```toml
[dependencies]
pravaha = "0.1.0"
```

Default HTTP backend is `curl`. For `reqwest` instead:

```toml
[dependencies]
pravaha = { version = "0.1.0", default-features = false, features = ["reqwest"] }
```

## Usage

You have a large file on a server and don't want to download all of it. Maybe you only need specific parts, or you want to stream it as you process. This library makes remote files work like local files.


Basic:

```rust
use pravaha::open;

let mut file = open("https://example.com/file.bin", "r")?;
let mut buffer = vec![0u8; 4096];
file.read(&mut buffer)?;
file.seek(1000)?;

if let Some(size) = file.size() {
    println!("File is {} bytes", size);
}
```

With custom configuration:

```rust
use pravaha::HttpFileSystem;
use std::time::Duration;

let fs = HttpFileSystem::builder()
    .chunk_size(1024 * 1024)
    .cache_max_bytes(64 * 1024 * 1024)
    .read_ahead(true)
    .retry_max_attempts(5)
    .build();

let mut file = fs.open("https://example.com/big-file.bin", "r")?;
```

## C API

Build with `--features capi` to get C bindings.


Example usage:

```c
#include "pravaha.h"

pravaha_file_t* file = pravaha_open_url("https://example.com/data.bin", "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
    return 1;
}

char buffer[1024];
ssize_t n = pravaha_read(file, buffer, sizeof(buffer));
pravaha_file_close(file);
```

## How it works

- Fetches in configurable chunks (default 256KB)
- LRU cache for previously read chunks
- Background prefetching for sequential reads (auto-disables for random access)
- Exponential backoff retry on network failures
- Implements standard `Read` and `Seek` traits

## Requirements

Server must support HTTP Range requests (RFC 7233). Returns error if server doesn't support partial content.

## Thread safety

`FileSystem` is `Send + Sync`. `File` handles are `Send` but not `Sync`.

## License

Apache License 2.0 - see [LICENSE](./LICENSE)
