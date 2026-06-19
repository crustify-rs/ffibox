//! Integration tests for `SelfPtr<'this, T>`.

#![allow(missing_docs)]

use crustify_prim::SelfPtr;

#[test]
fn new_from_borrow_reads_back() {
    let value = 0xABCD_u32;
    let p = SelfPtr::new(&value);
    assert_eq!(*p.get(), 0xABCD);
}

#[test]
fn from_raw_null_returns_none() {
    let p: *mut u32 = core::ptr::null_mut();
    assert!(unsafe { SelfPtr::from_raw(p) }.is_none());
}

#[test]
fn from_raw_non_null_reads_back() {
    let mut value = 77_i64;
    let p = unsafe { SelfPtr::from_raw(&mut value as *mut i64) }.unwrap();
    assert_eq!(*p.get(), 77);
}

#[test]
fn as_ptr_round_trips_to_input() {
    let value = 5_u8;
    let r: &u8 = &value;
    let p = SelfPtr::new(r);
    assert_eq!(p.as_ptr(), r as *const u8);
}

#[test]
fn get_is_bound_to_this_not_self() {
    // The borrow returned by `get` outlives the `SelfPtr` itself: it
    // is tied to `'this`, not to `&self`. This block only compiles if
    // `get` returns `&'this T`.
    let value = 9_u32;
    let borrowed: &u32 = {
        let p = SelfPtr::new(&value);
        p.get()
    };
    assert_eq!(*borrowed, 9);
}

#[test]
fn is_copy() {
    let value = 1_u32;
    let p = SelfPtr::new(&value);
    let q = p; // copy, not move
    assert_eq!(*p.get(), *q.get());
}

#[test]
fn parent_back_pointer_pattern() {
    // A child struct holding a SelfPtr back to its parent — the
    // canonical "upward self-ref" use case.
    struct Parent {
        tag: u32,
    }
    struct Child<'p> {
        parent: SelfPtr<'p, Parent>,
    }

    let parent = Parent { tag: 0xFEED };
    let child = Child {
        parent: SelfPtr::new(&parent),
    };
    assert_eq!(child.parent.get().tag, 0xFEED);
}

#[test]
fn option_is_layout_compatible_with_pointer() {
    assert_eq!(
        core::mem::size_of::<Option<SelfPtr<'static, u32>>>(),
        core::mem::size_of::<*const u32>(),
    );
    assert_eq!(
        core::mem::size_of::<SelfPtr<'static, u64>>(),
        core::mem::size_of::<*const u64>(),
    );
}
