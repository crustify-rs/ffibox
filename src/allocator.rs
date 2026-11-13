//! [`CAllocator`]: the standard [`Allocator`] over a C allocation family.
//!
//! A [`CLenAlloc`] policy is a C allocation family: `malloc` and the
//! alignment it vouches for ([`c_alloc_len`](CLenAlloc::c_alloc_len),
//! [`ALIGN`](CLenAlloc::ALIGN)), `realloc`
//! ([`c_realloc_len`](CLenAlloc::c_realloc_len)) and `free`
//! ([`c_drop_len`](crate::CLenDrop::c_drop_len)), declared once with
//! [`impl_clendrop!`](crate::impl_clendrop) and
//! [`impl_clenalloc!`](crate::impl_clenalloc). `CAllocator<P>` is that family
//! as the standard `Allocator`, so the standard owners hold memory C frees:
//!
//! - `Box<T, CAllocator<P>>` and `Vec<T, CAllocator<P>>` (`alloc` feature)
//!   allocate, grow and free through the family, and adopt what C allocated
//!   with it;
//! - `CBox<Foo, CAllocator<P>>`, `CStrBox<CAllocator<P>>` and
//!   `CVec<T, CAllocator<P>>` take it as their policy, a storage-only one,
//!   and convert to and from the standard owners without copying.
//!
//! It is the C counterpart of [`RustAlloc`](crate::RustAlloc), which offers
//! Rust's allocator to the owners: replacing one with the other changes the
//! owner's policy and nothing its consumers write.

use core::alloc::{AllocError, Allocator, Layout};
use core::ffi::{CStr, c_char};
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::traits::{
    CAlloc, CAllocZeroed, CCell, CDrop, CDupClone, CLenAlloc, CLenClone, CLenDrop, CNew, CZeroable,
};

