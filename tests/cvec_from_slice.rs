//! `CVec::from_slice` and `CSlice::to_cvec`: copying memory the buffer does not
//! own — a Rust slice, or a run inside a C object — into the policy's allocator.

#![allow(missing_docs)]

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use ffibox::{define_ctype, impl_clenclone, impl_clendrop, CLenClone, CLenDrop, CSlice, CVec};

const ALIGN: usize = 16;

/// Mock `memdup` over Rust's allocator, aligned to `ALIGN`; fails on a
/// `FAIL_LEN`-byte request so the `None` path is reachable.
unsafe fn mock_memdup(ptr: *mut u8, byte_len: usize) -> *mut u8 {
    if byte_len == FAIL_LEN {
        return core::ptr::null_mut();
    }
    COPIES.fetch_add(1, Ordering::SeqCst);
    let layout = std::alloc::Layout::from_size_align(byte_len, ALIGN).expect("valid layout");
    // SAFETY: callers never request zero bytes here (empty copies skip C).
    let copy = unsafe { std::alloc::alloc(layout) };
    if !copy.is_null() {
        // SAFETY: `byte_len` readable source bytes, a fresh non-overlapping copy.
        unsafe { core::ptr::copy_nonoverlapping(ptr, copy, byte_len) };
    }
    copy
}

/// # Safety
///
/// `ptr` must come from `mock_memdup` with this `byte_len`.
unsafe fn mock_free(ptr: *mut u8, byte_len: usize) {
    let layout = std::alloc::Layout::from_size_align(byte_len, ALIGN).expect("valid layout");
    // SAFETY: allocated by `mock_memdup` with this layout.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

const FAIL_LEN: usize = 12345;
static COPIES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default)]
struct Aligned;
impl_clendrop!(Aligned, mock_free);
impl_clenclone!(Aligned, mock_memdup, align = 16);

#[derive(Clone, Copy, Debug, Default)]
struct Bytewise;
impl_clendrop!(Bytewise, mock_free);
impl_clenclone!(Bytewise, mock_memdup);

#[test]
fn the_macro_records_the_policy_alignment() {
    assert_eq!(<Aligned as CLenClone>::ALIGN, 16);
    assert_eq!(<Bytewise as CLenClone>::ALIGN, 1);
}

#[test]
fn from_slice_copies_a_rust_slice_independently() {
    let mut src = vec![1u32, 2, 3, 4];
    let v = CVec::<u32, Aligned>::from_slice(&src).unwrap();
    src[0] = 99;
    assert_eq!(v.as_slice(), &[1, 2, 3, 4]);
    assert_eq!(v.byte_len(), 16);
    assert_eq!(v.as_ptr() as usize % ALIGN, 0);
    assert_ne!(v.as_ptr().cast_const(), src.as_ptr());
}

#[test]
fn byte_aligned_policies_still_copy_bytes() {
    let v = CVec::<u8, Bytewise>::from_slice(b"lavu").unwrap();
    assert_eq!(v.as_slice(), b"lavu");
}

#[test]
fn an_empty_slice_is_an_empty_buffer_without_c() {
    let before = COPIES.load(Ordering::SeqCst);
    let v = CVec::<u64, Aligned>::from_slice(&[]).unwrap();
    assert!(v.is_empty() && v.as_ptr().is_null());
    assert_eq!(COPIES.load(Ordering::SeqCst), before);
}

#[test]
fn a_failed_copy_is_none() {
    let src = vec![0u8; FAIL_LEN];
    assert!(CVec::<u8, Aligned>::from_slice_with(&src, Aligned).is_none());
}

#[test]
fn to_cvec_copies_a_borrowed_run_out_of_c_memory() {
    // A C object's array, borrowed as a run the library still points at.
    let mut table = [7u16, 8, 9];
    let c_ptr = table.as_mut_ptr();
    // SAFETY: `table` holds three initialised `u16` that outlive the view.
    let run = unsafe { CSlice::from_raw_parts(NonNull::new(c_ptr).unwrap(), 3) };
    let v: CVec<u16, Aligned> = run.to_cvec().unwrap();
    // C writes through the pointer it kept; the copy is unaffected.
    // SAFETY: in bounds, and no reference covers `table`.
    unsafe { c_ptr.add(1).write(0) };
    assert_eq!(v.as_slice(), &[7, 8, 9]);

    let tail = run.slice(1..).unwrap();
    assert_eq!(tail.to_cvec_with(Aligned).unwrap().as_slice(), &[0, 9]);
    assert!(run
        .slice(3..)
        .unwrap()
        .to_cvec::<Aligned>()
        .unwrap()
        .is_empty());
}

