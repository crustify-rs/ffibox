//! Declarative macros: [`define_ctype!`](crate::define_ctype) for the three
//! types per C type, and the policy macros, which implement a lifecycle trait
//! on a policy type you declare by binding a C routine to it. The routine is a
//! plain path, called with a pointer of the exact C type, so one for the wrong
//! type does not compile; any other shape goes behind an `unsafe fn` adapter
//! or into a hand-written policy.
//!
//! | Macro | Implements | Routine |
//! |-------|------------|---------|
//! | [`impl_cdrop!`](crate::impl_cdrop) | [`CDrop<T>`](crate::CDrop) | a `*_free` |
//! | [`impl_cdupclone!`](crate::impl_cdupclone) | [`CDupClone<T>`](crate::CDupClone) | a `*_dup` |
//! | [`impl_crefclone!`](crate::impl_crefclone) | [`CRefClone<T>`](crate::CRefClone) | an `*_up_ref`, optionally a sole-owner check |
//! | [`impl_cdrop_str!`](crate::impl_cdrop_str) / [`impl_cdupclone_str!`](crate::impl_cdupclone_str) | the same, for `c_char` | a string free / `strdup` |
//! | [`impl_clendrop!`](crate::impl_clendrop) | [`CLenDrop`](crate::CLenDrop) | a buffer free, given the byte length |
//! | [`impl_clenclone!`](crate::impl_clenclone) | [`CLenClone`](crate::CLenClone) | a buffer memdup |
//! | [`impl_cdispose!`](crate::impl_cdispose) | [`CDispose<T>`](crate::CDispose) | a `*_uninit` / `*_clear` |
//! | [`impl_cguarded!`](crate::impl_cguarded) | [`CGuarded`](crate::CGuarded) (and the `FooLocked` handle), on the layout type | the C lock / unlock |

