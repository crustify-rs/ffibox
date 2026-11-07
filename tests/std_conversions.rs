//! The owners convert to and compare like their standard counterparts.

#![allow(missing_docs)]

use core::ffi::CStr;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;

use ffibox::{CPayload, CStrBox, CVec, RustAlloc};

#[test]
fn a_string_derefs_compares_and_hashes_by_content() {
    let a: CStrBox<RustAlloc> = CStrBox::from_c_str(c"key").unwrap();
    let b: CStrBox<RustAlloc> = CStrBox::from_c_str(c"key").unwrap();
    assert_eq!(a, b);
    assert_eq!(a, *c"key");
    assert_eq!(a, c"key");
    assert_eq!(a.to_bytes(), b"key", "CStr methods through Deref");
    assert!(CStrBox::<RustAlloc>::from_c_str(c"a").unwrap() < a);

    let mut m: HashMap<CStrBox<RustAlloc>, u32> = HashMap::new();
    m.insert(a, 1);
    assert_eq!(
        m.get(c"key"),
        Some(&1),
        "looked up by `&CStr`, through Borrow"
    );
    let s: HashSet<_> = [b].into_iter().collect();
    assert!(s.contains(c"key"));
}

#[test]
fn a_string_converts_into_a_cstring() {
    let a: CStrBox<RustAlloc> = CStrBox::from_c_str(c"owned").unwrap();
    let c: CString = a.into();
    assert_eq!(c.as_c_str(), c"owned");
    let x: CStrBox<RustAlloc> = CStrBox::from_c_str(c"x").unwrap();
    let r: &CStr = x.as_ref();
    assert_eq!(r.to_owned(), CString::from(c"x"));
}

#[test]
fn a_buffer_derefs_to_a_slice_and_compares_element_wise() {
    let mut v: CVec<u32, RustAlloc> = CVec::from_slice(&[3, 1, 2]).unwrap();
    v.sort_unstable();
    assert_eq!(&v[..], &[1, 2, 3]);
    assert_eq!(v.iter().sum::<u32>(), 6);
    for x in &mut v {
        *x *= 2;
    }
    assert_eq!(v, *[2, 4, 6].as_slice());
    let w: CVec<u32, RustAlloc> = CVec::from_fn(3, |i| (i as u32 + 1) * 2).unwrap();
    assert_eq!(v, w);
    let total: u32 = (&w).into_iter().copied().sum();
    assert_eq!(total, 12);
    let h: HashSet<CVec<u32, RustAlloc>> = [v].into_iter().collect();
    assert!(h.contains(&w));
    assert_eq!(w.to_vec(), vec![2, 4, 6]);
}

#[test]
fn a_payload_comes_from_and_goes_back_to_a_box() {
    let p: CPayload<String> = Box::new(String::from("v")).into();
    let raw = p.into_raw();
    // SAFETY: taken back once.
    let b: Box<String> = unsafe { CPayload::<String>::from_raw(raw) }.into_box();
    assert_eq!(*b, "v");
}
