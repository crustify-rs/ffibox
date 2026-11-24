//! The arguments an exported function receives from C.
//!
//! An export (`#[unsafe(no_mangle)] extern "C" fn`) receives raw pointers,
//! and its `# Safety` section, which its C callers satisfy, states what each
//! one points to. The types here let the export's signature carry that
//! contract instead: each has the ABI of the C pointer it replaces
//! (`#[repr(transparent)]` over it), so C passes the same bits, and the body
//! reads the argument with safe code.
//!
//! | C parameter | Export parameter | Rust counterpart | Read with |
//! |---|---|---|---|
//! | `const char *s`: NULL, or a string nothing writes during the call | [`CStrArg<'_>`] | `Option<&CStr>` | [`get`](CStrArg::get), [`to_bytes`](CStrArg::to_bytes) |
//! | `T *out`, written and not read | [`COut<'_, T>`] | `&mut MaybeUninit<T>` | [`write`](COut::write) |
//! | `const T *x`, read, nothing writes it during the call | `Option<&T>` | `Option<&T>` | `match` |
//! | `T *x`, the call has the only access | `Option<&mut T>` | `Option<&mut T>` | `match` |
//! | `const T *v, size_t n`, read, nothing writes it during the call | `*const T`, `usize` | `&[T]` | [`c_slice`] (`unsafe`) |
//!
//! Standard Rust already covers the two reference rows: `Option<&T>` and
//! `Option<&mut T>` are guaranteed to have the ABI of the C pointer, with
//! NULL as `None`.
//!
//! ## Aliasing
//!
//! A reference argument is protected for the whole call under the Stacked
//! and Tree Borrows models: a write to its pointee that does not go through
//! it, or a free, before the export returns, is undefined behaviour, even if
//! the reference is not used again. That includes the export's own writes
//! through another argument, for example when C lets the caller pass the
//! same object as input and as `out`. So choose `Option<&T>` only when
//! nothing (another thread, a callback, the export through another pointer)
//! writes the pointee, except through `UnsafeCell`, and nothing frees it,
//! during the call. Otherwise keep the raw pointer, and convert it where the
//! export reads it.
//!
//! [`CStrArg`] and [`COut`] form no reference to the pointee until it is
//! read or written, and then only for that read or write, so they keep the
//! raw pointer's aliasing behaviour. A `CStrArg` still promises that nothing
//! writes the string during the call. A C contract for a `const char *`
//! promises that the caller and other threads leave it alone, not that the
//! call itself does: a string that may lie in a buffer the call writes stays
//! a `*const c_char`, read where the export reads it.
//!
//! ## Inbound
//!
//! The other direction, Rust calling a C function with a string, an output
//! location or an array, is plain Rust: `&CStr::as_ptr`, `&mut T` or
//! `MaybeUninit::as_mut_ptr`, and `<[T]>::as_ptr` with the length. Rust code
//! calling an export makes these types from references, safely (`From<&CStr>`,
//! `From<&mut T>`); from a raw pointer only through their `unsafe`
//! constructors, whose contract is the C caller's.
//!
//! ```
//! use core::ffi::{c_char, c_int};
//! use ffibox::args::{COut, CStrArg};
//!
//! /// The length of `s`, written to `out`.
//! ///
//! /// # Safety
//! ///
//! /// `s` is NULL or a NUL-terminated string nothing writes during the call;
//! /// `out` is NULL or valid for a write of a `usize`.
//! #[unsafe(no_mangle)]
//! pub unsafe extern "C" fn demo_strlen(mut out: COut<'_, usize>, s: CStrArg<'_>) -> c_int {
//!     let Some(s) = s.get() else { return -1 };
//!     out.write(s.to_bytes().len());
//!     0
//! }
//!
//! let mut n = 0;
//! // SAFETY: a string and a writable `usize`, as the export requires.
//! assert_eq!(unsafe { demo_strlen(COut::from(&mut n), CStrArg::from(c"four")) }, 0);
//! assert_eq!(n, 4);
//! // SAFETY: NULL is allowed for both.
//! assert_eq!(unsafe { demo_strlen(COut::null(), CStrArg::NULL) }, -1);
//! ```

use core::ffi::{CStr, c_char};
use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;

