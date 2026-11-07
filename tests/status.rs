//! Each C error convention becomes a `Result` with the binding's own error.

#![allow(missing_docs)]

use core::ffi::{c_int, c_long};
use core::ptr::{NonNull, null_mut};

use ffibox::status;

#[derive(Debug, PartialEq)]
struct Error(i64);
impl From<c_int> for Error {
    fn from(code: c_int) -> Self {
        Error(code.into())
    }
}
impl From<c_long> for Error {
    fn from(code: c_long) -> Self {
        Error(code)
    }
}

/// Stand-ins for C routines, each mapped with `?` into `Error`.
fn count(rc: c_int) -> Result<c_int, Error> {
    Ok(status::nonneg(rc)?)
}
fn seek(rc: c_long) -> Result<c_long, Error> {
    Ok(status::nonneg(rc)?)
}
fn lock(rc: c_int) -> Result<(), Error> {
    Ok(status::zero(rc)?)
}

#[test]
fn a_negative_return_is_an_error_code() {
    assert_eq!(count(0), Ok(0));
    assert_eq!(count(17), Ok(17));
    assert_eq!(count(-5), Err(Error(-5)));
    assert_eq!(seek(-1), Err(Error(-1)));
}

#[test]
fn a_non_zero_return_is_an_error_code() {
    assert_eq!(lock(0), Ok(()));
    assert_eq!(lock(11), Err(Error(11)));
    assert_eq!(lock(-3), Err(Error(-3)));
}

#[test]
fn a_null_pointer_takes_its_error_from_elsewhere() {
    let mut x = 3u8;
    let p = &raw mut x;
    assert_eq!(
        status::non_null(p, || Error(0)),
        Ok(NonNull::new(p).unwrap())
    );
    assert_eq!(
        status::non_null(null_mut::<u8>(), || Error(-12)),
        Err(Error(-12))
    );
}

#[test]
#[cfg_attr(miri, ignore = "opens a file to set errno")]
fn minus_one_takes_its_error_from_errno() {
    assert_eq!(status::errno::<c_int>(4).unwrap(), 4);
    // Set errno through a failing call the standard library makes.
    let _ = std::fs::File::open("/nonexistent/ffibox-status-test");
    let e = status::errno::<c_int>(-1).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
}
