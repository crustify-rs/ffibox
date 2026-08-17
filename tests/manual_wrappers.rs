#![allow(
    missing_docs,
    dead_code,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
//! `define_ctype!` covers only the trivial base case; lifetime- and
//! type-generic newtypes are written by hand against `CCell` (native Rust
//! generics, no macro arm). These tests guard that hand-written path: the
//! wrapper supplies `type C` plus its own two handle types and the constructors
//! that build them, and the layout stays `#[repr(transparent)]` throughout.
//!
//! The invariant under test is the crate's premise: **no reference to a wrapped
//! C object is ever formed.** Every accessor lives on a handle, which is one
//! pointer of Rust-owned storage.

use core::marker::PhantomData;
use core::mem::size_of;
use core::ops::Deref;
use core::ptr::{addr_of, addr_of_mut, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};
use ffibox::{define_ctype, CBox, CBoxWith, CCell, CDropped, CDropper, CPtr, CType};

#[repr(C)]
pub struct foo_st {
    pub a: u64,
    pub p: *mut u8,
}

// Base case — via the macro.
define_ctype!(FooFull, FooFullRef, FooFullMut, foo_st);

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

/// The hand-written shared handle. The generic parameters ride along, so
/// borrowing a pointer cannot lose the element type or the free strategy.
#[repr(transparent)]
pub struct StackRef<'a, T, S>(CPtr<'a, Stack<T, S>>);
impl<T, S> Clone for StackRef<'_, T, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, S> Copy for StackRef<'_, T, S> {}

/// The hand-written exclusive handle.
#[repr(transparent)]
pub struct StackMut<'a, T, S>(StackRef<'a, T, S>);

// SAFETY: `Stack` is `#[repr(transparent)]` over `CType<stack_st>`; both handles
// are transparent over `CPtr<'a, Stack<T, S>>` and expose no reference to
// `Stack`; the shared one has no write path.
unsafe impl<T, S> CCell for Stack<T, S> {
    type C = stack_st;
    type Ref<'a>
        = StackRef<'a, T, S>
    where
        Self: 'a;
    type Mut<'a>
        = StackMut<'a, T, S>
    where
        Self: 'a;

    unsafe fn ref_from_raw<'a>(p: NonNull<Self>) -> StackRef<'a, T, S>
    where
        Self: 'a,
    {
        StackRef(unsafe { CPtr::new(p) })
    }
    unsafe fn mut_from_raw<'a>(p: NonNull<Self>) -> StackMut<'a, T, S>
    where
        Self: 'a,
    {
        StackMut(StackRef(unsafe { CPtr::new(p) }))
    }
}

impl<T, S> Stack<T, S> {
    /// All-zero is a valid `stack_st` (an `i32` and a raw pointer).
    fn zeroed() -> Self {
        Self(unsafe { CType::zeroed() }, PhantomData)
    }
}

impl<'a, T, S> StackRef<'a, T, S> {
    unsafe fn from_ptr(p: *mut stack_st) -> Option<Self> {
        NonNull::new(p.cast::<Stack<T, S>>()).map(|p| StackRef(unsafe { CPtr::new(p) }))
    }
    fn as_ptr(&self) -> *const stack_st {
        self.0.as_non_null().as_ptr().cast()
    }
    /// A getter: reads through the raw pointer, forms no reference.
    fn num(&self) -> i32 {
        unsafe { addr_of!((*self.as_ptr()).num).read() }
    }
}

impl<'a, T, S> StackMut<'a, T, S> {
    fn as_mut_ptr(&mut self) -> *mut stack_st {
        self.0 .0.as_non_null().as_ptr().cast()
    }
    /// A setter: `&mut self` on the HANDLE — one pointer of Rust stack.
    fn set_num(&mut self, v: i32) {
        unsafe { addr_of_mut!((*self.as_mut_ptr()).num).write(v) }
    }
}

impl<'a, T, S> Deref for StackMut<'a, T, S> {
    type Target = StackRef<'a, T, S>;
    fn deref(&self) -> &StackRef<'a, T, S> {
        &self.0
    }
}

// Each STACK_OF(T) is a zero-cost alias picking a strategy — not a redefinition.
pub type StackOfX509Borrowed = Stack<X509, Borrowed>;
pub type StackOfX509Owned = Stack<X509, Owned<X509Free>>;

#[test]
fn layout_and_niche() {
    // The layout newtype keeps the C struct's size, so it embeds by value.
    assert_eq!(size_of::<FooFull>(), size_of::<foo_st>());
    assert_eq!(size_of::<Stack<X509, Borrowed>>(), size_of::<stack_st>());
    assert_eq!(size_of::<StackOfX509Owned>(), size_of::<stack_st>());

    // The handles are one pointer regardless of the generics, with the niche.
    assert_eq!(
        size_of::<StackRef<'_, X509, Borrowed>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(
        size_of::<StackMut<'_, X509, Borrowed>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(
        size_of::<Option<StackRef<'_, X509, Borrowed>>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(size_of::<Option<CBox<FooFull>>>(), size_of::<*mut foo_st>());
}

// Covariance: a longer borrow is usable where a shorter one is expected, just
// like `&'a T`.
fn _covariant<'a>(x: StackRef<'static, X509, Borrowed>) -> StackRef<'a, X509, Borrowed> {
    x
}

