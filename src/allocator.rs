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

#[cfg(feature = "alloc")]
use crate::refs::{CBox, CVec};
#[cfg(feature = "alloc")]
use crate::traits::CSliceElem;
use crate::traits::{
    CAlloc, CAllocZeroed, CCell, CDrop, CDupClone, CLenAlloc, CLenClone, CLenDrop, CNew, CZeroable,
};

/// The standard [`Allocator`] over a C allocation family, the [`CLenAlloc`]
/// policy `P`; and, as a policy itself, the storage-only one over it.
///
/// Its standard counterpart is `std::alloc::System`, an `Allocator` over a
/// heap; this one's heap is the C family's, so that what a `Box` or `Vec`
/// over it allocates C may free, and what C allocated they may adopt.
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
///   dangling pointer reaches its `realloc` or `free`. The typed paths ffibox
///   offers ([`try_box`](Self::try_box), the policy impls and the
///   conversions) refuse a zero-sized type at compile time, so none yields a
///   dangling pointer for C to free. std's own `Box::new_in` and
///   `Vec::new_in` accept one, as for any allocator, and a `Box` of a
///   zero-sized type, like every empty `Vec`, holds a dangling pointer: a raw
///   pointer taken out of a `Box` over `CAllocator`
///   (`Box::into_raw_with_allocator`) may go to C's free only for a
///   non-zero-sized `T`, as [`try_box`](Self::try_box) guarantees, and a run
///   goes to C through [`CVec`]'s `TryFrom<Vec>` and its `into_raw_parts`,
///   which turn an empty `Vec` into NULL.
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
/// - **It is an `Allocator` only over a `Copy` policy**, which has no drop
///   glue: the standard `Allocator` forbids an allocator's drop to unwind,
///   and a `Copy` bound says so in a type, where a policy with a `Drop` would
///   need a new obligation on every `CLenAlloc` impl. A unit-struct policy,
///   or one holding a handle, is `Copy`; as a policy of `CBox`, `CStrBox` and
///   `CVec`, `CAllocator<P>` takes any `P`.
///
///   ```compile_fail,E0277
///   # use core::ptr::NonNull;
///   # use ffibox::{CAllocator, CLenAlloc, CLenDrop};
///   /// A family policy that is not `Copy`.
///   struct Pool;
///   # unsafe impl CLenDrop for Pool { unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {} }
///   # unsafe impl CLenAlloc for Pool {
///   #     fn c_alloc_len(&self, _: usize) -> Option<NonNull<u8>> { None }
///   # }
///   let _ = Box::new_in(1u32, CAllocator::new(Pool));
///   ```
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
/// - A non-empty buffer of `P`'s family whose ownership has passed to the
///   caller, whether `CAllocator<P>` allocated it or C did with the
///   family's routines and gave it up (as a `CVec` adopted with
///   `from_raw_parts` owns it), is *currently allocated* by every
///   `CAllocator<P>` whose `P` releases it (every instance of a stateless
///   `P`), until the caller gives it to `deallocate`, a successful `grow` or
///   `shrink`, or back to C. Nothing else ends it: C frees only what it owns,
///   and a buffer the caller owns is not C's.
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
/// [`CVec`]'s conversion frees it first.
///
/// # As a policy
///
/// `CAllocator<P>` is also a storage-only policy over the same family, as
/// [`RustAlloc`](crate::RustAlloc) is over Rust's:
///
/// | Owner | Through |
/// |-------|---------|
/// | `CBox<Foo, CAllocator<P>>` | [`CBox::new`](crate::CBox::new) / [`new_zeroed`](crate::CBox::new_zeroed) for a [`CZeroable`] `Foo`, [`CBox::new_uninit`](crate::CBox::new_uninit) otherwise; released by dropping the `Foo` in place and freeing its `size_of::<Foo>()` bytes, as a `Box` is |
/// | `CStrBox<CAllocator<P>>` | [`CStrBox::from_c_str`](crate::CStrBox::from_c_str), copying into `strlen + 1` bytes; freed with that length, so a string adopted with [`CStrBox::from_raw`](crate::CStrBox::from_raw) under a free that takes a size must be an allocation of exactly `strlen + 1` bytes |
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
impl<P: CLenAlloc + Copy> CAllocator<P> {
    /// `value` in a fresh box of the family, or [`AllocError`] if the
    /// allocation fails, dropping `value` — `Box::try_new_in(value, self)`,
    /// which std has not stabilized;
    /// [`Box::new_in`](alloc::boxed::Box::new_in) aborts on that failure
    /// instead.
    ///
    /// A zero-sized `T` is refused at compile time, as by the conversions: its
    /// box would hold a dangling pointer, never allocated, which C's `free`
    /// would be handed through `Box::into_raw_with_allocator`:
    ///
    /// ```compile_fail,E0080
    /// # use ffibox::{CAllocator, RustAlloc};
    /// let _ = CAllocator::new(RustAlloc).try_box(());
    /// ```
    ///
    /// So is a `T` aligned beyond the family's `ALIGN`:
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
        let layout = Self::object_layout::<T>();
        let ptr = self.allocate(layout)?.cast::<T>();
        // SAFETY: `ptr` is a fresh, non-empty block of `T`'s layout from
        // `self` (`object_layout` refuses a zero-sized `T`), written once
        // with `value` and then owned by the box.
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
// are refused), whose ownership passes to the caller; a buffer C allocated
// counts as currently allocated only once its ownership has passed to the
// caller, as the docs above define. Either stays valid until the caller
// gives it to `deallocate`, a successful `grow` or `shrink`, or back to C:
// C frees only buffers it owns, `CAllocator` has no `&mut` API, and it has
// no drop glue (`P: Copy`), so dropping it neither frees nor unwinds.
// Blocks of size 0 are dangling and well aligned, and never reach the
// family. The family's blocks are disjoint and end within the address
// space, as any allocator's. A failed `grow` or `shrink` leaves the block
// as it was (`c_realloc_len`'s contract). `deallocate` and the moves hand
// `c_drop_len` / `c_realloc_len` the block's size, which fits it as the
// docs above define. No method unwinds (`no_unwind`), none reads or writes a
// block the caller holds beyond what `grow_zeroed` zeroes.
unsafe impl<P: CLenAlloc + Copy> Allocator for CAllocator<P> {
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

// ---------------------------------------------------------------------------
// The standard owners: Box and Vec, to and from CBox and CVec
// ---------------------------------------------------------------------------

/// `Box<T, CAllocator<P>>` as the [`CBox`] it is: the same pointer, the same
/// allocator, nothing copied — `Box` → `CBox`, where C keeps or frees the
/// object from now on (`into_c` hands it the pointer). Dropping either
/// drops the `T` in place and frees its bytes through the family.
///
/// A box of a zero-sized `T` holds a dangling pointer that was never
/// allocated, which C would free; a `T` aligned beyond the family's `ALIGN`
/// has no buffer of the family. Both are refused at compile time:
///
/// ```compile_fail,E0080
/// # use ffibox::{define_ctype, CAllocator, CBox, CZeroable, RustAlloc};
/// #[repr(C)]
/// pub struct empty_st {}
/// define_ctype!(Empty, EmptyRef, EmptyMut, empty_st);
/// # // SAFETY: no bytes.
/// # unsafe impl CZeroable for Empty {}
/// let b = Box::new_in(Empty::zeroed(), CAllocator::new(RustAlloc));
/// let _ = CBox::from(b);
/// ```
///
/// ```compile_fail,E0080
/// # use ffibox::{define_ctype, CAllocator, CBox, CZeroable, RustAlloc};
/// #[repr(C, align(32))]
/// pub struct wide_st { x: u8 }
/// define_ctype!(Wide, WideRef, WideMut, wide_st);
/// # // SAFETY: any byte.
/// # unsafe impl CZeroable for Wide {}
/// # let b = Box::new(Wide::zeroed());
/// # let b = unsafe { Box::from_raw_in(Box::into_raw(b), CAllocator::new(RustAlloc)) };
/// let _ = CBox::from(b); // `RustAlloc`'s buffers are 16-aligned
/// ```
///
/// The policy is stateless, as every [`CBox`]'s is.
#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl<T: CCell, P: CLenAlloc + Copy> From<alloc::boxed::Box<T, CAllocator<P>>>
    for CBox<T, CAllocator<P>>
{
    #[inline]
    fn from(b: alloc::boxed::Box<T, CAllocator<P>>) -> Self {
        let _ = CAllocator::<P>::object_layout::<T>();
        let (ptr, a) = alloc::boxed::Box::into_non_null_with_allocator(b);
        // SAFETY: the box owned a live `T` in a non-empty block of the family
        // (`T` is not zero-sized), which `CBox`'s `CDrop<T>` frees as the box
        // did; nothing else uses it.
        unsafe { CBox::from_non_null_with(ptr, a) }
    }
}

#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl<T: CCell, P: CLenAlloc + Copy> CBox<T, CAllocator<P>> {
    /// The object as a `Box<T, CAllocator<P>>`: the same pointer and
    /// allocator, nothing copied — `CBox` → `Box`, once C no longer keeps
    /// the object. The box drops it in place and frees it through the
    /// family, as this `CBox` would have.
    ///
    /// Zero-sized and over-aligned `T` are refused at compile time, as by
    /// [`CBox::from`].
    ///
    /// # Safety
    ///
    /// A `Box` lends `&T` and `&mut T`, through which safe code moves the
    /// object's bytes (`mem::swap`, `mem::replace`), copies or moves the `T`
    /// out (`*b`) and reads it under a shared reference, none of which a
    /// `CBox` does: it reaches the object through handles alone. So:
    ///
    /// - the object's bytes must be a valid `T`: every field initialised and
    ///   a valid value of its Rust type, where C may have left a field
    ///   unwritten (storage from `new_uninit` that C filled in part) or
    ///   written a value Rust rejects (a `bool` field holding 2, an
    ///   out-of-range enum);
    /// - the object must not be address-sensitive: neither C nor the object
    ///   itself may hold a pointer into it (C registered it, or it points
    ///   into itself), as for a value [`define_ctype!`](crate::define_ctype)'s
    ///   layout type holds by value;
    /// - nothing but the box may reach it from here on, C included, as for
    ///   any `Box`.
    ///
    /// No type says any of them, so the caller does. A [`CZeroable`] `T`
    /// meets the second by its contract, and none of the others: its zeroed
    /// state is valid, not what C wrote since.
    #[inline]
    pub unsafe fn into_box(self) -> alloc::boxed::Box<T, CAllocator<P>> {
        let _ = CAllocator::<P>::object_layout::<T>();
        let (ptr, a) = self.into_raw_with();
        // SAFETY: a live `T` in a `size_of::<T>()`-byte block of the family,
        // aligned for it, which the box frees with `T`'s layout, as this
        // policy's `CDrop<T>` did; the caller vouches for the rest.
        unsafe { alloc::boxed::Box::from_raw_in(ptr, a) }
    }
}

/// `CVec<T, CAllocator<P>>` as the `Vec` it is: the same buffer and
/// allocator, capacity = length, nothing copied — `CVec` → `Vec`, once C no
/// longer keeps the buffer. The `Vec` grows it through the family's
/// `realloc` and frees it with its `free`, which accept its length as
/// [`CAllocator`]'s fit rule says.
///
/// An empty `CVec` becomes an empty `Vec`, which holds no allocation: a NULL
/// buffer allocates nothing, and a non-NULL buffer of length 0 (C's
/// `malloc(0)`) is freed first, since a `Vec` of capacity 0 never frees
/// one.
///
/// For elements a `CVec` hands out as a slice ([`CSliceElem`]); a buffer of
/// C objects stays behind handles. Zero-sized elements, and elements aligned
/// beyond the family's `ALIGN`, are refused at compile time:
///
/// ```compile_fail,E0080
/// # use ffibox::{CAllocator, CVec, RustAlloc};
/// let v = CVec::<(), CAllocator<RustAlloc>>::empty();
/// let _ = Vec::from(v);
/// ```
///
/// ```compile_fail,E0080
/// # use ffibox::{CAllocator, CPlainElem, CVec, RustAlloc};
/// #[derive(Clone, Copy)]
/// #[repr(align(32))]
/// struct Wide(u8);
/// // SAFETY: any byte is a `Wide`.
/// unsafe impl CPlainElem for Wide {}
/// let v = CVec::<Wide, CAllocator<RustAlloc>>::empty();
/// let _ = Vec::from(v);
/// ```
#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl<T: CSliceElem, P: CLenAlloc + Copy> From<CVec<T, CAllocator<P>>>
    for alloc::vec::Vec<T, CAllocator<P>>
{
    #[inline]
    fn from(v: CVec<T, CAllocator<P>>) -> Self {
        let _ = CAllocator::<P>::object_layout::<T>();
        let (ptr, len, a) = v.into_raw_parts_with();
        let Some(ptr) = NonNull::new(ptr) else {
            return Self::new_in(a);
        };
        if len == 0 {
            // SAFETY: a live buffer of the family the `CVec` owned, freed with
            // the length it would have freed it with.
            no_unwind(|| unsafe { a.0.c_drop_len(ptr.as_ptr().cast(), 0) });
            return Self::new_in(a);
        }
        // SAFETY: `len` initialised `T` in a buffer of the family, aligned for
        // `T`, its `len * size_of::<T>()` bytes a length `c_drop_len` accepts
        // (`CVec::from_raw_parts`' contract), which is a layout that fits it
        // under `CAllocator`'s rule; nothing else owns it.
        unsafe { Self::from_raw_parts_in(ptr.as_ptr(), len, len, a) }
    }
}

/// A `Vec<T, CAllocator<P>>` as a [`CVec`], to hand C a buffer it frees with
/// the family's `free`: the same buffer and allocator, nothing copied —
/// `Vec` → `CVec`. A `CVec` records its length alone, so the buffer is first
/// made exactly that long: spare capacity is given back through the family's
/// `realloc` (a free that takes a size is then given the right one), and if
/// that fails the `Vec` comes back unchanged as the error.
///
/// An empty `Vec` holds a dangling pointer, which C must never free: it
/// becomes the NULL `CVec`, its allocation, if it had capacity, freed.
/// Zero-sized elements, which a `Vec` never allocates, and elements aligned
/// beyond the family's `ALIGN` are refused at compile time:
///
/// ```compile_fail,E0080
/// # use ffibox::{CAllocator, CVec, RustAlloc};
/// let v: Vec<(), _> = Vec::new_in(CAllocator::new(RustAlloc));
/// let _ = CVec::try_from(v);
/// ```
#[cfg(feature = "alloc")]
#[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
impl<T, P: CLenAlloc + Copy> TryFrom<alloc::vec::Vec<T, CAllocator<P>>> for CVec<T, CAllocator<P>> {
    /// The `Vec`, unchanged, when its spare capacity could not be given back.
    type Error = alloc::vec::Vec<T, CAllocator<P>>;