/// Define a wrapped `*-sys` type: the layout newtype plus its two borrowed
/// handles, and the [`CCell`](crate::CCell) impl linking them.
///
/// ```ignore
/// define_ctype!(Widget, WidgetRef, WidgetMut, ffi::widget_st);
/// ```
///
/// The names are spelled out because `macro_rules!` cannot concatenate
/// identifiers; the order is layout, shared, exclusive. What each type is for
/// is in the [README](https://github.com/crustify-rs/ffibox#1-the-types-you-get).
///
/// Getters go on `$rf<'a>` taking `&self`, setters on `$mt<'a>` taking
/// `&mut self`, both through the `pub(crate)` raw seam (`from_ptr`, `as_ptr`,
/// `as_mut_ptr`, and the `void *` variants). Never write an accessor on
/// `$name`: a `&$name` covers the C object's bytes.
///
/// # Values Rust owns
///
/// A `$name` value — from [`CZeroable::zeroed`](crate::CZeroable::zeroed)
/// for a type that implements it, or a wrapper constructor around a C function
/// filling a local — is Rust-owned storage, so `$name::as_ref` /
/// `as_mut` reach its handles directly. That is the one place a reference
/// covers a C struct's bytes, sound because nothing else yields a `&$name`.
/// Do not give C a pointer to such a value that outlives the call; a struct C
/// registers or self-references belongs behind
/// [`CBox`](crate::CBox). One that must be disposed on drop is a
/// [`CVal`](crate::CVal).
///
/// # Thread safety is opt-in
///
/// `$name` carries a zero-sized `PhantomData<*const ()>`, so it and every
/// owner and handle over it is `!Send` / `!Sync` — a bindgen opaque type would
/// otherwise make all of them `Send + Sync`. (Owners that never name a
/// `$name` — `CStrBox`, a `CVec` of plain elements — follow their
/// policy instead, and cross threads by default.) Earn the traits back with a
/// safety proof:
///
/// ```ignore
/// // SAFETY: <why ownership of the C object is thread-mobile>
/// unsafe impl Send for Foo {}
/// unsafe impl Send for FooMut<'_> {}
///
/// // SAFETY: <why concurrent shared access is race-free>
/// unsafe impl Sync for Foo {}
/// unsafe impl Send for FooRef<'_> {}
/// unsafe impl Sync for FooRef<'_> {}
/// unsafe impl Sync for FooMut<'_> {}
/// ```
///
/// The owners and run views follow from `Foo` and their policy; the handles
/// need their own lines because `where Foo: Sync` on a concrete type is a trivial bound,
/// rejected on stable. Before granting `Sync`, audit every operation taking
/// `FooRef<'_>`: one that writes (a lazy cache, a non-atomic refcount) belongs
/// on `FooMut<'_>`.
///
/// # No `Deref` from `$mt` to `$rf`
///
/// `Deref::Target` cannot name a lifetime taken from `&self`, so it would have
/// to be `$rf<'a>`, and since `$rf` is `Copy`, `*m` would copy a shared handle
/// out while `m` stays usable exclusively. `as_ref` ties the shared handle to
/// the borrow instead, so the borrow checker rejects the pair:
///
/// ```compile_fail,E0502
/// # #[repr(C)] pub struct bar_st { pub payload: u64 }
/// ffibox::define_ctype!(Bar, BarRef, BarMut, bar_st);
///
/// fn demo(m: &mut BarMut<'_>) {
///     let shared = m.as_ref();   // immutable borrow of `*m` starts here
///     let _write = m.as_mut_ptr();
///     let _read = shared.as_ptr();   // ... and is still live here
/// }
/// ```
///
/// # Reborrowing `$mt`
///
/// `$mt` is move-only, so passing it by value hands it over. `as_mut` makes a
/// fresh exclusive handle bound to `&mut self`, as passing a `&mut T` does
/// implicitly:
///
/// ```
/// # #[repr(C)] pub struct pt_st { x: i32 }
/// ffibox::define_ctype!(Pt, PtRef, PtMut, pt_st);
/// impl PtMut<'_> {
///     pub fn set_x(&mut self, v: i32) {
///         // SAFETY: a write through the exclusive handle's pointer.
///         unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).x).write(v) }
///     }
/// }
/// fn reset(mut m: PtMut<'_>) { m.set_x(0); }
/// // SAFETY: any `x` is valid, zero included.
/// unsafe impl ffibox::CZeroable for Pt {}
///
/// let mut p = <Pt as ffibox::CZeroable>::zeroed();
/// let mut m = p.as_mut();
/// reset(m.as_mut());  // a reborrow ...
/// m.set_x(1);         // ... so `m` is still usable
/// ```
///
/// The original is frozen while the reborrow lives, so the two are never used
/// together:
///
/// ```compile_fail,E0499
/// # #[repr(C)] pub struct pt_st { x: i32 }
/// ffibox::define_ctype!(Pt, PtRef, PtMut, pt_st);
/// fn demo(mut m: PtMut<'_>) {
///     let inner = m.as_mut();
///     let _outer = m.as_mut_ptr(); // second exclusive use while `inner` lives
///     drop(inner);
/// }
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that
/// `$c_type` is the C type `$name` mirrors, and that all-zero is a valid
/// `$c_type` (`zeroed` relies on it).
#[macro_export]
macro_rules! define_ctype {
    ($(#[$attr:meta])* $name:ident, $rf:ident, $mt:ident, $c_type:ty) => {
        $(#[$attr])*
        #[repr(transparent)]
        pub struct $name($c_type, ::core::marker::PhantomData<*const ()>);

        #[doc = concat!("Shared borrow of a [`", stringify!($name), "`]. `Copy`, like `&T`; the getters live here.")]
        #[repr(transparent)]
        #[derive(Clone, Copy)]
        pub struct $rf<'a>($crate::CBorrowedPtr<'a, $name>);

        #[doc = concat!("Exclusive borrow of a [`", stringify!($name), "`]. Reaches the getters with [`as_ref`](Self::as_ref); the setters live here.")]
        #[repr(transparent)]
        pub struct $mt<'a>(
            $rf<'a>,
            // Invariant in the pointee, as `&'a mut` is; see `CCell`'s contract.
            ::core::marker::PhantomData<&'a mut $name>,
        );

        // SAFETY: `$name` is `#[repr(transparent)]` over `$c_type`; the
        // handles are transparent over `CBorrowedPtr<'a, $name>` and expose no
        // reference to `$name`; `$rf` has no write operation.
        unsafe impl $crate::CCell for $name {
            type C = $c_type;
            type Ref<'a> = $rf<'a>;
            type Mut<'a> = $mt<'a>;
        }

        impl $name {
            /// Shared handle to this value — the getters.
            ///
            /// Takes `&self`, a reference covering the C struct's bytes, which
            /// is sound because a `$name` value is Rust-owned inline storage:
            /// nothing hands out `&$name` over C-owned memory, and C reaches
            /// these bytes only through a pointer passed for one call. See
            /// [`define_ctype!`](crate::define_ctype) — "Values Rust owns".
            #[inline]
            #[must_use]
            pub fn as_ref(&self) -> $rf<'_> {
                // SAFETY: `self` is a live, initialised `$name` borrowed for
                // the handle's lifetime.
                $rf(unsafe { $crate::CBorrowedPtr::new(::core::ptr::NonNull::from(self)) })
            }

            /// Exclusive handle to this value — the setters, and the pointer
            /// for C calls that write it.
            #[inline]
            #[must_use]
            pub fn as_mut(&mut self) -> $mt<'_> {
                // SAFETY: as `as_ref`, from an exclusive borrow, so the
                // pointer carries write provenance and no other handle exists.
                $mt(
                    $rf(unsafe { $crate::CBorrowedPtr::new(::core::ptr::NonNull::from(self)) }),
                    ::core::marker::PhantomData,
                )
            }
        }

        impl<'a> $rf<'a> {
            /// Borrow a raw pointer; `None` if null.
            ///
            /// # Safety
            ///
            /// `ptr` must address a live, initialised `$c_type` (or be null)
            /// that outlives `'a`.
            #[inline]
            #[allow(dead_code)]
            pub(crate) unsafe fn from_ptr(ptr: *mut $c_type) -> ::core::option::Option<Self> {
                // SAFETY: layout-preserving cast per `#[repr(transparent)]`;
                // the caller upholds liveness and the lifetime.
                ::core::ptr::NonNull::new(ptr.cast::<$name>())
                    .map(|p| $rf(unsafe { $crate::CBorrowedPtr::new(p) }))
            }

            /// Read-only pointer to the C object, for FFI calls taking
            /// `*const $c_type` and for field reads through `addr_of!`.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            pub(crate) fn as_ptr(&self) -> *const $c_type {
                self.0.as_non_null().as_ptr().cast::<$c_type>()
            }

            /// Type-erased `*const c_void`, for read-only `void *` shims.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            pub(crate) fn as_void_ptr(&self) -> *const ::core::ffi::c_void {
                self.as_ptr().cast()
            }

            /// Borrow a type-erased `void *` back; `None` if null. The inbound
            /// dual of [`as_void_ptr`](Self::as_void_ptr), for C slots that
            /// hand an opaque pointer back (a `get_user_data` getter,
            /// a callback's `void *arg`). Ownership is not transferred.
            ///
            /// # Safety
            ///
            /// As [`from_ptr`](Self::from_ptr), plus: `ptr` must be the pointer
            /// erased *from this very type*. Nothing in a `void *` records the
            /// type, so one erased from another reconstitutes as confusion.
            #[inline]
            #[allow(dead_code)]
            pub(crate) unsafe fn from_void_ptr(
                ptr: *mut ::core::ffi::c_void,
            ) -> ::core::option::Option<Self> {
                // SAFETY: the caller asserts `ptr` addresses a live `$c_type`.
                unsafe { Self::from_ptr(ptr.cast::<$c_type>()) }
            }
        }

        impl<'a> $mt<'a> {
            /// Borrow a raw pointer exclusively; `None` if null.
            ///
            /// # Safety
            ///
            /// As [`from_ptr`](Self::from_ptr), plus: no other handle to the
            /// same object may be used while the result lives.
            #[inline]
            #[allow(dead_code)]
            pub(crate) unsafe fn from_ptr(ptr: *mut $c_type) -> ::core::option::Option<Self> {
                // SAFETY: caller upholds liveness, the lifetime and exclusivity.
                ::core::ptr::NonNull::new(ptr.cast::<$name>())
                    .map(|p| $mt($rf(unsafe { $crate::CBorrowedPtr::new(p) }), ::core::marker::PhantomData))
            }

            /// Writable pointer to the C object, for FFI calls taking
            /// `*mut $c_type` and for field writes through `addr_of_mut!`.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            pub(crate) fn as_mut_ptr(&mut self) -> *mut $c_type {
                self.0 .0.as_non_null().as_ptr().cast::<$c_type>()
            }

            /// Type-erased `*mut c_void`, for writing `void *` shims.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            pub(crate) fn as_mut_void_ptr(&mut self) -> *mut ::core::ffi::c_void {
                self.as_mut_ptr().cast()
            }

            /// Reborrow shared, for passing where a getter-only handle is
            /// wanted.
            #[inline]
            #[must_use]
            pub fn as_ref(&self) -> $rf<'_> {
                self.0
            }

            /// Reborrow exclusively, for passing to a function that takes the
            /// handle by value while keeping this one — what `&mut` does
            /// implicitly. This handle is frozen while the result lives.
            #[inline]
            #[must_use]
            pub fn as_mut(&mut self) -> $mt<'_> {
                $mt(self.0, ::core::marker::PhantomData)
            }
        }

    };
}

