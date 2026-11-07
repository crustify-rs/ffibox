//! Foreign iteration conventions as Rust iterators.
//!
//! A C API that hands out a sequence follows one of a few conventions, and
//! each is reached as an [`Iterator`]:
//!
//! | C convention | Rust |
//! |--------------|------|
//! | an array and its count | [`CVec::as_slice`](crate::CVec::as_slice) / [`as_handles`](crate::CVec::as_handles), or a [`CSlice`](crate::CSlice) when borrowed |
//! | a NULL-terminated array of pointers (`char **argv`, `foo **list`) | [`CNullTerminated`] |
//! | a `count` and a `get(i)` | `(0..count).map(get)` |
//! | a `->next` chain | [`core::iter::successors`] over the handles' `next` getter |
//! | a cursor advanced by `next(it)` until NULL | [`core::iter::from_fn`] over a safe `next` wrapper |
//!
//! The last three need no primitive: once the binding wraps `get` or `next`
//! safely, the standard adapters build the iterator.

use core::ffi::{CStr, c_char};
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::traits::CCell;

/// What a [`CNullTerminated`] array points at, and the borrow each element
/// becomes: a wrapped C object's shared handle, or a [`CStr`] for a string.
pub trait CNullTermElem {
    /// The C type each slot points at.
    type Ptr;

    /// The borrow an element becomes.
    type Item<'a>
    where
        Self: 'a;

    /// Borrow the pointee of one slot.
    ///
    /// # Safety
    ///
    /// `ptr` addresses a live pointee that outlives `'a`, which nothing writes
    /// meanwhile.
    #[doc(hidden)]
    unsafe fn borrow<'a>(ptr: NonNull<Self::Ptr>) -> Self::Item<'a>
    where
        Self: 'a;
}

impl<T: CCell> CNullTermElem for T {
    type Ptr = T::C;
    type Item<'a>
        = T::Ref<'a>
    where
        T: 'a;

    #[inline]
    unsafe fn borrow<'a>(ptr: NonNull<T::C>) -> T::Ref<'a>
    where
        T: 'a,
    {
        // SAFETY: `T` is transparent over `T::C` (`CCell`'s contract); the
        // caller upholds liveness for `'a`.
        unsafe { crate::refs::handle_ref(ptr.cast::<T>()) }
    }
}

impl CNullTermElem for CStr {
    type Ptr = c_char;
    type Item<'a> = &'a CStr;

    #[inline]
    unsafe fn borrow<'a>(ptr: NonNull<c_char>) -> &'a CStr {
        // SAFETY: the caller passes a live NUL-terminated string that nothing
        // writes for `'a`.
        unsafe { CStr::from_ptr(ptr.as_ptr()) }
    }
}

/// A borrowed, NULL-terminated array of pointers, iterated as the borrows its
/// elements become: `char **` as `&CStr`s, `foo **` as `FooRef`s.
///
/// ```
/// use core::ffi::{CStr, c_char};
/// use ffibox::iter::CNullTerminated;
///
/// let names: [*const c_char; 3] = [c"lo".as_ptr(), c"eth0".as_ptr(), core::ptr::null()];
/// // SAFETY: a NULL-terminated array of live strings, unchanged while borrowed.
/// let run = unsafe { CNullTerminated::<CStr>::from_ptr(names.as_ptr()) }.unwrap();
/// let v: Vec<&CStr> = run.collect();
/// assert_eq!(v, [c"lo", c"eth0"]);
/// ```
///
/// Each step reads the next slot; the array is not copied. `Copy`, like a
/// [`CSlice`](crate::CSlice), and so a fresh iteration from the start is a
/// copy taken before the first step.
pub struct CNullTerminated<'a, T: CNullTermElem + ?Sized> {
    next: NonNull<*const T::Ptr>,
    _borrow: PhantomData<&'a T>,
}

impl<'a, T: CNullTermElem + ?Sized> CNullTerminated<'a, T> {
    /// Borrow the array at `ptr`; `None` if `ptr` itself is NULL.
    ///
    /// # Safety
    ///
    /// `ptr` addresses an array of pointers ended by a NULL one, each before
    /// it addressing a live pointee of `T::Ptr`, and neither the array nor
    /// the pointees are written or freed while `'a` lasts.
    #[inline]
    pub unsafe fn from_ptr(ptr: *const *const T::Ptr) -> Option<Self> {
        NonNull::new(ptr.cast_mut()).map(|next| Self {
            next,
            _borrow: PhantomData,
        })
    }

    /// The number of elements before the terminator, counted from the
    /// current position: O(n).
    #[inline]
    #[must_use]
    pub fn count_remaining(&self) -> usize {
        (*self).count()
    }
}

impl<T: CNullTermElem + ?Sized> Clone for CNullTerminated<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: CNullTermElem + ?Sized> Copy for CNullTerminated<'_, T> {}

impl<'a, T: CNullTermElem + ?Sized + 'a> Iterator for CNullTerminated<'a, T> {
    type Item = T::Item<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: `next` lies within the array, at or before its terminator
        // (`from_ptr`'s contract, and we stop at the terminator).
        let slot = unsafe { self.next.as_ptr().read() };
        let ptr = NonNull::new(slot.cast_mut())?;
        // SAFETY: the slot was not the terminator, so the next one is still
        // within the array.
        self.next = unsafe { self.next.add(1) };
        // SAFETY: a non-NULL slot addresses a live pointee for `'a`.
        Some(unsafe { T::borrow(ptr) })
    }
}

impl<'a, T: CNullTermElem + ?Sized + 'a> core::iter::FusedIterator for CNullTerminated<'a, T> {}

impl<T: CNullTermElem + ?Sized> core::fmt::Debug for CNullTerminated<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CNullTerminated")
            .field("next", &self.next)
            .finish()
    }
}

// SAFETY: a shared view of pointees, like `&[&T]`: sending or sharing it
// shares them, which `T: Sync` permits; the array itself is only read.
unsafe impl<T: CNullTermElem + ?Sized + Sync> Send for CNullTerminated<'_, T> {}
// SAFETY: as above.
unsafe impl<T: CNullTermElem + ?Sized + Sync> Sync for CNullTerminated<'_, T> {}
