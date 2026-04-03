# Pravaha C API Documentation

Build with `--features capi` to get C bindings.

## Header File

```c
#include "pravaha.h"
```

## Blocking behaviour

All operations are blocking. Internally the library uses asynchronous I/O (Tokio), but the C API presents a fully synchronous interface - every call blocks the calling thread until the operation completes or fails.

## ABI Versioning

```c
#define PRAVAHA_ABI_VERSION 1
```

Increment when the ABI changes in a backwards-incompatible way. Consumers can guard against version mismatches at compile time:

```c
#if PRAVAHA_ABI_VERSION != 1
#  error "Unexpected pravaha ABI version"
#endif
```

---

## Types

### `pravaha_filesystem_t`
Opaque handle to a filesystem instance. Safe to share between threads.

### `pravaha_file_t`
Opaque handle to an open file.

Thread safety depends on which functions you call:

| Function group | Pointer type | Thread safety |
|---|---|---|
| `pravaha_read`, `pravaha_seek`, `pravaha_tell`, `pravaha_eof` | `pravaha_file_t*` (mutable) | **Not thread-safe.** Use from one thread at a time only. |
| `pravaha_read_at`, `pravaha_size` | `const pravaha_file_t*` (const) | **Thread-safe.** Multiple threads may call concurrently on the same handle. |

In practice: if you only use `pravaha_read_at`, you can share one handle across
threads with no locking. If you also need stateful `pravaha_read`/`pravaha_seek`,
protect the handle with a mutex.

### `PravahaErrorCode`
Error codes returned by functions:

```c
enum PravahaErrorCode {
    PRAVAHA_SUCCESS              = 0,   /* Operation succeeded              */
    PRAVAHA_NETWORK              = 1,   /* TCP / DNS / connection error     */
    PRAVAHA_PROTOCOL             = 2,   /* HTTP protocol violation          */
    PRAVAHA_IO                   = 3,   /* Local I/O error                  */
    PRAVAHA_FILE_CLOSED          = 4,   /* Operation on a closed file       */
    PRAVAHA_UNSUPPORTED_PROTOCOL = 5,   /* Non-HTTP(S) URL                  */
    PRAVAHA_INVALID_ARGUMENT     = 6,   /* NULL pointer or bad argument     */
    PRAVAHA_RATE_LIMITED         = 7,   /* Server returned 429 / 503        */
    PRAVAHA_PANIC                = 8,   /* Internal panic (please report)   */
    PRAVAHA_UNKNOWN              = 99
};
```

### Panic fallback behaviour

If an internal panic occurs, it is caught at the FFI boundary and converted to
an error rather than unwinding into C (which would be undefined behaviour).
In that case:

- Functions returning pointers return `NULL`
- `pravaha_read` and `pravaha_read_at` return `-1`
- All other functions return `PRAVAHA_PANIC`

In all cases `pravaha_last_error()` is set to a descriptive message. If you
encounter `PRAVAHA_PANIC` please report it as a bug.

---

## Error Handling

```c
const char* pravaha_last_error(void);
```

Returns the last error message for the current thread.

**Returns:**
- Pointer to a null-terminated error string if an error occurred
- `NULL` if no error has occurred since the last successful call

**Note:** The pointer is valid until the next pravaha call on the same thread.
Do not free it. Error strings are stored in thread-local storage, so
`pravaha_last_error()` is always thread-safe.

**Example:**
```c
pravaha_file_t* file = pravaha_open_url(url, "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
}
```

---

## Library Version

```c
const char* pravaha_version(void);
```

**Returns:** Pointer to a static version string (e.g. `"0.1.1"`). Never NULL.

---

## Filesystem Operations

```c
pravaha_filesystem_t* pravaha_create(const char* url);
```

Creates a filesystem handle for the given base URL.

**Parameters:**
- `url` - Null-terminated URL string (must start with `http://` or `https://`)

**Returns:** Handle on success, `NULL` on error.

**Note:** Caller must free with `pravaha_filesystem_free()`.

**Example:**
```c
pravaha_filesystem_t* fs = pravaha_create("https://example.com");
if (!fs) {
    fprintf(stderr, "Failed: %s\n", pravaha_last_error());
    return 1;
}
```

---

```c
void pravaha_filesystem_free(pravaha_filesystem_t* fs);
```

Frees a filesystem handle. Passing `NULL` is safe and a no-op.

**Warning:** Do not use or free the handle again after this call.

---

## File Operations

```c
pravaha_file_t* pravaha_open(pravaha_filesystem_t* fs,
                              const char* path,
                              const char* mode);
```

Opens a file using an existing filesystem handle.

**Parameters:**
- `fs` - Valid filesystem handle
- `path` - Path or URL to open
- `mode` - `"r"` or `"rb"` (read-only; write modes are not supported)

**Returns:** Handle on success, `NULL` on error.

**Note:** Caller must free with `pravaha_file_close()`.

---

```c
pravaha_file_t* pravaha_open_url(const char* url, const char* mode);
```

Opens a file directly without a separate filesystem handle.

**Parameters:**
- `url` - Null-terminated URL string
- `mode` - `"r"` or `"rb"`

