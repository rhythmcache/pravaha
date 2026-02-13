# Pravaha C API Documentation

Build with `--features capi` to get C bindings.

## Header File

```c
#include "pravaha.h"
```

## Types

### `PravahaFilesystem`
Opaque handle to a filesystem instance.

### `PravahaFile`
Opaque handle to an open file.

### `PravahaErrorCode`
Error codes returned by functions:
```c
enum PravahaErrorCode {
    PRAVAHA_SUCCESS = 0,
    PRAVAHA_NETWORK = 1,
    PRAVAHA_PROTOCOL = 2,
    PRAVAHA_IO = 3,
    PRAVAHA_FILE_CLOSED = 4,
    PRAVAHA_UNSUPPORTED_PROTOCOL = 5,
    PRAVAHA_INVALID_ARGUMENT = 6,
    PRAVAHA_UNKNOWN = 99
};
```

## Functions

### Library Version

```c
const char* pravaha_version(void);
```
Returns a pointer to a static string containing the library version (e.g., "0.1.0").

**Returns:** Pointer to static version string (never NULL).

---

### Error Handling

```c
const char* pravaha_last_error(void);
```
Returns the last error message for the current thread.

**Returns:** 
- Pointer to error string if an error occurred
- NULL if no error

**Note:** The returned string is valid until the next pravaha function call on the same thread.

**Example:**
```c
PravahaFile* file = pravaha_open_url(url, "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
}
```

---

### Filesystem Operations

```c
PravahaFilesystem* pravaha_create(const char* url);
```
Creates a filesystem for the given URL.

