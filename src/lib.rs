//! # ffibox
//!
//! Generic smart pointers and traits for building safe Rust wrappers over C
//! types — opaque handles, and also structs you need field access to and a
//! C-ABI layout for.
//!
//! The [README] is the primary guide: the types a wrapper crate gets, how
//! teardown policies work, and a decision procedure from a C declaration to
//! its wrapper. This page is the API reference.
//!
//! ## Two rules
//!
//! - **No reference to a wrapped C object is ever formed.** A `&Wrapper` over
//!   the object's bytes would assert `noalias` / `readonly` over memory C may
//!   write, so access goes through handles that hold the pointer by value (see
//!   [`refs`]).
//! - **Teardown is a policy, not a property of `T`.** [`CDrop`] and its
//!   siblings are implemented on a policy type the owner stores inline, so an
//!   owner cannot exist without a destructor and one C type can have several
//!   (see [`traits`]).
//!
//! ## The pieces
//!
//! | Item | Role |
//! |------|------|
//! | [`define_ctype!`] | per C type: the layout type `Foo` and its handles `FooRef<'a>` / `FooMut<'a>` |
//! | [`CBox<T, D>`] | the sole owner of a C-allocated object of a [`CCell`] layout type |
//! | [`CArc<T, D>`] | one counted reference to a refcounted object; [`lock`](CArc::lock) takes the C lock a [`CGuarded`] object carries |
//! | [`CGuard<'a, T>`] | a held C lock, released on drop; hands out the `FooLocked<'a>` handle |
//! | [`CGuardedRef<'a, T>`] | a borrow reached only through the object's lock, never released — a C global under its C lock |
//! | [`CStrBox<D>`] | an owned NUL-terminated `char *` |
//! | [`CVec<T, S>`] | an owned `(ptr, len)` buffer |
//! | [`CVal<T, D>`] | a C struct held by value, disposed on drop |
//! | [`CSlice<'a, T>`] / [`CSliceMut<'a, T>`] | a borrowed run of elements |
//! | `impl_*!` | bind a C routine to a policy |
//!
//! Like `Box`, the owners adopt a raw pointer with an `unsafe` `from_raw` and
//! give one out with a safe `into_raw` / `as_ptr`. A type alias names an owner with its policy.
//! A policy that also constructs gives [`CBox`] and [`CArc`] safe constructors
//! that allocate through it: `new` ([`CNew`], a C `*_alloc`), `new_zeroed`
//! ([`CAllocZeroed`], for a [`CZeroable`] type) and `new_uninit` ([`CAlloc`],
//! filled in place by C and promoted with `assume_init`).
//!
//! ## Quick example
//!
//! ```
//! use ffibox::{define_ctype, impl_cdrop, impl_cdupclone, CBox};
//!
//! mod ffi {
//!     #[repr(C)]
//!     pub struct point_st { pub x: i32, pub y: i32 }
//!     // Stand-ins for a C library's constructor, copy and destructor.
//!     pub unsafe extern "C" fn point_new(x: i32, y: i32) -> *mut point_st {
//!         Box::into_raw(Box::new(point_st { x, y }))
//!     }
//!     pub unsafe extern "C" fn point_dup(p: *mut point_st) -> *mut point_st {
//!         unsafe { point_new((*p).x, (*p).y) }
//!     }
//!     pub unsafe extern "C" fn point_free(p: *mut point_st) {
//!         drop(unsafe { Box::from_raw(p) });
//!     }
//! }
//!
//! define_ctype!(Point, PointRef, PointMut, ffi::point_st);
//!
//! #[derive(Clone, Copy, Debug, Default)]
//! pub struct PointFree;
//! impl_cdrop!(PointFree, Point, ffi::point_free);
//! impl_cdupclone!(PointFree, Point, ffi::point_dup);
//!
//! pub type PointBox = CBox<Point, PointFree>;
//!
//! impl Point {
//!     pub fn new(x: i32, y: i32) -> Option<PointBox> {
//!         // SAFETY: point_new returns an owned object or NULL.
//!         unsafe { PointBox::from_c(ffi::point_new(x, y)) }
//!     }
//! }
//! impl PointRef<'_> {
//!     pub fn x(&self) -> i32 {
//!         // SAFETY: a read through the raw pointer; no reference is formed.
//!         unsafe { core::ptr::addr_of!((*self.as_ptr()).x).read() }
//!     }
//! }
//! impl PointMut<'_> {
//!     pub fn set_x(&mut self, v: i32) {
//!         // SAFETY: as above, through the exclusive handle.
//!         unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
//!     }
//! }
//!
//! let mut p = Point::new(3, 4).unwrap();
//! p.as_mut().set_x(5);
//! let q = p.clone();                  // point_dup
//! assert_eq!(q.as_ref().x(), 5);
//! // point_free runs twice here.
//! ```
//!
//! ## `no_std`
//!
//! `#![no_std]`, and nothing allocates. The `std` feature (default) selects
//! [`std::process::abort`](https://doc.rust-lang.org/std/process/fn.abort.html) for the unrecoverable-failure path; without it that
//! path is a double-panic.
//!
//! [README]: https://github.com/crustify-rs/ffibox#readme

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "std")]
extern crate std;

pub mod macros;
pub mod refs;
pub mod shared;
pub mod traits;

// Re-export the primary items at the crate root for convenience.
// Compiles the README's examples, so they cannot drift from the API. They are
// `no_run`: their `sys` modules declare C routines that are never linked.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

pub use crate::refs::{CBorrowedPtr, CBox, CSlice, CSliceMut, CStrBox, CVal, CVec};
pub use crate::shared::{CArc, CGuard, CGuardedRef};
pub use crate::traits::{
    CAlloc, CAllocZeroed, CCell, CDispose, CDrop, CDupClone, CGuarded, CGuardedAll, CLenClone,
    CLenDrop, CNew, CPlainElem, CRefClone, CZeroable, LockFields, LockScope, LockWhole,
};
