//! `CAtomicOwner` as a C struct's lazily set pointer field: filled once from
//! several threads with the losers dropped, read by C through the field,
//! filled by C with compare-and-swap, replaced under `replace`'s contract,
//! and holding each kind of pointer owner.

#![allow(missing_docs, non_camel_case_types)]

use core::ffi::{CStr, c_char};
use core::ptr::{self, NonNull, addr_of};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use ffibox::{
    CArc, CAtomicOwner, CBox, CDrop, CRefClone, CRefSeam, CStrBox, define_ctype, impl_cdrop,
    impl_crefclone,
};

// ---------------------------------------------------------------------------
// A C struct with lazily published fields
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct odb_st {
    rc: AtomicUsize,
    id: u32,
    /// Counts frees; a test that checks them passes its own counter.
    freed: &'static AtomicUsize,
}

static ODB_FREED: AtomicUsize = AtomicUsize::new(0);

fn odb_new(id: u32) -> *mut odb_st {
    odb_new_counted(id, &ODB_FREED)
}

fn odb_new_counted(id: u32, freed: &'static AtomicUsize) -> *mut odb_st {
    Box::into_raw(Box::new(odb_st {
        rc: AtomicUsize::new(1),
        id,
        freed,
    }))
}

/// A free counter of its own for each test that names one: a static, so
/// that nothing leaks.
macro_rules! counter {
    () => {{
        static FREED: AtomicUsize = AtomicUsize::new(0);
        &FREED
    }};
}
unsafe fn odb_up_ref(p: *mut odb_st) {
    // SAFETY: the caller holds a reference.
    unsafe { (*p).rc.fetch_add(1, Ordering::Relaxed) };
}
unsafe fn odb_free(p: *mut odb_st) {
    // SAFETY: the caller gives a reference up.
    if unsafe { (*p).rc.fetch_sub(1, Ordering::AcqRel) } == 1 {
        // SAFETY: the last reference.
        unsafe { (*p).freed.fetch_add(1, Ordering::Relaxed) };
        // SAFETY: as above.
        drop(unsafe { Box::from_raw(p) });
    }
}

define_ctype!(Odb, OdbRef, OdbMut, odb_st);
impl OdbRef<'_> {
    fn id(&self) -> u32 {
        // SAFETY: `id` is fixed once the odb exists.
        unsafe { addr_of!((*self.as_ptr()).id).read() }
    }
}
// SAFETY: the count is atomic and `id` immutable.
unsafe impl Send for Odb {}
// SAFETY: as above.
unsafe impl Sync for Odb {}

#[derive(Clone, Copy, Debug, Default)]
pub struct OdbUnref;
impl_cdrop!(OdbUnref, Odb, odb_free);
impl_crefclone!(OdbUnref, Odb, odb_up_ref);
pub type OdbArc = CArc<Odb, OdbUnref>;

/// `struct git_repository`'s prefix: `git_odb *_odb; char *workdir;`.
#[repr(C)]
pub struct Repo {
    odb: CAtomicOwner<OdbArc>,
    workdir: CAtomicOwner<CStrBox<StrFree>>,
}

