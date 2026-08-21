//! Non-owning pointer wrappers — raw C pointers represented **without** taking
//! ownership. They carry no teardown and no lifecycle contract (contrast the
//! owning pointers in [`owned_refs`](crate::owned_refs)); they answer *how a
//! pointer is used*, not *who frees it*.
//!
//! - [`COut<'a, T>`] — the write-end of a C `*mut T` out-parameter (a
//!   `&'a mut MaybeUninit<T>` the callee writes once).
//! - [`CSlice<'a, T>`] / [`CSliceMut<'a, T>`] — a shared or exclusive run of
//!   `len` contiguous elements, as a pointer and a count rather than a slice
//!   reference.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::c_type::CCell;
use crate::traits::CElem;

// ===========================================================================
// COut<'a, T> — C scalar out-parameter slot
// ===========================================================================

/// Typed handle to a C scalar out-parameter slot — a named alias for
/// [`&'a mut MaybeUninit<T>`](MaybeUninit), plus the [`from_ptr`] helper hiding
/// the `*mut T` → `*mut MaybeUninit<T>` cast at the boundary.
///
/// C APIs return values through `*mut T` arguments (`*mut size_t`,
/// `*mut c_int`); `&'a mut MaybeUninit<T>` is Rust's type for exclusive write
/// access to a possibly-uninitialised `T`. The alias keeps the "C out-param"
/// vocabulary greppable while call sites use [`MaybeUninit::write`] directly.
///
/// An alias rather than a newtype because a newtype would add no guarantee:
/// `&'a mut MaybeUninit<T>` already carries the lifetime tag, invariance in
/// `T`, exclusive-borrow semantics, and an `Option` niche layout-compatible
/// with `*mut T`.
///
/// # Typical use
///
/// ```ignore
/// use ffibox::{COut, c_out};
///
/// pub unsafe extern "C" fn my_get_len(out_len: *mut usize) -> i32 {
///     if let Some(out) = unsafe { c_out::from_ptr(out_len) } {
///         out.write(computed_len);
///     }
///     1
/// }
/// ```
pub type COut<'a, T> = &'a mut MaybeUninit<T>;

/// Wrap a raw `*mut T` as a [`COut<'a, T>`]. Returns `None` if `ptr` is null.
///
/// # Safety
///
/// The caller asserts the pointer is:
///
/// - valid for writes of `T` for at least the lifetime `'a`,
/// - properly aligned for `T`,
/// - not aliased by any other reference for `'a`,
/// - referring to a slot whose current content is *not* a valid `T` that needs
///   dropping (uninitialised, or a non-`Drop` primitive — the typical
///   C-out-param case).
#[inline]
pub unsafe fn from_ptr<'a, T>(ptr: *mut T) -> Option<COut<'a, T>> {
    // SAFETY: the caller upholds the invariants above, and any `*mut T` slot
    // satisfying them is a valid `*mut MaybeUninit<T>` — same layout, weaker
    // validity requirement.
    unsafe { (ptr as *mut MaybeUninit<T>).as_mut() }
}

// ===========================================================================

// ---------------------------------------------------------------------------
// CSlice<'a, T> — a borrowed run of wrapped C objects
// ---------------------------------------------------------------------------

/// A borrowed run of `len` contiguous wrapped C objects, yielded as handles.
///
/// The slice analogue of a `Ref` handle, and for the same reason: `&[T]` over
/// wrapped C objects would be a reference covering them, asserting `noalias` /
/// `readonly` / validity over memory C may write. A `CSlice` is a pointer and a
/// count, so it asserts nothing; [`get`](CSlice::get) and [`iter`](CSlice::iter)
/// hand out per-element handles.
///
/// Reached with [`CVec::as_handles`](crate::CVec::as_handles). A buffer of plain
/// Rust values takes [`CVec::as_slice`](crate::CVec::as_slice) instead, which is
/// a real `&[T]`.
pub struct CSlice<'a, T> {
    ptr: NonNull<T>,
    len: usize,
    _borrow: PhantomData<&'a T>,
}

impl<T> Clone for CSlice<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
// Copies like `&[T]`; `#[derive]` would add a spurious `T: Copy`.
impl<T> Copy for CSlice<'_, T> {}

