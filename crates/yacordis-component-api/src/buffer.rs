//! Boundary buffer allocation.
//!
//! Buffers use the process's `malloc`/`free` (both binaries link the one
//! libc instance), so **either side may free a buffer regardless of
//! which side allocated it**. The only rule is: free exactly once.
//! Component closure state, by contrast, is ordinary Rust heap memory and
//! stays owned by the side that created it (hence `Callback::drop_fn`).

use std::os::raw::c_void;

use crate::abi::RawBuffer;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// Allocate a `len`-byte boundary buffer (never null; 0 allocates 1 byte).
///
/// # Panics
/// Panics if the system allocator returns null.
#[must_use]
pub fn alloc_buffer(len: usize) -> *mut u8 {
    // SAFETY: `malloc` with a non-zero size; null-checked below.
    let ptr = unsafe { malloc(len.max(1)) }.cast::<u8>();
    if ptr.is_null() {
        panic!("yacordis: boundary buffer allocation failed");
    }
    ptr
}

/// Free a boundary buffer (from either side). Null pointers are ignored,
/// so `RawBuffer::EMPTY` is safe to pass.
pub fn free_buffer(buffer: RawBuffer) {
    if buffer.ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` came from `malloc` in this module and is freed once.
    unsafe { free(buffer.ptr.cast::<c_void>()) };
}

/// Copy `data` into a freshly allocated boundary buffer.
#[must_use]
pub fn buffer_from_vec(data: Vec<u8>) -> RawBuffer {
    let len = data.len();
    let ptr = alloc_buffer(len);
    // SAFETY: `ptr` is valid for `len` bytes and `data` is a local Vec.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, len);
    }
    RawBuffer { ptr, len }
}

/// Copy a boundary buffer into an owned `Vec`. Does **not** free the
/// buffer; the consumer keeps ownership.
#[must_use]
pub fn buffer_into_vec(buffer: RawBuffer) -> Vec<u8> {
    if buffer.ptr.is_null() || buffer.len == 0 {
        return Vec::new();
    }
    // SAFETY: the producer contract guarantees `len` readable bytes.
    unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len).to_vec() }
}

/// Read a boundary buffer as UTF-8, replacing invalid sequences.
#[must_use]
pub fn buffer_to_string(buffer: RawBuffer) -> String {
    String::from_utf8_lossy(&buffer_into_vec(buffer)).into_owned()
}