/// A `const char *` argument: NULL, or a NUL-terminated string that stays
/// readable, and that nothing writes, for `'a`.
///
/// Its Rust counterpart is `Option<&'a CStr>`. It differs in one way:
/// nothing is read until [`get`](Self::get) or
/// [`to_bytes`](Self::to_bytes), so no `strlen` runs up front, and no
/// reference to the string exists before the export reads it.
///
/// It has the ABI of `*const c_char`. In an export's signature, `'a` is the
/// call, and the export's `# Safety` states the requirement above as its C
/// callers' contract.
///
/// A `CStrArg` made from a string does not outlive it:
///
/// ```compile_fail,E0597
/// use ffibox::args::CStrArg;
/// let arg;
/// {
///     let s = std::ffi::CString::new("x").unwrap();
///     arg = CStrArg::from(s.as_c_str());
/// }
/// let _ = arg.get();
/// ```
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct CStrArg<'a> {
    // Invariant, established by every constructor: NULL, or a NUL-terminated
    // string readable, and written by nothing, for `'a`.
    ptr: *const c_char,
    _life: PhantomData<&'a CStr>,
}

impl<'a> CStrArg<'a> {
    /// The NULL string.
    pub const NULL: CStrArg<'static> = CStrArg {
        ptr: ptr::null(),
        _life: PhantomData,
    };

    /// `p` as a string argument.
    ///
    /// # Safety
    ///
    /// `p` is NULL, or points to a NUL-terminated string that stays
    /// readable, and that nothing writes, for `'a`.
    #[inline]
    #[must_use]
    pub const unsafe fn from_ptr(p: *const c_char) -> Self {
        CStrArg {
            ptr: p,
            _life: PhantomData,
        }
    }

    /// The pointer C passed.
    #[inline]
    #[must_use]
    pub const fn as_ptr(self) -> *const c_char {
        self.ptr
    }

    /// Whether C passed NULL.
    #[inline]
    #[must_use]
    pub fn is_null(self) -> bool {
        self.ptr.is_null()
    }

    /// The string, or `None` for NULL.
    #[inline]
    #[must_use]
    pub fn get(self) -> Option<&'a CStr> {
        if self.ptr.is_null() {
            None
        } else {
            // SAFETY: the type's invariant: a non-null `ptr` is a
            // NUL-terminated string readable and unwritten for `'a`.
            Some(unsafe { CStr::from_ptr(self.ptr) })
        }
    }

    /// The string, or `""` for NULL.
    #[inline]
    #[must_use]
    pub fn or_empty(self) -> &'a CStr {
        self.get().unwrap_or(c"")
    }

    /// The string's bytes without the NUL; empty for NULL.
    #[inline]
    #[must_use]
    pub fn to_bytes(self) -> &'a [u8] {
        match self.get() {
            Some(s) => s.to_bytes(),
            None => &[],
        }
    }
}

impl<'a> From<&'a CStr> for CStrArg<'a> {
    #[inline]
    fn from(s: &'a CStr) -> Self {
        CStrArg {
            ptr: s.as_ptr(),
            _life: PhantomData,
        }
    }
}

impl<'a> From<Option<&'a CStr>> for CStrArg<'a> {
    #[inline]
    fn from(s: Option<&'a CStr>) -> Self {
        s.map_or(CStrArg::NULL, CStrArg::from)
    }
}

impl<'a> From<CStrArg<'a>> for Option<&'a CStr> {
    #[inline]
    fn from(s: CStrArg<'a>) -> Self {
        s.get()
    }
}

impl fmt::Debug for CStrArg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// A `T *out` argument that the export writes and does not read: NULL, or
/// valid for writes of a `T` for `'a`.
///
/// Its Rust counterpart is `&'a mut MaybeUninit<T>`. It differs in two ways.
/// [`write`](Self::write) does nothing for NULL, which is what C's optional
/// out-parameters ask. And no reference to the pointee is formed, so the
/// pointer may alias another argument, and each write happens where the
/// export makes it. The old value is neither read nor dropped, so `T` may be
/// uninitialised storage.
///
/// It has the ABI of `*mut T`.
///
/// It is not `Copy` or `Clone`, as `&mut` is not, and it holds the
/// location's borrow:
///
/// ```compile_fail,E0499
/// use ffibox::args::COut;
/// let mut x = 0u32;
/// let mut a = COut::from(&mut x);
/// let mut b = COut::from(&mut x);
/// a.write(1);
/// b.write(2);
/// ```
#[repr(transparent)]
pub struct COut<'a, T> {
    // Invariant, established by every constructor: NULL, or aligned and
    // valid for writes of a `T` for `'a`, with no reference to the pointee
    // live while this value writes it.
    ptr: *mut T,
    _life: PhantomData<&'a mut T>,
}