**Parameters:**
- `url` - Null-terminated URL string (must start with http:// or https://)

**Returns:**
- Pointer to filesystem handle on success
- NULL on error (call `pravaha_last_error()` for details)

**Note:** Caller must free the returned pointer with `pravaha_filesystem_free()`.

**Example:**
```c
PravahaFilesystem* fs = pravaha_create("https://example.com");
if (!fs) {
    fprintf(stderr, "Failed to create filesystem: %s\n", pravaha_last_error());
    return 1;
}
```

---

```c
void pravaha_filesystem_free(PravahaFilesystem* fs);
```
Frees a filesystem handle.

**Parameters:**
- `fs` - Filesystem handle (can be NULL)

**Warning:**
- The handle must not be used or freed again after this call
- Passing the same handle twice to `pravaha_filesystem_free()` results in undefined behavior
- Double-free will cause crashes

---

### File Operations

```c
PravahaFile* pravaha_open(PravahaFilesystem* fs, 
                          const char* path, 
                          const char* mode);
```
Opens a file using an existing filesystem handle.

**Parameters:**
- `fs` - Valid filesystem handle
- `path` - Null-terminated path/URL string
- `mode` - File mode ("r" or "rb" for read-only)

**Returns:**
- Pointer to file handle on success
- NULL on error (call `pravaha_last_error()` for details)

**Note:** Caller must free the returned pointer with `pravaha_file_close()`.

**Example:**
```c
PravahaFile* file = pravaha_open(fs, "https://example.com/data.bin", "r");
if (!file) {
    fprintf(stderr, "Failed to open file: %s\n", pravaha_last_error());
}
```

---

```c
PravahaFile* pravaha_open_url(const char* url, const char* mode);
```
Opens a file directly without creating a filesystem handle first.

**Parameters:**
- `url` - Null-terminated URL string
- `mode` - File mode ("r" or "rb" for read-only)

**Returns:**
- Pointer to file handle on success
- NULL on error (call `pravaha_last_error()` for details)

**Note:** Caller must free the returned pointer with `pravaha_file_close()`.

**Example:**
```c
PravahaFile* file = pravaha_open_url("https://example.com/data.bin", "r");
if (!file) {
    fprintf(stderr, "Error: %s\n", pravaha_last_error());
    return 1;
}
```

---

```c
ssize_t pravaha_read(PravahaFile* file, void* buffer, size_t size);
```
Reads up to `size` bytes from the file into `buffer`.

**Parameters:**
- `file` - Valid file handle
- `buffer` - Buffer to read data into (must be at least `size` bytes)
- `size` - Maximum number of bytes to read

**Returns:**
- Number of bytes read (0 indicates EOF)
- -1 on error (call `pravaha_last_error()` for details)

**Note:** Returns a signed integer (`ssize_t` on POSIX systems, `isize` equivalent on Windows).

**Example:**
```c
char buffer[1024];
ssize_t n = pravaha_read(file, buffer, sizeof(buffer));
if (n < 0) {
    fprintf(stderr, "Read error: %s\n", pravaha_last_error());
} else if (n == 0) {
    printf("End of file\n");
} else {
    printf("Read %zd bytes\n", n);
}
```

---

```c
int pravaha_seek(PravahaFile* file, uint64_t pos);
```
Seeks to an absolute position in the file.

**Parameters:**
- `file` - Valid file handle
- `pos` - Absolute byte position to seek to

**Returns:**
- `PRAVAHA_SUCCESS` (0) on success
- Error code from `PravahaErrorCode` enum on failure

**Example:**
```c
if (pravaha_seek(file, 1000) != PRAVAHA_SUCCESS) {
    fprintf(stderr, "Seek error: %s\n", pravaha_last_error());
}
```

---

```c
uint64_t pravaha_tell(const PravahaFile* file);
```
Gets the current position in the file.

**Parameters:**
- `file` - Valid file handle

**Returns:**
- Current byte position
- 0 if file is invalid (error set via `pravaha_last_error()`)

**Note:** Since 0 is also a valid position, callers should check `pravaha_last_error()` if 0 is unexpected.

**Example:**
```c
uint64_t pos = pravaha_tell(file);
printf("Current position: %lu\n", pos);
```

---

```c
uint64_t pravaha_size(const PravahaFile* file, int* has_size);
```
Gets the file size if available.

**Parameters:**
- `file` - Valid file handle
- `has_size` - Output parameter: set to 1 if size is available, 0 otherwise

**Returns:**
- File size in bytes if available
- 0 if size is not available or on error

**Note:** Check `has_size` to determine if the returned size is valid. Streams and chunked responses may not have a known size.

**Example:**
```c
int has_size;
uint64_t size = pravaha_size(file, &has_size);
if (has_size) {
    printf("File size: %lu bytes\n", size);
} else {
    printf("File size unknown\n");
}
```

---

```c
int pravaha_eof(const PravahaFile* file);
```
Checks if the file is at end-of-file.

**Parameters:**
- `file` - Valid file handle

**Returns:**
- 1 if at EOF
- 0 otherwise

**Example:**
```c
if (pravaha_eof(file)) {
    printf("Reached end of file\n");
}
```

---

```c
void pravaha_file_close(PravahaFile* file);
```
Closes a file and frees its resources.

**Parameters:**
- `file` - File handle (can be NULL)

**Warning:** 
- The handle must not be used or freed again after this call
- Passing the same handle twice to `pravaha_file_close()` results in undefined behavior
- Double-free will cause crashes

**Example:**
```c
pravaha_file_close(file);
file = NULL;  // Good practice to prevent accidental reuse
```

---

## Complete Example

```c
#include <stdio.h>
#include <pravaha.h>

int main(void) {
    // Print library version
    printf("Pravaha version: %s\n", pravaha_version());
    
    // Open a file
    PravahaFile* file = pravaha_open_url("https://example.com/data.bin", "r");
    if (!file) {
        fprintf(stderr, "Failed to open: %s\n", pravaha_last_error());
        return 1;
    }
    
    // Get file size
    int has_size;
    uint64_t size = pravaha_size(file, &has_size);
    if (has_size) {
        printf("File size: %lu bytes\n", size);
    }
    
    // Read some data
    char buffer[1024];
    ssize_t n = pravaha_read(file, buffer, sizeof(buffer));
    if (n < 0) {
        fprintf(stderr, "Read error: %s\n", pravaha_last_error());
        pravaha_file_close(file);
        return 1;
    }
    printf("Read %zd bytes\n", n);
    
    // Seek to position
    if (pravaha_seek(file, 1000) != PRAVAHA_SUCCESS) {
        fprintf(stderr, "Seek error: %s\n", pravaha_last_error());
    }
    
    // Get current position
    uint64_t pos = pravaha_tell(file);
    printf("Current position: %lu\n", pos);
    
    // Check EOF
    if (pravaha_eof(file)) {
        printf("At end of file\n");
    }
    
    // Clean up
    pravaha_file_close(file);
    
    return 0;
}
```

## Thread Safety

- Functions are thread-safe as long as individual `PravahaFile` handles are not used concurrently from multiple threads
- Error messages are stored per-thread
- `PravahaFile` handles should not be shared between threads
- `PravahaFilesystem` handles can be safely shared between threads

## Memory Management

- Caller must free filesystem handles with `pravaha_filesystem_free()`
- Caller must free file handles with `pravaha_file_close()`
- Error strings are managed internally and should not be freed
- NULL handles are safe to pass to cleanup functions