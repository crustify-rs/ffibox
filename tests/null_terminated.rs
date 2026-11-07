//! `CNullTerminated` iterates a NULL-terminated array of pointers as the
//! borrows its elements become: strings as `&CStr`, objects as handles.

#![allow(missing_docs)]

use core::ffi::{CStr, c_char};
use core::ptr::null;

use ffibox::iter::CNullTerminated;
use ffibox::{CRefSeam, define_ctype};

#[repr(C)]
pub struct item_st {
    v: u32,
}
define_ctype!(Item, ItemRef, ItemMut, item_st);

#[test]
fn strings_iterate_as_c_strs() {
    let argv: [*const c_char; 4] = [c"a".as_ptr(), c"bc".as_ptr(), c"".as_ptr(), null()];
    // SAFETY: a NULL-terminated array of live strings.
    let run = unsafe { CNullTerminated::<CStr>::from_ptr(argv.as_ptr()) }.unwrap();
    assert_eq!(run.count_remaining(), 3);
    let again = run;
    assert_eq!(run.collect::<Vec<_>>(), [c"a", c"bc", c""]);
    assert_eq!(
        again.map(CStr::to_bytes).collect::<Vec<_>>(),
        [&b"a"[..], b"bc", b""]
    );
}

#[test]
fn objects_iterate_as_handles() {
    let items = [item_st { v: 4 }, item_st { v: 5 }];
    let ptrs: [*const item_st; 3] = [&raw const items[0], &raw const items[1], null()];
    // SAFETY: a NULL-terminated array of live items, unchanged while borrowed.
    let run = unsafe { CNullTerminated::<Item>::from_ptr(ptrs.as_ptr()) }.unwrap();
    // SAFETY: raw reads through the shared handles.
    let vs: Vec<u32> = run
        .map(|h: ItemRef<'_>| unsafe { (*h.as_ptr()).v })
        .collect();
    assert_eq!(vs, [4, 5]);
}

#[test]
fn an_empty_array_and_a_null_one() {
    let empty: [*const c_char; 1] = [null()];
    // SAFETY: just the terminator.
    let mut run = unsafe { CNullTerminated::<CStr>::from_ptr(empty.as_ptr()) }.unwrap();
    assert!(run.next().is_none());
    assert!(run.next().is_none(), "fused at the terminator");
    // SAFETY: NULL is refused before anything is read.
    assert!(unsafe { CNullTerminated::<CStr>::from_ptr(null()) }.is_none());
}
