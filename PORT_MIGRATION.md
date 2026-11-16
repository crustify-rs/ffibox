# Migrating libgit2-rs to ffibox's new primitives

This plan applies the primitives added on `wp/grow` to the port at
`/work/libgit2/rust/libgit2-rs` (master `44a5f05`). It is a plan only: the port
is changing concurrently, so the line numbers are those of `44a5f05`, and each
entry names the function or item so that it can be found again after the code
moves. The paths are relative to `rust/libgit2-rs/src/`.

"Removes" counts each `unsafe { … }` block and `unsafe impl` that the
replacement deletes from the port. Their obligations move into one
`unsafe impl` per type, or into ffibox. An `unsafe fn` whose contract a caller
still needs stays, and is not counted.

| Primitive | Sites | Unsafe removed (approx.) | New unsafe in the port |
|---|---|---|---|
| `CShared<T>` + `CCounted` / `CShareable` | odb, config, refdb, index; their owners, frees and exports; `repository/handles.rs` | ~45 | 4 `unsafe impl CCounted`, 2 `unsafe impl CShareable`, and the `c_*` bodies (one projection each) |
| `CAtomicOwner<O>` | `GitRepository`'s 4 object fields, 2 grafts fields, 5 string fields; `set_owned`, `publish`, the weak getters and `replace_string` / `init_string` | ~35 | the setters stay `unsafe fn` (C's weak-pointer contract), each one `unsafe { slot.replace(..) }` |
| `CBits` / `CBitfield` / `CAtomicBitfield` | commit-list node (2 copies of the masks), index, repository, process options, commit-create options, index writer | 0: masks are safe code. It removes ~60 lines of hand-written masks and the duplicate mask table in `revwalk/node.rs`, and fixes a lost update in `GitIndex`'s bit setters | 0 |

Migrate in this order: `CShared` first, then `CAtomicOwner<CShared<_>>`,
which builds on it. The bitfields are independent.

Prerequisite: the port's `Cargo.toml` points ffibox at `/work/ffibox`. Merge
`wp/grow` into ffibox's master first, or point it at the worktree while
migrating.

---

## 1. `CShared<T>`: the refcounted Rust objects

Four types start with a `git_refcount` that C's macros (`GIT_REFCOUNT_INC`,
`GIT_REFCOUNT_OWN`) and C's frees (`git_odb_free`, …) touch in place. Each
has a hand-written `Arc`: `Clone` → `rc.inc()`, `Drop` → `rc.dec()` → free,
`from_raw` / `new_ref` / `into_raw` / `inner`, and an export that repeats the
`Drop`.

### 1.1 The count, once: `util/thread.rs`

`GitRefcount` (`util/thread.rs:423-473`) keeps `inc` / `dec` / `own` /
`owner`. Add one generic helper, so that each type's `CCounted` impl is a few
lines:

