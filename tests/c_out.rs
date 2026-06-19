//! Integration tests for the `COut<'a, T>` type alias and its
//! `from_ptr` constructor.

#![allow(missing_docs)]

use core::mem::MaybeUninit;

use crustify_prim::{c_out, COut};

#[test]
fn null_pointer_returns_none() {
    let p: *mut i32 = core::ptr::null_mut();
    assert!(unsafe { c_out::from_ptr(p) }.is_none());
}

#[test]
fn write_writes_through_pointer() {
    let mut slot: i32 = 0;
    {
        let out = unsafe { c_out::from_ptr::<i32>(&mut slot) }.unwrap();
        out.write(42);
    }
    assert_eq!(slot, 42);
}

#[test]
fn multiple_writes_overwrite() {
    let mut slot: u64 = 0;
    {
        let out = unsafe { c_out::from_ptr::<u64>(&mut slot) }.unwrap();
        out.write(1);
        out.write(2);
        out.write(3);
    }
    assert_eq!(slot, 3);
}

#[test]
fn as_mut_ptr_round_trips_to_input() {
    let mut slot: i32 = 0;
    let p: *mut i32 = &mut slot;
    let out = unsafe { c_out::from_ptr(p) }.unwrap();
    assert_eq!(out.as_mut_ptr() as usize, p as usize);
}

#[test]
fn write_into_uninit_slot() {
    let mut slot: MaybeUninit<u32> = MaybeUninit::uninit();
    {
        let out = unsafe { c_out::from_ptr(slot.as_mut_ptr()) }.unwrap();
        out.write(0xDEAD_BEEF);
    }
    // SAFETY: just initialised by `write`.
    assert_eq!(unsafe { slot.assume_init() }, 0xDEAD_BEEF);
}

#[test]
fn alias_is_just_a_mut_maybeuninit_ref() {
    // Type-level proof: COut<'a, T> and &'a mut MaybeUninit<T>
    // are the same type. The function body is a no-op coercion.
    fn _coerce<'a, T>(x: COut<'a, T>) -> &'a mut MaybeUninit<T> {
        x
    }
    fn _coerce_back<'a, T>(x: &'a mut MaybeUninit<T>) -> COut<'a, T> {
        x
    }
}

#[test]
fn option_is_layout_compatible_with_pointer() {
    assert_eq!(
        core::mem::size_of::<Option<COut<'static, i32>>>(),
        core::mem::size_of::<*mut i32>(),
    );
    assert_eq!(
        core::mem::size_of::<COut<'static, u64>>(),
        core::mem::size_of::<*mut u64>(),
    );
}
