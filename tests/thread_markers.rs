//! The `Send`/`Sync` opt-in: withheld by default, granted per type.
use ffibox::{define_ctype, impl_dropped, CBox};

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
impl_dropped!(NoMarkers, opaque_a, drop_a);
impl_dropped!(Both, opaque_b, drop_b);

fn is_send<T: Send>() {}
fn is_sync<T: Sync>() {}

#[test]
fn opted_in_type_is_send_and_sync() {
    is_send::<Both>();
    is_sync::<Both>();
    is_send::<BothRef<'_>>(); // T: Sync  => &T: Send
    is_sync::<BothRef<'_>>();
    is_send::<BothMut<'_>>(); // T: Send  => &mut T: Send
    is_sync::<BothMut<'_>>();
    is_send::<CBox<Both>>(); // follows from the generic impl
    is_sync::<CBox<Both>>();
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
