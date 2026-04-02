use libc::{size_t, ssize_t};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic;
use std::ptr;
use std::slice;

use crate::{File, FileSystem, FsError};

#[repr(C)]
#[allow(non_camel_case_types)]
pub enum PravahaErrorCode {
    PRAVAHA_SUCCESS = 0,
    PRAVAHA_NETWORK = 1,
    PRAVAHA_PROTOCOL = 2,
    PRAVAHA_IO = 3,
    PRAVAHA_FILE_CLOSED = 4,
    PRAVAHA_UNSUPPORTED_PROTOCOL = 5,
    PRAVAHA_INVALID_ARGUMENT = 6,
    PRAVAHA_RATE_LIMITED = 7,
    PRAVAHA_PANIC = 8,
    PRAVAHA_UNKNOWN = 99,
}

impl From<&FsError> for PravahaErrorCode {
    fn from(err: &FsError) -> Self {
        match err {
            FsError::Network(_) => PravahaErrorCode::PRAVAHA_NETWORK,
            FsError::Protocol(_) => PravahaErrorCode::PRAVAHA_PROTOCOL,
            FsError::Io(_) => PravahaErrorCode::PRAVAHA_IO,
            FsError::FileClosed => PravahaErrorCode::PRAVAHA_FILE_CLOSED,
            FsError::UnsupportedProtocol(_) => PravahaErrorCode::PRAVAHA_UNSUPPORTED_PROTOCOL,
            FsError::RateLimited { .. } => PravahaErrorCode::PRAVAHA_RATE_LIMITED,
        }
    }
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<Option<CString>> =
        const { std::cell::RefCell::new(None) };
}

fn set_last_error(err: &FsError) {
    let msg = CString::new(err.to_string())
        .unwrap_or_else(|_| CString::new("Failed to format error message").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg));
}

fn set_last_error_str(msg: &str) {
    let cs = CString::new(msg)
        .unwrap_or_else(|_| CString::new("Failed to format error message").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(cs));
}

fn clear_last_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

//
// Every extern "C" body is wrapped in this so that a Rust panic never
// unwinds across the FFI boundary (which is undefined behaviour).
// On panic the error is recorded and the supplied fallback value is returned.

fn ffi_catch<F, T>(fallback: T, f: F) -> T
where
    F: FnOnce() -> T + panic::UnwindSafe,
{
    match panic::catch_unwind(f) {
        Ok(v) => v,
        Err(_) => {
            set_last_error_str("internal panic — please report this bug");
            fallback
        }
    }
}

/// Opaque filesystem handle.
pub struct PravahaFilesystem {
    inner: Box<dyn FileSystem>,
}

/// Opaque file handle.
pub struct PravahaFile {
    inner: Box<dyn File>,
}

/// Get the last error message for this thread.
/// Returns NULL if no error has occurred.
/// The pointer is valid until the next pravaha call on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn pravaha_last_error() -> *const c_char {
    LAST_ERROR.with(|e| {
        e.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}

/// Create a filesystem handle for the given base URL.
/// Returns NULL on error; call `pravaha_last_error()` for details.
///
/// # Safety
/// - `url` must be a valid null-terminated UTF-8 C string.
/// - Caller must free with `pravaha_filesystem_free()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_create(url: *const c_char) -> *mut PravahaFilesystem {
    clear_last_error();

    if url.is_null() {
        set_last_error_str("URL is null");
        return ptr::null_mut();
    }

    let url_str = match unsafe { CStr::from_ptr(url) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error_str("Invalid UTF-8 in URL");
            return ptr::null_mut();
        }
    };

    // Capture url_str as owned string so the closure is UnwindSafe.
    let url_owned = url_str.to_owned();
    ffi_catch(ptr::null_mut(), move || match crate::create(&url_owned) {
        Ok(fs) => Box::into_raw(Box::new(PravahaFilesystem { inner: fs })),
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
    })
}

/// Free a filesystem handle.
///
/// # Safety
/// - `fs` must be a valid handle or NULL.
/// - Must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_filesystem_free(fs: *mut PravahaFilesystem) {
    if !fs.is_null() {
        ffi_catch((), move || drop(unsafe { Box::from_raw(fs) }));
    }
}

/// Open a file via an existing filesystem handle.
/// Returns NULL on error; call `pravaha_last_error()` for details.
///
/// # Safety
/// - `fs` must be a valid filesystem handle.
/// - `path` and `mode` must be valid null-terminated UTF-8 C strings.
/// - Caller must free with `pravaha_file_close()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_open(
    fs: *mut PravahaFilesystem,
    path: *const c_char,
    mode: *const c_char,
) -> *mut PravahaFile {
    clear_last_error();

    if fs.is_null() || path.is_null() || mode.is_null() {
        set_last_error_str("Null pointer argument");
        return ptr::null_mut();
    }

    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error_str("Invalid UTF-8 in path");
            return ptr::null_mut();
        }
    };
    let mode_str = match unsafe { CStr::from_ptr(mode) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error_str("Invalid UTF-8 in mode");
            return ptr::null_mut();
        }
    };

    let path_owned = path_str.to_owned();
    let mode_owned = mode_str.to_owned();

    ffi_catch(ptr::null_mut(), move || {
        match unsafe { &*fs }.inner.open(&path_owned, &mode_owned) {
            Ok(file) => Box::into_raw(Box::new(PravahaFile { inner: file })),
            Err(e) => {
                set_last_error(&e);
                ptr::null_mut()
            }
        }
    })
}