/// The standard [`Allocator`] over a C allocation family, the [`CLenAlloc`]
/// policy `P`; and, as a policy itself, the storage-only one over it.
///
/// `#[repr(transparent)]` over `P`, so with a unit-struct policy it is
/// zero-sized and `Copy`, and `Box<T, CAllocator<P>>` is one pointer wide,
/// C's `T *`; `Send` and `Sync` follow `P`, so a family that must free on the
/// allocating thread keeps its boxes and vectors there (see
/// [Thread safety](https://github.com/crustify-rs/ffibox/blob/main/docs/ownership.md#conventions)):
///
/// ```compile_fail,E0277
/// # use ffibox::CAllocator;
/// /// A family that frees on the allocating thread only.
/// struct LocalHeap(core::marker::PhantomData<*const ()>);
/// fn send<T: Send>() {}
/// send::<CAllocator<LocalHeap>>();
/// ```
///
/// ```
/// use core::alloc::{Allocator, Layout};
/// use ffibox::{CAllocator, RustAlloc};
///
/// let a = CAllocator::new(RustAlloc);
/// let mut v: Vec<u32, CAllocator<RustAlloc>> = Vec::new_in(a);
/// v.extend([1, 2, 3]); // grows through the family's realloc
/// assert_eq!(v, [1, 2, 3]);
/// // Beyond the family's alignment is an `AllocError`, never a misaligned block.
/// assert!(a.allocate(Layout::from_size_align(64, 64).unwrap()).is_err());
/// ```
///
/// # The allocator
///
/// - **Zero-sized requests never reach C.** `allocate` of size 0 returns
///   [`Layout::dangling_ptr`], `deallocate` of size 0 does nothing, `grow`
///   from size 0 allocates afresh and `shrink` to size 0 frees and returns a
///   dangling pointer. So C's `malloc` is never asked for 0 bytes, and no
///   dangling pointer reaches its `realloc` or `free`.
/// - **Alignment beyond `P::ALIGN` is an [`AllocError`]**, from every
///   allocating method; the typed paths ffibox offers
///   ([`try_box`](Self::try_box), the policy impls and the conversions)
///   refuse such a type at compile time instead.
/// - **`grow` and `shrink` go through
///   [`c_realloc_len`](CLenAlloc::c_realloc_len)**, the family's `realloc`
///   where it has one: on failure the old block is untouched and still
///   allocated, as the standard contract requires. `grow_zeroed` zeroes the
///   bytes past the old size after it. A shrink to the same size keeps the
///   block.
/// - **No method unwinds**: if a policy's routine panics, the process aborts,
///   as the standard `Allocator` requires.
///
/// Every block is exactly the size requested, so a `Box` or `Vec` hands
/// [`c_drop_len`](CLenDrop::c_drop_len) the size the block was allocated
/// with, as a free that takes a size needs.
///
/// # What it counts as allocated, and what fits
///
/// The standard `Allocator` lets a block be freed only with a layout that
/// *fits* it. `CAllocator<P>` relaxes that, and the relaxation is what makes
/// adopting C's buffers sound:
///
/// - Every live, non-empty buffer of `P`'s family, allocated by
///   `CAllocator<P>` or by C with the family's routines, is *currently
///   allocated* by every `CAllocator<P>` whose `P` releases it (every
///   instance of a stateless `P`).
/// - A layout *fits* such a buffer when its alignment is at most
///   `P::ALIGN` and its size is not 0 and is a length
///   [`c_drop_len`](CLenDrop::c_drop_len) accepts for the buffer: no larger
///   than the allocation, and exactly its size when the family's free takes
///   a size. `deallocate`, `grow` and `shrink` ignore the alignment, and a
///   free that ignores the size ignores the layout's size too.
///
/// So `Vec::from_raw_parts_in(ptr, len, len, a)` adopts `len` elements C
/// allocated with `malloc` and never recorded a capacity for, where the
/// family's free ignores the size: the Vec frees, grows and shrinks it with
/// `len` elements' bytes, which `free` and `realloc` accept. Under a free
/// that takes the size, `len` must be exactly the allocation's. A size of 0
/// fits no C buffer: a buffer C allocated with 0 bytes (`malloc(0)`'s
/// non-NULL result) adopted with capacity 0 is leaked, never freed, so
/// [`CVec`](crate::CVec)'s conversion frees it first.
///
/// # As a policy
///
/// `CAllocator<P>` is also a storage-only policy over the same family, as
/// [`RustAlloc`](crate::RustAlloc) is over Rust's:
///
/// | Owner | Through |
/// |-------|---------|
/// | `CBox<Foo, CAllocator<P>>` | [`CBox::new`](crate::CBox::new) / [`new_zeroed`](crate::CBox::new_zeroed) for a [`CZeroable`] `Foo`, [`CBox::new_uninit`](crate::CBox::new_uninit) otherwise; released by dropping the `Foo` in place and freeing its `size_of::<Foo>()` bytes, as a `Box` is |
/// | `CStrBox<CAllocator<P>>` | [`CStrBox::from_c_str`](crate::CStrBox::from_c_str), copying into `strlen + 1` bytes; freed with that length |
/// | `CVec<T, CAllocator<P>>` | everything `CVec<T, P>` does, forwarded to `P` |
///
/// Zero-sized objects, and objects aligned beyond `P::ALIGN`, are refused at
/// compile time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct CAllocator<P>(P);

impl<P> CAllocator<P> {
    /// The allocator over the family `policy` belongs to.
    #[inline]
    #[must_use]
    pub const fn new(policy: P) -> Self {
        Self(policy)
    }

    /// The family's policy.
    #[inline]
    #[must_use]
    pub const fn policy(&self) -> &P {
        &self.0
    }
}

impl<P: CLenAlloc> CAllocator<P> {
    /// One `T`'s layout, refusing zero-sized and over-aligned `T` at compile
    /// time: an object of the family is a non-empty buffer it aligns.
    #[inline(always)]
    pub(crate) const fn object_layout<T>() -> Layout {
        const {
            assert!(
                core::mem::size_of::<T>() != 0,
                "a zero-sized T has no storage in a C allocation family"
            );
            assert!(
                core::mem::align_of::<T>() <= P::ALIGN,
                "the family's allocator cannot align T"
            );
        }
        Layout::new::<T>()
    }