```rust
/// The `git_refcount` at offset 0 of a live `T` (`#[repr(C)]`, `rc` first).
/// # Safety
/// `p` is live while the result is used; `T` starts with a `GitRefcount`.
pub(crate) unsafe fn refcount_of<'a, T>(p: NonNull<T>) -> &'a GitRefcount {
    // SAFETY: the caller's contract; `GitRefcount` is all atomics.
    unsafe { p.cast::<GitRefcount>().as_ref() }
}
```

This is `repository/handles.rs:32-36` (`refcount`), made generic and moved.
Delete the old one when §2 lands. Each type also gets
`const _: () = assert!(offset_of!(T, rc) == 0);`.

### 1.2 `git_odb`: `odb.rs`, `odb/api.rs`, `odb/ffi.rs`

```rust
// SAFETY: `rc` is `git_refcount`, which C changes only with GIT_REFCOUNT_*
// (atomic); `dec` keeps an owned odb (its repository releases it); odbs are
// built only by `GitOdb::new`, count 1, no owner; they live in GIT_ALLOC.
unsafe impl CCounted for GitOdb {
    type Alloc = GitAlloc;
    unsafe fn c_up_ref(p: NonNull<Self>) { unsafe { refcount_of(p) }.inc() }
    unsafe fn c_down_ref(p: NonNull<Self>) -> bool { unsafe { refcount_of(p) }.dec() }
}
// SAFETY: C writes only `rc`; holders reach `locked` through its mutex, and
// `own_cache` / `do_fsync` are synchronised; `options` is fixed.
unsafe impl CShareable for GitOdb {}
```

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `odb.rs:877-903` `GitOdb::new` | `GIT_ALLOC.try_box(db)`, `Box::into_raw_with_allocator`, `NonNull::new_unchecked` → `Result<NonNull<GitOdb>>` | `CShared::try_new(db).map_err(\|_\| errors::oom())` → `Result<CShared<GitOdb>>` | 1 |
| `odb.rs:905-930` `GitOdb::free` | `unsafe fn`, `Box::from_raw_in`, frees the backends | `impl Drop for GitOdb` with the same body over `&mut self` (the default `c_release` drops the box). The backends' `free` slot calls stay | 1 (`from_raw_in`) |
| `odb.rs:2086-2097` `odb_decref` | `rc.dec()` → `GitOdb::free` | deleted; callers drop a `CShared` or call `CShared::decrement_count` | 2 |
| `odb/api.rs:54-60` `struct Odb<'r>` | `raw: NonNull<GitOdb>, _parent` | `#[derive(Clone)] pub struct Odb<'r>(CShared<GitOdb>, PhantomData<&'r ()>)` | — |
| `odb/api.rs:62-69` | `unsafe impl Send` / `Sync for Odb` | deleted: `CShared<GitOdb>` is `Send + Sync` on `Arc`'s terms (`GitOdb` is auto `Send + Sync`) | 2 |
| `odb/api.rs:80-95` | hand-written `Clone` (`rc.inc()`) and `Drop` (`odb_decref`) | derived `Clone`; no `Drop` | 1 |
| `odb/api.rs:97-101` `inner` | `unsafe { self.raw.as_ref() }` | `&self.0` (deref) | 1 |
| `odb/api.rs:103-127` `from_raw`, `new_ref` | hand-written; `new_ref` does `raw.as_ref().rc.inc()` | `CShared::from_raw(p)`, `CShared::from_borrowed(p)` (still `unsafe fn` for their callers) | 1 |
| `odb/api.rs:129-140` `as_raw`, `into_raw` | `mem::forget` | `self.0.as_ptr()`, `self.0.into_raw()` | — |
| `odb/ffi.rs:435-443` `git_odb_free` | `barrier` + `odb_decref` | `barrier(\|\| unsafe { CShared::<GitOdb>::release(db) })` | — (1 stays, shorter) |
| `repository/ffi.rs:558-580` `git_repository_odb` | `p.as_ref().rc.inc()` after the weak pointer | `CShared::<GitOdb>::increment_count(p)` | 1 |

### 1.3 `git_config`: `config.rs`

Same impls as for the odb, with `type Alloc = std::alloc::Global` (configs
are `Box<GitConfig>`). `CShareable`: `backends` is an `RwLock`, and C writes
only `rc`.

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `config.rs:832-838` `GitConfig::new` → `Box<GitConfig>`; also `open_ondisk`, `open_default`, `open_level`, `open_global`, `snapshot` returning `Box<GitConfig>` | boxes adopted by `Config::from_box` | return `CShared<GitConfig>` (`CShared::from_box(Box::new(..))`) | — |
| `config.rs:2085-2091` `out_config` | `out.write(Box::into_raw(c))` | `out.write(c.into_raw())` | — |
| `config.rs:3512-3517` `git_config_open_default` | `Box::into_raw(c)` | `c.into_raw()` | — |
| `config.rs:2136-2155` `git_config_free` | `cfg(config).rc.dec()` then `Box::from_raw` | `CShared::<GitConfig>::release(config)` in the barrier | 2 |
| `config.rs:3793-3798` `struct Config<'r>` | `raw` + `_parent` | `#[derive(Clone)] struct Config<'r>(CShared<GitConfig>, PhantomData<&'r ()>)` | — |
| `config.rs:3800-3803` | `unsafe impl Send` / `Sync for Config` | deleted | 2 |
| `config.rs:3805-3820` | `Clone` (`rc.inc()`), `Drop` (`git_config_free`) | derived / none | 1 |
| `config.rs:3828-3876` `from_box`, `inner`, `from_raw`, `new_ref`, `into_raw` | hand-written | `CShared` methods; `inner` is the deref | 2 |
| `repository/ffi.rs:424-445` `git_repository_config` | `Config::new_ref(NonNull::from(c))` | `CShared::from_borrowed(ptr::from_ref(c).cast_mut())` | — (same count) |

### 1.4 `git_refdb`: `refdb.rs`, `refs.rs`, `refdb_fs.rs`

`GitRefdb` (`refdb.rs:66-75`) is calloc'd from `git__allocator`
(`new_db`, `refdb.rs:724-733`) and freed by `free_db` (`refdb.rs:247-255`),
which frees the backend through its slot first.

```rust
unsafe impl CCounted for GitRefdb {
    type Alloc = GitAlloc;
    unsafe fn c_up_ref(p: NonNull<Self>) { unsafe { refcount_of(p) }.inc() }
    unsafe fn c_down_ref(p: NonNull<Self>) -> bool { unsafe { refcount_of(p) }.dec() }
    // `free_db`'s body: the backend through its `free` slot, the memzero, then
    // `alloc::free`. Or move the backend free into `impl Drop for GitRefdb` and
    // build with `CShared::try_new`, keeping the default release.
    unsafe fn c_release(p: NonNull<Self>) { unsafe { free_db(p.as_ptr()) } }
}
unsafe impl CShareable for GitRefdb {} // rc and backend are atomics; repo is fixed
```

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `refdb.rs:276-287` `decref` | `(*db).rc.dec()` → `free_db` | `CShared::<GitRefdb>::decrement_count(db)` at the callers, or deleted | 2 |
| `refdb.rs:373-376` `RefdbPtr::incref` | `rc.inc()` | `CShared::from_borrowed(self.as_ptr())` where a counted reference is wanted | — |
| `refdb.rs:724-733` `new_db` | `calloc` + `(*db).repo = repo` + `rc.inc()` | `CShared::try_new(GitRefdb { rc: GitRefcount::new(1), repo, backend: AtomicPtr::default() })` | 2 |
| `refdb.rs:750-757` `open`, error path | `decref(db.as_ptr())` | drop the `CShared` | 1 |
| `refdb.rs:880-891` `iterator_free` | `decref((*iter).db)` | `CShared::decrement_count(db)` (the iterator's `db` is a C field of `git_reference_iterator`; see the note below) | 1 |
| `refdb.rs:1014-1027` `git_refdb_free` | `decref` | `CShared::<GitRefdb>::release(db)` | — |
| `refs.rs:403-409` `RefBox::attach_db` | `(*db).rc.inc()` then the field write | `CShared::increment_count(db)` then the write | 1 |
| `refs.rs:494-497` reference free | `refdb::decref(db)` | `CShared::decrement_count(db)` | — |
| `refdb_fs.rs:935-946` `struct Counted` + its `Drop` | a hand-written owner of one count | `let counted = unsafe { CShared::from_raw(db) }` | 1 (the `Drop`'s) |
| `repository/handles.rs:151-155` `owner!(Refdb: GitRefdb, git_refdb_free)` | macro owner | `struct Refdb<'r>(CShared<GitRefdb>, PhantomData<&'r ()>)` | 1 (macro `Drop`) |
| `repository/ffi.rs:642-660` `git_repository_refdb` | `handles::refcount(p.cast()).inc()` | `CShared::<GitRefdb>::increment_count(p)` | 1 |

Note: `git_reference.db` and `git_reference_iterator.db` are pointer fields
of C-visible structs that hold one count. Once the blocks that contain them
are dropped as Rust values, they can be `Option<CShared<GitRefdb>>`, which
has the same layout. Today both blocks are freed by `alloc::free`, so the
count is dropped by hand, as above.

### 1.5 `git_index`: `index/mod.rs`, `index/api.rs` (not `CShareable`)

C writes `entries`, the bits, `tree` and others directly, and C callbacks run
while the index is in use. So no `&GitIndex` may live across foreign code
(module docs, `index/mod.rs:27-40`), and `GitIndex` gets `CCounted` without
`CShareable`. `CShared<GitIndex>` then has no `Deref`. The index is reached
through `IndexPtr`, as today, and `get_mut()` gives `&mut GitIndex` when the
count proves the reference sole.

```rust
unsafe impl CCounted for GitIndex {
    type Alloc = std::alloc::Global;
    unsafe fn c_up_ref(p: NonNull<Self>) { unsafe { refcount_of(p) }.inc() }
    unsafe fn c_down_ref(p: NonNull<Self>) -> bool { unsafe { refcount_of(p) }.dec() }
    unsafe fn c_is_sole(p: NonNull<Self>) -> bool { /* val() == 1 && owner().is_null() */ }
    unsafe fn c_release(p: NonNull<Self>) { unsafe { index_free(IndexPtr::new(p)) } }
}
```

`index_free` (`index/mod.rs:2000-2012`) keeps its `readers != 0` early
return, which leaks as the C does.

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `index/mod.rs:1954-1960` `IndexPtr::rc` | `&*ptr.cast::<GitRefcount>()` | `refcount_of(self.0)` (shared helper) | 1 |
| `index/mod.rs:1973-1997` `incref`, `decref` | `rc().inc()`, `rc().dec()` → `index_free` | `CShared::increment_count` / `decrement_count` | 1 |
| `index/api.rs:366-395` `struct Index<'r>`, `Clone`, `Drop` | `ptr: IndexPtr` + `incref` / `decref` | `#[derive(Clone)] struct Index<'r>(CShared<GitIndex>, PhantomData<(*mut (), &'r ())>)`, reached as `IndexPtr::new(self.0.as_ptr())` | 1 |
| `index/api.rs:407-419` `Index::new_ref` | `IndexPtr::new` + `incref` | `CShared::from_borrowed` | — |
| `index/ops.rs:545-572` `open_ext` | `Box::leak` at count 0, `ix.decref()` on a read failure (which frees at 0), `ix.incref()` on success | `GitIndex::new` (`index/mod.rs:950-978`) sets the count to 1; `let ix = CShared::from_box(index)`; on failure `drop(ix)`; return `ix`. Also `index/ops.rs:806, 1083, 1524` `incref()` → `CShared::from_borrowed` / `increment_count` | 1 |
| `repository/ffi.rs:728-750` `git_repository_index` | `IndexPtr::new(p).incref()` | `CShared::<GitIndex>::increment_count(p)` | 1 |
| `index/ffi.rs:397-405` `git_index_free` | `handle(index)` + `decref` | `CShared::<GitIndex>::release(index)` in the barrier | 1 |

### 1.6 Not migrated

`sys/pathspec.rs:16-25` `GitPathspec`: C allocates and frees it (pathspec.c
stays C), and the index embeds one on its stack. That is `CArc` territory,
not `CShared`, and needs no change.

---

## 2. `CAtomicOwner<O>`: `GitRepository`'s lazily set fields

`GitRepository` (`repository/mod.rs:155-183`) is calloc'd. All-zero is an
empty `CAtomicOwner`, which ffibox documents and tests, so `alloc()` needs no
change.

### 2.1 The four object fields

| Field (`repository/mod.rs`) | Today | Becomes |
|---|---|---|
| `:158` `odb` | `AtomicPtr<GitOdb>` | `CAtomicOwner<CShared<GitOdb>>` |
| `:159` `refdb` | `AtomicPtr<GitRefdb>` | `CAtomicOwner<CShared<GitRefdb>>` |
| `:160` `config` | `AtomicPtr<GitConfig>` | `CAtomicOwner<CShared<GitConfig>>` |
| `:161` `index` | `AtomicPtr<GitIndex>` | `CAtomicOwner<CShared<GitIndex>>` |

The layout test is unchanged: each field is `#[repr(transparent)]` over
`AtomicPtr`.

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `repository/mod.rs:588-614` `set_owned` | `handles::refcount` + `own` + `inc` on the new object, `thread::swap`, `own(NULL)` + `free` on the old | `unsafe fn set_owned<T: CCounted>(&self, slot: &CAtomicOwner<CShared<T>>, new: Option<CShared<T>>)`: `own(self)` on the new one, then `let old = unsafe { slot.replace(new) }`, then `own(NULL)` on the old one and drop it. The caller passes a `CShared` it cloned, which replaces the `inc` | 5 → 3 (the `own` calls keep their contract; `replace` is 1) |
| `repository/mod.rs:616-640` `publish` | `own`, `compare_and_swap`, `own(NULL)` + `free` on loss | `own(self)` on the candidate, then `match slot.try_insert(c) { Ok(w) => w, Err((w, loser)) => { own(NULL) on loser; drop(loser); w } }` | 3 → 2 |
| `repository/mod.rs:520-586` `set_odb`, `set_refdb`, `set_config`, `set_index` | each takes `Option<NonNull<T>>` and builds a `free` closure with an `unsafe` adopt | take `Option<CShared<T>>`; no closure | 4 (the closures' adopts) |
| `repository/mod.rs:642-675` `config_weak` | `load`, `publish(.., \|p\| drop(Config::from_raw(p)))`, `p.as_ref()` | `self.config.get()` or `get_or_try_init` (through `publish` when the owner must be set first); returns `&GitConfig` by deref, with no `unsafe` | 2 |
| `repository/mod.rs:677-704` `odb_weak` | the same for the odb, with `own` before `set_caps` | `publish(&self.odb, odb.0)`; the `own` / disown around `set_caps` stays | 1 |
| `repository/mod.rs:706-713` `odb` | `p.as_ref()` | `self.odb.get().map(\|o\| &**o)` after `odb_weak` | 1 |
| `repository/mod.rs:720-737` `refdb_weak` | `publish(.., \|p\| git_refdb_free(p))` | `publish(&self.refdb, CShared::from_raw(out))` | 1 (the closure's) |
| `repository/mod.rs:739-766` `index_weak` | `publish` with `IndexPtr::decref` | `publish(&self.index, idx)`, where `idx: CShared<GitIndex>` | 1 |
| `repository/mod.rs:768-783` `odb_ptr`, `refdb_ptr`, `config_ptr`, `index_ptr` | `load(..)` | `self.odb.as_ptr()` and so on | — |
| `repository/mod.rs:314-333` `cleanup` | four `set_*(None)`, with a SAFETY each | unchanged calls; `set_*` stays `unsafe fn` | — |
| `repository/ffi.rs:389-410, 514-535, 606-628, 692-715` the `*__weakptr` exports | write `r.*_ptr()` | unchanged | — |
| `repository/ffi.rs:491, 583, 669, 756` `git_repository_set_*` | pass `NonNull<T>` | pass `CShared::from_borrowed(p)`: the repository takes its own reference, as `GIT_REFCOUNT_INC` did | — |
| `repository/handles.rs:24-36` `refcount` | `unsafe fn` over `NonNull<c_void>` | deleted; `refcount_of::<T>` (§1.1) | 1 |

`GIT_REFCOUNT_OWN` stays with the repository (`own(self.as_ptr())` before
publishing). Only the repository knows it is the owner, and the count's
`c_down_ref` reads the owner field. ffibox does not model owners.

### 2.2 The grafts and the strings

| Location | Today | Becomes | Removes |
|---|---|---|---|
| `repository/mod.rs:178-179` `grafts`, `shallow_grafts` | `AtomicPtr<c_void>`, freed by `git_grafts_free` in `cleanup` (`:319-321`) | `CAtomicOwner<CBox<Grafts, GraftsFree>>` with `impl_cdrop!(GraftsFree, Grafts, c::git_grafts_free)`. `cleanup` does `unsafe { self.grafts.replace(None) }` | 2 → 2 (each `git_grafts_free` call becomes a `replace`; no raw free) |
| `repository/open.rs:750-775` `load_grafts` | C fills `repo.grafts_slot()` with `git_grafts_new(&slot)` | unchanged: C fills an empty field, which ffibox allows. Better: `git_grafts_new` into a local, then `self.grafts.set(..)` | — |
| `repository/mod.rs:166-172` `gitlink` … `namespace` (5 strings; `ident_name` / `ident_email` likewise) | `AtomicPtr<c_char>` | `CAtomicOwner<GitCStr>` (`GitCStr = CStrBox<GitAlloc>`, whose policy is `Default`) | — |
| `repository/mod.rs:420-430` `replace_string` | `swap` + `alloc::free(old)` | `drop(unsafe { self.string_field(f).replace(v) })` | 1 |
| `repository/mod.rs:432-446` `init_string` | `compare_exchange` + `alloc::free` of the loser | `let _ = self.string_field(f).set(v);` (the loser drops) | 1 |
| `repository/mod.rs:362-374` `read_str`, behind `gitdir()`, `workdir()`, … | `load` + `CStr::from_ptr` | `f.get().map(GitCStr::as_c_str)` | 1 |
| `repository/mod.rs:164-165, 182` `attrcache`, `diff_drivers`, `submodule_cache` | `AtomicPtr<c_void>`, set and freed by C | unchanged: C owns them, and Rust only hands out the slot's address | — |

### 2.3 Tests

`repository/tests.rs:285-365` (`publish_races` and its sibling) test the
hand-written `publish` over raw `GitRefcount` boxes. Keep them, ported to
`CShared<…>`. ffibox's `tests/atomic_owner.rs` and `tests/counted.rs`
(`published_lazily_and_the_loser_released`) cover the primitive under Miri.

---

## 3. `CBits` / `CBitfield` / `CAtomicBitfield`: the bitfields

None of these sites has `unsafe` for the masks themselves. The win is
declaring each layout once from the C declaration, and in one place a bug.

### 3.1 `git_index`'s bits: a lost update (fix this first)

`index/mod.rs:915-936` (`bit_accessors!`) sets a bit with a `load`, then a
`store` of the whole word. Two threads that set different bits at the same
time (`threads::diff` runs read paths on 32 threads) can lose one of the
changes. The skip-when-already-set is right; the store is not.

| Location | Today | Becomes |
|---|---|---|
| `index/mod.rs:848-856` `mod bit` | five `1 << n` masks | `const ON_DISK: CBits<u32> = CBits::first(1); const IGNORE_CASE = ON_DISK.next(1); …` |
| `index/mod.rs:880-882` `pub bits: AtomicU32` | | `pub bits: CAtomicBitfield<u32>` (same layout) |
| `index/mod.rs:915-936` `bit_accessors!` | load/store macro | `self.bits.is_set(DIRTY, Relaxed)` and `self.bits.set_flag(DIRTY, v, Relaxed)`: one `fetch_or` / `fetch_and`, skipped when already set. The macro can go |
| `index/mod.rs:939-945` the invocation | | five pairs of one-line methods, or the call sites use the fields directly |
| `index/ops.rs:609` comment, `tree.rs:1012` comment | "the bits atomic" | unchanged |

### 3.2 `git_commit_list_node`: one layout, declared twice

| Location | Today | Becomes |
|---|---|---|
| `commit_list.rs:46-60` `GitCommitListNode.bits: u16` | | `bits: CBitfield<u16>` |
| `commit_list.rs:62-73` `BIT_SEEN` … `FLAGS_SHIFT` | masks and a shift | `pub const SEEN: CBits<u16> = CBits::first(1); UNINTERESTING = SEEN.next(1); TOPO_DELAY = …; PARSED = …; ADDED = …; FLAGS = ADDED.next(4);` |
| `commit_list.rs:75-86` `parsed`, `flags`, `set_flags` | hand shifts | `self.bits.is_set(PARSED)`, `self.bits.get(FLAGS)`, `self.bits.set(FLAGS, f as u16)` |
| `commit_list.rs:286` `(*node).bits \|= BIT_PARSED` | through the raw pointer | `(*node).bits.set_flag(PARSED, true)` (the same raw-pointer write; `CBitfield` is `Copy`) |
| `revwalk/node.rs:31-45` `SEEN` … `FLAGS_MASK` | **a second copy** of the masks | `pub use crate::commit_list::{SEEN, UNINTERESTING, TOPO_DELAY, PARSED, ADDED, FLAGS};` |
| `revwalk/node.rs:92-155` `bits`, `set_bits`, `set_flag`, `seen` … `reset` | `field!(…, bits: u16)` read, mask, write | read `field!(…, bits: CBitfield<u16>)`, `get` / `set_flag` on the copy, write it back. `reset` clears the five fields with `set(F, 0)` each. The 2 `unsafe` field accesses stay, since the node is reached by raw pointer |

### 3.3 The others

| Location | Today | Becomes |
|---|---|---|
| `repository/mod.rs:138-143` `BIT_USE_ENV`, `BIT_IS_BARE`, `BIT_IS_WORKTREE`; `:174` `bits: AtomicU32`; `:448-500` `bit` / `set_bit` and the six accessors | masks; `fetch_or` / `fetch_and` (correct) | `CAtomicBitfield<u32>` and `USE_ENV = CBits::first(1)`, `IS_BARE = USE_ENV.next(1)`, `IS_WORKTREE = IS_BARE.next(1)`. Same code, without masks. `repository/tests.rs:241-260` (`bitfield_bits_match_c`) stays as the C-compiled check |
| `util/process.rs:36-63` `GitProcessOptions.flags: c_uint` and `mod flags` | five masks | `flags: CBitfield<c_uint>` and `USE_SHELL = CBits::first(1)` … `EXCLUDE_ENV`. `:1044-1048` read `o.flags.is_set(..)`; `:1134` `merged.flags.set_flag(USE_SHELL, true)` |
| `commit.rs:144-178` `GitCommitCreateOptions.flags`, `allow_empty_commit` | `self.flags & 1` | `CBitfield<c_uint>` and `ALLOW_EMPTY_COMMIT = CBits::first(1)`. This is a public-header type, so the layout test must keep `flags` at its offset (it does: `#[repr(transparent)]`) |
| `index/mod.rs:2019-2021` `GitIndexWriter.should_write: c_uint`; `index/ops.rs:790, 835, 852` | whole-word writes of 0 and 1 | `CBitfield<c_uint>` and `SHOULD_WRITE = CBits::first(1)`. Writing the word also wrote the padding bits, which is harmless, but `set_flag` writes only the field |

---

## What is not migrated, and why

These are the candidate patterns that did not become primitives (see the
report).

- **Rust statics that C reads and writes** (`cache.rs:93-103`,
  `mwindow.rs:69-76`, `odb.rs:805-812`, `pack.rs:88`, `refs.rs:91-92`,
  `repository/mod.rs:1071-1079`, `index/mod.rs:184`, `settings.rs:26-33`,
  and the `extern` atomics in `sys/indexer.rs:22-27`, `sys/streams.rs`,
  `sys/transports.rs`, `sys/pending/l_repo.rs`). They stay std atomics,
  which std guarantees to have the scalar's layout.
- **`git__allocator`** (`util/alloc.rs:61-145`): its six
  `transmute::<*mut c_void, Option<fn>>` calls are the only atomic
  function-pointer cell in the port. Not migrated.
- **`MutexProtected`** (`mwindow.rs:88-100`) and the `GitMutex` /
  `GitRwLock` family (`util/thread.rs`): unchanged. L-PACK-1 is fixed and
  tested there.
- **NUL-terminated copies and `MaybeUninit` out-buffers**: std's `CStr`,
  `CString` and `MaybeUninit` slices, already used after the reviews
  (`util/fs_path/ffi.rs:986-1020`, `util/process.rs:694`).