/// Open a file directly from a URL without a separate filesystem handle.
/// Returns NULL on error; call `pravaha_last_error()` for details.
///
/// # Safety
/// - `url` and `mode` must be valid null-terminated UTF-8 C strings.
/// - Caller must free with `pravaha_file_close()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_open_url(
    url: *const c_char,
    mode: *const c_char,
) -> *mut PravahaFile {
    clear_last_error();

    if url.is_null() || mode.is_null() {
        set_last_error_str("Null pointer argument");
        return ptr::null_mut();
    }

    let url_str = match unsafe { CStr::from_ptr(url) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error_str("Invalid UTF-8 in URL");
            return ptr::null_mut();
        }
    };
    let mode_str = match unsafe { CStr::from_ptr(mode) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error_str("Invalid UTF-8 in mode");
            return ptr::null_mut();
        }
    };

    let url_owned = url_str.to_owned();
    let mode_owned = mode_str.to_owned();

    ffi_catch(ptr::null_mut(), move || {
        match crate::open(&url_owned, &mode_owned) {
            Ok(file) => Box::into_raw(Box::new(PravahaFile { inner: file })),
            Err(e) => {
                set_last_error(&e);
                ptr::null_mut()
            }
        }
    })
}

/// Close a file and free its resources.
///
/// # Safety
/// - `file` must be a valid handle or NULL.
/// - Must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_file_close(file: *mut PravahaFile) {
    if !file.is_null() {
        ffi_catch((), move || drop(unsafe { Box::from_raw(file) }));
    }
}

/// Read up to `size` bytes into `buffer`.
/// Returns bytes read (0 = EOF), or -1 on error.
///
/// # Safety
/// - `file` must be a valid file handle.
/// - `buffer` must be valid for writes of at least `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_read(
    file: *mut PravahaFile,
    buffer: *mut c_void,
    size: size_t,
) -> ssize_t {
    clear_last_error();

    if file.is_null() || buffer.is_null() {
        set_last_error_str("Null pointer argument");
        return -1;
    }

    // caller guarantees buffer is valid for `size` bytes.
    let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, size) };

    ffi_catch(-1, move || match unsafe { &mut *file }.inner.read(buf) {
        Ok(n) => n as ssize_t,
        Err(e) => {
            set_last_error(&e);
            -1
        }
    })
}

/// Seek to an absolute byte position.
/// Returns `PRAVAHA_SUCCESS` (0) on success, or an error code on failure.
///
/// # Safety
/// - `file` must be a valid file handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_seek(file: *mut PravahaFile, pos: u64) -> c_int {
    clear_last_error();

    if file.is_null() {
        set_last_error_str("Null file pointer");
        return PravahaErrorCode::PRAVAHA_INVALID_ARGUMENT as c_int;
    }

    ffi_catch(
        PravahaErrorCode::PRAVAHA_PANIC as c_int,
        move || match unsafe { &mut *file }.inner.seek(pos) {
            Ok(_) => PravahaErrorCode::PRAVAHA_SUCCESS as c_int,
            Err(e) => {
                let c = PravahaErrorCode::from(&e) as c_int;
                set_last_error(&e);
                c
            }
        },
    )
}

/// Get the current byte position in the file.
/// Writes position into `*out_pos`. Returns `PRAVAHA_SUCCESS` or an error code.
///
/// # Safety
/// - `file` must be a valid file handle.
/// - `out_pos` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_tell(file: *const PravahaFile, out_pos: *mut u64) -> c_int {
    clear_last_error();

    if file.is_null() || out_pos.is_null() {
        set_last_error_str("Null pointer argument");
        return PravahaErrorCode::PRAVAHA_INVALID_ARGUMENT as c_int;
    }

    ffi_catch(PravahaErrorCode::PRAVAHA_PANIC as c_int, move || {
        unsafe { *out_pos = (*file).inner.tell() };
        PravahaErrorCode::PRAVAHA_SUCCESS as c_int
    })
}

/// Get the file size if available.
/// Sets `*has_size` to 1 and writes size into `*out_size` when known,
/// or sets `*has_size` to 0 when Content-Length was not provided.
/// Returns `PRAVAHA_SUCCESS` or an error code.
///
/// # Safety
/// - `file` must be a valid file handle.
/// - `out_size` and `has_size` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_size(
    file: *const PravahaFile,
    out_size: *mut u64,
    has_size: *mut c_int,
) -> c_int {
    clear_last_error();

    if file.is_null() || out_size.is_null() || has_size.is_null() {
        set_last_error_str("Null pointer argument");
        return PravahaErrorCode::PRAVAHA_INVALID_ARGUMENT as c_int;
    }

    ffi_catch(PravahaErrorCode::PRAVAHA_PANIC as c_int, move || {
        match unsafe { &*file }.inner.size() {
            Some(sz) => unsafe {
                *out_size = sz;
                *has_size = 1
            },
            None => unsafe {
                *out_size = 0;
                *has_size = 0
            },
        }
        PravahaErrorCode::PRAVAHA_SUCCESS as c_int
    })
}

/// Check if the file position is at EOF.
/// Returns 1 if EOF, 0 otherwise.
/// Sets the last error if `file` is NULL.
///
/// # Safety
/// - `file` must be a valid file handle or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_eof(file: *const PravahaFile) -> c_int {
    clear_last_error();

    if file.is_null() {
        set_last_error_str("Null file pointer");
        return 0;
    }

    ffi_catch(0, move || if unsafe { &*file }.inner.eof() { 1 } else { 0 })
}

/// Returns a pointer to a static null-terminated version string.
#[unsafe(no_mangle)]
pub extern "C" fn pravaha_version() -> *const c_char {
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION.as_ptr() as *const c_char
}