// ===========================================================================
// Policies: bind a C routine to a policy type
// ===========================================================================
//
// Every policy macro takes the routine as a plain path and calls it with a
// pointer of the exact C type, so a routine for the wrong type is a compile
// error rather than a silent cast. A destructor of any other shape — taking a
// pointer to the slot, a `void *`, extra arguments — goes behind a small
// `unsafe fn` adapter passed by path, or into a hand-written policy.

/// Implement [`CDrop<T>`](crate::CDrop) on a policy type, binding the C
/// destructor that settles one owned `*mut T` — a `*_free` or a down-ref.
///
/// ```ignore
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct FooFree;
/// impl_cdrop!(FooFree, Foo, ffi::foo_free);
/// ```
///
/// `Foo` is a [`CCell`](crate::CCell) pointee, and the routine is called with
/// a `*mut <Foo as CCell>::C` — `ffi::foo_st` here. Binding a routine for
/// another C type does not compile:
///
/// ```compile_fail,E0308
/// # #[repr(C)] pub struct foo_st { x: u8 }
/// # #[repr(C)] pub struct bar_st { y: u8 }
/// # unsafe fn bar_free(_: *mut bar_st) {}
/// ffibox::define_ctype!(Foo, FooRef, FooMut, foo_st);
/// pub struct FooFree;
/// ffibox::impl_cdrop!(FooFree, Foo, bar_free); // expects *mut foo_st
/// ```
///
/// For a `c_char` string, which is its own C type, use
/// [`impl_cdrop_str!`](crate::impl_cdrop_str).
///
/// A destructor of another shape is adapted by an `unsafe fn` passed by path:
///
/// ```ignore
/// /// # Safety
/// /// `p` must be an owned `dict_st`.
/// unsafe fn dict_free(mut p: *mut ffi::dict_st) {
///     // SAFETY: the caller transfers the dictionary; the local slot is writable.
///     unsafe { ffi::dict_free_slot(&mut p) } // takes `dict_st **` and nulls it
/// }
/// impl_cdrop!(DictFree, Dict, dict_free);
/// ```
///
/// The routine's return value, if any, is discarded. The policy is yours to
/// declare — usually a unit struct deriving `Clone, Copy, Debug, Default`, so
/// its owner gets `from_raw(ptr)`. A policy carrying
/// runtime state implements [`CDrop`](crate::CDrop) by hand.
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that
/// the routine releases exactly one unit of ownership of a live object.
#[macro_export]
macro_rules! impl_cdrop {
    (@impl $policy:ty, $t:ty, $c:ty, $f:path) => {
        // SAFETY: the invoker asserts `$f` releases one unit of ownership of a
        // live `$c`, which `$t` is layout-compatible with.
        unsafe impl $crate::CDrop<$t> for $policy {
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_drop(&self, ptr: ::core::ptr::NonNull<$t>) {
                let raw: *mut $c = ptr.as_ptr().cast();
                // SAFETY: the caller upholds `c_drop`'s contract.
                let _ = unsafe { $f(raw) };
            }
        }
    };
    ($policy:ty, $t:ty, $f:path) => {
        $crate::impl_cdrop!(@impl $policy, $t, <$t as $crate::CCell>::C, $f);
    };
}