    fn try_from(v: alloc::vec::Vec<T, CAllocator<P>>) -> Result<Self, Self::Error> {
        let elem = CAllocator::<P>::object_layout::<T>();
        let (ptr, len, cap, a) = v.into_raw_parts_with_allocator();
        if cap == 0 {
            // No allocation: the dangling pointer stays in Rust.
            return Ok(Self::empty_with(a));
        }
        // SAFETY: a `Vec` with capacity holds a non-null block of
        // `cap * size_of::<T>() <= isize::MAX` bytes from `a`, aligned for `T`.
        let (base, held) = unsafe {
            (
                NonNull::new_unchecked(ptr),
                Layout::from_size_align_unchecked(cap * elem.size(), elem.align()),
            )
        };
        if len == 0 {
            // SAFETY: the block the `Vec` held, with its layout; freed once.
            unsafe { a.deallocate(base.cast(), held) };
            return Ok(Self::empty_with(a));
        }
        let base = if len == cap {
            base
        } else {
            // SAFETY: `len < cap`, so the size is in range.
            let used =
                unsafe { Layout::from_size_align_unchecked(len * elem.size(), elem.align()) };
            // SAFETY: the block the `Vec` held, which `held` fits; on failure
            // it is untouched.
            match unsafe { a.shrink(base.cast(), held, used) } {
                Ok(block) => block.cast::<T>(),
                Err(AllocError) => {
                    // SAFETY: the parts the `Vec` gave up, unchanged.
                    let v = unsafe { Self::Error::from_raw_parts_in(ptr, len, cap, a) };
                    return Err(v);
                }
            }
        };
        // SAFETY: `len` initialised `T` in a block of the family exactly
        // `len * size_of::<T>()` bytes long, aligned for `T`, which the
        // `CVec`'s policy frees with that length; nothing else owns it.
        Ok(unsafe { Self::from_non_null_parts_with(base, len, a) })
    }
}