**Returns:** Handle on success, `NULL` on error.

**Note:** Caller must free with `pravaha_file_close()`.

**Example:**
```c
pravaha_file_t* file = pravaha_open_url("https://example.com/data.bin", "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
    return 1;
}
```

---

```c
void pravaha_file_close(pravaha_file_t* file);
```

Closes a file and frees its resources. Passing `NULL` is safe and a no-op.
Cancels any in-progress prefetch operations.

**Warning:** Do not use or free the handle again after this call.

```c
pravaha_file_close(file);
file = NULL;  /* prevent accidental reuse */
```

---

## File I/O

```c
ssize_t pravaha_read(pravaha_file_t* file, void* buffer, size_t size);
```

Reads up to `size` bytes into `buffer` starting at the current cursor position,
then advances the cursor by the number of bytes read. Blocks until data is
available.

**Not thread-safe** - do not call on the same handle from multiple threads
simultaneously. Use `pravaha_read_at` for concurrent access.

**Returns:**
- `> 0` - number of bytes read
- `0` - end of file
- `-1` - error (call `pravaha_last_error()`)

**Example:**
```c
char buf[4096];
ssize_t n = pravaha_read(file, buf, sizeof(buf));
if (n < 0)       fprintf(stderr, "Read error: %s\n", pravaha_last_error());
else if (n == 0) printf("EOF\n");
else             printf("Read %zd bytes\n", n);
```

---

```c
ssize_t pravaha_read_at(const pravaha_file_t* file,
                         uint64_t offset,
                         void* buffer,
                         size_t size);
```

Reads up to `size` bytes into `buffer` starting at `offset`, **without moving
the cursor**. Equivalent to POSIX `pread(2)`.

**Thread-safe** - multiple threads may call `pravaha_read_at` concurrently on
the same handle with no external locking required. The underlying chunk cache
and in-flight deduplication are shared: two threads reading overlapping byte
ranges will issue only one HTTP request between them.

**Returns:**
- `> 0` - number of bytes read
- `0` - `offset` is at or past end of file
- `-1` - error (call `pravaha_last_error()`)

**Example:**
```c
char buf[4096];
ssize_t n = pravaha_read_at(file, 1048576, buf, sizeof(buf));
if (n < 0) fprintf(stderr, "read_at error: %s\n", pravaha_last_error());
```

**Concurrent example (pthreads):**
```c
struct ReadArgs { const pravaha_file_t* file; uint64_t offset; };

void* thread_fn(void* arg) {
    struct ReadArgs* a = arg;
    char buf[1024 * 1024];
    ssize_t n = pravaha_read_at(a->file, a->offset, buf, sizeof(buf));
    /* n >= 0: process buf[0..n] */
    return NULL;
}

/* Four threads, four offsets, one shared handle - no mutex needed. */
pthread_t threads[4];
struct ReadArgs args[4] = {
    { file,  0 * 1024 * 1024 },
    { file, 64 * 1024 * 1024 },
    { file, 128 * 1024 * 1024 },
    { file, 192 * 1024 * 1024 },
};
for (int i = 0; i < 4; i++)
    pthread_create(&threads[i], NULL, thread_fn, &args[i]);
for (int i = 0; i < 4; i++)
    pthread_join(threads[i], NULL);
```

---

```c
int pravaha_seek(pravaha_file_t* file, uint64_t pos);
```

Seeks to an absolute byte position. Updates the internal cursor used by
`pravaha_read`. Unlike local file I/O, seeking may trigger HTTP range requests
and is not constant-time.

**Not thread-safe** - do not mix with concurrent `pravaha_read` calls on the
same handle.

**Returns:** `PRAVAHA_SUCCESS` (0) on success, or a `PravahaErrorCode` on failure.

**Example:**
```c
if (pravaha_seek(file, 65536) != PRAVAHA_SUCCESS) {
    fprintf(stderr, "Seek error: %s\n", pravaha_last_error());
}
```

---

```c
int pravaha_tell(const pravaha_file_t* file, uint64_t* out_pos);
```

Gets the current cursor position (as set by `pravaha_read` / `pravaha_seek`).
`pravaha_read_at` does not affect the value returned by `pravaha_tell`.

**Parameters:**
- `file` - Valid file handle
- `out_pos` - Output: receives the current position on success; unchanged on failure

**Returns:** `PRAVAHA_SUCCESS` on success, or a `PravahaErrorCode` on failure.

**Example:**
```c
uint64_t pos;
if (pravaha_tell(file, &pos) == PRAVAHA_SUCCESS)
    printf("Position: %" PRIu64 "\n", pos);
```

---

```c
int pravaha_size(const pravaha_file_t* file, uint64_t* out_size, int* has_size);
```

Gets the file size if the server provided a `Content-Length`. May perform an
HTTP HEAD request the first time it is called; the result is cached for
subsequent calls.

**Thread-safe** - may be called concurrently with `pravaha_read_at`.

**Parameters:**
- `file` - Valid file handle
- `out_size` - Output: receives the file size in bytes when `*has_size == 1`
- `has_size` - Output: set to `1` if size is known, `0` otherwise