/// [`impl_cdrop!`](crate::impl_cdrop) for an owned C string: implements
/// [`CDrop<c_char>`](crate::CDrop), calling the routine with a `*mut c_char`.
/// The policy for a [`CStrBox`](crate::CStrBox).
///
/// ```ignore
/// impl_cdrop_str!(LibStrFree, ffi::lib_str_free);
/// ```
///
/// A generic `void *` free (`free`, a library's `lib_free`) goes behind an `unsafe fn`
/// adapter taking `*mut c_char`.
///
/// # Safety
///
/// As [`impl_cdrop!`](crate::impl_cdrop).
#[macro_export]
macro_rules! impl_cdrop_str {
    ($policy:ty, $f:path) => {
        $crate::impl_cdrop!(@impl $policy, ::core::ffi::c_char, ::core::ffi::c_char, $f);
    };
}

/// Implement [`CDupClone<T>`](crate::CDupClone) on a policy that already
/// implements [`CDrop<T>`](crate::CDrop), binding a deep-copy routine: it
/// returns a NEW pointer, or NULL on failure, that the same policy frees.
///
/// ```ignore
/// impl_cdrop!(KeyFree, Key, ffi::key_free);
/// impl_cdupclone!(KeyFree, Key, ffi::key_dup);
/// ```
///
/// As in [`impl_cdrop!`](crate::impl_cdrop), the routine is called with a
/// `*mut <T as CCell>::C` and must return the same pointer type.
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// routine leaves the original live and unmodified and deep-copies it into a
/// fresh, independent allocation the policy's `c_drop` releases, or returns
/// NULL on failure.
#[macro_export]
macro_rules! impl_cdupclone {
    (@impl $policy:ty, $t:ty, $c:ty, $f:path) => {
        // SAFETY: the invoker asserts `$f` returns a fresh, independent copy
        // of a live `$c`, or NULL, that the policy's `c_drop` releases.
        unsafe impl $crate::CDupClone<$t> for $policy {
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_dup(
                &self,
                ptr: ::core::ptr::NonNull<$t>,
            ) -> ::core::option::Option<::core::ptr::NonNull<$t>> {
                let raw: *mut $c = ptr.as_ptr().cast();
                // SAFETY: the caller upholds `c_dup`'s contract.
                let copy: *mut $c = unsafe { $f(raw) };
                ::core::ptr::NonNull::new(copy.cast::<$t>())
            }
        }
    };
    ($policy:ty, $t:ty, $f:path) => {
        $crate::impl_cdupclone!(@impl $policy, $t, <$t as $crate::CCell>::C, $f);
    };
}