impl<T> COut<'_, T> {
    /// `p` as an out-parameter.
    ///
    /// # Safety
    ///
    /// `p` is NULL, or aligned and valid for writes of a `T` for the
    /// result's lifetime, and no reference to `*p` is live while the result
    /// writes it.
    #[inline]
    #[must_use]
    pub const unsafe fn from_ptr(p: *mut T) -> Self {
        COut {
            ptr: p,
            _life: PhantomData,
        }
    }

    /// The NULL out-parameter: nothing is written.
    #[inline]
    #[must_use]
    pub const fn null() -> Self {
        COut {
            ptr: ptr::null_mut(),
            _life: PhantomData,
        }
    }

    /// The pointer C passed.
    #[inline]
    #[must_use]
    pub const fn as_ptr(&self) -> *mut T {
        self.ptr
    }

    /// Whether C passed NULL.
    #[inline]
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// Store `value`, C's `*out = value`. Nothing happens for NULL: an
    /// export whose C requires the out-parameter rejects NULL first, with
    /// [`is_null`](Self::is_null).
    #[inline]
    pub fn write(&mut self, value: T) {
        if !self.ptr.is_null() {
            // SAFETY: the type's invariant: a non-null `ptr` is aligned and
            // valid for writes of a `T` for `'a`, which this borrow of
            // `self` is within, and no reference covers the pointee.
            unsafe { self.ptr.write(value) };
        }
    }

    /// A shorter-lived out-parameter over the same pointer, to hand on, as
    /// a `&mut` reborrow.
    #[inline]
    pub fn reborrow(&mut self) -> COut<'_, T> {
        COut {
            ptr: self.ptr,
            _life: PhantomData,
        }
    }
}

impl<'a, T> From<&'a mut T> for COut<'a, T> {
    #[inline]
    fn from(r: &'a mut T) -> Self {
        COut {
            ptr: r,
            _life: PhantomData,
        }
    }
}

impl<'a, T> From<&'a mut MaybeUninit<T>> for COut<'a, T> {
    #[inline]
    fn from(r: &'a mut MaybeUninit<T>) -> Self {
        COut {
            ptr: r.as_mut_ptr(),
            _life: PhantomData,
        }
    }
}

impl<T> fmt::Debug for COut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("COut").field(&self.ptr).finish()
    }
}

