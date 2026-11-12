# Examples

Each example is self-contained and compiled as a doctest; the `sys` modules
declare C routines a real `*-sys` crate would provide.

## A boxed object — `CBox`

```rust,no_run
use ffibox::{define_ctype, impl_cdrop, impl_cdupclone, CBox, CMutSeam, CRefSeam};

mod sys {
    #[repr(C)] pub struct point_st { pub x: i32, pub y: i32 }
    unsafe extern "C" {
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

`CBox` is a foreign type, so constructors that take arguments go on the local
`Foo` (or in free functions) rather than in an `impl CBox<Foo, _>` block. A
zero-argument constructor can instead go on the policy, as the next example
shows.

## Constructed by its policy — `CNew` / `CAlloc` / `CAllocZeroed`

```rust,no_run
use core::{mem::MaybeUninit, ptr::NonNull};
use ffibox::{
    define_ctype, impl_cdrop, CAlloc, CAllocZeroed, CBox, CCell, CDrop, CNew, CZeroable,
};

mod sys {
    use core::ffi::c_void;
    #[repr(C)] pub struct frame_st { pub format: i32 }
    #[repr(C)] pub struct rational_st { pub num: i32, pub den: i32 }
    unsafe extern "C" {
        pub fn frame_alloc() -> *mut frame_st;       // sets `format = -1`
        pub fn frame_free(f: *mut frame_st);
        pub fn rational_init(r: *mut rational_st, num: i32, den: i32);
        pub fn mem_alloc(n: usize) -> *mut c_void;   // 16-byte aligned
        pub fn mem_allocz(n: usize) -> *mut c_void;  // zero-filled
        pub fn mem_free(p: *mut c_void);
    }
}

// A type's own zero-argument constructor, paired with its free.
define_ctype!(Frame, FrameRef, FrameMut, sys::frame_st);
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameFree;
impl_cdrop!(FrameFree, Frame, sys::frame_free);
// SAFETY: a fresh, initialised frame, which `frame_free` releases.
unsafe impl CNew<Frame> for FrameFree {
    fn c_new(&self) -> Option<NonNull<Frame>> {
        NonNull::new(unsafe { sys::frame_alloc() }.cast())
    }
}

let frame = CBox::<Frame, FrameFree>::new().unwrap();   // frame_alloc

// A generic allocator: one hand-written policy for every type. Its free never
// reads the object, so one `CDrop<T>` impl also covers `MaybeUninit<T>`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemFree;
// SAFETY: releases `mem_alloc` / `mem_allocz` storage once, without reading it.
unsafe impl<T> CDrop<T> for MemFree {
    unsafe fn c_drop(&self, p: NonNull<T>) {
        unsafe { sys::mem_free(p.as_ptr().cast()) }
    }
}
// SAFETY: fresh storage of `T`'s size; over-aligned types are rejected.
unsafe impl<T: CCell> CAlloc<T> for MemFree {
    fn c_alloc(&self) -> Option<NonNull<MaybeUninit<T>>> {
        const { assert!(core::mem::align_of::<T>() <= 16) };
        NonNull::new(unsafe { sys::mem_alloc(core::mem::size_of::<T>()) }.cast())
    }
}
// SAFETY: as above, zero-filled.
unsafe impl<T: CZeroable> CAllocZeroed<T> for MemFree {
    fn c_alloc_zeroed(&self) -> Option<NonNull<T>> {
        const { assert!(core::mem::align_of::<T>() <= 16) };
        NonNull::new(unsafe { sys::mem_allocz(core::mem::size_of::<T>()) }.cast())
    }
}

define_ctype!(Rational, RationalRef, RationalMut, sys::rational_st);
// SAFETY: 0/0 is a value every accessor accepts; nothing reads it as a divisor.
unsafe impl CZeroable for Rational {}

let zero = CBox::<Rational, MemFree>::new_zeroed().unwrap();    // mem_allocz
let mut slot = CBox::<Rational, MemFree>::new_uninit().unwrap(); // mem_alloc
unsafe { sys::rational_init(slot.as_c_ptr(), 1, 2) };           // C fills in place
let half = unsafe { slot.assume_init() };   // CBox<Rational, MemFree>
// `mem_free` runs for both, and `frame_free` for `frame`.
```

`Frame` is not `CZeroable`, so `CBox::<Frame, MemFree>::new_zeroed()` does not
compile: its C constructor sets a non-zero default that a zeroed frame would
lack. Every constructor returns `None` when C reports failure. `CArc` has the
same `new` / `new_zeroed`, and a filled `CBox` converts with `.into()`.

## A shared object — `CArc`, and its lock

```rust,no_run
use ffibox::{define_ctype, impl_cdrop, impl_cguarded, impl_crefclone, CArc, CLockedSeam, CRefSeam};