/// [`impl_cdupclone!`](crate::impl_cdupclone) for an owned C string: a
/// `strdup`, `*mut c_char -> *mut c_char`.
///
/// # Safety
///
/// As [`impl_cdupclone!`](crate::impl_cdupclone).
#[macro_export]
macro_rules! impl_cdupclone_str {
    ($policy:ty, $f:path) => {
        $crate::impl_cdupclone!(@impl $policy, ::core::ffi::c_char, ::core::ffi::c_char, $f);
    };
}

/// Implement [`CRefClone<T>`](crate::CRefClone) on a policy whose
/// [`CDrop<T>`](crate::CDrop) is the matching down-ref, binding the `up_ref`,
/// and optionally a sole-owner check.
///
/// ```ignore
/// impl_cdrop!(FooUnref, Foo, ffi::foo_free);
/// impl_crefclone!(FooUnref, Foo, ffi::foo_up_ref); // returns `()`
///
/// // An up_ref that reports failure: `ok` maps its return to success.
/// impl_cdrop!(SessionUnref, Session, ffi::session_free);
/// impl_crefclone!(SessionUnref, Session, ffi::session_up_ref, ok = |r| r == 1);
///
/// // With a count the library lets you read: an `unsafe fn(*mut C) -> bool`.
/// impl_crefclone!(FooUnref, Foo, ffi::foo_up_ref, sole = foo_refcount_is_one);
/// ```
///
/// Without `ok`, the up_ref must return `()`: a status the macro would
/// otherwise discard does not compile. Without `sole`,
/// [`c_is_sole_owner`](crate::CRefClone::c_is_sole_owner) keeps its
/// always-`false` default.
///
/// ```compile_fail,E0277
/// # #[repr(C)] pub struct foo_st { x: u8 }
/// # unsafe fn foo_free(_: *mut foo_st) {}
/// # unsafe fn foo_up_ref(_: *mut foo_st) -> i32 { 1 }
/// ffibox::define_ctype!(Foo, FooRef, FooMut, foo_st);
/// pub struct FooUnref;
/// ffibox::impl_cdrop!(FooUnref, Foo, foo_free);
/// ffibox::impl_crefclone!(FooUnref, Foo, foo_up_ref); // needs `ok = …`
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// up_ref increments the reference count of a live object, and — with `ok` —
/// that it changes nothing when `ok` rejects its return value. A `sole`
/// routine must meet `c_is_sole_owner`'s contract: an Acquire read of a count
/// that covers every reference.
#[macro_export]
macro_rules! impl_crefclone {
    (@impl $policy:ty, $t:ty, $c:ty, $f:path, [$($ok:expr)?], [$($sole:path)?]) => {
        // SAFETY: the invoker asserts `$f` increments the count of a live `$c`
        // whenever the return check passes, so the object owes one more
        // `c_drop`, and that any sole-owner routine meets `c_is_sole_owner`'s
        // contract.
        unsafe impl $crate::CRefClone<$t> for $policy {
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_up_ref(&self, ptr: ::core::ptr::NonNull<$t>) -> bool {
                let raw: *mut $c = ptr.as_ptr().cast();
                // SAFETY: the caller upholds `c_up_ref`'s contract.
                let ret = unsafe { $f(raw) };
                $crate::__ffibox_check_ret!(ret [$($ok)?])
            }
            $(
                #[inline]
                #[allow(unused_unsafe)]
                unsafe fn c_is_sole_owner(&self, ptr: ::core::ptr::NonNull<$t>) -> bool {
                    let raw: *mut $c = ptr.as_ptr().cast();
                    // SAFETY: the caller upholds `c_is_sole_owner`'s contract.
                    unsafe { $sole(raw) }
                }
            )?
        }
    };
    ($policy:ty, $t:ty, $f:path $(, ok = $ok:expr)? $(, sole = $sole:path)? $(,)?) => {
        $crate::impl_crefclone!(
            @impl $policy, $t, <$t as $crate::CCell>::C, $f, [$($ok)?], [$($sole)?]
        );
    };
}