/// The `len` elements at `ptr`, for a C `(const T *ptr, size_t len)` pair:
/// empty when `len` is 0 or `ptr` is NULL, whatever the other is.
///
/// Its Rust counterpart is [`core::slice::from_raw_parts`], which needs a
/// non-null, aligned pointer even for an empty slice; C passes NULL with 0.
///
/// A length whose byte size exceeds `isize::MAX`, which no readable range
/// can have but a C `size_t` can carry, panics rather than form the slice.
/// An export's panic barrier (`callback::barrier`) turns the panic into the
/// export's error.
///
/// # Safety
///
/// When `len > 0` and `ptr` is not NULL: `ptr` is aligned for `T` and valid
/// for reads of `len` initialized `T`s for `'a`, and nothing writes them
/// during `'a`, except inside an `UnsafeCell` of `T`.
///
/// # Panics
///
/// If `ptr` is not NULL and `len` elements of `T` take more than
/// `isize::MAX` bytes.
///
/// ```
/// # use core::ptr;
/// let v = [1u32, 2, 3];
/// // SAFETY: `v` is readable for 3 elements, and nothing writes it.
/// assert_eq!(unsafe { ffibox::args::c_slice(v.as_ptr(), 3) }, &[1, 2, 3]);
/// // SAFETY: NULL is never read.
/// assert!(unsafe { ffibox::args::c_slice::<u32>(ptr::null(), 0) }.is_empty());
/// ```
#[inline]
pub unsafe fn c_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 || ptr.is_null() {
        return &[];
    }
    assert!(
        len <= isize::MAX as usize / size_of::<T>().max(1),
        "length beyond isize::MAX bytes"
    );
    // SAFETY: `ptr` is non-null, and the caller guarantees it aligned and
    // valid for reads of `len` initialized `T`s, unwritten for `'a`; their
    // size, `len * size_of::<T>()`, is at most `isize::MAX` (checked).
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cstr_arg_reads_lazily_and_null_is_empty() {
        let s = CStrArg::from(c"abc");
        assert_eq!(s.to_bytes(), b"abc");
        assert_eq!(s.get(), Some(c"abc"));
        assert_eq!(Option::<&CStr>::from(s), Some(c"abc"));
        assert!(CStrArg::NULL.is_null());
        assert_eq!(CStrArg::NULL.get(), None);
        assert_eq!(CStrArg::NULL.to_bytes(), b"");
        assert_eq!(CStrArg::NULL.or_empty(), c"");
        assert_eq!(CStrArg::from(None).as_ptr(), ptr::null());
    }

    #[test]
    fn out_writes_and_skips_null() {
        let mut x = 1u32;
        COut::from(&mut x).write(7);
        assert_eq!(x, 7);
        let mut slot = MaybeUninit::<u64>::uninit();
        let mut o = COut::from(&mut slot);
        o.reborrow().write(3);
        o.write(4);
        // SAFETY: written just above.
        assert_eq!(unsafe { slot.assume_init() }, 4);
        let mut n = COut::<u32>::null();
        assert!(n.is_null());
        n.write(5);
    }

    #[test]
    fn c_slice_maps_null_and_zero_to_empty() {
        let v = [1u32, 2, 3];
        // SAFETY: `v` is readable for 3 elements and unwritten.
        assert_eq!(unsafe { c_slice(v.as_ptr(), 3) }, &[1, 2, 3]);
        // SAFETY: NULL forms no slice of the pointee.
        assert!(unsafe { c_slice::<u32>(ptr::null(), 5) }.is_empty());
        // SAFETY: 0 forms no slice of the pointee.
        assert!(unsafe { c_slice::<u32>(ptr::dangling(), 0) }.is_empty());
    }

    #[test]
    #[should_panic(expected = "isize::MAX")]
    fn c_slice_rejects_lengths_beyond_isize_max() {
        let v = [0u64; 1];
        // SAFETY: the length check panics before any slice is formed.
        let _ = unsafe { c_slice(v.as_ptr(), usize::MAX / 8) };
    }

    // The ABI exports rely on: C's pointers pass as these types. Miri checks
    // the call's ABI compatibility.
    unsafe extern "C" fn takes(s: CStrArg<'_>, mut o: COut<'_, usize>, r: Option<&u8>) -> usize {
        o.write(s.to_bytes().len());
        r.map_or(0, |v| usize::from(*v))
    }

    #[test]
    fn abi_matches_raw_pointers() {
        type Raw = unsafe extern "C" fn(*const c_char, *mut usize, *const u8) -> usize;
        type Typed = unsafe extern "C" fn(CStrArg<'_>, COut<'_, usize>, Option<&u8>) -> usize;
        let typed: Typed = takes;
        // SAFETY: each argument type has the ABI of the raw pointer:
        // `#[repr(transparent)]` over it, and `Option<&u8>` is guaranteed to
        // be `*const u8`.
        let f: Raw = unsafe { core::mem::transmute::<Typed, Raw>(typed) };
        let mut n = 0usize;
        let b = 9u8;
        // SAFETY: a string, a writable `usize`, a readable `u8`.
        let r = unsafe { f(c"four".as_ptr(), &raw mut n, &raw const b) };
        assert_eq!((n, r), (4, 9));
        // SAFETY: a NULL string and reference are allowed; `n` is writable.
        let r = unsafe { f(ptr::null(), &raw mut n, ptr::null()) };
        assert_eq!((n, r), (0, 0));
    }

    #[test]
    fn sizes_are_one_pointer() {
        assert_eq!(size_of::<CStrArg<'_>>(), size_of::<*const c_char>());
        assert_eq!(size_of::<COut<'_, u64>>(), size_of::<*mut u64>());
    }
}
