use libc::size_t;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
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
        }
    }
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<Option<CString>>  = const { std::cell::RefCell::new(None) };
}

fn set_last_error(err: &FsError) {
    let error_msg = CString::new(err.to_string())
        .unwrap_or_else(|_| CString::new("Failed to format error message").unwrap());
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = Some(error_msg);
    });
}

fn clear_last_error() {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = None;
    });
}

/// Opaque filesystem handle
pub struct PravahaFilesystem {
    inner: Box<dyn FileSystem>,
}

/// Opaque file handle
pub struct PravahaFile {
    inner: Box<dyn File>,
}

/// Get the last error message for this thread
/// Returns NULL if no error
/// The returned string is valid until the next pravaha call on this thread
#[unsafe(no_mangle)]
pub extern "C" fn pravaha_last_error() -> *const c_char {
    LAST_ERROR.with(|e| {
        e.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}

/// Create a filesystem for the given URL
/// Returns NULL on error
///
/// # Safety
/// >> url must be a valid null-terminated C string
/// >> Caller must free the returned pointer with pravaha_filesystem_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_create(url: *const c_char) -> *mut PravahaFilesystem {
    clear_last_error();

    if url.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "URL is null",
        )));
        return ptr::null_mut();
    }

    let url_str = unsafe {
        match CStr::from_ptr(url).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(&FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid UTF-8 in URL",
                )));
                return ptr::null_mut();
            }
        }
    };

    match crate::create(url_str) {
        Ok(fs) => Box::into_raw(Box::new(PravahaFilesystem { inner: fs })),
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
    }
}

/// Open a file
/// Returns NULL on error
///
/// # Safety
///  fs must be a valid filesystem handle
/// >> path must be a valid null-terminated C string
/// >> mode must be a valid null-terminated C string
/// >> Caller must free the returned pointer with pravaha_file_close()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_open(
    fs: *mut PravahaFilesystem,
    path: *const c_char,
    mode: *const c_char,
) -> *mut PravahaFile {
    clear_last_error();

    if fs.is_null() || path.is_null() || mode.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null pointer argument",
        )));
        return ptr::null_mut();
    }

    let fs_ref = unsafe { &*fs };

    let path_str = unsafe {
        match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(&FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid UTF-8 in path",
                )));
                return ptr::null_mut();
            }
        }
    };

    let mode_str = unsafe {
        match CStr::from_ptr(mode).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(&FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid UTF-8 in mode",
                )));
                return ptr::null_mut();
            }
        }
    };

    match fs_ref.inner.open(path_str, mode_str) {
        Ok(file) => Box::into_raw(Box::new(PravahaFile { inner: file })),
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
    }
}

/// Read up to size bytes from file into buffer
/// Returns number of bytes read, or -1 on error
///
/// # Safety
/// >> file must be a valid file handle
/// >> buffer must be valid for writes of at least size bytes
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_read(
    file: *mut PravahaFile,
    buffer: *mut c_void,
    size: size_t,
) -> isize {
    clear_last_error();

    if file.is_null() || buffer.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null pointer argument",
        )));
        return -1;
    }

    let file_ref = unsafe { &mut *file };
    let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, size) };

    match file_ref.inner.read(buf) {
        Ok(n) => n as isize,
        Err(e) => {
            set_last_error(&e);
            -1
        }
    }
}

/// Seek to absolute position in file
/// Returns 0 on success, error code on failure
///
/// # Safety
/// >> file must be a valid file handle
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_seek(file: *mut PravahaFile, pos: u64) -> c_int {
    clear_last_error();

    if file.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null file pointer",
        )));
        return PravahaErrorCode::PRAVAHA_INVALID_ARGUMENT as c_int;
    }

    let file_ref = unsafe { &mut *file };

    match file_ref.inner.seek(pos) {
        Ok(_) => PravahaErrorCode::PRAVAHA_SUCCESS as c_int,
        Err(e) => {
            let code = PravahaErrorCode::from(&e);
            set_last_error(&e);
            code as c_int
        }
    }
}

/// Get current position in file
/// Returns current position, or 0 if file is invalid
///
/// # Safety
/// >> file must be a valid file handle
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_tell(file: *const PravahaFile) -> u64 {
    clear_last_error();

    if file.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null file pointer",
        )));
        return 0;
    }

    let file_ref = unsafe { &*file };
    file_ref.inner.tell()
}

/// Get file size if available
/// Returns size, or 0 if not available
/// Sets has_size to 1 if size is available, 0 otherwise
///
/// # Safety
/// >> file must be a valid file handle
/// >> has_size must be valid for writes
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_size(file: *const PravahaFile, has_size: *mut c_int) -> u64 {
    clear_last_error();

    if file.is_null() || has_size.is_null() {
        if !has_size.is_null() {
            unsafe { *has_size = 0 };
        }
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null pointer argument",
        )));
        return 0;
    }

    let file_ref = unsafe { &*file };

    match file_ref.inner.size() {
        Some(size) => {
            unsafe { *has_size = 1 };
            size
        }
        None => {
            unsafe { *has_size = 0 };
            0
        }
    }
}

/// Check if at end of file
/// Returns 1 if EOF, 0 otherwise
///
/// # Safety
/// >> file must be a valid file handle
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_eof(file: *const PravahaFile) -> c_int {
    clear_last_error();

    if file.is_null() {
        return 0;
    }

    let file_ref = unsafe { &*file };
    if file_ref.inner.eof() { 1 } else { 0 }
}

/// Close a file and free its resources
///
/// # Safety
/// >> file must be a valid file handle or NULL
/// >> file must not be used after this call
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_file_close(file: *mut PravahaFile) {
    if !file.is_null() {
        let mut file_box = unsafe { Box::from_raw(file) };
        file_box.inner.close();
    }
}

/// Free a filesystem handle
///
/// # Safety
/// >> fs must be a valid filesystem handle or NULL
/// >> fs must not be used after this call
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_filesystem_free(fs: *mut PravahaFilesystem) {
    if !fs.is_null() {
        let _ = unsafe { Box::from_raw(fs) };
    }
}

/// Get library version string
/// Returns pointer to static version string
#[unsafe(no_mangle)]
pub extern "C" fn pravaha_version() -> *const c_char {
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    VERSION.as_ptr() as *const c_char
}

/// Open a file directly without creating a filesystem handle
/// Returns NULL on error
///
/// # Safety
/// >> url must be a valid null-terminated C string
/// >> mode must be a valid null-terminated C string
/// >> Caller must free the returned pointer with pravaha_file_close()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pravaha_open_url(
    url: *const c_char,
    mode: *const c_char,
) -> *mut PravahaFile {
    clear_last_error();

    if url.is_null() || mode.is_null() {
        set_last_error(&FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Null pointer argument",
        )));
        return ptr::null_mut();
    }

    let url_str = unsafe {
        match CStr::from_ptr(url).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(&FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid UTF-8 in URL",
                )));
                return ptr::null_mut();
            }
        }
    };

    let mode_str = unsafe {
        match CStr::from_ptr(mode).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(&FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid UTF-8 in mode",
                )));
                return ptr::null_mut();
            }
        }
    };

    match crate::open(url_str, mode_str) {
        Ok(file) => Box::into_raw(Box::new(PravahaFile { inner: file })),
        Err(e) => {
            set_last_error(&e);
            ptr::null_mut()
        }
    }
}
