//! Regression coverage for `CVec`'s byte-copy clone contract.

#![allow(missing_docs)]

use core::ptr::NonNull;

use ffibox::{CLenCloned, CLenDropped, CVec};

struct Memdup;

// SAFETY: every buffer in this test is allocated with the global allocator
// using the exact byte length passed back to this strategy.
unsafe impl CLenDropped for Memdup {
    unsafe fn c_drop_len(ptr: *mut u8, byte_len: usize) {
        let layout = std::alloc::Layout::from_size_align(byte_len, 1).expect("valid layout");
        // SAFETY: `ptr` was allocated with this exact layout.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

// SAFETY: this allocates a distinct buffer with the same layout, copies every
// source byte, and leaves the source allocation untouched.
unsafe impl CLenCloned for Memdup {
    unsafe fn c_clone_len(ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>> {
        let layout = std::alloc::Layout::from_size_align(byte_len, 1).ok()?;
        // SAFETY: `layout` is non-zero in this test.
        let copy = NonNull::new(unsafe { std::alloc::alloc(layout) })?;
        // SAFETY: both allocations hold `byte_len` live, non-overlapping bytes.
        unsafe { core::ptr::copy_nonoverlapping(ptr, copy.as_ptr(), byte_len) };
        Some(copy)
    }
}

fn make_bytes(bytes: &[u8]) -> CVec<u8, Memdup> {
    let layout = std::alloc::Layout::from_size_align(bytes.len(), 1).expect("valid layout");
    // SAFETY: the test input is non-empty, so the layout is non-zero.
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    // SAFETY: `ptr` holds `bytes.len()` writable bytes and cannot overlap the
    // borrowed input slice.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
    // SAFETY: `ptr` owns `bytes.len()` initialized bytes allocated for Memdup.
    unsafe { CVec::from_raw_parts(ptr, bytes.len()) }.expect("non-null allocation")
}

#[test]
fn copy_elements_clone_into_an_independent_allocation() {
    let original = make_bytes(&[1, 2, 3, 4]);
    let mut cloned = original.try_clone().expect("memdup succeeds");

    assert_ne!(original.as_ptr(), cloned.as_ptr());
    assert_eq!(original.as_slice(), cloned.as_slice());

    cloned.as_mut_slice()[0] = 9;
    assert_eq!(original.as_slice(), &[1, 2, 3, 4]);
    assert_eq!(cloned.as_slice(), &[9, 2, 3, 4]);
}
