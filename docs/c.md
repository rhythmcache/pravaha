# Pravaha C API Documentation

Build with `--features capi` to get C bindings.

## Header File

```c
#include "pravaha.h"
```

## Blocking behaviour

All operations are blocking. Internally the library uses asynchronous I/O (Tokio), but the C API presents a fully synchronous interface — every call blocks the calling thread until the operation completes or fails.

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
Opaque handle to a filesystem instance. Can be shared between threads.

### `pravaha_file_t`
Opaque handle to an open file. Must not be used from multiple threads simultaneously, and functions are not reentrant for the same handle (e.g. do not call back into the API with the same handle from within a signal handler or callback).

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

If an internal panic occurs, it is caught at the FFI boundary and converted to an error rather than unwinding into C (which would be undefined behaviour). In that case:

- Functions returning pointers return `NULL`
- `pravaha_read` returns `-1`
- All other functions return `PRAVAHA_PANIC`

In all cases `pravaha_last_error()` is set to a descriptive message. If you encounter `PRAVAHA_PANIC` please report it as a bug.

---

## Error Handling

```c
const char* pravaha_last_error(void);
```

Returns the last error message for the current thread.

**Returns:**
- Pointer to a null-terminated error string if an error occurred
- `NULL` if no error

**Note:** The pointer is valid until the next pravaha call on the same thread. Do not free it.

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

**Returns:** Pointer to a static version string (e.g. `"0.1.0"`). Never NULL.

---

## Filesystem Operations

```c
pravaha_filesystem_t* pravaha_create(const char* url);
```

Creates a filesystem handle for the given base URL.

**Parameters:**
- `url` — Null-terminated URL string (must start with `http://` or `https://`)

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

**Warning:** Do not use or free the handle again after this call (double-free = undefined behaviour).

---

## File Operations

```c
pravaha_file_t* pravaha_open(pravaha_filesystem_t* fs,
                              const char* path,
                              const char* mode);
```

Opens a file using an existing filesystem handle.

**Parameters:**
- `fs` — Valid filesystem handle
- `path` — Path or URL resolved relative to the filesystem base URL
- `mode` — `"r"` or `"rb"` (read-only; write modes are not supported)

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

**Warning:** Do not use or free the handle again after this call.

**Example:**
```c
pravaha_file_close(file);
file = NULL;  /* prevent accidental reuse */
```

---

## File I/O

```c
ssize_t pravaha_read(pravaha_file_t* file, void* buffer, size_t size);
```

Reads up to `size` bytes into `buffer`. Blocks until data is available.

**Returns:**
- `> 0` — number of bytes read
- `0` — end of file
- `-1` — error (call `pravaha_last_error()`)

**Example:**
```c
char buf[4096];
ssize_t n = pravaha_read(file, buf, sizeof(buf));
if (n < 0)       fprintf(stderr, "Read error: %s\n", pravaha_last_error());
else if (n == 0) printf("EOF\n");
else             printf("Read %lld bytes\n", (long long)n);
```

---

```c
int pravaha_seek(pravaha_file_t* file, uint64_t pos);
```

Seeks to an absolute byte position. Unlike local file I/O, seeking may trigger additional HTTP range requests and is not constant-time.

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

Gets the current byte position.

**Parameters:**
- `file` — Valid file handle
- `out_pos` — Output: receives the current position on success; unchanged on failure

**Returns:** `PRAVAHA_SUCCESS` on success, or a `PravahaErrorCode` on failure.

**Example:**
```c
uint64_t pos;
if (pravaha_tell(file, &pos) == PRAVAHA_SUCCESS) {
    printf("Position: %" PRIu64 "\n", pos);
}
```

---

```c
int pravaha_size(const pravaha_file_t* file, uint64_t* out_size, int* has_size);
```

Gets the file size if the server provided a `Content-Length`. This may perform a network request (HTTP HEAD) if the size has not been cached yet.

**Parameters:**
- `file` — Valid file handle
- `out_size` — Output: receives the file size in bytes when `*has_size == 1`
- `has_size` — Output: set to `1` if size is known, `0` otherwise

**Returns:** `PRAVAHA_SUCCESS` on success, or a `PravahaErrorCode` on failure.

**Note:** Streams and chunked-transfer responses may not have a known size.

**Example:**
```c
uint64_t size;
int has_size;
if (pravaha_size(file, &size, &has_size) == PRAVAHA_SUCCESS && has_size) {
    printf("File size: %" PRIu64 " bytes\n", size);
} else {
    printf("File size unknown\n");
}
```

---

```c
int pravaha_eof(const pravaha_file_t* file);
```

Checks whether the read position is at end-of-file.

**Returns:**
- `1` if at EOF
- `0` if not at EOF

If `file` is `NULL`, returns `0` and sets the last error. Call `pravaha_last_error()` to distinguish a genuine not-EOF result from a null-pointer error.

---

## Complete Example

```c
#include <inttypes.h>
#include <stdio.h>
#include "pravaha.h"

int main(void) {
    printf("pravaha %s (ABI %d)\n", pravaha_version(), PRAVAHA_ABI_VERSION);

    pravaha_file_t* file = pravaha_open_url("https://example.com/data.bin", "r");
    if (!file) {
        fprintf(stderr, "Open failed: %s\n", pravaha_last_error());
        return 1;
    }

    /* File size (may perform a HEAD request) */
    uint64_t size;
    int has_size;
    if (pravaha_size(file, &size, &has_size) == PRAVAHA_SUCCESS && has_size)
        printf("Size: %" PRIu64 " bytes\n", size);

    /* Read */
    char buf[4096];
    ssize_t n = pravaha_read(file, buf, sizeof(buf));
    if (n < 0) {
        fprintf(stderr, "Read error: %s\n", pravaha_last_error());
        pravaha_file_close(file);
        return 1;
    }
    printf("Read %lld bytes\n", (long long)n);

    /* Seek (may trigger HTTP range request) */
    if (pravaha_seek(file, 65536) != PRAVAHA_SUCCESS)
        fprintf(stderr, "Seek error: %s\n", pravaha_last_error());

    /* Tell */
    uint64_t pos;
    if (pravaha_tell(file, &pos) == PRAVAHA_SUCCESS)
        printf("Position: %" PRIu64 "\n", pos);

    /* EOF check */
    if (pravaha_eof(file))
        printf("At end of file\n");

    pravaha_file_close(file);
    return 0;
}
```

---

## Thread Safety

- Each `pravaha_file_t` handle must be used from only one thread at a time.
- Functions are not reentrant for the same handle.
- `pravaha_filesystem_t` handles are safe to share across threads.
- Error strings are stored in thread-local storage - `pravaha_last_error()` is always thread-safe.

## Memory Management

| What                    | How to free                  |
|-------------------------|------------------------------|
| `pravaha_filesystem_t*` | `pravaha_filesystem_free()`  |
| `pravaha_file_t*`       | `pravaha_file_close()`       |
| Error strings           | Do **not** free — managed internally |

Passing `NULL` to either free function is safe and a no-op.