//! A `CVec` drops its elements, then frees its buffer, as a `Vec` does: a
//! buffer of owners (`foo **`) releases each slot through the element type's
//! own policy. `from_fn` and `new_uninit` build buffers through a
//! `CLenAlloc` policy, cleaning up after a constructor that panics part-way.

#![allow(missing_docs)]

use core::cell::{Cell, RefCell};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::vec::Vec;

use ffibox::{
    CBox, CDispose, CDrop, CLenAlloc, CLenDrop, CStrBox, CVal, CVec, CZeroable, RustAlloc,
    define_ctype,
};

#[repr(C)]
pub struct elem_st {
    id: u32,
}
define_ctype!(Elem, ElemRef, ElemMut, elem_st);
// SAFETY: any id is valid, zero included.
unsafe impl CZeroable for Elem {}

thread_local! {
    /// What the policies did, in order: `elem <id>`, `dispose <id>`, `buf`.
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static BUFS_LIVE: Cell<isize> = const { Cell::new(0) };
}

fn log(s: String) {
    LOG.with(|l| l.borrow_mut().push(s));
}

fn take_log() -> Vec<String> {
    LOG.with(|l| core::mem::take(&mut *l.borrow_mut()))
}

/// Frees an `Elem` from `RustAlloc`, logging its id.
#[derive(Clone, Copy, Debug, Default)]
struct ElemFree;
// SAFETY: delegates to `RustAlloc`, which allocated every element here.
unsafe impl CDrop<Elem> for ElemFree {
    unsafe fn c_drop(&self, ptr: NonNull<Elem>) {
        // SAFETY: the caller owns a live `Elem`; reading its id is a raw read.
        let id = unsafe { (*ptr.as_ptr().cast::<elem_st>()).id };
        log(format!("elem {id}"));
        // SAFETY: allocated by `RustAlloc`.
        unsafe { CDrop::<Elem>::c_drop(&RustAlloc, ptr) }
    }
}

/// Disposes an inline `Elem`, logging its id.
#[derive(Clone, Copy, Debug, Default)]
struct ElemDispose;
// SAFETY: owns nothing to release; only logs.
unsafe impl CDispose<Elem> for ElemDispose {
    unsafe fn c_dispose(&self, ptr: NonNull<Elem>) {
        // SAFETY: the caller owns a live value.
        let id = unsafe { (*ptr.as_ptr().cast::<elem_st>()).id };
        log(format!("dispose {id}"));
    }
}

/// A `RustAlloc` buffer policy that logs its frees and counts live buffers.
#[derive(Clone, Copy, Debug, Default)]
struct Buf;
// SAFETY: delegates to `RustAlloc`.
unsafe impl CLenDrop for Buf {
    unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
        log("buf".into());
        BUFS_LIVE.with(|n| n.set(n.get() - 1));
        // SAFETY: allocated by `RustAlloc` with this length.
        unsafe { RustAlloc.c_drop_len(ptr, byte_len) }
    }
}
// SAFETY: delegates to `RustAlloc`, freed by `c_drop_len` above.
unsafe impl CLenAlloc for Buf {
    const ALIGN: usize = RustAlloc::BUF_ALIGN;
    fn c_alloc_len(&self, byte_len: usize) -> Option<NonNull<u8>> {
        BUFS_LIVE.with(|n| n.set(n.get() + 1));
        RustAlloc.c_alloc_len(byte_len)
    }
}

fn elem(id: u32) -> CBox<Elem, ElemFree> {
    let b: CBox<Elem, RustAlloc> = CBox::new_zeroed().unwrap();
    let raw = b.into_raw();
    // SAFETY: a live `Elem` we own; the id write is a raw write.
    unsafe { (*raw.cast::<elem_st>()).id = id };
    // SAFETY: `ElemFree` releases `RustAlloc` objects.
    unsafe { CBox::from_raw(raw) }.unwrap()
}

#[test]
fn dropping_a_buffer_of_owners_releases_each_slot_then_the_buffer() {
    take_log();
    let v: CVec<Option<CBox<Elem, ElemFree>>, Buf> =
        CVec::from_fn(4, |i| (i != 2).then(|| elem(i as u32))).unwrap();
    assert_eq!(v.len(), 4);
    assert!(v.as_slice()[2].is_none());
    drop(v);
    assert_eq!(take_log(), ["elem 0", "elem 1", "elem 3", "buf"]);
    assert_eq!(BUFS_LIVE.with(Cell::get), 0);
}