    /// A block of `layout`'s size from the family, or the dangling block of
    /// size 0.
    #[inline]
    fn alloc_block(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.align() > P::ALIGN {
            return Err(AllocError);
        }
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(layout.dangling_ptr(), 0));
        }
        let ptr = self.0.c_alloc_len(layout.size()).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    /// Move a block of `old` to one of `new` through the family's `realloc`.
    ///
    /// # Safety
    ///
    /// As [`Allocator::grow`] or [`Allocator::shrink`], by which `new` is
    /// larger or smaller than `old`.
    #[inline]
    unsafe fn realloc_block(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new.align() > P::ALIGN {
            return Err(AllocError);
        }
        if old.size() == 0 {
            // A block of size 0 is the dangling one: nothing to move or free.
            return self.alloc_block(new);
        }
        if new.size() == 0 {
            // SAFETY: the caller passes a block `old` fits; it is freed once.
            unsafe { self.0.c_drop_len(ptr.as_ptr(), old.size()) };
            return Ok(NonNull::slice_from_raw_parts(new.dangling_ptr(), 0));
        }
        if new.size() == old.size() {
            // The block is aligned to `P::ALIGN >= new.align()` already.
            return Ok(NonNull::slice_from_raw_parts(ptr, old.size()));
        }
        // SAFETY: the caller passes a live block of the family that `old`
        // fits, so `old.size()` is a length `c_drop_len` accepts for it;
        // neither length is 0. On `None` the block is untouched.
        let moved = unsafe { self.0.c_realloc_len(ptr.as_ptr(), old.size(), new.size()) };
        let moved = moved.ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(moved, new.size()))
    }
}

#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl<P: CLenAlloc> CAllocator<P> {
    /// `value` in a fresh box of the family, or [`AllocError`] if the
    /// allocation fails, dropping `value` — `Box::try_new_in(value, self)`,
    /// which std has not stabilized; [`Box::new_in`] aborts on that failure
    /// instead. A zero-sized `T` allocates nothing, as a `Box` of one never
    /// does. A `T` aligned beyond the family's `ALIGN` is refused at compile
    /// time:
    ///
    /// ```compile_fail,E0080
    /// # use ffibox::{CAllocator, RustAlloc};
    /// #[repr(align(32))]
    /// struct Wide(u8);
    /// // `RustAlloc`'s buffers are 16-aligned.
    /// let _ = CAllocator::new(RustAlloc).try_box(Wide(0));
    /// ```
    #[inline]
    pub fn try_box<T>(self, value: T) -> Result<alloc::boxed::Box<T, Self>, AllocError> {
        const {
            assert!(
                core::mem::align_of::<T>() <= P::ALIGN,
                "the family's allocator cannot align T"
            );
        }
        let ptr = self.allocate(Layout::new::<T>())?.cast::<T>();
        // SAFETY: `ptr` is a fresh block of `T`'s layout from `self` (the
        // dangling one for a zero-sized `T`, which `Box` never frees),
        // written once with `value` and then owned by the box.
        unsafe {
            ptr.as_ptr().write(value);
            Ok(alloc::boxed::Box::from_non_null_in(ptr, self))
        }
    }
}

/// Aborts the process if dropped: armed around a policy's routines, which
/// the standard `Allocator` forbids to unwind, and disarmed with
/// `mem::forget` once they return.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    #[inline]
    fn drop(&mut self) {
        crate::refs::abort_process()
    }
}

/// Run `f`, aborting rather than unwinding out of it.
#[inline(always)]
fn no_unwind<R>(f: impl FnOnce() -> R) -> R {
    let guard = AbortOnUnwind;
    let r = f();
    core::mem::forget(guard);
    r
}