/// The repository as C sees it.
#[repr(C)]
pub struct repo_c {
    odb: *mut odb_st,
    workdir: *mut c_char,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StrFree;
// SAFETY: frees strings `CStrBox::from_c_str` under this policy made, below.
unsafe impl CDrop<c_char> for StrFree {
    unsafe fn c_drop(&self, ptr: NonNull<c_char>) {
        // SAFETY: a `CString` that `make_str` leaked.
        drop(unsafe { std::ffi::CString::from_raw(ptr.as_ptr()) });
    }
}
fn make_str(s: &CStr) -> CStrBox<StrFree> {
    // SAFETY: a fresh string `StrFree` frees.
    unsafe { CStrBox::from_raw(s.to_owned().into_raw()) }.unwrap()
}

fn open_odb(id: u32) -> OdbArc {
    open_odb_counted(id, &ODB_FREED)
}

fn open_odb_counted(id: u32, freed: &'static AtomicUsize) -> OdbArc {
    // SAFETY: a fresh odb with one reference.
    unsafe { OdbArc::from_raw(odb_new_counted(id, freed).cast()) }.unwrap()
}

/// C's `git_repository_odb__weakptr` fast path: a plain read of the field.
unsafe fn c_odb_weakptr(repo: *const repo_c) -> *mut odb_st {
    // SAFETY: the caller passes a live repository; the field is read once.
    unsafe { ptr::read_volatile(addr_of!((*repo).odb)) }
}

/// C's own publish: `git_atomic_compare_and_swap(&repo->_odb, NULL, odb)`.
unsafe fn c_publish(repo: *mut repo_c, odb: *mut odb_st) -> *mut odb_st {
    // SAFETY: the field is a C pointer that Rust accesses atomically; C uses
    // the same atomic operation.
    let field = unsafe { AtomicPtr::from_ptr(&raw mut (*repo).odb) };
    match field.compare_exchange(ptr::null_mut(), odb, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => odb,
        Err(winner) => {
            // SAFETY: the loser's reference.
            unsafe { odb_free(odb) };
            winner
        }
    }
}

#[test]
fn a_zeroed_field_is_empty() {
    // SAFETY: all-zero bytes are an empty field (documented), as in a
    // `calloc`ed repository.
    let mut z: CAtomicOwner<OdbArc> = unsafe { core::mem::zeroed() };
    assert!(z.get().is_none() && z.as_ptr().is_null() && z.take().is_none());
}

#[test]
fn layout_is_the_c_pointer() {
    assert_eq!(size_of::<CAtomicOwner<OdbArc>>(), size_of::<*mut odb_st>());
    assert_eq!(
        align_of::<CAtomicOwner<OdbArc>>(),
        align_of::<*mut odb_st>()
    );
    assert_eq!(size_of::<Repo>(), size_of::<repo_c>());
}

#[test]
fn filled_once_from_many_threads_losers_dropped() {
    let freed = counter!();
    let repo = Repo {
        odb: CAtomicOwner::new(),
        workdir: CAtomicOwner::new(),
    };
    let made = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8 {
            let (repo, made) = (&repo, &made);
            s.spawn(move || {
                let odb = repo.odb.get_or_init(|| {
                    made.fetch_add(1, Ordering::Relaxed);
                    open_odb_counted(t, freed)
                });
                // The weak borrow and a strong clone agree.
                let strong = odb.clone();
                assert_eq!(strong.as_ref().id(), odb.as_ref().id());
            });
        }
    });
    let id = repo.odb.get().unwrap().as_ref().id();
    let made = made.load(Ordering::Relaxed);
    assert_eq!(freed.load(Ordering::Relaxed), made - 1);
    // C reads the same object through its pointer.
    let c = ptr::from_ref(&repo).cast::<repo_c>();
    // SAFETY: `Repo` has `repo_c`'s layout.
    let p = unsafe { c_odb_weakptr(c) };
    assert_eq!(p, repo.odb.as_ptr().cast());
    // SAFETY: the published odb, live while the repository holds it.
    assert_eq!(unsafe { (*p).id }, id);
    drop(repo);
    assert_eq!(freed.load(Ordering::Relaxed), made);
}

#[test]
fn c_publishes_and_rust_borrows() {
    let repo = Repo {
        odb: CAtomicOwner::new(),
        workdir: CAtomicOwner::new(),
    };
    let c = ptr::from_ref(&repo).cast::<repo_c>().cast_mut();
    // SAFETY: a fresh odb's reference goes to the field.
    let won = unsafe { c_publish(c, odb_new(41)) };
    // SAFETY: as above; this one loses and is freed.
    let lost = unsafe { c_publish(c, odb_new(42)) };
    assert_eq!(won, lost);
    assert_eq!(repo.odb.get().unwrap().as_ref().id(), 41);
    // Rust's insert loses to C's, and gets its value back.
    let (winner, mine) = repo.odb.try_insert(open_odb(43)).unwrap_err();
    assert_eq!(winner.as_ref().id(), 41);
    assert_eq!(mine.as_ref().id(), 43);
}

