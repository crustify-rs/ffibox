//! Reborrowing and splitting the exclusive handles and runs: `FooMut::as_mut`,
//! `CSliceMut::{as_mut, split_at_mut, slice_mut, into_mut}`, and the shared
//! `CSlice::{split_at, slice}`. The borrow-checker side — the original frozen
//! while a reborrow lives — is covered by `compile_fail` doctests.

#![allow(non_camel_case_types, missing_docs)]

use core::ptr::{addr_of, addr_of_mut, NonNull};

use ffibox::{define_ctype, CSlice, CSliceMut, CZeroable as _};

#[repr(C)]
pub struct pt_st {
    x: i32,
}
define_ctype!(Pt, PtRef, PtMut, pt_st);
// SAFETY: any `x` is valid, zero included.
unsafe impl ffibox::CZeroable for Pt {}

impl PtRef<'_> {
    fn x(&self) -> i32 {
        // SAFETY: a read through the handle's pointer; no reference is formed.
        unsafe { addr_of!((*self.as_ptr()).x).read() }
    }
}
impl PtMut<'_> {
    fn set_x(&mut self, v: i32) {
        // SAFETY: a write through the exclusive handle's pointer.
        unsafe { addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
    }
}

fn reset(mut m: PtMut<'_>) {
    m.set_x(0);
}

fn bump_all(mut s: CSliceMut<'_, Pt>) {
    for mut m in s.iter_mut() {
        let x = m.as_ref().x();
        m.set_x(x + 1);
    }
}

/// An exclusive run over `pts`, which Rust owns and lends for the call.
fn run(pts: &mut [Pt]) -> CSliceMut<'_, Pt> {
    // SAFETY: `pts` is a live, exclusively borrowed run of initialised `Pt`.
    unsafe { CSliceMut::from_raw_parts(NonNull::from(&mut *pts).cast(), pts.len()) }
}

fn xs(pts: &[Pt]) -> Vec<i32> {
    pts.iter().map(|p| p.as_ref().x()).collect()
}

fn four() -> [Pt; 4] {
    core::array::from_fn(|i| {
        let mut p = Pt::zeroed();
        p.as_mut().set_x(i as i32 * 10);
        p
    })
}

#[test]
fn a_reborrowed_handle_leaves_the_original_usable() {
    let mut p = Pt::zeroed();
    let mut m = p.as_mut();
    m.set_x(5);
    reset(m.as_mut());
    assert_eq!(m.as_ref().x(), 0);
    m.set_x(1);
    assert_eq!(p.as_ref().x(), 1);
}

#[test]
fn a_reborrowed_run_leaves_the_original_usable() {
    let mut pts = four();
    let mut s = run(&mut pts);
    bump_all(s.as_mut());
    bump_all(s.as_mut());
    s.get_mut(0).unwrap().set_x(-1);
    assert_eq!(xs(&pts), [-1, 12, 22, 32]);
}

#[test]
fn split_halves_are_disjoint_and_usable_together() {
    let mut pts = four();
    let mut s = run(&mut pts);
    let (mut lo, mut hi) = s.split_at_mut(1).unwrap();
    assert_eq!((lo.len(), hi.len()), (1, 3));
    let mut a = lo.get_mut(0).unwrap();
    let mut b = hi.get_mut(0).unwrap();
    a.set_x(100);
    b.set_x(200);
    a.set_x(101); // both exclusive handles live at once
    assert!(s
        .split_at_mut(4)
        .is_some_and(|(l, r)| l.len() == 4 && r.is_empty()));
    assert!(s.split_at_mut(5).is_none());
    assert_eq!(xs(&pts), [101, 200, 20, 30]);
}

#[test]
fn sub_ranges_resolve_like_slice_indexing() {
    let mut pts = four();
    let mut s = run(&mut pts);
    let len = |s: Option<CSliceMut<'_, Pt>>| s.map(|s| s.len());
    assert_eq!(len(s.slice_mut(..)), Some(4));
    assert_eq!(len(s.slice_mut(1..3)), Some(2));
    assert_eq!(len(s.slice_mut(1..=3)), Some(3));
    assert_eq!(len(s.slice_mut(..=0)), Some(1));
    assert_eq!(len(s.slice_mut(4..)), Some(0));
    assert_eq!(len(s.slice_mut(5..)), None);
    assert_eq!(len(s.slice_mut(..5)), None);
    #[allow(clippy::reversed_empty_ranges)]
    let reversed = s.slice_mut(3..2);
    assert_eq!(len(reversed), None);
    assert_eq!(len(s.slice_mut(..=usize::MAX)), None);
    assert_eq!(
        len(s.slice_mut((
            core::ops::Bound::Excluded(usize::MAX),
            core::ops::Bound::Unbounded
        ))),
        None
    );

    bump_all(s.slice_mut(2..).unwrap());
    assert_eq!(xs(&pts), [0, 10, 21, 31]);
}

/// Returns an element handle for the run's whole lifetime — impossible with
/// `get_mut`, whose result borrows the local view.
fn first<'a>(s: CSliceMut<'a, Pt>) -> PtMut<'a> {
    s.into_mut(0).unwrap()
}

#[test]
fn into_mut_hands_out_an_element_for_the_runs_lifetime() {
    let mut pts = four();
    first(run(&mut pts)).set_x(7);
    assert!(run(&mut pts).into_mut(4).is_none());
    assert_eq!(xs(&pts), [7, 10, 20, 30]);
}

/// Both halves keep `'a`, outliving the local view they were split from.
fn halves<'a>(s: CSlice<'a, u32>) -> (CSlice<'a, u32>, CSlice<'a, u32>) {
    s.split_at(2).unwrap()
}

#[test]
fn shared_runs_split_and_narrow_without_freezing() {
    let data = [1u32, 2, 3, 4, 5];
    // SAFETY: `data` is a live run of five `u32`, read-only for the borrow.
    let s = unsafe { CSlice::from_raw_parts(NonNull::from(&data).cast::<u32>(), data.len()) };
    let (a, b) = halves(s);
    assert_eq!(a.elems().collect::<Vec<_>>(), [1, 2]);
    assert_eq!(b.elems().collect::<Vec<_>>(), [3, 4, 5]);
    assert_eq!(
        s.slice(1..4).unwrap().elems().collect::<Vec<_>>(),
        [2, 3, 4]
    );
    assert!(s.slice(..6).is_none());
    assert!(s.split_at(6).is_none());
}

#[test]
fn plain_value_runs_split_and_write_in_place() {
    let mut data = [0u32; 4];
    // SAFETY: `data` is a live, exclusively borrowed run of four `u32`.
    let mut s = unsafe { CSliceMut::from_raw_parts(NonNull::from(&mut data).cast::<u32>(), 4) };
    let (mut lo, mut hi) = s.split_at_mut(2).unwrap();
    assert!(lo.copy_from_slice(&[1, 2]));
    assert!(hi.set_elem(1, 9));
    assert!(s.slice_mut(1..3).unwrap().set_elem(1, 5));
    assert_eq!(data, [1, 2, 5, 9]);
}
