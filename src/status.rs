//! A C library's error convention as a [`Result`].
//!
//! A C routine reports failure through its return value, in one of a few
//! conventions; each is one function here, turning the return value into a
//! `Result` whose error is the code itself, so that `?` converts it into the
//! binding's own error type through its [`From`] impl:
//!
//! | Convention | Function | Typical of |
//! |------------|----------|------------|
//! | negative is an error code, the rest a value | [`nonneg`] | `AVERROR`, libgit2's `GIT_E*` |
//! | zero is success, anything else an error code | [`zero`] | `pthread_*`, `ERR_*`-style codes |
//! | NULL is failure, the error kept elsewhere | [`non_null`] | constructors, lookups |
//! | `-1` is failure, the error in `errno` | [`errno`] | POSIX |
//!
//! ```
//! use core::ffi::c_int;
//! use ffibox::status;
//!
//! #[derive(Debug, PartialEq)]
//! pub struct Error(c_int);
//! impl From<c_int> for Error {
//!     fn from(code: c_int) -> Self {
//!         Error(code)
//!     }
//! }
//!
//! fn read_into(rc: c_int) -> Result<usize, Error> {
//!     // `rc` stands in for a C routine returning a count or a negative code.
//!     let n = status::nonneg(rc)?;
//!     Ok(n as usize)
//! }
//! assert_eq!(read_into(5), Ok(5));
//! assert_eq!(read_into(-22), Err(Error(-22)));
//! ```

use core::ptr::NonNull;

/// A return value that is a non-negative result or a negative error code:
/// `Ok(ret)` when `ret >= 0`, `Err(ret)` otherwise.
#[inline]
pub fn nonneg<T>(ret: T) -> Result<T, T>
where
    T: Copy + PartialOrd + Default,
{
    if ret < T::default() {
        Err(ret)
    } else {
        Ok(ret)
    }
}

/// A return value that is zero on success and an error code otherwise:
/// `Ok(())` when `ret == 0`, `Err(ret)` otherwise.
#[inline]
pub fn zero<T>(ret: T) -> Result<(), T>
where
    T: Copy + PartialEq + Default,
{
    if ret == T::default() {
        Ok(())
    } else {
        Err(ret)
    }
}

/// A pointer that is NULL on failure, the error kept elsewhere (`errno`, a
/// library's last-error getter), which `err` reads.
///
/// For an owned result, adopt the pointer afterwards, or use the owner's
/// `from_c` and turn its `None` into the error.
#[inline]
pub fn non_null<T, E>(ptr: *mut T, err: impl FnOnce() -> E) -> Result<NonNull<T>, E> {
    NonNull::new(ptr).ok_or_else(err)
}

/// A return value that is `-1` on failure, the error in `errno`, as POSIX
/// routines report it: `Ok(ret)` otherwise.
///
/// Call it straight after the routine: anything run in between may change
/// `errno`.
#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
#[inline]
pub fn errno<T>(ret: T) -> std::io::Result<T>
where
    T: Copy + PartialEq + From<i8>,
{
    if ret == T::from(-1) {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}