#[test]
fn an_owner_taken_out_of_the_slice_is_released_by_its_new_holder() {
    take_log();
    let mut v: CVec<Option<CBox<Elem, ElemFree>>, Buf> =
        CVec::from_fn(2, |i| Some(elem(i as u32))).unwrap();
    let first = v.as_mut_slice()[0].take().unwrap();
    drop(v);
    assert_eq!(take_log(), ["elem 1", "buf"]);
    drop(first);
    assert_eq!(take_log(), ["elem 0"]);
}

#[test]
fn a_buffer_of_values_disposes_each_in_place() {
    take_log();
    let v: CVec<CVal<Elem, ElemDispose>, Buf> = CVec::from_fn(3, |i| {
        let mut e = Elem::zeroed();
        // SAFETY: a raw write into a value we own.
        unsafe { (*core::ptr::from_mut(&mut e).cast::<elem_st>()).id = i as u32 };
        // SAFETY: the value owns nothing, which `ElemDispose` accepts.
        unsafe { CVal::new(e) }
    })
    .unwrap();
    drop(v);
    assert_eq!(take_log(), ["dispose 0", "dispose 1", "dispose 2", "buf"]);
}

#[test]
fn a_buffer_of_owned_strings_frees_each_string() {
    let v: CVec<Option<CStrBox<RustAlloc>>, RustAlloc> = CVec::from_fn(3, |i| match i {
        0 => CStrBox::from_c_str(c"alpha"),
        1 => None,
        _ => CStrBox::from_c_str(c"gamma"),
    })
    .unwrap();
    let names: Vec<_> = v
        .as_slice()
        .iter()
        .map(|s| s.as_ref().map(CStrBox::as_c_str))
        .collect();
    assert_eq!(names, [Some(c"alpha"), None, Some(c"gamma")]);
    // Miri and the sanitizers check each string and the buffer are freed.
}

#[test]
fn a_panicking_constructor_drops_what_it_wrote_and_frees_the_buffer() {
    take_log();
    let r = catch_unwind(AssertUnwindSafe(|| {
        CVec::<Option<CBox<Elem, ElemFree>>, Buf>::from_fn(4, |i| {
            assert!(i < 2, "third element fails");
            Some(elem(i as u32))
        })
    }));
    assert!(r.is_err());
    assert_eq!(take_log(), ["elem 0", "elem 1", "buf"]);
    assert_eq!(BUFS_LIVE.with(Cell::get), 0);
}

/// An element whose drop panics.
struct Bomb(u32);
impl Drop for Bomb {
    fn drop(&mut self) {
        log(format!("bomb {}", self.0));
        assert_ne!(self.0, 1, "element 1 panics in drop");
    }
}

#[test]
fn a_panicking_element_drop_still_frees_the_buffer() {
    take_log();
    let v: CVec<Bomb, Buf> = CVec::from_fn(3, |i| Bomb(i as u32)).unwrap();
    let r = catch_unwind(AssertUnwindSafe(|| drop(v)));
    assert!(r.is_err());
    // Slice drop glue keeps dropping after a panicking element, then the
    // buffer goes.
    assert_eq!(take_log(), ["bomb 0", "bomb 1", "bomb 2", "buf"]);
    assert_eq!(BUFS_LIVE.with(Cell::get), 0);
}

#[test]
fn an_empty_buffer_allocates_nothing() {
    take_log();
    let v: CVec<Option<CBox<Elem, ElemFree>>, Buf> = CVec::from_fn(0, |_| unreachable!()).unwrap();
    assert!(v.as_ptr().is_null());
    drop(v);
    assert!(take_log().is_empty(), "nothing allocated, nothing freed");
    let u = CVec::<u64, Buf>::new_uninit(0).unwrap();
    assert!(u.as_ptr().is_null());
}

#[test]
fn an_uninitialised_buffer_is_filled_then_promoted() {
    let mut u = CVec::<u64, RustAlloc>::new_uninit(3).unwrap();
    for (i, slot) in u.as_mut_slice().iter_mut().enumerate() {
        slot.write(i as u64 + 7);
    }
    // SAFETY: every element was written above.
    let v: CVec<u64, RustAlloc> = unsafe { u.assume_init() };
    assert_eq!(v.as_slice(), &[7, 8, 9]);
    let copy = v.clone();
    assert_eq!(copy.as_slice(), v.as_slice());
    assert_ne!(copy.as_ptr(), v.as_ptr());
}

#[test]
fn an_unfilled_buffer_of_owners_frees_only_the_buffer() {
    take_log();
    let u = CVec::<Option<CBox<Elem, ElemFree>>, Buf>::new_uninit(2).unwrap();
    let _: &[MaybeUninit<Option<CBox<Elem, ElemFree>>>] = u.as_slice();
    drop(u);
    assert_eq!(take_log(), ["buf"]);
}