mod sys {
    #[repr(C)] pub struct session_st { _opaque: [u8; 0] }
    #[repr(C)] pub struct store_st { _opaque: [u8; 0] }
    unsafe extern "C" {
        pub fn session_new() -> *mut session_st;
        pub fn session_up_ref(s: *mut session_st);         // cannot fail
        pub fn session_free(s: *mut session_st);           // a down-ref
        pub fn store_new() -> *mut store_st;
        pub fn store_up_ref(s: *mut store_st) -> i32;      // 1 on success
        pub fn store_free(s: *mut store_st);
        pub fn store_name(s: *const store_st) -> i32;      // fixed after setup
        pub fn store_add(s: *mut store_st, v: i32) -> i32; // locks internally
        pub fn store_lock(s: *mut store_st) -> i32;        // 1 on success
        pub fn store_unlock(s: *mut store_st) -> i32;
        pub fn store_len_locked(s: *const store_st) -> usize; // caller holds the lock
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
let _shared = b.as_ref();               // shared handle; no `as_mut`

// A lock over part of the object. `impl_cguarded!` also defines
// `StoreLocked`, the handle a held lock grants.
define_ctype!(Store, StoreRef, StoreMut, sys::store_st);
// SAFETY: the state holders mutate is only touched under the store's lock.
unsafe impl Send for Store {}
unsafe impl Sync for Store {}
impl_cguarded!(Store, StoreLocked, lock = sys::store_lock,
               unlock = sys::store_unlock, ok = |r| r == 1);
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreUnref;
impl_cdrop!(StoreUnref, Store, sys::store_free);
impl_crefclone!(StoreUnref, Store, sys::store_up_ref, ok = |r| r == 1);
pub type StoreArc = CArc<Store, StoreUnref>;

impl StoreRef<'_> {
    // Fixed state, and routines that take the lock themselves.
    pub fn name(&self) -> i32 { unsafe { sys::store_name(self.as_ptr()) } }
    pub fn add(&self, v: i32) -> bool {
        unsafe { sys::store_add(self.as_ptr().cast_mut(), v) == 1 }
    }
}
impl StoreLocked<'_> {
    // The protected state: reads as well as writes need the lock.
    pub fn len(&mut self) -> usize { unsafe { sys::store_len_locked(self.as_mut_ptr()) } }
}

let s = unsafe { StoreArc::from_c(sys::store_new()) }.unwrap();
let t = s.clone();
std::thread::spawn(move || t.as_ref().add(42));   // locks inside C
let n = s.lock().as_locked().len();               // locked until the guard drops
let _ = s.as_ref().name();                        // no lock needed
```

## A global under one lock — `CGuardedRef`

```rust,no_run
use core::ptr::{addr_of, addr_of_mut};
use ffibox::{define_ctype, impl_cguarded, CGuardedRef, CMutSeam, CRefSeam};