#[test]
fn try_insert_hands_the_loser_back_to_undo_its_setup() {
    let repo = Repo {
        odb: CAtomicOwner::new(),
        workdir: CAtomicOwner::new(),
    };
    assert!(repo.odb.set(open_odb(1)).is_ok());
    let loser = repo.odb.set(open_odb(2)).unwrap_err();
    assert_eq!(loser.as_ref().id(), 2);
    assert_eq!(repo.odb.get().unwrap().as_ref().id(), 1);
}

#[test]
fn get_or_try_init_leaves_the_field_empty_on_failure() {
    let slot = CAtomicOwner::<OdbArc>::new();
    assert_eq!(
        slot.get_or_try_init(|| Err::<OdbArc, _>(-1)).err(),
        Some(-1)
    );
    assert!(slot.get().is_none());
    assert!(slot.as_ptr().is_null());
    let v = slot.get_or_try_init(|| Ok::<_, i32>(open_odb(9))).unwrap();
    assert_eq!(v.as_ref().id(), 9);
}

#[test]
fn replace_take_and_get_mut() {
    let freed = counter!();
    let mut repo = Repo {
        odb: CAtomicOwner::with(open_odb_counted(1, freed)),
        workdir: CAtomicOwner::from(Some(make_str(c"/w"))),
    };
    assert_eq!(repo.workdir.get().unwrap().as_c_str(), c"/w");
    // `git_repository_set_odb`: no borrow of the old odb is live.
    // SAFETY: nothing borrows the field.
    let old = unsafe { repo.odb.replace(Some(open_odb_counted(2, freed))) }.unwrap();
    assert_eq!(old.as_ref().id(), 1);
    drop(old);
    assert_eq!(freed.load(Ordering::Relaxed), 1);
    // Exclusive access swaps in place.
    let slot = repo.odb.get_mut().unwrap();
    let older = core::mem::replace(slot, open_odb_counted(3, freed));
    assert_eq!(older.as_ref().id(), 2);
    drop(older);
    assert_eq!(repo.odb.take().unwrap().as_ref().id(), 3);
    assert!(repo.odb.get_mut().is_none());
    assert!(repo.odb.take().is_none());
    // SAFETY: nothing borrows the field.
    let w = unsafe { repo.workdir.replace(None) };
    assert_eq!(w.unwrap().as_c_str(), c"/w");
    assert!(repo.workdir.into_inner().is_none());
    assert_eq!(freed.load(Ordering::Relaxed), 3);
}

#[test]
fn holds_every_pointer_owner() {
    // `CBox`.
    #[repr(C)]
    pub struct cfg_st {
        v: u8,
    }
    define_ctype!(Cfg, CfgRef, CfgMut, cfg_st);
    #[derive(Clone, Copy, Debug, Default)]
    pub struct CfgFree;
    unsafe fn cfg_free(p: *mut cfg_st) {
        // SAFETY: a box this test leaked.
        drop(unsafe { Box::from_raw(p) });
    }
    impl_cdrop!(CfgFree, Cfg, cfg_free);
    let b = CAtomicOwner::<CBox<Cfg, CfgFree>>::new();
    // SAFETY: a fresh object `cfg_free` frees.
    let cfg = unsafe { CBox::from_raw(Box::into_raw(Box::new(cfg_st { v: 3 })).cast()) }.unwrap();
    let got = b.get_or_init(|| cfg);
    // SAFETY: a read through the shared handle.
    assert_eq!(unsafe { (*got.as_ref().as_ptr()).v }, 3);

    // `Box`.
    let bx = CAtomicOwner::<Box<Vec<u8>>>::default();
    bx.get_or_init(|| Box::new(vec![1, 2]));
    assert_eq!(**bx.get().unwrap(), [1, 2]);
    assert_eq!(format!("{bx:?}"), "CAtomicOwner(Some([1, 2]))");

    // `CArc`, cloned out of the field.
    fn assert_ref_clone<P: CRefClone<Odb>>() {}
    assert_ref_clone::<OdbUnref>();
    let a = CAtomicOwner::<OdbArc>::from(open_odb(5));
    let strong = a.get().cloned().unwrap();
    drop(a);
    assert_eq!(strong.as_ref().id(), 5);
}