/// The routines a fallible macro binds without `ok`: those returning `()`.
/// Anything else is a status the macro would discard, and the diagnostic says
/// how to keep it.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "the bound routine returns `{Self}`, a status the macro would discard",
    label = "returns `{Self}`, not `()`",
    note = "bind it with an `ok` check that maps the status to success, e.g. `ok = |r| r == 1`"
)]
pub trait __UnitStatus {}
impl __UnitStatus for () {}

/// Success for a routine that cannot report failure.
#[doc(hidden)]
#[inline(always)]
pub fn __unit_status<R: __UnitStatus>(_: R) -> bool {
    true
}

/// Turn a bound routine's return value into the `bool` a fallible trait method
/// reports. Without a check the routine must return `()`, so a status code is
/// never silently discarded; with one, the check decides.
#[doc(hidden)]
#[macro_export]
macro_rules! __ffibox_check_ret {
    ($ret:ident []) => {
        $crate::macros::__unit_status($ret)
    };
    ($ret:ident [$ok:expr]) => {
        ($ok)($ret)
    };
}

/// The shared handle of the object at `p`, for the `Locked` handles
/// [`impl_cguarded!`](crate::impl_cguarded) generates in another crate.
///
/// # Safety
///
/// As a `Ref` handle: `p` is live for `'a` and its reads do not race.
#[doc(hidden)]
#[inline(always)]
pub unsafe fn __handle_ref<'a, T: crate::CCell + 'a>(p: ::core::ptr::NonNull<T>) -> T::Ref<'a> {
    // SAFETY: the caller upholds `handle_ref`'s contract.
    unsafe { crate::refs::handle_ref(p) }
}

