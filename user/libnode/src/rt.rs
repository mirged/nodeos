//! Freestanding runtime shims for userspace.
//!
//! # Why these exist
//!
//! NodeOS builds userspace exactly the way it builds the kernel: with a stable
//! toolchain against the precompiled `core`/`compiler_builtins` for
//! `x86_64-unknown-linux-gnu`. On that target `compiler_builtins` omits its
//! `mem` feature, because a hosted program gets `memcpy` and friends from libc.
//! There is no libc here, so the symbols LLVM emits calls to come from this
//! module.

use core::ffi::c_void;

/// `memcpy(dest, src, n)`
///
/// # Safety
/// Standard C contract: both regions must be valid for `n` bytes and must not
/// overlap.
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // Copy 8 bytes at a time, then the tail. `rep movsb` would be faster on
    // ERMSB parts but is not always available.
    let mut i = 0usize;
    // SAFETY: the caller guarantees `n` valid, non-overlapping bytes.
    unsafe {
        while i + 8 <= n {
            (dest.add(i) as *mut u64).write_unaligned((src.add(i) as *const u64).read_unaligned());
            i += 8;
        }
        while i < n {
            *dest.add(i) = *src.add(i);
            i += 1;
        }
    }
    dest
}

/// `memmove(dest, src, n)`
///
/// # Safety
/// Standard C contract; the regions may overlap.
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // SAFETY: caller guarantees both regions are valid for `n` bytes.
    unsafe {
        if (dest as usize) < (src as usize) || (dest as usize) >= (src as usize) + n {
            memcpy(dest, src, n);
        } else {
            let mut i = n;
            while i != 0 {
                i -= 1;
                *dest.add(i) = *src.add(i);
            }
        }
    }
    dest
}

/// `memset(dest, c, n)`
///
/// # Safety
/// `dest` must be valid for writes of `n` bytes.
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    let byte = c as u8;
    let word = u64::from_ne_bytes([byte; 8]);
    let mut i = 0usize;
    // SAFETY: caller guarantees `n` writable bytes.
    unsafe {
        while i + 8 <= n {
            (dest.add(i) as *mut u64).write_unaligned(word);
            i += 8;
        }
        while i < n {
            *dest.add(i) = byte;
            i += 1;
        }
    }
    dest
}

/// `memcmp(a, b, n)`
///
/// # Safety
/// Both pointers must be valid for reads of `n` bytes.
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0usize;
    // SAFETY: caller guarantees `n` readable bytes in both regions.
    unsafe {
        while i < n {
            let (x, y) = (*a.add(i), *b.add(i));
            if x != y {
                return x as i32 - y as i32;
            }
            i += 1;
        }
    }
    0
}

/// `bcmp(a, b, n)` -- LLVM emits this for equality-only comparisons.
///
/// # Safety
/// See [`memcmp`].
#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    // SAFETY: forwarded contract.
    unsafe { memcmp(a, b, n) }
}

/// `strlen(s)` -- referenced by some `core::ffi::CStr` paths.
///
/// # Safety
/// `s` must point to a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0usize;
    // SAFETY: caller guarantees a NUL terminator.
    unsafe {
        while *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}

/// Placeholder for the C `free` some LLVM lowerings reference. Reaching it is a
/// bug: nothing here allocates through a C allocator.
///
/// # Safety
/// Never call this.
#[no_mangle]
pub unsafe extern "C" fn free(_ptr: *mut c_void) {
    panic!("libnode called libc free()");
}