#[test]
fn targets_of_box_and_cstrbox() {
    let bx = CAtomicOwner::<Box<Vec<u8>>>::new();
    assert!(bx.get_target().is_none());
    let v: &Vec<u8> = bx.get_or_init_target(|| Box::new(vec![7]));
    assert_eq!(*v, [7]);
    assert!(core::ptr::eq(v, bx.as_ptr().cast_const()));
    let (held, back) = bx.try_insert_target(Box::new(vec![8])).unwrap_err();
    assert!(core::ptr::eq(held, v));
    assert_eq!(*back, [8]);

    // The pointer variant, for any owner.
    let a = CAtomicOwner::<OdbArc>::new();
    let p = a.try_insert_ptr(open_odb(9)).unwrap();
    let (held, loser) = a.try_insert_ptr(open_odb(10)).unwrap_err();
    assert_eq!(held, p);
    assert_eq!(p.as_ptr(), a.as_ptr());
    drop(loser);

    let st = CAtomicOwner::<CStrBox<StrFree>>::new();
    let s: &CStr = st.try_insert_target(make_str(c"gitdir")).unwrap();
    assert_eq!(s, c"gitdir");
    assert_eq!(st.get_target(), Some(c"gitdir"));
    let e: Result<&CStr, ()> = st.get_or_try_init_target(|| Err(()));
    assert_eq!(e, Ok(c"gitdir"));
    let empty = CAtomicOwner::<CStrBox<StrFree>>::new();
    assert_eq!(empty.get_or_try_init_target(|| Err(1)), Err(1));
    assert!(empty.get_target().is_none());
}

#[test]
fn a_panicking_initialiser_leaves_the_field_empty() {
    let slot = CAtomicOwner::<Box<u32>>::new();
    let r = std::panic::catch_unwind(|| {
        slot.get_or_init(|| panic!("no value"));
    });
    assert!(r.is_err());
    assert!(slot.get().is_none());
    assert_eq!(**slot.get_or_init(|| Box::new(3)), 3);
}

#[test]
fn a_loser_whose_drop_panics_leaves_the_winner_in_place() {
    struct Loud(bool);
    impl Drop for Loud {
        fn drop(&mut self) {
            assert!(!self.0, "the loser's drop panics");
        }
    }
    let slot = CAtomicOwner::<Box<Loud>>::with(Box::new(Loud(false)));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The field is full: `f`'s value loses, and its drop panics.
        let _ = slot.set(Box::new(Loud(true)));
    }));
    assert!(r.is_err());
    assert!(!slot.get().unwrap().0);
}

/// The consumer side writes no `unsafe`.
#[forbid(unsafe_code)]
mod consumer {
    use ffibox::CAtomicOwner;

    #[test]
    fn safe_api() {
        let slot = CAtomicOwner::<Box<String>>::new();
        let v = slot.get_or_init(|| Box::new("a".into()));
        assert_eq!(v.as_str(), "a");
        assert!(slot.set(Box::new("b".into())).is_err());
        let mut slot = slot;
        assert_eq!(slot.take().as_deref().map(String::as_str), Some("a"));
    }
}
