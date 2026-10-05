# ffibox

Generic smart pointers and traits for building safe Rust wrappers over C
types and pointers. Designed for arbitrary user-space C interop, including the
case where you need direct field access and a C-ABI-compatible layout (porting
C internals to Rust in place), not just opaque-handle wrapping.

## Why

Wrapping a C API in Rust means re-homing C's ownership and lifecycle
conventions into RAII. The recurring shapes:

- A C-allocated object with a destructor (`free`), and maybe a copy (`dup`).
- A refcounted object shared by several holders (`up_ref` / `unref`), maybe
  guarded by its own lock.
- Types with multiple or runtime-conditional destructors.
- A by-value struct whose teardown disposes its fields but not the struct.
- An owned length-aware buffer or NUL-terminated `char *` with a C destructor.
- A type-erased `void *` that stays opaque end to end.

Each shape gets one owner type, generic over a **policy** — a type you declare
that names the C routines. `impl_cdrop!` and its siblings bind a C routine to a
policy; a type alias names the pair: `pub type FooBox = CBox<Foo, FooFree>;`.

## Mental model in one paragraph

No reference to C-owned memory is ever formed. Each C type gets a layout type
`Foo` (`#[repr(transparent)]` over `ffi::foo`) and two borrowed handles,
`FooRef<'a>` / `FooMut<'a>`, each one pointer wide and carrying the getters and
setters; field access projects a raw pointer out of the handle. Ownership
comes in three shapes: `CBox<Foo, P>` is the sole owner of an object behind a
pointer and releases it through the policy `P`; `CArc<Foo, P>` is one of
several counted references and hands out only the shared handle (its
`CGuardedArc` sibling reaches the object through the object's own lock); and
`CVal<Foo, P>` holds the object inline and disposes its resources on drop.
Strings, buffers and borrowed runs get their own types. [Section 1](#1-the-types-you-get) lists them all,
[section 2](#2-policies--what-teardown-means) covers policies, and
[section 3](#3-decision-procedure) walks from a C declaration to the type it
wants.

## Examples

Each example is self-contained and compiled as a doctest; the `sys` modules
declare C routines a real `*-sys` crate would provide.

### A boxed object — `CBox`

```rust,no_run
use ffibox::{define_ctype, impl_cdrop, impl_cdupclone, CBox};

mod sys {
    #[repr(C)] pub struct point_st { pub x: i32, pub y: i32 }
    extern "C" {
        pub fn point_new(x: i32, y: i32) -> *mut point_st;
        pub fn point_dup(p: *mut point_st) -> *mut point_st;
        pub fn point_free(p: *mut point_st);
    }
}

define_ctype!(Point, PointRef, PointMut, sys::point_st);

// The policy: a type you declare, bound to the C routines.
#[derive(Clone, Copy, Debug, Default)]
pub struct PointFree;
impl_cdrop!(PointFree, Point, sys::point_free);
impl_cdupclone!(PointFree, Point, sys::point_dup);

pub type PointBox = CBox<Point, PointFree>;

impl Point {
    pub fn new(x: i32, y: i32) -> Option<PointBox> {
        // `from_c` adopts the C type's pointer, no cast.
        unsafe { PointBox::from_c(sys::point_new(x, y)) }
    }
}

// Getters live on the shared handle, setters on the exclusive one.
impl PointRef<'_> {
    pub fn x(&self) -> i32 {
        unsafe { core::ptr::addr_of!((*self.as_ptr()).x).read() }
    }
}
impl PointMut<'_> {
    pub fn set_x(&mut self, v: i32) {
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
    }
}

// Downstream: no `unsafe`, no raw pointer.
let mut p = Point::new(3, 4).unwrap();
assert_eq!(p.as_ref().x(), 3);
p.as_mut().set_x(5);
let q = p.clone();           // point_dup
// `point_free` runs twice here.
```

`CBox` is a foreign type, so constructors go on the local `Foo` (or in free
functions) rather than in an `impl CBox<Foo, _>` block.

### A shared object — `CArc` / `CGuardedArc`

```rust,no_run
use ffibox::{define_ctype, impl_cdrop, impl_cguarded, impl_crefclone, CArc, CGuardedArc};

mod sys {
    #[repr(C)] pub struct session_st { _opaque: [u8; 0] }
    #[repr(C)] pub struct store_st { _opaque: [u8; 0] }
    extern "C" {
        pub fn session_new() -> *mut session_st;
        pub fn session_up_ref(s: *mut session_st);         // cannot fail
        pub fn session_free(s: *mut session_st);           // a down-ref
        pub fn store_new() -> *mut store_st;
        pub fn store_up_ref(s: *mut store_st) -> i32;      // 1 on success
        pub fn store_free(s: *mut store_st);
        pub fn store_write_lock(s: *mut store_st) -> i32;  // 1 on success
        pub fn store_read_lock(s: *mut store_st) -> i32;
        pub fn store_unlock(s: *mut store_st) -> i32;
        pub fn store_add(s: *mut store_st, v: i32);
        pub fn store_len(s: *const store_st) -> usize;
    }
}

// Shared, read-only: the down-ref and the up_ref on one policy.
define_ctype!(Session, SessionRef, SessionMut, sys::session_st);
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionUnref;
impl_cdrop!(SessionUnref, Session, sys::session_free);
impl_crefclone!(SessionUnref, Session, sys::session_up_ref);
pub type SessionArc = CArc<Session, SessionUnref>;

let a = unsafe { SessionArc::from_c(sys::session_new()) }.unwrap();
let b = a.clone();                      // session_up_ref; same object
assert!(SessionArc::ptr_eq(&a, &b));
let _shared = b.as_ref();               // shared handle only: no `as_mut`

// Mutated by every holder: each access takes the object's own lock. Routines
// that report a status are bound with `ok`, so a failure is never discarded.
define_ctype!(Store, StoreRef, StoreMut, sys::store_st);
// SAFETY: the store's state is only touched under its lock.
unsafe impl Send for Store {}
unsafe impl Sync for Store {}
impl_cguarded!(Store, lock = sys::store_write_lock, unlock = sys::store_unlock,
               read_lock = sys::store_read_lock, read_unlock = sys::store_unlock,
               ok = |r| r == 1);
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreUnref;
impl_cdrop!(StoreUnref, Store, sys::store_free);
impl_crefclone!(StoreUnref, Store, sys::store_up_ref, ok = |r| r == 1);
pub type StoreArc = CGuardedArc<Store, StoreUnref>;

impl StoreRef<'_> {
    pub fn len(&self) -> usize { unsafe { sys::store_len(self.as_ptr()) } }
}
impl StoreMut<'_> {
    pub fn add(&mut self, v: i32) { unsafe { sys::store_add(self.as_mut_ptr(), v) } }
}

let s = unsafe { StoreArc::from_c(sys::store_new()) }.unwrap();
let t = s.clone();
std::thread::spawn(move || t.write().as_mut().add(42));   // write lock
let n = s.read().as_ref().len();                          // read lock
```

### A global behind its lock — `CGuardedRef`

```rust,no_run
use core::ptr::{addr_of, addr_of_mut};
use ffibox::{define_ctype, impl_cguarded, CGuardedRef};

mod sys {
    #[repr(C)] pub struct registry_st { pub count: i32 }
    extern "C" {
        pub static mut registry: registry_st;  // a C global
        pub fn registry_lock() -> i32;         // guards `registry`; 0 on success
        pub fn registry_unlock() -> i32;
    }
}

define_ctype!(Registry, RegistryRef, RegistryMut, sys::registry_st);
// SAFETY: `registry` is only touched under `registry_lock`.
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

// The lock is a separate global taking no arguments: adapters give it the
// shape `impl_cguarded!` calls.
unsafe fn lock(_: *mut sys::registry_st) -> i32 { unsafe { sys::registry_lock() } }
unsafe fn unlock(_: *mut sys::registry_st) -> i32 { unsafe { sys::registry_unlock() } }
impl_cguarded!(Registry, lock = lock, unlock = unlock, ok = |r| r == 0);

impl RegistryRef<'_> {
    pub fn count(&self) -> i32 { unsafe { addr_of!((*self.as_ptr()).count).read() } }
}
impl RegistryMut<'_> {
    pub fn set_count(&mut self, v: i32) {
        unsafe { addr_of_mut!((*self.as_mut_ptr()).count).write(v) }
    }
}

/// Never freed, so no owner and no policy: a `'static` borrow under the lock.
pub fn registry() -> CGuardedRef<'static, Registry> {
    unsafe { CGuardedRef::from_ptr(addr_of_mut!(sys::registry)) }.unwrap()
}

let mut w = registry().write();          // registry_lock
let n = w.as_ref().count();
w.as_mut().set_count(n + 1);             // registry_unlock when `w` drops
```

### An opaque payload — `CVoidBox`

```rust,no_run
use ffibox::{impl_cdrop_void, CVoidBox};

mod sys {
    use core::ffi::c_void;
    extern "C" {
        pub fn arena_alloc(n: usize) -> *mut c_void;
        pub fn arena_fill(p: *mut c_void, n: usize);
        pub fn arena_free(p: *mut c_void);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaFree;
impl_cdrop_void!(ArenaFree, sys::arena_free);

/// Arena bytes; the type names the destructor, the bytes stay opaque.
pub type ArenaBuf = CVoidBox<ArenaFree>;

let buf = unsafe { ArenaBuf::from_raw(sys::arena_alloc(64)) }.unwrap();
unsafe { sys::arena_fill(buf.as_ptr(), 64) };
// `arena_free` runs here.
```

### By value — `CVal`

```rust,no_run
use ffibox::{define_ctype, impl_cdispose, CVal};

mod sys {
    #[repr(C)] pub struct buf_st { pub ptr: *mut u8, pub len: usize }
    #[repr(C)] pub struct rational_st { pub num: i32, pub den: i32 }
    extern "C" {
        pub fn buf_init(b: *mut buf_st, n: usize) -> i32;
        pub fn buf_dispose(b: *mut buf_st);
    }
}

define_ctype!(Buf, BufRef, BufMut, sys::buf_st);
#[derive(Clone, Copy, Debug, Default)]
pub struct BufDispose;
impl_cdispose!(BufDispose, Buf, sys::buf_dispose);
// Rust owns the struct BY VALUE; C owns what its fields point at.
pub type BufVal = CVal<Buf, BufDispose>;

let mut b = BufVal::new(Buf::zeroed());
let mut handle = b.as_mut();      // BufMut<'_>, as on a CBox
unsafe { sys::buf_init(handle.as_mut_ptr(), 64) };   // C fills Rust's storage
// `buf_dispose` runs when `b` drops; the struct itself is Rust's storage.

// A resource-free struct needs no wrapper: the `define_ctype!` type is the value.
define_ctype!(Rational, RationalRef, RationalMut, sys::rational_st);
impl RationalMut<'_> {
    pub fn set_num(&mut self, v: i32) {
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).num).write(v) }
    }
}
let mut q = Rational::zeroed();
q.as_mut().set_num(1);
```

### A buffer and a string — `CVec` / `CStrBox`

```rust,no_run
use ffibox::{impl_cdrop_str, impl_cdupclone_str, impl_clendrop, CStrBox, CVec};

mod sys {
    use core::ffi::c_char;
    extern "C" {
        pub fn oids_new(n: usize) -> *mut u32;
        pub fn oids_free(p: *mut u32, n: usize);
        pub fn lib_name() -> *mut c_char;
        pub fn lib_str_free(s: *mut c_char);
        pub fn lib_strdup(s: *const c_char) -> *mut c_char;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OidsFree;
// Freeing needs the element count back: an adapter, passed by path.
unsafe fn oids_free(ptr: *mut u8, byte_len: usize) {
    unsafe { sys::oids_free(ptr.cast(), byte_len / core::mem::size_of::<u32>()) }
}
impl_clendrop!(OidsFree, oids_free);
pub type Oids = CVec<u32, OidsFree>;

#[derive(Clone, Copy, Debug, Default)]
pub struct LibStrFree;
impl_cdrop_str!(LibStrFree, sys::lib_str_free);     // takes *mut c_char
impl_cdupclone_str!(LibStrFree, sys::lib_strdup);
pub type LibStr = CStrBox<LibStrFree>;

let v = unsafe { Oids::from_raw_parts(sys::oids_new(3), 3) }.unwrap();
assert_eq!(v.as_slice().len(), 3);
let s = unsafe { LibStr::from_raw(sys::lib_name()) }.unwrap();
assert_eq!(s.to_str(), Ok("name"));
```

---

## 1. The types you get

This is the reference table for what each type is; the rustdoc links here
rather than repeating it.

| Scope | Type | From | Role |
|-------|------|------|------|
| per C type | `Foo` | `define_ctype!` | the layout, the C struct's size: `#[repr(transparent)]` over `ffi::foo`, embeds by value in a `#[repr(C)]` mirror, and is what a `CBox` points at. Never referenced over C-owned memory. Also the value itself when it owns no resources (`AVRational`). |
| | `FooRef<'a>` | `define_ctype!` | shared borrow, one pointer wide, `Copy`; the getters |
| | `FooMut<'a>` | `define_ctype!` | exclusive borrow, move-only; the setters, and the getters through `as_ref()` |
| owners | `CBox<Foo, P>` | ffibox | the sole owner of an object behind a pointer, released by `P` on drop; `Clone` when `P` deep-copies |
| | `CVal<Foo, P>` | ffibox | an owned value held inline, its resources disposed by `P` on drop |
| | `CStrBox<P>` | ffibox | an owned NUL-terminated `char *`; read-only `CStr` / `str` / byte views |
| | `CVec<T, P>` | ffibox | an owned `(ptr, len)` array, NULL when empty; `&[T]` for plain elements, `CSlice` for wrapped C objects |
| | `CVoidBox<P>` | ffibox | an owned `void *` that is never looked inside (`CBox<c_void, P>`) |
| shared owners | `CArc<Foo, P>` | ffibox | one counted reference to a refcounted object; `Clone` through the up_ref; shared handles only, plus `get_mut` / `make_mut` when the count proves it sole or after a copy |
| | `CGuardedArc<Foo, P>` | ffibox | a `CArc` reached through the object's own lock: `read()` → `CReadGuard`, `write()` → `CWriteGuard`, each unlocking on drop — `Arc<RwLock<_>>` in one type, as the lock lives in the object |
| guarded borrow | `CGuardedRef<'a, Foo>` | ffibox | an object reached through its own lock and never released, typically a C global under its C lock (`'static`): `read()` / `write()` as on a `CGuardedArc` — `&RwLock<_>` to its `Arc<RwLock<_>>` |
| views | `CSlice<'a, T>` / `CSliceMut<'a, T>` | ffibox | any borrowed run of elements — a buffer's, a struct field's, a getter's — as handles or copies, never a `&[T]` |

A wrapper that needs generic parameters (a lifetime-carrying layout type, or
derived sub-types over a generic field) is written by hand against the same
contract — see [Under the hood](#4-under-the-hood-hand-written-wrappers).

### Conventions

- **Accessors live on the handles.** Getters on `FooRef<'a>` taking `&self`,
  setters on `FooMut<'a>` taking `&mut self`; `FooMut` reaches the getters with
  `as_ref()`, so they are written once. Both project a raw pointer out of the
  handle and read or write through `addr_of!` / `addr_of_mut!`.
- **Every holder reaches the handles the same way:** `as_ref()` / `as_mut()` on
  `Foo`, `CBox`, `CVal`, a `CWriteGuard` and `FooMut` itself (where `as_mut()`
  is the reborrow `&mut` gets implicitly: `helper(m.as_mut())` keeps `m`);
  `as_ref()` alone on `CArc` and a `CReadGuard`. `CSliceMut` reborrows the same
  way, and splits and sub-ranges like `&mut [T]`. Never `Deref`:
  `Deref::Target` cannot name a lifetime taken from `&self`, and since `FooRef`
  is `Copy`, a `Deref<Target = FooRef<'a>>` on `FooMut` would let safe code copy
  a shared handle out while keeping the exclusive one.
- **No reference covers C memory.** `&FooRef` covers one pointer of Rust stack,
  never the object. The one place a reference covers a C struct's bytes is
  `Foo::as_ref` / `as_mut` on a value Rust holds inline.
- **Adopting a pointer is `unsafe`; giving one out is not — as on `Box`.**
  `from_raw*` / `from_c*` are `unsafe`, since they assert ownership of what the
  pointer addresses; `into_raw*` / `into_c` and `as_ptr` / `as_c_ptr` are safe,
  since a raw pointer asserts nothing until someone dereferences it. The
  owners' `from_raw` / `into_raw` / `as_ptr` speak the Rust-side pointee; `CBox`'s `from_c` / `into_c` /
  `as_c_ptr` speak the C type, so C interop is cast-free. The handles' seams
  (`from_ptr`, `as_ptr`, `as_mut_ptr`, the `void *` pair) are `pub(crate)`
  in the crate that invokes `define_ctype!`.
- **`from_raw*` adopts ownership; `from_ptr*` borrows.** Pick the verb by what
  you return.
- **Thread safety is opt-in for wrapped C types, opt-out for raw memory.**
  `Foo` carries a zero-sized marker withholding `Send` / `Sync`; `CBox`,
  `CVal`, `CVec` and the views over a `Foo` inherit them once the wrapper
  writes `unsafe impl Send / Sync for Foo` with a safety proof, on the terms
  of their std counterparts — `CBox` like `Box`, while `CArc` and
  `CGuardedArc` need `Foo: Send + Sync`, like `Arc` and `Arc<RwLock<_>>`.
  Owners that never name a `Foo` — `CStrBox`, `CVoidBox`, and a `CVec` of
  plain elements (`CVec<u8, _>`) — follow their policy alone, and a unit-struct
  policy is `Send + Sync`, so **they cross threads by default**. That is right
  for an allocator any thread may free into (`free`, `OPENSSL_free`,
  `av_free`); for one that must free on the allocating thread, opt out by
  giving the policy a `PhantomData<*const ()>` field.

---

## 2. Policies — what teardown means

Every owner's destructor is a **policy**: a type you declare, usually a unit
struct deriving `Clone, Copy, Debug, Default`, implementing the lifecycle
traits. Bind a C routine to it with the `impl_*!` macros, or implement the
traits by hand when teardown needs runtime state. A unit struct is
`Send + Sync`, which makes a `CStrBox`, `CVoidBox` or plain-element `CVec`
under it free on whichever thread drops it; a thread-bound allocator's policy
carries `PhantomData<*const ()>` instead (see the conventions in
[section 1](#1-the-types-you-get)).

| Trait | Defines | Macro | Drives |
|-------|---------|-------|--------|
| `CDrop<T>` | `c_drop` — a `*_free`, or a refcount down-ref | `impl_cdrop!(P, Foo, f)`; `_str` / `_void` for `c_char` / `c_void` | `CBox`, `CStrBox`, `CArc` |
| `CDupClone<T>: CDrop<T>` | `c_dup` — a deep copy (a NEW pointer, NULL on failure) | `impl_cdupclone!(P, Foo, f)`; `_str` for `strdup` | `Clone` / `try_clone` |
| `CRefClone<T>: CDrop<T>` | `c_up_ref` — a refcount increment on the SAME pointer; `c_is_sole_owner` (default `false`) | `impl_crefclone!(P, Foo, f)`; `…, ok = |r| r == 1`, `…, sole = g` | `CArc` / `CGuardedArc` `Clone`, `get_mut`, `make_mut` |
| `CLenDrop` | `c_drop_len` — a buffer free, given the byte length | `impl_clendrop!(P, f)` | `CVec` |
| `CLenClone: CLenDrop` | `c_clone_len` — a buffer memdup (`T: Copy` only) | `impl_clenclone!(P, f)` | `CVec`'s `Clone` |
| `CDispose<T>` | `c_dispose` — `*_uninit` / `*_clear` on a value | `impl_cdispose!(P, Foo, f)` | `CVal` |
| `CGuarded`, on `Foo` | `c_lock` / `c_unlock`, optionally `c_read_lock` / `c_read_unlock` | `impl_cguarded!(Foo, lock = f, unlock = g)`; `…, ok = |r| r == 0` | the guards of `CGuardedArc` / `CGuardedRef` |

**Because the policy is a type parameter, an owner cannot exist without a
destructor, and one C type can have several.** `CBox<Foo, FooFree>` next to
`CBox<Foo, FooUnref>`: exactly one ever runs per box, chosen by its type. And
since each clone trait sits on the same policy as the drop trait, a `*_dup` is
always settled by its `*_free`.

**`CBox` is a sole owner; `CArc` shares.** `CBox::as_mut` hands out the
exclusive handle, which is sound only while nothing else uses the object, so
`CBox` clones only by deep copy. A refcounted object fits a `CBox` while the
box holds its only reference (with the down-ref as `c_drop`); `CArc::from`
shares it, and `CBox::try_from` (or `try_into_box`) takes it back once the count
is 1 again.

**Shared and mutable means locked.** A `CArc` has no unlocked write path: its
exclusive handle needs the count to prove the reference sole (`get_mut`, which
needs `c_is_sole_owner` — an Acquire read of a count that covers every
reference, C's own included), or a private copy (`make_mut`, through
`CDupClone` — never a byte copy, which would duplicate the object's
sub-allocations, its count and its lock). An object every holder mutates goes
in a `CGuardedArc`, which has no unlocked path at all: readers take the read
lock together, a writer takes the write lock alone, and `CGuarded`'s contract
makes C take the same lock. The exclusive lock must not be reentrant: a second
`write` on the same thread blocks forever, or panics when the lock reports the
self-deadlock (`pthread_rwlock_*`'s `EDEADLK`), rather than hand out a second
handle. A failing C lock call panics, as `std`'s locks do; there is no
poisoning.

**Routines are plain paths, type-checked.** Each macro calls the routine with a
pointer of the exact C type; a routine for the wrong type does not compile.
Where a call can fail — an up_ref, a lock — the routine either returns `()` or
is bound with `ok = |r| r == 1` (whatever its success value is); a status code
is never silently discarded, so `pthread_rwlock_wrlock`'s `EDEADLK` becomes a
panic rather than a second exclusive handle. A
destructor of any other shape — one taking the slot (`ffi::foo_free(&mut p)`),
a `void *` allocator free, extra arguments — goes behind a small `unsafe fn`
adapter passed by path. Teardown is unconditional: a gate that suppresses it on
some paths folds into the routine itself.

**Runtime-state policies.** When teardown needs a value chosen at the wrapping
site (`OPENSSL_sk_pop_free(stack, elem_free_fn)`), write the policy by hand — a
struct holding the state, implementing `CDrop<T>` (and `CDupClone<T>` + `Clone`
to clone). Without `Default` there is no `from_raw(ptr)`; adopt with
`from_raw_with(ptr, ElemFree(f))` and release with `into_raw_with()`. The box
then carries the state and is no longer pointer-sized.

**The construction phase.** An allocation Rust is still filling in is held as
`CBox<Foo, StorageFree>` — a hand-written policy that frees the storage and
touches no field — then promoted with `with_policy(FooFree)`. Bail with `?`
before promoting and only the storage is freed.

---

## 3. Decision procedure

Walk from a C declaration to the type it wants. Each step narrows one
dimension; the answer is always a type from [section 1](#1-the-types-you-get).

**Step 1 — Is it owned in Rust?** That is: does dropping the Rust value
release it? This is about who *releases* the object, not who allocated it — a
`CBox` usually points at memory C allocated.

- **No — C, or a parent object, releases it.** Borrow it:
  - one object → `FooRef<'a>` / `FooMut<'a>`, its lifetime tied to whatever
    keeps it alive (a parent's handle, the call);
  - a run of elements → `CSlice<'a, T>` / `CSliceMut<'a, T>`;
  - an object every access reaches under its own lock — typically a C global
    and the C lock protecting it → `CGuardedRef<'a, Foo>`, `'static` for a
    global, with `impl_cguarded!` on `Foo`;
  - a NUL string → `&CStr` / `&str` / `&[u8]` tied to the owner's borrow;
  - an out-parameter slot → `&'a mut MaybeUninit<T>` from
    `ptr.cast::<MaybeUninit<T>>().as_mut()`; no ffibox type;
  - a `void *` → if it is really a `ffi::foo`, erase a `FooRef` with
    `as_void_ptr` and recover it with `from_void_ptr`; if it is a cookie never
    looked inside, pass the raw `*mut c_void` through unchanged.

  A borrow that must outlive its parent's scope (stored next to the parent,
  sent to a thread) cannot be expressed as a lifetime; copy the child into an
  owned object instead.
- **Yes** → step 2.

**Step 2 — What shape is the owned thing?**

- **A NUL-terminated string** (`char *`, no stored length) → `CStrBox<P>`.
  `strlen` recovers the length, so one `CDrop<c_char>` policy covers a plain
  free and a clearing free alike. Its views are read-only, like `CStr`.
- **A counted array** → `CVec<T, P>`, one policy per allocator family. Plain
  elements (`CElem`: integers, floats, raw pointers, `MaybeUninit`) read out as
  a real `&[T]` because the buffer is exclusively owned; wrapped C objects come
  out as handles through `as_handles` → `CSlice`, since `&[Foo]` would cover
  them. Where C returns NULL with 0 for an empty array, adopt with
  `from_raw_parts_or_empty`; plain `from_raw_parts` treats every NULL as
  failure.
- **One object held inline** — a local, a field, an array element, with no
  pointer of its own:
  - owns no resources → the `Foo` itself;
  - owns resources and stands alone → `CVal<Foo, P>`;
  - embedded in a parent C struct → a bare `Foo`, disposed by the parent's
    teardown (a `Drop` on it would dispose twice);
  - address-sensitive (points into itself, or C recorded its address) → not
    inline at all: behind a pointer, as below.
- **One object behind a pointer** → `CBox<Foo, P>` if this is the only
  reference, `CArc<Foo, P>` / `CGuardedArc<Foo, P>` if it is one of several
  counted ones; a type-erased `void *` payload → `CVoidBox<P>`.

**Step 3 — Which policy?** (boxes, strings, buffers)

| C routines | Policy |
|------------|--------|
| `foo_free` | `impl_cdrop!` |
| `foo_free` + `foo_dup` | `impl_cdrop!` + `impl_cdupclone!` |
| `foo_unref`, held as the only reference | `impl_cdrop!` with the down-ref; no `Clone` |
| `foo_unref` + `foo_up_ref`, shared, read-only | `impl_cdrop!` + `impl_crefclone!` → `CArc` |
| … and a readable count | `impl_crefclone!(…, sole = g)`: `get_mut` without a copy |
| … mutated by every holder, with a lock | + `impl_cguarded!` on `Foo` → `CGuardedArc` |
| a global, never freed, under a C lock | no policy: `impl_cguarded!` on `Foo` → `CGuardedRef<'static, Foo>` |
| a counted reference with no up_ref | `impl_cdrop!` alone → `CArc` without `Clone` |
| teardown needs runtime state | a hand-written policy; adopt with `from_raw_with` |
| built in Rust, not by a C constructor | storage-only policy → `with_policy` |
| destructor of another shape | an `unsafe fn` adapter, passed by path |

An owned pointer crosses the FFI boundary on the owner's raw seam: `into_raw`
surrenders it to C, `from_raw` adopts one C allocated.

---

## 4. Under the hood (hand-written wrappers)

Only a wrapper `define_ctype!` cannot express — generic parameters, a
lifetime-carrying layout type — needs this layer.

| Item | Role | Source |
|------|------|--------|
| `CCell` | the linking trait: `type C` (the FFI type), `type Ref<'a>`, `type Mut<'a>`. No methods. Its `unsafe impl` promises that `Self` is layout-compatible with `C`, and that `Ref` / `Mut` are `#[repr(transparent)]` over `CBorrowedPtr<'a, Self>` with no `Drop`, and `Mut` invariant in `Self` (a `PhantomData<&'a mut Self>` field) — ffibox builds handles from that layout, and checks their size at compile time. | `src/traits.rs` |
| `CBorrowedPtr<'a, T>` | what every handle wraps: one pointer tagged with the borrow's lifetime, `Copy` and covariant like `&'a T`. | `src/refs.rs` |
| `CElem` | marker for buffer elements every bit pattern of which is valid; a wrapped C type implements `CCell` instead, so `&[Foo]` does not typecheck. | `src/traits.rs` |

**Layout.** With a zero-sized policy, `CBox<Foo, P>` and the arcs are
pointer-sized and `Option<CBox<Foo, P>>` is a null-niche `*mut ffi::foo`, so it
substitutes for a raw pointer field in a `#[repr(C)]` struct; `CVal<Foo, P>` is the size of
`Foo`. The owners are `#[repr(C)]`, not `#[repr(transparent)]` (the compiler
cannot prove a generic policy is a 1-ZST), so passing one *by value* in place
of a pointer across an `extern "C"` signature is not ABI-guaranteed. A
stateful policy makes the owner fat; a buffer is pointer + length, the pointer
NULL when empty, as C's `{ T *ptr; size_t len; }`.

## no_std

ffibox is `#![no_std]`. The `std` feature (on by default) selects
`std::process::abort` for the unrecoverable-failure path (`Clone` or
`make_mut` when the C copy or up_ref fails); without it that path is a
double-panic. Nothing
allocates.

```toml
[dependencies]
ffibox = { version = "0.1", default-features = false }
```

## Maintainers

- Marius Momeu <marius.momeu@berkeley.edu>

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms
or conditions.


## Acknowledgements

This material is based upon work supported by the Defense Advanced Research Projects Agency (DARPA)
Translating All C To Rust (TRACTOR) program under Agreement No. HR00112590134.