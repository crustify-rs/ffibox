//! [`RustAlloc`]: a policy over Rust's global allocator.
//!
//! The policies elsewhere bind a foreign allocator's routines. This one binds
//! Rust's, so an object or buffer Rust allocates is held by the same owners,
//! with the same handles, as one C allocated: `CBox<Foo, RustAlloc>` beside
//! `CBox<Foo, FooFree>`. Two uses follow:
//!
//! - **Outbound.** A value Rust allocates and hands to C keeps a stable
//!   address and a paired release, whether Rust or C (through an export)
//!   frees it.
//! - **Migration.** When a type's allocation moves from C to Rust during a
//!   port, its owner's alias changes from `CBox<Foo, FooFree>` to
//!   `CBox<Foo, RustAlloc>` and its consumers do not change.
//!
//! Memory from `RustAlloc` is freed by `RustAlloc` alone: handing it to C's
//! `free` is a mismatched release, which the policy in the owner's type
//! rules out on the Rust side, and an export has to rule out on C's.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use core::ffi::{CStr, c_char};
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::traits::{
    CAlloc, CAllocZeroed, CCell, CDrop, CDupClone, CLenAlloc, CLenClone, CLenDrop, CNew, CZeroable,
};

/// The policy over Rust's global allocator: [`alloc`](alloc::alloc::alloc)
/// and [`dealloc`](alloc::alloc::dealloc()) as a lifecycle policy, the Rust
/// counterpart of a `malloc` / `free` policy.
///
/// | Owner | Through |
/// |-------|---------|
/// | `CBox<Foo, RustAlloc>` | [`CBox::new`](crate::CBox::new) / [`new_zeroed`](crate::CBox::new_zeroed) for a [`CZeroable`] `Foo`, [`CBox::new_uninit`](crate::CBox::new_uninit) otherwise |
/// | `CArc<Foo, RustAlloc>` | a `CBox` shared with `CArc::from`; no up_ref, so not `Clone` |
/// | `CVec<T, RustAlloc>` | [`CVec::from_fn`](crate::CVec::from_fn), [`CVec::new_uninit`](crate::CVec::new_uninit), [`CVec::from_slice`](crate::CVec::from_slice) |
/// | `CStrBox<RustAlloc>` | [`CStrBox::from_c_str`](crate::CStrBox::from_c_str) |
///
/// Allocation failure is a `None`, never an abort. Objects are allocated
/// with their own layout; buffers and strings with an alignment of
/// [`BUF_ALIGN`](Self::BUF_ALIGN) and 1, which the frees repeat.
///
/// ```
/// use ffibox::{CBox, CVec, CZeroable, RustAlloc, define_ctype};
/// # #[repr(C)] pub struct point_st { x: i32, y: i32 }
/// define_ctype!(Point, PointRef, PointMut, point_st);
/// // SAFETY: any coordinates are valid, zero included.
/// unsafe impl CZeroable for Point {}
///
/// let p: CBox<Point, RustAlloc> = CBox::new_zeroed().unwrap();
/// let v: CVec<u32, RustAlloc> = CVec::from_fn(4, |i| i as u32 * 10).unwrap();
/// assert_eq!(v.as_slice(), &[0, 10, 20, 30]);
/// # drop(p);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RustAlloc;

impl RustAlloc {
    /// The alignment of every buffer this policy allocates, whatever its
    /// element type, so that the free, which knows only the byte length,
    /// repeats the layout. Element types aligned beyond it are rejected at
    /// compile time.
    pub const BUF_ALIGN: usize = 16;

    fn buf_layout(byte_len: usize) -> Option<Layout> {
        Layout::from_size_align(byte_len, Self::BUF_ALIGN).ok()
    }

    /// One `T`'s layout, rejecting zero-sized `T` at compile time: the global
    /// allocator takes no zero-sized requests.
    const fn object_layout<T>() -> Layout {
        const {
            assert!(
                core::mem::size_of::<T>() != 0,
                "RustAlloc does not allocate zero-sized objects"
            );
        }
        Layout::new::<T>()
    }
}

// SAFETY: `c_drop` drops the `T` in place and deallocates it with the layout
// `c_alloc` / `c_alloc_zeroed` allocated it with, once.
unsafe impl<T: CCell> CDrop<T> for RustAlloc {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<T>) {
        // SAFETY: the caller owns a live `T` this policy allocated with
        // `Layout::new::<T>()`; it is dropped, then freed, once.
        unsafe {
            core::ptr::drop_in_place(ptr.as_ptr());
            dealloc(ptr.as_ptr().cast(), Self::object_layout::<T>());
        }
    }
}

// SAFETY: storage for a `T` that holds no `T` yet is freed without being
// read, with the layout it was allocated with.
unsafe impl<T: CCell> CDrop<MaybeUninit<T>> for RustAlloc {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<MaybeUninit<T>>) {
        // SAFETY: the caller owns storage this policy allocated with
        // `Layout::new::<T>()`, which `MaybeUninit<T>` shares.
        unsafe { dealloc(ptr.as_ptr().cast(), Self::object_layout::<T>()) }
    }
}