// SAFETY: every non-empty block is a fresh allocation of the family
// (`c_alloc_len`, `c_realloc_len`), uniquely owned, of exactly the size
// requested and aligned to `P::ALIGN >= layout.align()` (larger alignments
// are refused), and it stays valid until `deallocate`, a successful `grow` or
// `shrink`, or C frees it: `CAllocator` has no `&mut` API and its drop frees
// nothing. Blocks of size 0 are dangling and well aligned, and never reach
// the family. The family's blocks are disjoint and end within the address
// space, as any allocator's. A failed `grow` or `shrink` leaves the block
// as it was (`c_realloc_len`'s contract). `deallocate` and the moves hand
// `c_drop_len` / `c_realloc_len` the block's size, which fits it as the
// docs above define. No method unwinds (`no_unwind`), none reads or writes a
// block the caller holds beyond what `grow_zeroed` zeroes.
unsafe impl<P: CLenAlloc> Allocator for CAllocator<P> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        no_unwind(|| self.alloc_block(layout))
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            // The dangling block of size 0: never the family's.
            return;
        }
        // SAFETY: the caller passes a block of the family `layout` fits, so
        // its size is a length `c_drop_len` accepts; it is freed once.
        no_unwind(|| unsafe { self.0.c_drop_len(ptr.as_ptr(), layout.size()) })
    }

    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller upholds `grow`'s contract.
        no_unwind(|| unsafe { self.realloc_block(ptr, old_layout, new_layout) })
    }

    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller upholds `grow_zeroed`'s contract, which is
        // `grow`'s.
        let block = unsafe { self.grow(ptr, old_layout, new_layout) }?;
        // SAFETY: the new block holds `new_layout.size() >= old_layout.size()`
        // bytes, the caller's from here on; the ones past the old size are
        // zeroed, as `grow_zeroed` promises.
        unsafe {
            block
                .cast::<u8>()
                .add(old_layout.size())
                .write_bytes(0, new_layout.size() - old_layout.size())
        };
        Ok(block)
    }

    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller upholds `shrink`'s contract.
        no_unwind(|| unsafe { self.realloc_block(ptr, old_layout, new_layout) })
    }
}

// ---------------------------------------------------------------------------
// As a policy: storage-only, over the same family
// ---------------------------------------------------------------------------

// SAFETY: `c_drop` drops the `T` in place and frees its `size_of::<T>()`
// bytes once, the length `c_alloc` / `c_alloc_zeroed` allocated them with —
// what dropping a `Box<T, CAllocator<P>>` does.
unsafe impl<P: CLenAlloc, T: CCell> CDrop<T> for CAllocator<P> {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<T>) {
        let size = Self::object_layout::<T>().size();
        // SAFETY: the caller owns a live `T` in a `size_of::<T>()`-byte
        // buffer of the family; it is dropped, then freed, once.
        unsafe {
            core::ptr::drop_in_place(ptr.as_ptr());
            self.0.c_drop_len(ptr.as_ptr().cast(), size);
        }
    }
}

// SAFETY: storage for a `T` that holds no `T` yet is freed without being
// read, with the length it was allocated with.
unsafe impl<P: CLenAlloc, T: CCell> CDrop<MaybeUninit<T>> for CAllocator<P> {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<MaybeUninit<T>>) {
        let size = Self::object_layout::<T>().size();
        // SAFETY: the caller owns a `size_of::<T>()`-byte buffer of the
        // family; it is freed once.
        unsafe { self.0.c_drop_len(ptr.as_ptr().cast(), size) }
    }
}

// SAFETY: a fresh, uniquely-owned buffer of `size_of::<T>()` bytes aligned to
// `P::ALIGN >= align_of::<T>()` (checked at compile time), which both `CDrop`
// impls above free without the uninitialised one reading it.
unsafe impl<P: CLenAlloc, T: CCell> CAlloc<T> for CAllocator<P> {
    #[inline]
    fn c_alloc(&self) -> Option<NonNull<MaybeUninit<T>>> {
        let size = Self::object_layout::<T>().size();
        // The size is not 0 (`object_layout`).
        self.0.c_alloc_len(size).map(NonNull::cast)
    }
}