/// Implement [`CGuarded`](crate::CGuarded) on a layout type, binding the C
/// lock routines, each called with a `*mut <T as CCell>::C`.
///
/// The common form names the [`Locked`](crate::CGuarded::Locked) handle to
/// generate, `FooLocked<'a>`: the handle a held [`CGuard`](crate::CGuard)
/// grants, where the getters and setters for the lock-protected state go.
///
/// ```ignore
/// define_ctype!(Cache, CacheRef, CacheMut, ffi::cache_st);
/// impl_cguarded!(Cache, CacheLocked, lock = ffi::cache_lock, unlock = ffi::cache_unlock);
///
/// impl CacheRef<'_> { /* fixed state; routines that lock internally */ }
/// impl CacheLocked<'_> { /* the state `cache_lock` protects */ }
///
/// // Lock routines that report failure: `ok` maps a lock's return to success.
/// impl_cguarded!(
///     Store,
///     StoreLocked,
///     lock = ffi::store_lock,
///     unlock = ffi::store_unlock,
///     ok = |r| r == 1,
/// );
/// ```
///
/// `FooLocked` is move-only and reaches the getters with `as_ref()`;
/// `as_locked()` reborrows it, and a crate-private `as_mut_ptr()` gives the
/// pointer for C calls that require the lock held.
///
/// With `all` in place of the handle name, the lock covers the whole object:
/// the macro also implements [`CGuardedAll`](crate::CGuardedAll), and the
/// locked handle is the `Mut` handle. Such an object is reached through a
/// [`CGuardedRef`](crate::CGuardedRef).
///
/// ```ignore
/// impl_cguarded!(Registry, all, lock = registry_lock, unlock = registry_unlock, ok = |r| r == 0);
/// ```
///
/// Without `ok`, `lock` must return `()`. A status the macro discarded would
/// be a failed lock reported as held — and a lock backed by
/// `pthread_mutex_*` *does* fail where it matters, with `EDEADLK` when the
/// thread already holds it, so a discarded status would hand out a second
/// locked handle instead of deadlocking. With `ok`, a rejected return makes
/// [`CArc::lock`](crate::CArc::lock) / [`CGuardedRef::lock`](crate::CGuardedRef::lock)
/// panic. The unlock routine's return value, if any, is discarded: a failed
/// unlock leaves the object locked, which can deadlock but never aliases.
///
/// ```compile_fail,E0277
/// # #[repr(C)] pub struct obj_st { x: u8 }
/// # unsafe fn obj_lock(_: *mut obj_st) -> i32 { 0 }
/// # unsafe fn obj_unlock(_: *mut obj_st) -> i32 { 0 }
/// ffibox::define_ctype!(Obj, ObjRef, ObjMut, obj_st);
/// ffibox::impl_cguarded!(Obj, ObjLocked, lock = obj_lock, unlock = obj_unlock); // needs `ok = …`
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert
/// [`CGuarded`](crate::CGuarded)'s contract: every C routine touching the
/// protected state takes this lock or requires it held, and a lock call that
/// passes the return check (or returns `()`) holds the lock — so a
/// same-thread relock must block or be rejected, never succeed. With `all`,
/// you also assert [`CGuardedAll`](crate::CGuardedAll)'s: nothing reaches the
/// object's state without the lock.
#[macro_export]
macro_rules! impl_cguarded {
    (
        $t:ty, all, lock = $lock:path, unlock = $unlock:path
        $(, ok = $ok:expr)? $(,)?
    ) => {
        $crate::impl_cguarded!(
            @impl $t, [<$t as $crate::CCell>::Mut<'a>], LockWhole, $lock, $unlock, [$($ok)?]
        );
        // SAFETY: the invoker asserts `CGuardedAll`'s contract; `Locked` is
        // `Mut` and `Scope` is `LockWhole` above.
        unsafe impl $crate::CGuardedAll for $t {}
    };
    (
        $t:ty, $lk:ident, lock = $lock:path, unlock = $unlock:path
        $(, ok = $ok:expr)? $(,)?
    ) => {
        #[doc = concat!("Exclusive borrow of a [`", stringify!($t), "`] under its lock, from a held [`CGuard`](", stringify!($crate), "::CGuard). The state the lock protects is reached here.")]
        #[repr(transparent)]
        pub struct $lk<'a>(
            $crate::CBorrowedPtr<'a, $t>,
            // Invariant in the pointee, as `&'a mut` is; see `CGuarded`'s contract.
            ::core::marker::PhantomData<&'a mut $t>,
        );

        impl<'a> $lk<'a> {
            /// Writable pointer to the C object, for FFI calls that require
            /// the lock held and for field access through `addr_of_mut!`.
            #[inline]
            #[must_use]
            #[allow(dead_code)]
            pub(crate) fn as_mut_ptr(&mut self) -> *mut <$t as $crate::CCell>::C {
                self.0.as_non_null().as_ptr().cast()
            }

            /// Reborrow shared, for the getters that need no lock.
            #[inline]
            #[must_use]
            pub fn as_ref(&self) -> <$t as $crate::CCell>::Ref<'_> {
                // SAFETY: the handle keeps the object live for the borrow,
                // and the held lock keeps writers out while it reads.
                unsafe { $crate::macros::__handle_ref(self.0.as_non_null()) }
            }

            /// Reborrow exclusively, for passing to a function that takes the
            /// handle by value while keeping this one. This handle is frozen
            /// while the result lives.
            #[inline]
            #[must_use]
            pub fn as_locked(&mut self) -> $lk<'_> {
                $lk(self.0, ::core::marker::PhantomData)
            }
        }

        $crate::impl_cguarded!(@impl $t, [$lk<'a>], LockFields, $lock, $unlock, [$($ok)?]);
    };
    // `$check` is one bracketed token tree, matched by the helper whatever
    // `ok` was given or omitted.
    (@impl $t:ty, [$($locked:tt)*], $scope:ident, $lock:path, $unlock:path, $check:tt) => {
        // SAFETY: the invoker asserts `CGuarded`'s contract for these
        // routines; the `Locked` handle is transparent over `CBorrowedPtr`
        // and invariant, as generated above or as the `Mut` handle is.
        unsafe impl $crate::CGuarded for $t {
            type Locked<'a> = $($locked)* where Self: 'a;
            type Scope = $crate::$scope;

            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_lock(ptr: ::core::ptr::NonNull<Self>) -> bool {
                let raw: *mut <$t as $crate::CCell>::C = ptr.as_ptr().cast();
                // SAFETY: the caller upholds `c_lock`'s contract.
                let ret = unsafe { $lock(raw) };
                $crate::__ffibox_check_ret!(ret $check)
            }
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_unlock(ptr: ::core::ptr::NonNull<Self>) {
                let raw: *mut <$t as $crate::CCell>::C = ptr.as_ptr().cast();
                // SAFETY: the caller holds the lock.
                let _ = unsafe { $unlock(raw) };
            }
        }
    };
}

