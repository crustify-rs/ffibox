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
use core::ptr::{addr_of, addr_of_mut, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ffibox::{define_ctype, impl_cdrop, CBorrowedPtr, CBox, CCell, CDrop};

#[repr(C)]
pub struct foo_st {
    pub a: u64,
    pub p: *mut u8,
}

// Base case — via the macro.
define_ctype!(FooFull, FooFullRef, FooFullMut, foo_st);

static FULL_FREES: AtomicUsize = AtomicUsize::new(0);

/// Mock full destructor for a formed `foo_st`.
///
/// # Safety
///
/// `p` must be a live, uniquely-owned `Box<foo_st>` allocation.
unsafe fn foo_full_free(p: *mut foo_st) {
    FULL_FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller transfers the `Box` allocation.
    drop(unsafe { Box::from_raw(p) });
}

/// The full destructor, run once the object is formed.
#[derive(Clone, Copy, Debug, Default)]
pub struct FooFullFree;
impl_cdrop!(FooFullFree, FooFull, foo_full_free);
pub type FooFullOwned = CBox<FooFull, FooFullFree>;

// --- Hand-written type-generic wrapper: a type-generic container Stack<T, S> ---
#[repr(C)]
pub struct stack_st {
    pub num: i32,
    pub data: *mut *mut core::ffi::c_void,
}
// Free strategies (the `owned_elem` axis).
pub struct Borrowed;
pub struct Owned<D>(PhantomData<D>);
pub struct Item;
pub struct ItemFree;

#[repr(transparent)]
pub struct Stack<T, S>(stack_st, PhantomData<(*const (), T, S)>);

/// The hand-written shared handle. The generic parameters ride along, so
/// borrowing a pointer cannot lose the element type or the free strategy.
#[repr(transparent)]
pub struct StackRef<'a, T, S>(CBorrowedPtr<'a, Stack<T, S>>);
impl<T, S> Clone for StackRef<'_, T, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, S> Copy for StackRef<'_, T, S> {}

/// The hand-written exclusive handle.
#[repr(transparent)]
pub struct StackMut<'a, T, S>(StackRef<'a, T, S>, PhantomData<&'a mut Stack<T, S>>);

// SAFETY: `Stack` is `#[repr(transparent)]` over `stack_st`; both handles
// are transparent over `CBorrowedPtr<'a, Stack<T, S>>` and expose no reference to
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
}

impl<T, S> Stack<T, S> {
    /// All-zero is a valid `stack_st` (an `i32` and a raw pointer).
    fn zeroed() -> Self {
        // SAFETY: all-zero is a valid `stack_st`.
        Self(unsafe { core::mem::zeroed() }, PhantomData)
    }
}

impl<'a, T, S> StackRef<'a, T, S> {
    unsafe fn from_ptr(p: *mut stack_st) -> Option<Self> {
        NonNull::new(p.cast::<Stack<T, S>>()).map(|p| StackRef(unsafe { CBorrowedPtr::new(p) }))
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
    /// Reborrow shared, bound to `&self`. Not `Deref`: its `Target` would have
    /// to be `StackRef<'a, _, _>`, and since that is `Copy`, `*m` would copy a
    /// shared handle out that outlives the borrow while `m` keeps writing.
    fn as_ref(&self) -> StackRef<'_, T, S> {
        self.0
    }
    fn as_mut_ptr(&mut self) -> *mut stack_st {
        self.0 .0.as_non_null().as_ptr().cast()
    }
    /// A setter: `&mut self` on the HANDLE — one pointer of Rust stack.
    fn set_num(&mut self, v: i32) {
        unsafe { addr_of_mut!((*self.as_mut_ptr()).num).write(v) }
    }
}

// Each `Stack<T, S>` alias is zero-cost, picking a strategy — not a redefinition.
pub type StackOfItemBorrowed = Stack<Item, Borrowed>;
pub type StackOfItemOwned = Stack<Item, Owned<ItemFree>>;

#[test]
fn layout_and_niche() {
    // The layout newtype keeps the C struct's size, so it embeds by value.
    assert_eq!(size_of::<FooFull>(), size_of::<foo_st>());
    assert_eq!(size_of::<Stack<Item, Borrowed>>(), size_of::<stack_st>());
    assert_eq!(size_of::<StackOfItemOwned>(), size_of::<stack_st>());

    // The handles are one pointer regardless of the generics, with the niche.
    assert_eq!(
        size_of::<StackRef<'_, Item, Borrowed>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(
        size_of::<StackMut<'_, Item, Borrowed>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(
        size_of::<Option<StackRef<'_, Item, Borrowed>>>(),
        size_of::<*const stack_st>()
    );
    assert_eq!(size_of::<Option<FooFullOwned>>(), size_of::<*mut foo_st>());
}

// Covariance: a longer borrow is usable where a shorter one is expected, just
// like `&'a T`.
fn _covariant<'a>(x: StackRef<'static, Item, Borrowed>) -> StackRef<'a, Item, Borrowed> {
    x
}

