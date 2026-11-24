//! The safe interface of `ffibox::args`, as a consumer calling an export
//! from Rust uses it: no `unsafe` (goal G2).
#![forbid(unsafe_code)]

use core::ffi::CStr;
use core::mem::MaybeUninit;

use ffibox::args::{COut, CStrArg};

fn len_of(s: CStrArg<'_>, mut out: COut<'_, usize>) -> bool {
    match s.get() {
        Some(s) => {
            out.write(s.to_bytes().len());
            true
        }
        None => false,
    }
}

#[test]
fn strings_and_outputs_from_references() {
    let mut n = 0;
    assert!(len_of(CStrArg::from(c"hello"), COut::from(&mut n)));
    assert_eq!(n, 5);
    let mut slot = MaybeUninit::<usize>::uninit();
    assert!(len_of(CStrArg::from(Some(c"ab")), COut::from(&mut slot)));
    assert!(!len_of(CStrArg::NULL, COut::null()));
    assert_eq!(Option::<&CStr>::from(CStrArg::from(c"x")), Some(c"x"));
    assert_eq!(format!("{:?}", CStrArg::from(c"x")), "Some(\"x\")");
}