/// Implement [`CLenDrop`](crate::CLenDrop) on a buffer policy. The routine is a
/// plain path called with the buffer and its byte length,
/// `unsafe fn(*mut u8, usize)`; its return value, if any, is discarded.
///
/// ```ignore
/// /// # Safety
/// /// `ptr` must be a `lib_malloc` buffer.
/// unsafe fn lib_vec_free(ptr: *mut u8, _byte_len: usize) {
///     // SAFETY: the caller transfers the buffer.
///     unsafe { ffi::lib_free(ptr.cast()) }
/// }
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct LibVecFree;
/// impl_clendrop!(LibVecFree, lib_vec_free);
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// routine frees a `byte_len`-byte buffer from this allocator family.
#[macro_export]
macro_rules! impl_clendrop {
    ($policy:ty, $f:path) => {
        // SAFETY: the invoker asserts `$f` frees a `byte_len`-byte buffer from
        // this allocator family.
        unsafe impl $crate::CLenDrop for $policy {
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_drop_len(&self, ptr: *mut u8, byte_len: usize) {
                // SAFETY: the caller upholds `c_drop_len`'s contract.
                let _ = unsafe { $f(ptr, byte_len) };
            }
        }
    };
}

/// Implement [`CLenClone`](crate::CLenClone) on a policy that already
/// implements [`CLenDrop`](crate::CLenDrop). The routine is a plain path,
/// `unsafe fn(*mut u8, usize) -> *mut u8`, returning a fresh copy or NULL:
///
/// ```ignore
/// impl_clenclone!(LibVecFree, lib_vec_memdup);
/// ```
///
/// An allocator aligning beyond a byte says so, which lets the copying
/// constructors take wider element types:
///
/// ```ignore
/// impl_clenclone!(LibVecFree, lib_vec_memdup, align = 16);
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// routine only reads its source, returns a fresh `byte_len`-byte copy, or
/// NULL, that the policy's `c_drop_len` releases, aligned to `align` (1 when
/// omitted).
#[macro_export]
macro_rules! impl_clenclone {
    ($policy:ty, $f:path, align = $align:expr) => {
        $crate::impl_clenclone!(@impl $policy, $f, $align);
    };
    ($policy:ty, $f:path) => {
        $crate::impl_clenclone!(@impl $policy, $f, 1);
    };
    (@impl $policy:ty, $f:path, $align:expr) => {
        // SAFETY: the invoker asserts `$f` returns a fresh `byte_len`-byte
        // copy, or NULL, that the policy releases.
        unsafe impl $crate::CLenClone for $policy {
            const ALIGN: usize = $align;

            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_clone_len(
                &self,
                ptr: *mut u8,
                byte_len: usize,
            ) -> ::core::option::Option<::core::ptr::NonNull<u8>> {
                // SAFETY: the caller upholds `c_clone_len`'s contract.
                let copy: *mut u8 = unsafe { $f(ptr, byte_len) };
                ::core::ptr::NonNull::new(copy)
            }
        }
    };
}

/// Implement [`CDispose<T>`](crate::CDispose) on a policy, binding the C
/// routine that disposes a value's resources in place — `*_uninit`,
/// `*_clear` — for a [`CVal`](crate::CVal). The routine is called with a
/// `*mut <T as CCell>::C`; its return value, if any, is discarded.
///
/// ```ignore
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct ParamsUninit;
/// impl_cdispose!(ParamsUninit, Params, ffi::params_uninit);
/// ```
///
/// # Safety
///
/// The macro is safe to invoke but emits an `unsafe impl`. You assert that the
/// routine releases the value's owned resources exactly once, does not free
/// the value itself, and accepts every value safe code can reach: what a safe
/// constructor produces (`CZeroable::zeroed()` included), and whatever the wrapper's safe
/// setters then write — so a setter that could break the routine must
/// validate or be `unsafe`.
#[macro_export]
macro_rules! impl_cdispose {
    ($policy:ty, $t:ty, $f:path) => {
        // SAFETY: the invoker asserts `$f` disposes a live value's resources
        // once without freeing its storage.
        unsafe impl $crate::CDispose<$t> for $policy {
            #[inline]
            #[allow(unused_unsafe)]
            unsafe fn c_dispose(&self, ptr: ::core::ptr::NonNull<$t>) {
                let raw: *mut <$t as $crate::CCell>::C = ptr.as_ptr().cast();
                // SAFETY: the caller upholds `c_dispose`'s contract.
                let _ = unsafe { $f(raw) };
            }
        }
    };
}