#[test]
fn the_hand_written_seam_reads_and_writes() {
    let raw = Box::into_raw(Box::new(stack_st {
        num: 3,
        data: core::ptr::null_mut(),
    }));

    let r: StackRef<'_, Item, Borrowed> = unsafe { StackRef::from_ptr(raw) }.unwrap();
    assert_eq!(r.num(), 3);

    let mut m: StackMut<'_, Item, Borrowed> = StackMut(
        StackRef(unsafe { CBorrowedPtr::new(NonNull::new(raw.cast()).unwrap()) }),
        PhantomData,
    );
    m.set_num(7);
    // Getters reach the shared handle through `as_ref`, bound to the borrow.
    assert_eq!(m.as_ref().num(), 7);

    // `zeroed` builds a value for inline storage; the pointer to it comes from
    // `addr_of_mut!`, never `&mut`.
    let mut inline: Stack<Item, Borrowed> = Stack::zeroed();
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
    let mut b = unsafe { FooFullOwned::from_c(raw) }.unwrap();

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
// CBox<T, D> with a stateful policy, and the construction-phase handle
// ---------------------------------------------------------------------------

static POP_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
static ELEM_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn item_free_mock(_p: *mut core::ffi::c_void) {
    ELEM_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
}

/// The policy IS the runtime state: the caller-supplied element-free fn.
#[derive(Clone, Copy)]
pub struct ElemFree(unsafe extern "C" fn(*mut core::ffi::c_void));

// SAFETY: stand-in for `list_pop_free(ptr, self.0)` — calls the fn it
// carried once, as for a one-element stack, then reclaims the Box-backed mock
// allocation exactly once.
unsafe impl CDrop<Stack<Item, Borrowed>> for ElemFree {
    unsafe fn c_drop(&self, ptr: NonNull<Stack<Item, Borrowed>>) {
        POP_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the carried element free accepts any element pointer.
        unsafe { (self.0)(core::ptr::null_mut()) };
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<stack_st>()) });
    }
}

// A hand-written, stateful policy: no `Default`, so the box is built with
// `from_c_with`, which takes the policy value.
pub type StackOwned = CBox<Stack<Item, Borrowed>, ElemFree>;

#[test]
fn stateful_policy_runs_with_runtime_state() {
    POP_FREE_CALLS.store(0, Ordering::SeqCst);
    ELEM_FREE_CALLS.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(stack_st {
        num: 0,
        data: core::ptr::null_mut(),
    }));
    // The free fn is fixed HERE, at the seam — the whole point of the fat owner.
    let owned = unsafe { StackOwned::from_c_with(raw, ElemFree(item_free_mock)) }.unwrap();
    assert_eq!(owned.as_ref().num(), 0);
    drop(owned);

    assert_eq!(POP_FREE_CALLS.load(Ordering::SeqCst), 1);
    // Observed by its call, not its address: two pointers to one fn need not
    // compare equal (and under Miri they do not).
    assert_eq!(
        ELEM_FREE_CALLS.load(Ordering::SeqCst),
        1,
        "the policy must carry the runtime free fn into teardown",
    );
}

// The construction phase `CBoxUninit` used to model: hold the allocation under
// a storage-only policy while filling it, then promote. One-way, and a type
// change, so a half-built object cannot reach code expecting a formed one.
static STORAGE_FREES: AtomicUsize = AtomicUsize::new(0);
// Serialises the tests that reset and read `STORAGE_FREES` / `FULL_FREES`.
static CONSTRUCTION_LOCK: Mutex<()> = Mutex::new(());

pub struct StorageFree;
// SAFETY: reclaims exactly the raw allocation, touching no field — the
// construction-phase contract.
unsafe impl CDrop<FooFull> for StorageFree {
    unsafe fn c_drop(&self, ptr: NonNull<FooFull>) {
        STORAGE_FREES.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(ptr.as_ptr().cast::<foo_st>()) });
    }
}

#[test]
fn construction_phase_promotes_with_exactly_one_teardown() {
    let _guard = CONSTRUCTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    STORAGE_FREES.store(0, Ordering::SeqCst);
    FULL_FREES.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(foo_st {
        a: 0,
        p: core::ptr::null_mut(),
    }));
    let mut slot = unsafe { CBox::<FooFull, StorageFree>::from_c_with(raw, StorageFree) }.unwrap();
    unsafe { addr_of_mut!((*slot.as_mut().as_mut_ptr()).a).write(42) };

    // Promote: the storage policy is swapped out, `FooFullFree` takes over.
    let (formed, StorageFree): (FooFullOwned, _) = unsafe { slot.with_policy(FooFullFree) };
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
    let _guard = CONSTRUCTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    STORAGE_FREES.store(0, Ordering::SeqCst);
    FULL_FREES.store(0, Ordering::SeqCst);

    let raw = Box::into_raw(Box::new(foo_st {
        a: 0,
        p: core::ptr::null_mut(),
    }));
    {
        let _slot = unsafe { CBox::<FooFull, StorageFree>::from_c_with(raw, StorageFree) };
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
fn owned_ptr_layout_thin_vs_fat() {
    // A ZST policy keeps the owner pointer-sized, with the niche.
    assert_eq!(
        size_of::<CBox<FooFull, StorageFree>>(),
        size_of::<*mut foo_st>()
    );
    assert_eq!(
        size_of::<Option<CBox<FooFull, StorageFree>>>(),
        size_of::<*mut foo_st>()
    );
    // With real state it is genuinely ptr + inline state (here a fn pointer).
    assert_eq!(
        size_of::<CBox<Stack<Item, Borrowed>, ElemFree>>(),
        size_of::<*mut stack_st>() + size_of::<ElemFree>(),
    );
}
