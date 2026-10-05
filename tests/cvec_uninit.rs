//! A buffer from a non-zeroing allocator is `CVec<MaybeUninit<T>, _>` until it
//! is filled; `assume_init` then yields `CVec<T, _>` over the same allocation,
//! freed once by the same policy with the same byte length.

#![allow(missing_docs)]

use core::mem::MaybeUninit;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::{alloc, dealloc, Layout};

use ffibox::{CLenDrop, CVec};

/// Frees a `u32`-aligned `std::alloc` buffer, recording the byte length it was
/// handed so the test can check `assume_init` kept it.
#[derive(Clone, Copy)]
struct StdFree(&'static AtomicUsize);

// SAFETY: frees exactly the `std::alloc` allocation `alloc_u32s` made, whose
// size is the byte length the owner passes back.
unsafe impl CLenDrop for StdFree {
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        self.0.store(byte_len, Ordering::SeqCst);
        // SAFETY: allocated by `alloc_u32s` with this layout.
        unsafe { dealloc(ptr, Layout::from_size_align(byte_len, 4).unwrap()) }
    }
}

/// Stands in for `malloc`: `n` uninitialized `u32`s.
fn alloc_u32s(n: usize, freed: &'static AtomicUsize) -> CVec<MaybeUninit<u32>, StdFree> {
    let layout = Layout::array::<u32>(n).unwrap();
    // SAFETY: `n > 0` in these tests, so the layout is non-zero-sized.
    let p = unsafe { alloc(layout) }.cast::<MaybeUninit<u32>>();
    // SAFETY: `n` elements from `std::alloc`, which `StdFree` releases;
    // `MaybeUninit` needs no initialization.
    unsafe { CVec::from_raw_parts_with(p, n, StdFree(freed)) }.unwrap()
}

#[test]
fn a_filled_buffer_becomes_initialized_elements() {
    static FREED: AtomicUsize = AtomicUsize::new(0);
    let mut buf = alloc_u32s(4, &FREED);
    let base = buf.as_ptr().cast::<u32>();
    for (i, slot) in buf.as_mut_slice().iter_mut().enumerate() {
        slot.write(i as u32 * 10);
    }
    // SAFETY: every element was written above.
    let buf: CVec<u32, StdFree> = unsafe { buf.assume_init() };
    assert_eq!(buf.as_slice(), &[0, 10, 20, 30]);
    assert_eq!(buf.as_ptr(), base, "same allocation");
    assert_eq!(buf.byte_len(), 16);
    drop(buf);
    assert_eq!(
        FREED.load(Ordering::SeqCst),
        16,
        "freed once, same byte length"
    );
}

#[test]
fn an_empty_null_buffer_stays_empty() {
    static FREED: AtomicUsize = AtomicUsize::new(usize::MAX);
    // SAFETY: NULL with length 0 owns nothing.
    let empty = unsafe {
        CVec::<MaybeUninit<u32>, _>::from_raw_parts_or_empty_with(null_mut(), 0, StdFree(&FREED))
    }
    .unwrap();
    // SAFETY: there are no elements to initialize.
    let empty: CVec<u32, StdFree> = unsafe { empty.assume_init() };
    assert!(empty.as_ptr().is_null() && empty.is_empty());
    drop(empty);
    assert_eq!(FREED.load(Ordering::SeqCst), usize::MAX, "nothing freed");
}