mod sys {
    #[repr(C)] pub struct registry_st { pub count: i32 }
    unsafe extern "C" {
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
// shape `impl_cguarded!` calls. `all`: it covers the whole object, so the
// locked handle is `RegistryMut`.
unsafe fn lock(_: *mut sys::registry_st) -> i32 { unsafe { sys::registry_lock() } }
unsafe fn unlock(_: *mut sys::registry_st) -> i32 { unsafe { sys::registry_unlock() } }
impl_cguarded!(Registry, all, lock = lock, unlock = unlock, ok = |r| r == 0);

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

let mut g = registry().lock();           // registry_lock
let n = g.as_ref().count();
g.as_locked().set_count(n + 1);          // registry_unlock when `g` drops
```

## An opaque payload — no erased owner

A `void *` crossing FFI is never a [`CBox`] or [`CArc`]: both own only
`CCell` layout types, such as `define_ctype!` defines. What the payload
really is decides the owner:

- **bytes** (an arena block, an I/O buffer) → `CVec<u8, P>` (or
  `CVec<MaybeUninit<u8>, P>` before it is filled), released through `CLenDrop`;
- **a C struct the caller knows** → `CBox<Foo, P>`, or `CArc<Foo, P>` when
  counted, after `define_ctype!`.

Neither `CBox` nor `CArc` owns a `c_void`: an erased payload whose type Rust
chooses (a refcounted user object, say) gets its own typed owner over the C
routines.

```rust,no_run
use core::mem::MaybeUninit;
use ffibox::{impl_clendrop, CVec};

mod sys {
    use core::ffi::c_void;
    unsafe extern "C" {
        pub fn arena_alloc(n: usize) -> *mut c_void;
        pub fn arena_free(p: *mut c_void);
    }
}

unsafe fn arena_free_len(ptr: *mut u8, _byte_len: usize) {
    unsafe { sys::arena_free(ptr.cast()) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaFree;
impl_clendrop!(ArenaFree, arena_free_len);

/// Arena bytes: a buffer, not an opaque object.
pub type ArenaBuf = CVec<MaybeUninit<u8>, ArenaFree>;

let mut buf = unsafe { ArenaBuf::from_raw_parts(sys::arena_alloc(64).cast(), 64) }.unwrap();
buf.as_mut_slice().fill(MaybeUninit::new(0));
let buf = unsafe { buf.assume_init() }; // CVec<u8, ArenaFree>
// `arena_free` runs here.
```

## By value — `CVal`

```rust,no_run
use ffibox::{define_ctype, impl_cdispose, CMutSeam, CVal, CZeroable};

mod sys {
    #[repr(C)] pub struct buf_st { pub ptr: *mut u8, pub len: usize }
    #[repr(C)] pub struct rational_st { pub num: i32, pub den: i32 }
    unsafe extern "C" {
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
// SAFETY: a null, empty buffer is valid for every accessor and `buf_dispose`.
unsafe impl CZeroable for Buf {}

let mut b = BufVal::zeroed();         // safe: zero bytes own nothing
let mut handle = b.as_mut();      // BufMut<'_>, as on a CBox
unsafe { sys::buf_init(handle.as_mut_ptr(), 64) };   // C fills Rust's storage
// `buf_dispose` runs when `b` drops; the struct itself is Rust's storage.
// A value C filled elsewhere is adopted with `unsafe { BufVal::new(raw) }`:
// the caller vouches that it owns what it points at, and that it is not a
// copy of a struct someone else still owns.

// A resource-free struct needs no wrapper: the `define_ctype!` type is the value.
define_ctype!(Rational, RationalRef, RationalMut, sys::rational_st);
impl RationalMut<'_> {
    pub fn set_num(&mut self, v: i32) {
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).num).write(v) }
    }
}
// SAFETY: 0/0 is a value every accessor accepts; nothing reads it as a divisor.
unsafe impl CZeroable for Rational {}
let mut q = Rational::zeroed();
q.as_mut().set_num(1);
```

## A buffer and a string — `CVec` / `CStrBox`

```rust,no_run
use ffibox::{impl_cdrop_str, impl_cdupclone_str, impl_clendrop, CStrBox, CVec};

mod sys {
    use core::ffi::c_char;
    unsafe extern "C" {
        pub fn oids_new(n: usize) -> *mut u32;
        pub fn oids_free(p: *mut u32, n: usize);
        pub fn lib_name() -> *mut c_char;
        pub fn lib_str_free(s: *mut c_char);
        pub fn lib_strdup(s: *const c_char) -> *mut c_char;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OidsFree;
// Freeing needs the element count back: a private adapter beside the policy,
// passed to the macro by path, rather than a hand-written `CLenDrop`.
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

## A layout type with a lifetime — `define_ctype!` with `phantom`

A C object that keeps a pointer to its parent, as a blob keeps its
repository, lives no longer than the parent: its layout type carries the
parent's borrow. `define_ctype!` takes the lifetime after the type's name and
the marker that carries it as `phantom`; the handles are `BlobRef<'a, 'r>` and
`BlobMut<'a, 'r>`. The policies come from the macros, with the lifetime
spelled `'_`, or named in a leading list where the macro needs it.

```rust,no_run
use core::{ffi::c_int, ptr};
use ffibox::{define_ctype, impl_cdrop, impl_crefclone, CArc, CBox, CRefSeam};

mod sys {
    use core::ffi::c_int;
    #[repr(C)] pub struct repo_st { _opaque: [u8; 0] }
    #[repr(C)] pub struct blob_st { _opaque: [u8; 0] }
    unsafe extern "C" {
        pub fn repo_open() -> *mut repo_st;
        pub fn repo_free(r: *mut repo_st);
        // The blob keeps `repo`, uncounted: it must not outlive it.
        pub fn blob_lookup(out: *mut *mut blob_st, repo: *mut repo_st, id: u32) -> c_int;
        pub fn blob_dup(out: *mut *mut blob_st, b: *mut blob_st) -> c_int; // an up_ref
        pub fn blob_free(b: *mut blob_st);                                  // a down-ref
        pub fn blob_rawcontent(b: *const blob_st) -> *const u8;
        pub fn blob_rawsize(b: *const blob_st) -> usize;
    }
}

define_ctype!(Repo, RepoRef, RepoMut, sys::repo_st);
#[derive(Clone, Copy, Debug, Default)]
pub struct RepoFree;
impl_cdrop!(RepoFree, Repo, sys::repo_free);

// `'r` is the repository's borrow; the blob only reads through it, so the
// marker is covariant, and `Blob` stays `!Send` / `!Sync` as every one does.
define_ctype!(Blob<'r>, BlobRef, BlobMut, sys::blob_st, phantom = &'r Repo);

// The policies, by the macros: `Blob<'_>` is one impl for every `'r`.
// `blob_dup` answers through an out-parameter, so a private adapter gives it
// the shape `impl_crefclone!` calls; no `unsafe impl` is written by hand.
/// # Safety
///
/// `b` is a live blob whose reference the caller holds.
unsafe fn blob_up_ref(b: *mut sys::blob_st) -> c_int {
    let mut out = ptr::null_mut();
    // SAFETY: `out` is a local slot; the caller's contract covers `b`.
    unsafe { sys::blob_dup(&raw mut out, b) }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct BlobFree;
impl_cdrop!(BlobFree, Blob<'_>, sys::blob_free);
impl_crefclone!(BlobFree, Blob<'_>, blob_up_ref, ok = |r| r == 0);
pub type BlobArc<'r> = CArc<Blob<'r>, BlobFree>;

impl<'r> Blob<'r> {
    pub fn lookup(repo: RepoRef<'r>, id: u32) -> Option<BlobArc<'r>> {
        let mut out = ptr::null_mut();
        // SAFETY: `out` is a writable slot; `repo` is live for `'r`.
        let rc = unsafe { sys::blob_lookup(&raw mut out, repo.as_ptr().cast_mut(), id) };
        // SAFETY: on success, one reference the arc takes over.
        if rc == 0 { unsafe { BlobArc::from_c(out) } } else { None }
    }
}

impl<'a> BlobRef<'a, '_> {
    /// The content: an immutable counted byte run the blob lends, as
    /// borrows.md's "a counted run of immutable bytes" describes.
    pub fn content(&self) -> &'a [u8] {
        // SAFETY: the handle keeps the blob live for `'a`; both calls only read it.
        let (data, len) = unsafe {
            (sys::blob_rawcontent(self.as_ptr()), sys::blob_rawsize(self.as_ptr()))
        };
        if data.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: `len` initialised bytes the blob owns, fixed when it was read
        // and never written, freed only with the blob, which outlives `'a`.
        unsafe { core::slice::from_raw_parts(data, len) }
    }
}

let repo = unsafe { CBox::<Repo, RepoFree>::from_c(sys::repo_open()) }.unwrap();
let blob = Blob::lookup(repo.as_ref(), 7).unwrap();
let again = blob.clone();                        // blob_up_ref → blob_dup
let n = again.as_ref().content().len();
// `drop(repo)` here would not compile: both blobs borrow it.
```

`impl_cdupclone!` and `impl_cguarded!` name the lifetime rather than elide
it, in a list ahead of their arguments: a deep copy over such a type is
`impl_cdupclone!(<'r> P, Blob<'r>, blob_copy)`, and a C lock
`impl_cguarded!(<'r> Blob<'r>, BlobLocked, lock = …, unlock = …)`, whose
handle is `BlobLocked<'a, 'r>`. A layout type generic over a type is still
written by hand, as [hand-written.md](hand-written.md) describes.
