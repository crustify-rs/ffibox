#![allow(
    missing_docs,
    dead_code,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
//! `define_type!` covers only the trivial base case; lifetime- and type-generic
//! newtypes are written by hand against `CCell` (native Rust generics, no macro
//! arm). These tests guard that hand-written path: the whole seam
//! (`as_ptr`/`as_void_ptr`/`from_ptr`/`uninit`/`zeroed`) comes from `CCell` with
//! only `type C = ...;`, and the layout stays `#[repr(transparent)]`.

use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use crustify_prim::{define_type, CBox, CBoxWith, CCell, CDropped, CDropper, CType};

#[repr(C)]
pub struct foo_st {
    pub a: u64,
    pub p: *mut u8,
}

// Base case — via the macro.
define_type!(FooOwned, foo_st);

// --- Hand-written lifetime-carrying wrapper (formerly the lifetime arm) ---
#[repr(transparent)]
pub struct FooBorrowed<'a>(CType<foo_st>, PhantomData<&'a ()>);
unsafe impl<'a> CCell for FooBorrowed<'a> {
    type C = foo_st;
}
// CDropped so we can box it and check niche.
unsafe impl<'a> CDropped for FooBorrowed<'a> {
    unsafe fn c_drop(_o: NonNull<Self>) {}
}

#[repr(transparent)]
pub struct FooBiBorrowed<'a, 'b>(CType<foo_st>, PhantomData<(&'a (), &'b ())>);
unsafe impl<'a, 'b> CCell for FooBiBorrowed<'a, 'b> {
    type C = foo_st;
}

// --- Hand-written type-generic wrapper: the STACK_OF shape Stack<T, S> ---
#[repr(C)]
pub struct stack_st {
    pub num: i32,
    pub data: *mut *mut core::ffi::c_void,
}
// Free strategies (the `owned_elem` axis).
pub struct Borrowed;
pub struct Owned<D>(PhantomData<D>);
pub struct X509;
pub struct X509Free;

#[repr(transparent)]
pub struct Stack<T, S>(CType<stack_st>, PhantomData<(T, S)>);
unsafe impl<T, S> CCell for Stack<T, S> {
    type C = stack_st;
}
// Each STACK_OF(T) is a zero-cost alias picking a strategy — not a redefinition.
pub type StackOfX509Borrowed = Stack<X509, Borrowed>;
pub type StackOfX509Owned = Stack<X509, Owned<X509Free>>;

#[test]
fn layout_and_niche() {
    // repr(transparent): wrapper == its C type.
    assert_eq!(
        size_of::<Option<&FooBorrowed<'static>>>(),
        size_of::<*const foo_st>()
    );
    assert_eq!(
        size_of::<Option<CBox<FooBorrowed<'static>>>>(),
        size_of::<*mut foo_st>()
    );
    assert_eq!(
        size_of::<FooBiBorrowed<'static, 'static>>(),
        size_of::<foo_st>()
    );
    // generic wrapper stays transparent over stack_st regardless of T, S.
    assert_eq!(size_of::<Stack<X509, Borrowed>>(), size_of::<stack_st>());
    assert_eq!(size_of::<StackOfX509Owned>(), size_of::<stack_st>());
}

// covariance: a 'static borrow is usable where a shorter one is expected.
fn _covariant<'a>(x: &'a FooBorrowed<'static>) -> &'a FooBorrowed<'a> {
    x
}

#[test]
fn seam_provided_by_ccell() {
    // uninit/zeroed/as_ptr/as_void_ptr are provided by CCell for hand-written
    // wrappers (only `type C` was written). Exercise the transmute_copy ctors.
    let s = Stack::<X509, Borrowed>::zeroed();
    let p: *mut stack_st = s.as_ptr();
    assert!(!p.is_null());
    let _v = s.as_void_ptr();
    let _u = StackOfX509Owned::uninit();
    let _b = FooBorrowed::<'static>::zeroed();

    // inherent forwarder on the macro base case: callable without `CCell` name.
    let f = FooOwned::zeroed();
    let _ = f.as_ptr();
}

// ---------------------------------------------------------------------------
// CBoxWith<T, D> — fat owner carrying a runtime teardown fn (the sk_pop_free
// shape): the element-free function is chosen at the wrapping site and threaded
// into `Drop`, which a zero-state `CBox<Stack<..>>` cannot express.
// ---------------------------------------------------------------------------

static POP_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static ELEM_FN_SEEN: AtomicUsize = AtomicUsize::new(0);

// Mock element destructor; identity is what we assert reached teardown.
extern "C" fn x509_free_mock(_p: *mut core::ffi::c_void) {}

// The dropper IS the runtime state: the caller-supplied element-free fn.
#[derive(Clone, Copy)]
pub struct ElemFree(unsafe extern "C" fn(*mut core::ffi::c_void));

// SAFETY: stand-in for `OPENSSL_sk_pop_free(ptr, self.0)` — records the call and
// the fn it carried, then reclaims the Box-backed mock allocation exactly once.
unsafe impl CDropper<Stack<X509, Borrowed>> for ElemFree {
    unsafe fn c_drop(&self, ptr: NonNull<Stack<X509, Borrowed>>) {
        POP_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        ELEM_FN_SEEN.store(self.0 as usize, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<stack_st>()) });
    }
}

#[test]
fn cboxwith_runs_dropper_with_runtime_state() {
    POP_FREE_CALLS.store(0, Ordering::SeqCst);
    ELEM_FN_SEEN.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(stack_st {
        num: 0,
        data: core::ptr::null_mut(),
    }));
    // The free fn is fixed HERE, at the seam — the whole point of the fat owner.
    // `from_raw` speaks the raw C type (`*mut stack_st`), cast-free.
    let owned = unsafe {
        CBoxWith::<Stack<X509, Borrowed>, ElemFree>::from_raw(raw, ElemFree(x509_free_mock))
    }
    .unwrap();
    drop(owned);

    assert_eq!(POP_FREE_CALLS.load(Ordering::SeqCst), 1);
    let expected: unsafe extern "C" fn(*mut core::ffi::c_void) = x509_free_mock;
    assert_eq!(
        ELEM_FN_SEEN.load(Ordering::SeqCst),
        expected as usize,
        "the dropper must carry the runtime free fn into teardown",
    );
}

#[test]
fn cboxwith_layout_thin_vs_fat() {
    // The thin owner stays pointer-sized, with the Option niche.
    assert_eq!(
        size_of::<CBox<FooBorrowed<'static>>>(),
        size_of::<*mut foo_st>()
    );
    assert_eq!(
        size_of::<Option<CBox<FooBorrowed<'static>>>>(),
        size_of::<*mut foo_st>()
    );
    // The fat owner is genuinely ptr + inline state (here a fn pointer).
    assert_eq!(
        size_of::<CBoxWith<Stack<X509, Borrowed>, ElemFree>>(),
        size_of::<*mut stack_st>() + size_of::<ElemFree>(),
    );
}