// SAFETY: as `CAlloc`, every byte zeroed before it is handed out, which
// `CZeroable` makes a valid `T`, released by `CDrop<T>` above.
unsafe impl<P: CLenAlloc, T: CZeroable> CAllocZeroed<T> for CAllocator<P> {
    #[inline]
    fn c_alloc_zeroed(&self) -> Option<NonNull<T>> {
        let size = Self::object_layout::<T>().size();
        let ptr = self.0.c_alloc_len(size)?;
        // SAFETY: a fresh allocation of `size` bytes, uniquely owned.
        unsafe { ptr.as_ptr().write_bytes(0, size) };
        Some(ptr.cast())
    }
}

// SAFETY: a zeroed `T` is a fully-initialised one by `CZeroable`, released by
// `CDrop<T>` above; `None` is an allocation failure.
unsafe impl<P: CLenAlloc, T: CZeroable> CNew<T> for CAllocator<P> {
    #[inline]
    fn c_new(&self) -> Option<NonNull<T>> {
        self.c_alloc_zeroed()
    }
}

// SAFETY: a string this policy allocated holds `strlen + 1` bytes, which
// `c_drop` recomputes; the views a `CStrBox` gives are read-only, so the
// length cannot change while it owns the string.
unsafe impl<P: CLenAlloc> CDrop<c_char> for CAllocator<P> {
    #[inline]
    unsafe fn c_drop(&self, ptr: NonNull<c_char>) {
        // SAFETY: the caller owns a live NUL-terminated string of the family,
        // `strlen + 1` bytes; it is freed once with that length.
        unsafe {
            let size = CStr::from_ptr(ptr.as_ptr()).to_bytes_with_nul().len();
            self.0.c_drop_len(ptr.as_ptr().cast(), size);
        }
    }
}

// SAFETY: copies any live NUL-terminated string, reading it only, into a
// fresh `strlen + 1`-byte buffer of the family, which `c_drop` frees.
unsafe impl<P: CLenAlloc> CDupClone<c_char> for CAllocator<P> {
    #[inline]
    unsafe fn c_dup(&self, ptr: NonNull<c_char>) -> Option<NonNull<c_char>> {
        // SAFETY: the caller passes a live NUL-terminated string.
        let src = unsafe { CStr::from_ptr(ptr.as_ptr()) }.to_bytes_with_nul();
        // At least one byte, the NUL.
        let dst = self.0.c_alloc_len(src.len())?;
        // SAFETY: `dst` is a fresh allocation of `src.len()` bytes, which the
        // source cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len()) };
        Some(dst.cast())
    }
}

// SAFETY: forwarded to `P`, whose contract this is.
unsafe impl<P: CLenAlloc> CLenDrop for CAllocator<P> {
    #[inline]
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        // SAFETY: the caller upholds `c_drop_len`'s contract, for `P`'s
        // allocator, which is this one's.
        unsafe { self.0.c_drop_len(ptr, byte_len) }
    }
}

// SAFETY: forwarded to `P`, whose contract this is; `c_drop_len` above is
// `P`'s.
unsafe impl<P: CLenAlloc> CLenAlloc for CAllocator<P> {
    const ALIGN: usize = P::ALIGN;

    #[inline]
    fn c_alloc_len(&self, byte_len: usize) -> Option<NonNull<u8>> {
        self.0.c_alloc_len(byte_len)
    }

    #[inline]
    unsafe fn c_realloc_len(
        &self,
        ptr: *mut u8,
        old_len: usize,
        new_len: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: the caller upholds `c_realloc_len`'s contract, for `P`'s
        // allocator, which is this one's.
        unsafe { self.0.c_realloc_len(ptr, old_len, new_len) }
    }
}

// SAFETY: forwarded to `P`, whose contract this is.
unsafe impl<P: CLenAlloc + CLenClone> CLenClone for CAllocator<P> {
    const ALIGN: usize = <P as CLenClone>::ALIGN;

    #[inline]
    unsafe fn c_clone_len(&self, ptr: *mut u8, byte_len: usize) -> Option<NonNull<u8>> {
        // SAFETY: the caller upholds `c_clone_len`'s contract.
        unsafe { self.0.c_clone_len(ptr, byte_len) }
    }
}
