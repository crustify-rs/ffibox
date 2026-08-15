#![allow(
    missing_docs,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
//! Integration tests for the keepalive / tether pair.
//!
//! Covers the pattern a lifetime cannot express: a child pointing INTO its
//! parent's allocation, which must outlive the borrow that produced it. The
//! tests assert the ordering that makes it sound — the parent handle may be
//! dropped while children live, and the C teardown fires only when the last
//! owner goes.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crustify_prim::{CBox, CCloned, CDropped, CKeepalive, CTethered};

static FREED: AtomicUsize = AtomicUsize::new(0);
static UPREFS: AtomicUsize = AtomicUsize::new(0);

/// A parent allocation with a child living inside it — the shape that makes a
/// tether necessary: `child` has no allocation of its own.
#[repr(C)]
pub struct RawParent {
    pub child: RawChild,
}
#[repr(C)]
pub struct RawChild {
    pub value: i32,
}

fn parent_new(value: i32) -> *mut RawParent {
    Box::into_raw(Box::new(RawParent {
        child: RawChild { value },
    }))
}
unsafe fn parent_free(p: *mut RawParent) {
    FREED.fetch_add(1, Ordering::SeqCst);
    drop(unsafe { Box::from_raw(p) });
}

crustify_prim::define_ctype! {
    /// Wraps: RawParent
    Parent, ParentRef, ParentMut, RawParent
}
crustify_prim::define_ctype! {
    /// Wraps: RawChild
    Child, ChildRef, ChildMut, RawChild
}

unsafe impl CDropped for Parent {
    unsafe fn c_drop(obj: NonNull<Self>) {
        unsafe { parent_free(obj.as_ptr().cast::<RawParent>()) }
    }
}

/// Stands in for a C-refcounted parent: `c_clone` is the `up_ref`, and each
/// clone owes one `c_drop`. The test only counts calls, so a leaked-on-purpose
/// second free is avoided by giving every clone its own allocation.
unsafe impl CCloned for Parent {
    unsafe fn c_clone(obj: NonNull<Self>) -> Option<NonNull<Self>> {
        UPREFS.fetch_add(1, Ordering::SeqCst);
        let v = unsafe { (*obj.as_ptr().cast::<RawParent>()).child.value };
        NonNull::new(parent_new(v).cast::<Parent>())
    }
}

fn parent_box(value: i32) -> CBox<Parent> {
    unsafe { CBox::from_raw(parent_new(value)) }.expect("non-null")
}

/// The pointer INTO the parent — no allocation of its own.
unsafe fn child_of(p: &CBox<Parent>) -> *mut RawChild {
    unsafe { &raw mut (*p.as_ptr().cast::<RawParent>()).child }
}

#[test]
fn child_outlives_the_parent_handle() {
    FREED.store(0, Ordering::SeqCst);

    let child: CTethered<Child, CKeepalive<Parent>> = {
        let parent = parent_box(42);
        let ptr = unsafe { child_of(&parent) };
        let keep = CKeepalive::new(parent); // parent handle moves in
        unsafe { CTethered::from_raw(ptr, keep) }.expect("non-null")
    }; // the block's `parent` binding is gone here

    // Nothing was freed: the keepalive still holds it.
    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    // And the interior pointer is still readable.
    assert_eq!(unsafe { (*child.as_ptr()).value }, 42);

    drop(child);
    assert_eq!(
        FREED.load(Ordering::SeqCst),
        1,
        "freed exactly once, at the last owner"
    );
}

#[test]
fn a_refcounted_parent_needs_no_allocation() {
    FREED.store(0, Ordering::SeqCst);
    UPREFS.store(0, Ordering::SeqCst);

    let parent = parent_box(7);
    // One `up_ref` per child; the keepalive is stored INLINE, no Arc.
    let a = unsafe {
        CTethered::<Child, _>::from_raw(child_of(&parent), CKeepalive::new(parent.clone()))
    }
    .expect("non-null");
    let b = unsafe {
        CTethered::<Child, _>::from_raw(child_of(&parent), CKeepalive::new(parent.clone()))
    }
    .expect("non-null");
    assert_eq!(UPREFS.load(Ordering::SeqCst), 2);

    drop(parent);
    assert_eq!(
        FREED.load(Ordering::SeqCst),
        1,
        "only the original handle's object"
    );
    drop(a);
    drop(b);
    assert_eq!(
        FREED.load(Ordering::SeqCst),
        3,
        "each clone owed one c_drop"
    );
}

#[test]
fn several_children_share_one_unrefcounted_parent() {
    FREED.store(0, Ordering::SeqCst);

    let parent = parent_box(99);
    let ptr = unsafe { child_of(&parent) };
    let keep: Arc<CKeepalive<Parent>> = Arc::new(CKeepalive::new(parent));

    let a = unsafe { CTethered::<Child, _>::from_raw(ptr, keep.clone()) }.expect("non-null");
    let b = unsafe { CTethered::<Child, _>::from_raw(ptr, keep.clone()) }.expect("non-null");
    drop(keep);

    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    assert_eq!(unsafe { (*a.as_ptr()).value }, 99);
    drop(a);
    assert_eq!(FREED.load(Ordering::SeqCst), 0, "b still holds a share");
    drop(b);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn the_handle_reaches_the_child_and_the_owner_is_borrowable() {
    FREED.store(0, Ordering::SeqCst);
    let parent = parent_box(5);
    let ptr = unsafe { child_of(&parent) };
    let t =
        unsafe { CTethered::<Child, _>::from_raw(ptr, CKeepalive::new(parent)) }.expect("non-null");

    let _: ChildRef<'_> = t.as_ref(); // the borrowed handle, not a reference
    let _: &CKeepalive<Parent> = t.owner();
    let owner = t.into_owner(); // view dropped, owner recovered
    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    drop(owner);
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}