impl<'a, T> CSlice<'a, T> {
    /// Borrow `len` contiguous elements starting at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address `len` contiguous, initialised `T` that outlive `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: NonNull<T>, len: usize) -> Self {
        Self {
            ptr,
            len,
            _borrow: PhantomData,
        }
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the run is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A run of wrapped C objects is reached as per-element handles.
impl<'a, T: CCell> CSlice<'a, T> {
    /// Shared handle to element `i`; `None` if out of range.
    ///
    /// The handle carries `'a`, not a lifetime from `&self`, and that is sound
    /// *here* precisely because this view is the shared one: it is `Copy`, so a
    /// caller can hold as many as it likes either way, and none of them grants
    /// write access. The exclusive view must not do this — see
    /// [`CSliceMut::get_mut`].
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<T::Ref<'a>>
    where
        T: 'a,
    {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, and the constructor guarantees `len` contiguous
        // initialised `T` living for `'a`.
        Some(unsafe { T::ref_from_raw(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as shared handles.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = T::Ref<'a>> + use<'a, T>
    where
        T: 'a,
    {
        let (ptr, len) = (self.ptr, self.len);
        (0..len).map(move |i| {
            // SAFETY: `i < len`, per the constructor's contract.
            unsafe { T::ref_from_raw(NonNull::new_unchecked(ptr.as_ptr().add(i))) }
        })
    }

    /// Raw pointer to the first element, for passing the run to C.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }
}

/// A run of plain values is read out element-wise, still without a `&[T]`.
///
/// [`CElem`] is what licenses [`CVec::as_slice`](crate::CVec::as_slice) to hand
/// out a real `&[T]`, and it is not enough here — it answers *is every bit
/// pattern a valid `T`*, while a `&[T]` also asserts `noalias` and `readonly`
/// over the whole run for the whole borrow. `CVec` earns those by owning its
/// buffer exclusively. A run inside a C object does not: the library keeps the
/// pointer and may write through it, and no Rust lifetime constrains that. So
/// the element type is not what decides between `&[T]` and a `CSlice` — the
/// owner is.
impl<'a, T: CElem> CSlice<'a, T> {
    /// Copy element `i` out; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn elem(&self, i: usize) -> Option<T>
    where
        T: Copy,
    {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, the constructor guarantees an initialised `T`
        // there, and `T: CElem` makes every bit pattern a valid one. The read
        // is a copy — no reference over C's memory is formed.
        Some(unsafe { self.ptr.as_ptr().add(i).read() })
    }

    /// Iterate copies of the elements.
    #[inline]
    pub fn elems(&self) -> impl Iterator<Item = T> + use<'a, T>
    where
        T: Copy,
    {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: as `elem`, for each `i < len`.
        (0..len).map(move |i| unsafe { ptr.as_ptr().add(i).read() })
    }

    /// Copy the whole run into `dst`. `false` — and nothing copied — if the
    /// lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_to_slice(&self, dst: &mut [T]) -> bool
    where
        T: Copy,
    {
        if dst.len() != self.len {
            return false;
        }
        // SAFETY: `len` initialised `T` at `ptr` per the constructor, `dst` is
        // a live slice of the same length, and the two cannot overlap — `dst`
        // is a Rust reference, which may not cover the C storage this views.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), dst.as_mut_ptr(), self.len) };
        true
    }

    /// Raw pointer to the first element, for passing the run to C.
    #[inline]
    #[must_use]
    pub fn as_elem_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }
}

// ---------------------------------------------------------------------------
// CSliceMut<'a, T> — the exclusive run
// ---------------------------------------------------------------------------

/// A borrowed run of `len` contiguous elements, exclusively.
///
/// The `Mut` handle's analogue at slice granularity, and the destination for a
/// `&mut [T]` that would otherwise cover memory C writes. Move-only rather than
/// `Copy`, because that is what exclusivity means; reach the shared view with
/// [`as_ref`](CSliceMut::as_ref), which binds it to the borrow.
pub struct CSliceMut<'a, T> {
    ptr: NonNull<T>,
    len: usize,
    _borrow: PhantomData<&'a mut T>,
}

impl<'a, T> CSliceMut<'a, T> {
    /// Borrow `len` contiguous elements exclusively, starting at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must address `len` contiguous, initialised `T` that outlive `'a`,
    /// and no other handle or reference to any of them may be used while the
    /// result lives.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: NonNull<T>, len: usize) -> Self {
        Self {
            ptr,
            len,
            _borrow: PhantomData,
        }
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the run is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reborrow shared, for passing where a read-only run is wanted.
    ///
    /// Bound by `&self`, so the shared view — and every copy of it — keeps this
    /// one immutably borrowed and no write path is reachable meanwhile. This is
    /// the operation `Deref` cannot express, since `Deref::Target` cannot name
    /// a lifetime taken from `&self`.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> CSlice<'_, T> {
        // SAFETY: this view's own contract gives `len` initialised `T` at
        // `ptr`; the result is bound by `&self`, so it cannot outlive it.
        unsafe { CSlice::from_raw_parts(self.ptr, self.len) }
    }
}

/// A run of wrapped C objects is reached as per-element handles.
impl<'a, T: CCell> CSliceMut<'a, T> {
    /// Shared handle to element `i`; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<T::Ref<'_>> {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len` and the constructor guarantees an initialised `T`
        // there; the handle is bound by `&self`.
        Some(unsafe { T::ref_from_raw(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Exclusive handle to element `i`; `None` if out of range.
    ///
    /// Bound by `&mut self`, NOT by `'a`. Handing out a `T::Mut<'a>` here would
    /// let a caller keep it while calling [`get`](CSliceMut::get) — two handles
    /// to one object, which is what the exclusive handle exists to forbid.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, i: usize) -> Option<T::Mut<'_>> {
        if i >= self.len {
            return None;
        }
        // SAFETY: `i < len`, initialised per the constructor, and the view's
        // own contract makes this the only handle to that element; the result
        // is bound by `&mut self`.
        Some(unsafe { T::mut_from_raw(NonNull::new_unchecked(self.ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as shared handles.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = T::Ref<'_>> {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: `i < len`, per the constructor's contract.
        (0..len).map(move |i| unsafe { T::ref_from_raw(NonNull::new_unchecked(ptr.as_ptr().add(i))) })
    }

    /// Iterate the run as exclusive handles.
    ///
    /// Every item borrows `&mut self`, so the whole run stays exclusively
    /// borrowed while any of them lives; the items are sound to hold at once
    /// because each addresses a distinct element, exactly as
    /// [`slice::iter_mut`](slice::iter_mut) does.
    #[inline]
    pub fn iter_mut<'s>(&'s mut self) -> impl Iterator<Item = T::Mut<'s>> + use<'s, T>
    where
        T: 's,
    {
        let (ptr, len) = (self.ptr, self.len);
        // SAFETY: `i` is distinct on every step, so no two items address the
        // same element; each is initialised per the constructor and bound by
        // the `&mut self` borrow.
        (0..len).map(move |i| unsafe { T::mut_from_raw(NonNull::new_unchecked(ptr.as_ptr().add(i))) })
    }

    /// Raw pointer to the first element, for passing the run to C.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }

    /// Writable pointer to the first element, for C calls that fill the run.
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut T::C {
        self.ptr.as_ptr().cast::<T::C>()
    }
}

/// A run of plain values is read and written element-wise. See the
/// corresponding [`CSlice`] block for why [`CElem`] does not license a `&[T]`
/// over storage a C object owns.
impl<'a, T: CElem> CSliceMut<'a, T> {
    /// Copy element `i` out; `None` if out of range.
    #[inline]
    #[must_use]
    pub fn elem(&self, i: usize) -> Option<T>
    where
        T: Copy,
    {
        self.as_ref().elem(i)
    }

    /// Write element `i`; `false` — and nothing written — if out of range.
    #[inline]
    #[must_use]
    pub fn set_elem(&mut self, i: usize, v: T) -> bool {
        if i >= self.len {
            return false;
        }
        // SAFETY: `i < len`, the slot is initialised per the constructor, and
        // this view has exclusive access to it.
        unsafe { self.ptr.as_ptr().add(i).write(v) };
        true
    }

    /// Copy the whole run into `dst`. `false` if the lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_to_slice(&self, dst: &mut [T]) -> bool
    where
        T: Copy,
    {
        self.as_ref().copy_to_slice(dst)
    }

    /// Overwrite the whole run from `src`. `false` — and nothing written — if
    /// the lengths differ.
    #[inline]
    #[must_use]
    pub fn copy_from_slice(&mut self, src: &[T]) -> bool
    where
        T: Copy,
    {
        if src.len() != self.len {
            return false;
        }
        // SAFETY: `len` slots at `ptr`, exclusive to this view, and `src` is a
        // live slice of the same length that cannot overlap them — it is a
        // Rust reference, which may not cover the C storage this views.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.as_ptr(), self.len) };
        true
    }

    /// Raw pointer to the first element, for passing the run to C.
    #[inline]
    #[must_use]
    pub fn as_elem_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Writable pointer to the first element, for C calls that fill the run.
    #[inline]
    #[must_use]
    pub fn as_mut_elem_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }
}