#[test]
fn the_hand_written_seam_reads_and_writes() {
    let raw = Box::into_raw(Box::new(stack_st {
        num: 3,
        data: core::ptr::null_mut(),
    }));

    let r: StackRef<'_, X509, Borrowed> = unsafe { StackRef::from_ptr(raw) }.unwrap();
    assert_eq!(r.num(), 3);

    let mut m: StackMut<'_, X509, Borrowed> =
        unsafe { Stack::mut_from_raw(NonNull::new(raw.cast()).unwrap()) };
    m.set_num(7);
    // Getters reach through `Deref` to the shared handle.
    assert_eq!(m.num(), 7);

    // `zeroed` builds a value for inline storage; the pointer to it comes from
    // `addr_of_mut!`, never `&mut`.
    let mut inline: Stack<X509, Borrowed> = Stack::zeroed();
    let slot = addr_of_mut!(inline);
    assert_eq!(
        unsafe { addr_of!((*slot.cast::<stack_st>()).num).read() },
        0
    );

    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn owning_handles_hand_out_handles_not_references() {
    let raw = Box::into_raw(Box::new(foo_st {
        a: 1,
        p: core::ptr::null_mut(),
    }));
    let mut b = unsafe { CBox::<FooFull>::from_raw(raw) }.unwrap();

    // `as_ref` / `as_mut` replace `Deref`: the handle carries the lifetime,
    // which `Deref::Target` could not name.
    let _shared: FooFullRef<'_> = b.as_ref();
    let mut excl: FooFullMut<'_> = b.as_mut();
    unsafe { addr_of_mut!((*excl.as_mut_ptr()).a).write(9) };
    assert_eq!(unsafe { addr_of!((*b.as_ref().as_ptr()).a).read() }, 9);

    core::mem::forget(b);
    drop(unsafe { Box::from_raw(raw) });
}

// ---------------------------------------------------------------------------
// CBoxWith<T, D> — the fat owner, and now the construction-phase handle too
// ---------------------------------------------------------------------------

static POP_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static ELEM_FN_SEEN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn x509_free_mock(_p: *mut core::ffi::c_void) {}

/// The dropper IS the runtime state: the caller-supplied element-free fn.
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

// The construction phase `CBoxUninit` used to model: hold the allocation under
// a storage-only dropper while filling it, then promote. One-way, and a type
// change, so a half-built object cannot reach code expecting a formed one.
static STORAGE_FREES: AtomicUsize = AtomicUsize::new(0);
static FULL_FREES: AtomicUsize = AtomicUsize::new(0);

pub struct StorageFree;
// SAFETY: reclaims exactly the raw allocation, touching no field — the
// construction-phase contract.
unsafe impl CDropper<FooFull> for StorageFree {
    unsafe fn c_drop(&self, ptr: NonNull<FooFull>) {
        STORAGE_FREES.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<foo_st>()) });
    }
}
// SAFETY: the full destructor, run once the object is formed.
unsafe impl CDropped for FooFull {
    unsafe fn c_drop(obj: NonNull<Self>) {
        FULL_FREES.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(obj.as_ptr().cast::<foo_st>()) });
    }
}

#[test]
fn construction_phase_promotes_with_exactly_one_teardown() {
    STORAGE_FREES.store(0, Ordering::SeqCst);
    FULL_FREES.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(foo_st {
        a: 0,
        p: core::ptr::null_mut(),
    }));
    let mut slot = unsafe { CBoxWith::<FooFull, StorageFree>::from_raw(raw, StorageFree) }.unwrap();
    unsafe { addr_of_mut!((*slot.as_mut().as_mut_ptr()).a).write(42) };

    // Promote: the storage dropper is forgotten, `FooFull::c_drop` takes over.
    let formed: CBox<FooFull> = unsafe { slot.into_box() };
    assert_eq!(
        unsafe { addr_of!((*formed.as_ref().as_ptr()).a).read() },
        42
    );
    assert_eq!(STORAGE_FREES.load(Ordering::SeqCst), 0);

    drop(formed);
    assert_eq!(FULL_FREES.load(Ordering::SeqCst), 1);
    assert_eq!(
        STORAGE_FREES.load(Ordering::SeqCst),
        0,
        "the construction-phase teardown must not run on a promoted object"
    );
}

#[test]
fn construction_phase_bails_with_storage_only_teardown() {
    STORAGE_FREES.store(0, Ordering::SeqCst);
    FULL_FREES.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(foo_st {
        a: 0,
        p: core::ptr::null_mut(),
    }));
    {
        let _slot = unsafe { CBoxWith::<FooFull, StorageFree>::from_raw(raw, StorageFree) };
        // dropped without promoting — the construction-failure path
    }
    assert_eq!(STORAGE_FREES.load(Ordering::SeqCst), 1);
    assert_eq!(
        FULL_FREES.load(Ordering::SeqCst),
        0,
        "the real destructor must not run over a half-built object"
    );
}

#[test]
fn cboxwith_layout_thin_vs_fat() {
    // A ZST dropper keeps the fat owner pointer-sized, with the niche.
    assert_eq!(
        size_of::<CBoxWith<FooFull, StorageFree>>(),
        size_of::<*mut foo_st>()
    );
    // With real state it is genuinely ptr + inline state (here a fn pointer).
    assert_eq!(
        size_of::<CBoxWith<Stack<X509, Borrowed>, ElemFree>>(),
        size_of::<*mut stack_st>() + size_of::<ElemFree>(),
    );
}