// SAFETY: a fresh allocation of `T`'s size and alignment, uniquely owned,
// freed by both `CDrop` impls above without the uninitialised one reading it.
unsafe impl<T: CCell> CAlloc<T> for RustAlloc {
    #[inline]
    fn c_alloc(&self) -> Option<NonNull<MaybeUninit<T>>> {
        // SAFETY: the layout is not zero-sized (`object_layout`).
        NonNull::new(unsafe { alloc(Self::object_layout::<T>()) }.cast())
    }
}

// SAFETY: a fresh, zero-filled allocation of one `T`, which `CZeroable`
// makes a valid `T`, released by `CDrop<T>` above.
unsafe impl<T: CZeroable> CAllocZeroed<T> for RustAlloc {
    #[inline]
    fn c_alloc_zeroed(&self) -> Option<NonNull<T>> {
        // SAFETY: the layout is not zero-sized (`object_layout`).
        NonNull::new(unsafe { alloc_zeroed(Self::object_layout::<T>()) }.cast())
    }
}

// SAFETY: a zeroed `T` is a fully-initialised one by `CZeroable`, released by
// `CDrop<T>` above; `None` is an allocation failure.
unsafe impl<T: CZeroable> CNew<T> for RustAlloc {
    #[inline]
    fn c_new(&self) -> Option<NonNull<T>> {
        self.c_alloc_zeroed()
    }
}

// SAFETY: a string this policy allocated holds `strlen + 1` bytes at
// alignment 1, which `c_drop` recomputes; the views a `CStrBox` gives are
// read-only, so the length cannot change while it owns the string.
unsafe impl CDrop<c_char> for RustAlloc {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<c_char>) {
        // SAFETY: the caller owns a live NUL-terminated string this policy
        // allocated, `strlen + 1` bytes at alignment 1.
        unsafe {
            let size = CStr::from_ptr(ptr.as_ptr()).to_bytes_with_nul().len();
            dealloc(
                ptr.as_ptr().cast(),
                Layout::from_size_align_unchecked(size, 1),
            );
        }
    }
}

// SAFETY: copies any live NUL-terminated string, reading it only, into a
// fresh `strlen + 1`-byte allocation at alignment 1, which `c_drop` frees.
unsafe impl CDupClone<c_char> for RustAlloc {
    #[inline]
    unsafe fn c_dup(&self, ptr: NonNull<c_char>) -> Option<NonNull<c_char>> {
        // SAFETY: the caller passes a live NUL-terminated string.
        let src = unsafe { CStr::from_ptr(ptr.as_ptr()) }.to_bytes_with_nul();
        let layout = Layout::from_size_align(src.len(), 1).ok()?;
        // SAFETY: `layout` is at least one byte, the NUL.
        let dst = NonNull::new(unsafe { alloc(layout) })?;
        // SAFETY: `dst` is a fresh allocation of `src.len()` bytes, which a
        // Rust slice cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len()) };
        Some(dst.cast())
    }
}

// SAFETY: frees a buffer `c_alloc_len` or `c_clone_len` allocated with
// `byte_len` bytes at `BUF_ALIGN`, repeating that layout; a buffer's
// `byte_len` is exactly its allocation's, as `CLenDrop` requires of a free
// that takes a size.
unsafe impl CLenDrop for RustAlloc {
    #[inline]
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        // SAFETY: the caller owns a live buffer of exactly `byte_len` bytes
        // this policy allocated, so the layout is the one it was allocated
        // with, and valid.
        unsafe {
            let layout = Layout::from_size_align_unchecked(byte_len, Self::BUF_ALIGN);
            dealloc(ptr, layout);
        }
    }
}

// SAFETY: a fresh, uniquely-owned allocation of `byte_len` bytes at
// `BUF_ALIGN`, released by `c_drop_len` given the same length.
unsafe impl CLenAlloc for RustAlloc {
    const ALIGN: usize = Self::BUF_ALIGN;

    #[inline]
    fn c_alloc_len(&self, byte_len: usize) -> Option<NonNull<u8>> {
        let layout = Self::buf_layout(byte_len)?;
        if byte_len == 0 {
            return None;
        }
        // SAFETY: `layout` is not zero-sized.
        NonNull::new(unsafe { alloc(layout) })
    }
}

// SAFETY: as `CLenAlloc`, filled with a byte copy of the source, which is
// only read.
unsafe impl CLenClone for RustAlloc {
    const ALIGN: usize = Self::BUF_ALIGN;

    #[inline]
    unsafe fn c_clone_len(&self, ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>> {
        let dst = self.c_alloc_len(byte_len)?;
        // SAFETY: the caller passes `byte_len` readable bytes; `dst` is a
        // fresh allocation of as many, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(ptr, dst.as_ptr(), byte_len) };
        Some(dst)
    }
}
