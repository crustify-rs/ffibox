//! C bitfields: fields named from the declaration order, read and written in
//! a plain word and in an atomic one, the C layout they reproduce, and the
//! atomic word keeping each thread's change to a different field.

#![allow(missing_docs)]

use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};

use ffibox::{CAtomicBitfield, CBitfield, CBits};

// `struct git_commit_list_node`'s word:
// `unsigned seen:1, uninteresting:1, topo_delay:1, parsed:1, added:1, flags:4;`
const SEEN: CBits<u16> = CBits::first(1);
const UNINTERESTING: CBits<u16> = SEEN.next(1);
const TOPO_DELAY: CBits<u16> = UNINTERESTING.next(1);
const PARSED: CBits<u16> = TOPO_DELAY.next(1);
const ADDED: CBits<u16> = PARSED.next(1);
const FLAGS: CBits<u16> = ADDED.next(4);

#[test]
fn declaration_order_gives_the_abi_bits() {
    // Little-endian: from bit 0 up; big-endian: from the top bit down. The
    // C compiler's own placement is checked in `bitfield_c_layout.rs`.
    let at = |lsb_le: u32, w: u32| {
        if cfg!(target_endian = "little") {
            lsb_le
        } else {
            16 - lsb_le - w
        }
    };
    assert_eq!(SEEN.mask(), 1 << at(0, 1));
    assert_eq!(UNINTERESTING.mask(), 1 << at(1, 1));
    assert_eq!(TOPO_DELAY.mask(), 1 << at(2, 1));
    assert_eq!(PARSED.mask(), 1 << at(3, 1));
    assert_eq!(ADDED.mask(), 1 << at(4, 1));
    assert_eq!(FLAGS.mask(), 0xf << at(5, 4));
    assert_eq!((FLAGS.lsb(), FLAGS.width()), (at(5, 4), 4));
    assert_eq!(CBits::<u16>::at(at(5, 4), 4), FLAGS);
}

#[test]
fn fields_are_independent() {
    let mut w = CBitfield::<u16>::default();
    w.set_flag(PARSED, true);
    w.set(FLAGS, 0b1010);
    w.set_flag(SEEN, true);
    assert!(w.is_set(PARSED) && w.is_set(SEEN));
    assert!(!w.is_set(UNINTERESTING) && !w.is_set(ADDED));
    assert_eq!(w.get(FLAGS), 0b1010);
    // C truncates an assignment to the field's width.
    w.set(FLAGS, 0xff3);
    assert_eq!(w.get(FLAGS), 3);
    assert!(w.is_set(PARSED) && w.is_set(SEEN));
    w.set_flag(PARSED, false);
    assert!(!w.is_set(PARSED));
    w.set_flag(FLAGS, true);
    assert_eq!(w.get(FLAGS), 0xf);
    assert_eq!(format!("{:?}", CBitfield::new(0x21u16)), "CBitfield(0x21)");
}

#[test]
fn whole_words_and_edges() {
    let all = CBits::<u8>::first(8);
    let mut w = CBitfield::new(0u8);
    w.set(all, 0xa5);
    assert_eq!(w.into_inner(), 0xa5);
    let top = CBits::<u64>::at(63, 1);
    assert_eq!(CBits::<u64>::first(64).mask(), u64::MAX);
    let mut w = CBitfield::new(0u64);
    w.set_flag(top, true);
    assert_eq!(w.into_inner(), 1 << 63);
    let wide = CBits::<u32>::first(31).next(1);
    let last = if cfg!(target_endian = "little") {
        1 << 31
    } else {
        1
    };
    let a = CAtomicBitfield::<u32>::new(0);
    assert!(!a.set_flag(wide, true, SeqCst));
    assert_eq!(a.load_word(SeqCst), last);
    assert_eq!(a.into_inner(), last);
}

#[test]
#[should_panic(expected = "does not fit")]
fn a_field_that_does_not_fit_panics_at_run_time() {
    let w = std::hint::black_box(7);
    let _ = CBits::<u8>::first(w).next(2);
}

// `struct git_index`'s word:
// `unsigned on_disk:1, ignore_case:1, distrust_filemode:1, no_symlinks:1, dirty:1;`
const ON_DISK: CBits<u32> = CBits::first(1);
const IGNORE_CASE: CBits<u32> = ON_DISK.next(1);
const DISTRUST_FILEMODE: CBits<u32> = IGNORE_CASE.next(1);
const NO_SYMLINKS: CBits<u32> = DISTRUST_FILEMODE.next(1);
const DIRTY: CBits<u32> = NO_SYMLINKS.next(1);

#[test]
fn atomic_flags_and_fields() {
    let w = CAtomicBitfield::<u32>::default();
    assert!(!w.set_flag(DIRTY, true, Release));
    assert!(w.set_flag(DIRTY, true, AcqRel), "already set: no write");
    assert!(w.is_set(DIRTY, Acquire));
    assert!(w.set_flag(DIRTY, false, Relaxed));
    assert!(!w.set_flag(DIRTY, false, Relaxed), "already clear");
    let level = DIRTY.next(3);
    assert_eq!(w.swap(level, 0b1101, AcqRel), 0);
    assert_eq!(w.load(level, Acquire), 0b101);
    w.store(level, 2, Release);
    assert_eq!(w.swap(level, 2, SeqCst), 2);
    assert_eq!(w.load(level, Relaxed), 2);
    assert!(!w.is_set(DIRTY, Relaxed));
    assert!(format!("{w:?}").starts_with("CAtomicBitfield("));
}

#[test]
fn concurrent_changes_to_different_fields_are_all_kept() {
    let fields = [ON_DISK, IGNORE_CASE, DISTRUST_FILEMODE, NO_SYMLINKS, DIRTY];
    let w = CAtomicBitfield::<u32>::new(0);
    let rounds = if cfg!(miri) { 20 } else { 2000 };
    std::thread::scope(|s| {
        for f in fields {
            let w = &w;
            s.spawn(move || {
                for i in 0..rounds {
                    w.set_flag(f, i % 2 == 0, Relaxed);
                }
                // Each thread ends with its own bit set.
                w.set_flag(f, true, Relaxed);
            });
        }
    });
    for f in fields {
        assert!(w.is_set(f, Relaxed));
    }
    let all = fields.iter().fold(0, |m, f| m | f.mask());
    assert_eq!(w.load_word(Relaxed), all);
    assert_eq!(all.count_ones(), 5);
}

/// The consumer side writes no `unsafe`: the primitive has none to offer.
#[forbid(unsafe_code)]
mod consumer {
    use super::*;
    #[test]
    fn safe_api() {
        let mut w = CBitfield::new(0u16);
        w.set(FLAGS, 9);
        assert_eq!(w.get(FLAGS), 9);
    }
}
