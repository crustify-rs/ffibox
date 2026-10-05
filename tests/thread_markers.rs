//! The `Send`/`Sync` opt-in: withheld by default, granted per type, and
//! inherited by the owners and views from their pointee and policy.

use core::ffi::{c_char, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;

use ffibox::{
    define_ctype, impl_cdispose, impl_cdrop, CBox, CDrop, CLenDrop, CSlice, CSliceMut, CStrBox,
    CVal, CVec, CVoidBox,
};

/// Stand-in for a bindgen opaque type that never opts in.
#[repr(C)]
pub struct opaque_a {
    _unused: [u8; 0],
}
/// Stand-in for a bindgen opaque type that opts into both markers.
#[repr(C)]
pub struct opaque_b {
    _unused: [u8; 0],
}

define_ctype!(
    /// Withholds both markers: the default.
    NoMarkers, NoMarkersRef, NoMarkersMut, opaque_a);
define_ctype!(
    /// Registers both markers below.
    Both, BothRef, BothMut, opaque_b);
// SAFETY: test stand-in; `opaque_b` is a ZST with no thread-affine state.
unsafe impl Send for Both {}
// SAFETY: `&mut T: Send` follows from `T: Send`.
unsafe impl Send for BothMut<'_> {}
// SAFETY: test stand-in; no interior mutation to race on.
unsafe impl Sync for Both {}
// SAFETY: `&T: Send` is precisely `T: Sync`.
unsafe impl Send for BothRef<'_> {}
// SAFETY: `&T: Sync` follows from `T: Sync`.
unsafe impl Sync for BothRef<'_> {}
// SAFETY: `&mut T: Sync` follows from `T: Sync`.
unsafe impl Sync for BothMut<'_> {}

unsafe extern "C" fn drop_a(_: *mut opaque_a) {}
unsafe extern "C" fn drop_b(_: *mut opaque_b) {}
/// Teardown policy for `NoMarkersBox`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMarkersFree;
impl_cdrop!(NoMarkersFree, NoMarkers, drop_a);
impl_cdispose!(NoMarkersFree, NoMarkers, drop_a);
/// Owned `NoMarkers`; withholds both markers like its pointee.
pub type NoMarkersBox = CBox<NoMarkers, NoMarkersFree>;

/// Teardown policy for `BothBox`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BothFree;
impl_cdrop!(BothFree, Both, drop_b);
impl_cdispose!(BothFree, Both, drop_b);
/// Owned `Both`; inherits both markers from its pointee.
pub type BothBox = CBox<Both, BothFree>;

/// A free any thread may call.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnyThreadFree;
// SAFETY: test stand-in; never called on a live pointer.
unsafe impl CDrop<c_char> for AnyThreadFree {
    unsafe fn c_drop(&self, _: NonNull<c_char>) {}
}
// SAFETY: as above.
unsafe impl CDrop<c_void> for AnyThreadFree {
    unsafe fn c_drop(&self, _: NonNull<c_void>) {}
}
// SAFETY: as above.
unsafe impl CLenDrop for AnyThreadFree {
    unsafe fn c_drop_len(&self, _: *mut u8, _: usize) {}
}

/// A free that must run on the allocating thread: the marker opts out.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThisThreadFree(PhantomData<*const ()>);
// SAFETY: as `AnyThreadFree`.
unsafe impl CDrop<c_char> for ThisThreadFree {
    unsafe fn c_drop(&self, _: NonNull<c_char>) {}
}
// SAFETY: as above.
unsafe impl CDrop<c_void> for ThisThreadFree {
    unsafe fn c_drop(&self, _: NonNull<c_void>) {}
}

fn is_send<T: Send>() {}
fn is_sync<T: Sync>() {}

/// Compiles only when `T` is NOT `Send`: for a `Send` type both impls apply
/// and the `_` inference is ambiguous.
trait AmbiguousIfSend<A> {
    fn check() {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

/// As above, for `Sync`.
trait AmbiguousIfSync<A> {
    fn check() {}
}
impl<T: ?Sized> AmbiguousIfSync<()> for T {}
impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

#[test]
fn opted_in_type_and_its_owners_are_send_and_sync() {
    is_send::<Both>();
    is_sync::<Both>();
    is_send::<BothRef<'_>>(); // T: Sync  => &T: Send
    is_sync::<BothRef<'_>>();
    is_send::<BothMut<'_>>(); // T: Send  => &mut T: Send
    is_sync::<BothMut<'_>>();
    is_send::<BothBox>();
    is_sync::<BothBox>();
    is_send::<CVal<Both, BothFree>>();
    is_sync::<CVal<Both, BothFree>>();
    is_send::<CVec<Both, AnyThreadFree>>();
    is_sync::<CVec<Both, AnyThreadFree>>();
    is_send::<CSlice<'_, Both>>();
    is_send::<CSliceMut<'_, Both>>();
}

#[test]
fn owners_and_views_withhold_markers_with_their_pointee() {
    <NoMarkersBox as AmbiguousIfSend<_>>::check();
    <NoMarkersBox as AmbiguousIfSync<_>>::check();
    <CVal<NoMarkers, NoMarkersFree> as AmbiguousIfSend<_>>::check();
    <CVec<NoMarkers, AnyThreadFree> as AmbiguousIfSend<_>>::check();
    <CSlice<'_, NoMarkers> as AmbiguousIfSend<_>>::check();
    <CSliceMut<'_, NoMarkers> as AmbiguousIfSend<_>>::check();
    assert_eq!(
        core::mem::size_of::<NoMarkersBox>(),
        core::mem::size_of::<*mut opaque_a>()
    );
}

#[test]
fn strings_and_payloads_follow_their_policy() {
    is_send::<CStrBox<AnyThreadFree>>();
    is_sync::<CStrBox<AnyThreadFree>>();
    is_send::<CVoidBox<AnyThreadFree>>();
    is_sync::<CVoidBox<AnyThreadFree>>();
    <CStrBox<ThisThreadFree> as AmbiguousIfSend<_>>::check();
    <CVoidBox<ThisThreadFree> as AmbiguousIfSend<_>>::check();
}

#[test]
fn layout_is_unchanged_by_the_marker() {
    assert_eq!(
        core::mem::size_of::<NoMarkers>(),
        core::mem::size_of::<opaque_a>()
    );
    assert_eq!(
        core::mem::align_of::<NoMarkers>(),
        core::mem::align_of::<opaque_a>()
    );
}