**Returns:** `PRAVAHA_SUCCESS` on success, or a `PravahaErrorCode` on failure.

**Note:** Streams and chunked-transfer responses may not have a known size.

**Example:**
```c
uint64_t size;
int has_size;
if (pravaha_size(file, &size, &has_size) == PRAVAHA_SUCCESS && has_size)
    printf("File size: %" PRIu64 " bytes\n", size);
else
    printf("File size unknown\n");
```

---

```c
int pravaha_eof(const pravaha_file_t* file);
```

Checks whether the cursor (as used by `pravaha_read`) has reached end-of-file.
This flag is set after `pravaha_read` returns `0` and is cleared by
`pravaha_seek`. It is unaffected by `pravaha_read_at`.

**Returns:**
- `1` if at EOF
- `0` if not at EOF (or if `file` is `NULL` - check `pravaha_last_error()` to distinguish)

---

## Complete Example

```c
#include <inttypes.h>
#include <pthread.h>
#include <stdio.h>
#include "pravaha.h"

/* Worker: reads 1 MB at a given offset using the shared handle. */
struct Args { const pravaha_file_t* file; uint64_t offset; int id; };

void* worker(void* arg) {
    struct Args* a = arg;
    char buf[1024 * 1024];
    ssize_t n = pravaha_read_at(a->file, a->offset, buf, sizeof(buf));
    if (n < 0)
        fprintf(stderr, "[%d] read_at error: %s\n", a->id, pravaha_last_error());
    else
        printf("[%d] read_at(%" PRIu64 ") -> %zd bytes\n", a->id, a->offset, n);
    return NULL;
}

int main(void) {
    printf("pravaha %s (ABI %d)\n", pravaha_version(), PRAVAHA_ABI_VERSION);

    pravaha_file_t* file = pravaha_open_url("https://example.com/data.bin", "r");
    if (!file) {
        fprintf(stderr, "Open failed: %s\n", pravaha_last_error());
        return 1;
    }

    /* File size (may perform a HEAD request; result is cached). */
    uint64_t size = 0;
    int has_size = 0;
    if (pravaha_size(file, &size, &has_size) == PRAVAHA_SUCCESS && has_size)
        printf("Size: %" PRIu64 " bytes\n", size);

    /* Stateful sequential read. */
    char buf[4096];
    ssize_t n = pravaha_read(file, buf, sizeof(buf));
    if (n < 0) {
        fprintf(stderr, "Read error: %s\n", pravaha_last_error());
        pravaha_file_close(file);
        return 1;
    }
    printf("Sequential read: %zd bytes\n", n);

    /* Seek and tell (stateful cursor). */
    if (pravaha_seek(file, 65536) != PRAVAHA_SUCCESS)
        fprintf(stderr, "Seek error: %s\n", pravaha_last_error());

    uint64_t pos;
    if (pravaha_tell(file, &pos) == PRAVAHA_SUCCESS)
        printf("Cursor after seek: %" PRIu64 "\n", pos);

    /* Stateless positional read - cursor unchanged. */
    n = pravaha_read_at(file, 1048576, buf, sizeof(buf));
    printf("read_at(1MB): %zd bytes\n", n);

    uint64_t pos2;
    pravaha_tell(file, &pos2);
    printf("Cursor still at: %" PRIu64 " (unchanged by read_at)\n", pos2);

    /* Four threads reading at different offsets - no mutex needed. */
    pthread_t threads[4];
    struct Args args[4] = {
        { file,  0 * 1024 * 1024, 0 },
        { file, 64 * 1024 * 1024, 1 },
        { file, 128 * 1024 * 1024, 2 },
        { file, 192 * 1024 * 1024, 3 },
    };
    for (int i = 0; i < 4; i++)
        pthread_create(&threads[i], NULL, worker, &args[i]);
    for (int i = 0; i < 4; i++)
        pthread_join(threads[i], NULL);

    pravaha_file_close(file);
    file = NULL;
    return 0;
}
```

---

## Thread Safety Summary

| Function | Pointer type | Thread-safe? |
|---|---|---|
| `pravaha_read_at` | `const pravaha_file_t*` | Yes - concurrent calls allowed |
| `pravaha_size` | `const pravaha_file_t*` | Yes - result is cached after first call |
| `pravaha_read` | `pravaha_file_t*` | No - one thread at a time |
| `pravaha_seek` | `pravaha_file_t*` | No - one thread at a time |
| `pravaha_tell` | `const pravaha_file_t*` | No - may race with `pravaha_read` |
| `pravaha_eof` | `const pravaha_file_t*` | No - may race with `pravaha_read` |
| `pravaha_last_error` | - | Yes - thread-local storage |
| `pravaha_filesystem_t` all ops | - | Yes - share freely |

---

## Memory Management

| What                    | How to free                  |
|-------------------------|------------------------------|
| `pravaha_filesystem_t*` | `pravaha_filesystem_free()`  |
| `pravaha_file_t*`       | `pravaha_file_close()`       |
| Error strings           | Do **not** free - managed internally |

Passing `NULL` to either free function is safe and a no-op.