/// A C struct that owns nothing: a wrapped C object whose bytes are the
/// whole value, so a run of them copies as a run of integers does.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rational_st {
    pub num: i32,
    pub den: i32,
}

define_ctype!(
    #[derive(Clone, Copy)]
    Rational,
    RationalRef,
    RationalMut,
    rational_st
);

impl RationalRef<'_> {
    fn parts(&self) -> (i32, i32) {
        // SAFETY: a read of the live, initialised `rational_st` the handle
        // addresses.
        unsafe { ((*self.as_ptr()).num, (*self.as_ptr()).den) }
    }
}

#[test]
fn to_cvec_copies_a_borrowed_run_of_wrapped_copy_objects() {
    // A C object's array of structs (a codec's supported frame rates), seen
    // through per-element handles because `&[Rational]` would cover C memory.
    let mut table = [
        rational_st {
            num: 24000,
            den: 1001,
        },
        rational_st { num: 25, den: 1 },
        rational_st { num: 0, den: 0 },
    ];
    let c_ptr = table.as_mut_ptr();
    // SAFETY: `table` holds three initialised structs, which `Rational`
    // mirrors transparently, outliving the view.
    let run = unsafe { CSlice::from_raw_parts(NonNull::new(c_ptr.cast::<Rational>()).unwrap(), 3) };
    let v: CVec<Rational, Aligned> = run.to_cvec().unwrap();
    // C writes through the pointer it kept; the copy is unaffected.
    // SAFETY: in bounds, and no reference covers `table`.
    unsafe { (*c_ptr.add(0)).num = 30000 };
    let parts: Vec<(i32, i32)> = v.as_handles().iter().map(|r| r.parts()).collect();
    assert_eq!(parts, [(24000, 1001), (25, 1), (0, 0)]);
    assert_eq!(run.get(0).unwrap().parts(), (30000, 1001));

    let tail: CVec<Rational, Aligned> = run.slice(1..).unwrap().to_cvec_with(Aligned).unwrap();
    assert_eq!(tail.as_handles().get(0).unwrap().parts(), (25, 1));
    assert!(run
        .slice(3..)
        .unwrap()
        .to_cvec::<Aligned>()
        .unwrap()
        .is_empty());
    // The copy clones as any `CVec` of `Copy` elements does.
    assert_eq!(v.clone().len(), 3);
}

#[test]
fn copies_cross_threads_with_their_policy() {
    let v = CVec::<u32, Aligned>::from_slice(&[5; 8]).unwrap();
    let sum = std::thread::spawn(move || v.as_slice().iter().sum::<u32>())
        .join()
        .unwrap();
    assert_eq!(sum, 40);
}

// A hand-written policy relies on the `ALIGN = 1` default.
struct Plain;
// SAFETY: frees what `Plain::c_clone_len` returned, with its layout.
unsafe impl CLenDrop for Plain {
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        // SAFETY: as `mock_free`, with byte alignment.
        unsafe {
            std::alloc::dealloc(
                ptr,
                std::alloc::Layout::from_size_align(byte_len, 1).unwrap(),
            )
        };
    }
}
// SAFETY: reads the source only and returns a fresh byte-aligned copy.
unsafe impl CLenClone for Plain {
    unsafe fn c_clone_len(&self, ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>> {
        let layout = std::alloc::Layout::from_size_align(byte_len, 1).ok()?;
        // SAFETY: non-zero layout; the source is readable for `byte_len`.
        let copy = NonNull::new(unsafe { std::alloc::alloc(layout) })?;
        // SAFETY: as above.
        unsafe { core::ptr::copy_nonoverlapping(ptr, copy.as_ptr(), byte_len) };
        Some(copy)
    }
}

#[test]
fn the_default_alignment_is_one_byte() {
    assert_eq!(<Plain as CLenClone>::ALIGN, 1);
    assert_eq!(
        CVec::<u8, Plain>::from_slice_with(&[1, 2], Plain)
            .unwrap()
            .as_slice(),
        &[1, 2]
    );
}
