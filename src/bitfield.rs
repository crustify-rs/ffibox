//! C bitfields: [`CBits<W>`] names a field of a storage unit `W` once, in
//! C's declaration order, and [`CBitfield<W>`] / [`CAtomicBitfield<W>`] are
//! the unit, read and written by field.
//!
//! Rust has no bitfields, so a mirror holds the bits as an unsigned integer
//! and reads each field through a shift and a mask. Two things are the ABI's:
//! which bits of its unit a field takes, which [`CBits`] computes, and which
//! bytes of the struct the unit is, which the binding chooses and a layout
//! check against the C compiler confirms.
//!
//! **Platform scope.** The placement within a unit is right for the System V
//! family (x86-64, i386, s390x, PowerPC) and AAPCS / AAPCS64 (Arm), little-
//! and big-endian, and for Microsoft's x64 ABI. It is checked against clang
//! for x86-64, AArch64, ARMv7, i686, s390x and PowerPC64 Linux and for
//! x86-64 Windows (`tests/bitfield_c_layout.rs`, from clang's own layouts,
//! `tests/data/bitfield_layouts.txt`). Other ABIs are not covered.
//!
//! **Bits within a unit.** The first field declared takes the least
//! significant bits on a little-endian target and the most significant on a
//! big-endian one, and each later field follows it. [`CBits::first`] and
//! [`CBits::next`] follow the declaration and compute the bits from the
//! target, so the binding writes the C's order and widths, not masks.
//!
//! **Which unit, System V and AAPCS.** A run of bitfields is packed bit after
//! bit, starting where the previous member ended. A field of declared type
//! `T` stays where it is only if it fits in some `sizeof(T)`-byte span that
//! starts at a multiple of `alignof(T)`; otherwise it moves on to the next
//! such boundary, and `:0` moves to the next boundary. The boundary is the
//! **alignment**, which is the size but for `long long` on i686 (aligned to
//! 4 there, so a 40-bit field after 5 bytes stays at bit 40). So the unit is
//! **not** the declared type:
//! the mirror's `W` is an unsigned integer covering the bytes the run
//! occupies, at the run's offset, and any byte of `W` beyond the run must be
//! padding in C, never another member. Members after the run may sit inside
//! the declared type's size (`unsigned a:4; char c;` puts `c` at offset 1),
//! and a run may start inside the previous member's unit (`char c;
//! unsigned a:4;` puts `a` in byte 1). The declared type still gives the
//! struct its alignment, which may need `#[repr(C, align(N))]`, and, where
//! the last unit extends past the last byte, its size.
//!
//! **Runs no `W` covers.** A run whose bytes no aligned unsigned integer
//! covers without also covering another member cannot be mirrored with
//! `CBitfield`: `unsigned char c[3]; unsigned long long a:40;` puts `a` in
//! bytes 3 to 7, and the only integer over them at an aligned offset, a
//! `u64` at 0, covers `c`. Mirror such a run as a byte array, with accessors
//! that read and write its bytes by hand (as the layout test's `T1` on i686
//! and `T2` do), or as a `#[repr(packed)]` integer at the run's offset.
//!
//! **Which unit, Microsoft.** Each run of fields of one declared type is a
//! unit of that type, aligned as it; a change of declared type starts a new
//! unit. There `W` is the declared type.
//!
//! ```c
//! struct node {                      /* System V: 8 bytes, align 4 */
//!     unsigned seen:1, uninteresting:1, flags:4;  /* bits 0-5 of byte 0 */
//!     unsigned short in_degree;      /* offset 2, inside the unsigned's size */
//!     unsigned short out_degree;
//! };
//! ```
//!
//! ```
//! use ffibox::bitfield::{CBitfield, CBits};
//!
//! // System V: the run takes 6 bits, so 2 bytes cover it before `in_degree`.
//! #[repr(C, align(4))] // the alignment of `unsigned`
//! pub struct Node {
//!     pub bits: CBitfield<u16>,
//!     pub in_degree: u16,
//!     pub out_degree: u16,
//! }
//! pub const SEEN: CBits<u16> = CBits::first(1);
//! pub const UNINTERESTING: CBits<u16> = SEEN.next(1);
//! pub const FLAGS: CBits<u16> = UNINTERESTING.next(4);
//!
//! let mut n = Node { bits: CBitfield::new(0), in_degree: 0, out_degree: 0 };
//! n.bits.set_flag(UNINTERESTING, true);
//! n.bits.set(FLAGS, 0b1_0110);           // truncated to 4 bits, as C does
//! assert!(!n.bits.is_set(SEEN));
//! assert!(n.bits.is_set(UNINTERESTING));
//! assert_eq!(n.bits.get(FLAGS), 0b0110);
//! # #[cfg(target_endian = "little")]
//! assert_eq!(n.bits.into_inner(), 0b0110_10);
//! assert_eq!((size_of::<Node>(), core::mem::offset_of!(Node, in_degree)), (8, 2));
//! ```
//!
//! A word that several threads update while C reads it is a
//! [`CAtomicBitfield<W>`], which changes one field with an atomic
//! read-modify-write and leaves the others as they are. A `load` then a
//! `store` of the whole word would lose another thread's concurrent change
//! to a different field; C's own bitfield assignment is that `load` and
//! `store`, so C writing a word that Rust writes concurrently is a race on
//! C's side, as it was between two C threads.
//!
//! Only unsigned fields: a signed one (`int x:3`) sign-extends, and plain
//! `int` bitfields are signed or not as the implementation chooses.

use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// An unsigned integer that holds C bitfields: `u8`, `u16`, `u32`, `u64`.
/// Sealed.
pub trait CBitWord: Copy + Eq + fmt::Debug + sealed::Sealed {
    /// Its width in bits.
    const BITS: u32;
    #[doc(hidden)]
    fn to_u64(self) -> u64;
    #[doc(hidden)]
    fn from_u64(v: u64) -> Self;
}

/// A [`CBitWord`] that a [`CAtomicBitfield`] can hold: one whose atomic has
/// its size and alignment, so that the atomic unit sits where C puts the
/// plain one. Sealed.
///
/// Implemented for `u8`, `u16` and `u32`, and for `u64` on targets with
/// 64-bit atomics where `u64` is 8-aligned: not on 32-bit x86 (i686), where
/// C aligns a `long long` member to 4 and `AtomicU64` to 8, so that
/// `CAtomicBitfield<u64>` is not a type there at all. A mirror holding one
/// fails to build, whether or not anything calls `new`.
pub trait CAtomicBitWord: CBitWord {
    /// The atomic of the same width.
    type Atomic: CAtomicWord<Word = Self>;
}

/// The atomic of a [`CAtomicBitWord`]. Sealed.
pub trait CAtomicWord: sealed::Sealed {
    /// The word it holds.
    type Word: CAtomicBitWord;
    #[doc(hidden)]
    fn new(v: Self::Word) -> Self;
    #[doc(hidden)]
    fn load(&self, order: Ordering) -> Self::Word;
    #[doc(hidden)]
    fn fetch_or(&self, v: Self::Word, order: Ordering) -> Self::Word;
    #[doc(hidden)]
    fn fetch_and(&self, v: Self::Word, order: Ordering) -> Self::Word;
    #[doc(hidden)]
    fn compare_exchange_weak(
        &self,
        current: Self::Word,
        new: Self::Word,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self::Word, Self::Word>;
    #[doc(hidden)]
    fn into_inner(self) -> Self::Word;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! bit_word {
    ($($w:ty),*) => {$(
        impl sealed::Sealed for $w {}
        impl CBitWord for $w {
            const BITS: u32 = <$w>::BITS;
            #[inline(always)]
            fn to_u64(self) -> u64 {
                self as u64
            }
            #[inline(always)]
            fn from_u64(v: u64) -> Self {
                v as $w
            }
        }
    )*};
}
bit_word!(u8, u16, u32, u64);

macro_rules! atomic_bit_word {
    ($($w:ty => $a:ty),*) => {$(
        impl sealed::Sealed for $a {}
        impl CAtomicBitWord for $w {
            type Atomic = $a;
        }
        // The property the trait states, checked for every target the impl
        // exists on: a target where it fails does not build ffibox.
        const _: () = assert!(
            core::mem::align_of::<$a>() == core::mem::align_of::<$w>()
                && core::mem::size_of::<$a>() == core::mem::size_of::<$w>()
                && core::mem::align_of::<CAtomicBitfield<$w>>() == core::mem::align_of::<$w>()
                && core::mem::size_of::<CAtomicBitfield<$w>>() == core::mem::size_of::<$w>()
        );
        impl CAtomicWord for $a {
            type Word = $w;
            #[inline(always)]
            fn new(v: $w) -> Self {
                <$a>::new(v)
            }
            #[inline(always)]
            fn load(&self, order: Ordering) -> $w {
                <$a>::load(self, order)
            }
            #[inline(always)]
            fn fetch_or(&self, v: $w, order: Ordering) -> $w {
                <$a>::fetch_or(self, v, order)
            }
            #[inline(always)]
            fn fetch_and(&self, v: $w, order: Ordering) -> $w {
                <$a>::fetch_and(self, v, order)
            }
            #[inline(always)]
            fn compare_exchange_weak(
                &self,
                current: $w,
                new: $w,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$w, $w> {
                <$a>::compare_exchange_weak(self, current, new, success, failure)
            }
            #[inline(always)]
            fn into_inner(self) -> $w {
                <$a>::into_inner(self)
            }
        }
    )*};
}
#[cfg(target_has_atomic = "8")]
atomic_bit_word!(u8 => AtomicU8);
#[cfg(target_has_atomic = "16")]
atomic_bit_word!(u16 => AtomicU16);
#[cfg(target_has_atomic = "32")]
atomic_bit_word!(u32 => AtomicU32);
// `u64` is 4-aligned in C and Rust on 32-bit x86 (and 2-aligned on m68k),
// but `AtomicU64` is 8-aligned.
#[cfg(all(
    target_has_atomic = "64",
    not(any(target_arch = "x86", target_arch = "m68k"))
))]
atomic_bit_word!(u64 => AtomicU64);

/// One field of a C bitfield word `W`: its position and width, which
/// [`first`](Self::first) and [`next`](Self::next) compute from the C
/// declaration, or [`at`](Self::at) states.
///
/// A field that does not fit the word fails to build:
///
/// ```compile_fail,E0080
/// use ffibox::bitfield::CBits;
/// const A: CBits<u8> = CBits::first(5);
/// const B: CBits<u8> = A.next(4); // 5 + 4 bits in an 8-bit unit
/// ```
///
/// nor does a field of no bits:
///
/// ```compile_fail,E0080
/// use ffibox::bitfield::CBits;
/// const NONE: CBits<u32> = CBits::first(0);
/// ```
///
/// and a field reads and writes only the word it was declared for:
///
/// ```compile_fail,E0308
/// use ffibox::bitfield::{CBitfield, CBits};
/// const DIRTY: CBits<u16> = CBits::first(1);
/// let w = CBitfield::<u32>::new(0);
/// let _ = w.is_set(DIRTY);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CBits<W> {
    /// The least significant bit, counted from bit 0.
    lsb: u32,
    width: u32,
    _word: PhantomData<fn() -> W>,
}

impl<W: CBitWord> CBits<W> {
    /// The field of `width` bits at bit `lsb`, for a layout the declaration
    /// order does not give (a field after a `:0`, a documented position).
    ///
    /// # Panics
    ///
    /// If the width is 0 or the field does not fit `W`; at compile time in
    /// a `const`.
    #[must_use]
    pub const fn at(lsb: u32, width: u32) -> Self {
        assert!(
            width > 0 && width <= W::BITS && lsb <= W::BITS - width,
            "ffibox: the bitfield does not fit its word"
        );
        Self {
            lsb,
            width,
            _word: PhantomData,
        }
    }

    /// The first field declared in the word, `width` bits wide.
    ///
    /// # Panics
    ///
    /// As [`at`](Self::at).
    #[must_use]
    pub const fn first(width: u32) -> Self {
        if cfg!(target_endian = "little") {
            Self::at(0, width)
        } else {
            assert!(
                width > 0 && width <= W::BITS,
                "ffibox: the bitfield does not fit its word"
            );
            Self::at(W::BITS - width, width)
        }
    }

    /// The field declared right after `self` in the same word, `width` bits
    /// wide.
    ///
    /// # Panics
    ///
    /// As [`at`](Self::at): the fields so far and this one exceed the word.
    #[must_use]
    pub const fn next(self, width: u32) -> Self {
        if cfg!(target_endian = "little") {
            Self::at(self.lsb + self.width, width)
        } else {
            assert!(
                width > 0 && width <= self.lsb,
                "ffibox: the bitfield does not fit its word"
            );
            Self::at(self.lsb - width, width)
        }
    }

    /// Its least significant bit.
    #[inline]
    #[must_use]
    pub const fn lsb(self) -> u32 {
        self.lsb
    }

    /// Its width in bits.
    #[inline]
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Its bits in the word, set.
    #[inline]
    #[must_use]
    pub fn mask(self) -> W {
        W::from_u64(self.low_mask() << self.lsb)
    }

    /// A value of the field's width, unshifted.
    #[inline]
    fn low_mask(self) -> u64 {
        u64::MAX >> (64 - self.width)
    }

    /// The field's value in `word`.
    #[inline]
    fn extract(self, word: W) -> W {
        W::from_u64((word.to_u64() >> self.lsb) & self.low_mask())
    }

    /// `word` with the field set to `value`, truncated to its width.
    #[inline]
    fn insert(self, word: W, value: W) -> W {
        let m = self.low_mask() << self.lsb;
        W::from_u64((word.to_u64() & !m) | ((value.to_u64() << self.lsb) & m))
    }
}

impl<W: CBitWord> fmt::Debug for CBits<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CBits")
            .field("lsb", &self.lsb)
            .field("width", &self.width)
            .finish()
    }
}

/// A C bitfield word, read and written by field: the integer `W`, with the
/// shifts and masks its [`CBits`] fields name. `#[repr(transparent)]` over
/// `W`, so it stands in for the storage unit in a `#[repr(C)]` mirror.
///
/// Its counterpart is the integer itself; the field methods are the
/// `(w >> lsb) & mask` a C compiler emits.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CBitfield<W>(W);

impl<W: CBitWord> CBitfield<W> {
    /// The word `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: W) -> Self {
        Self(raw)
    }

    /// The whole word.
    #[inline]
    #[must_use]
    pub const fn into_inner(self) -> W {
        self.0
    }

    /// The field's value.
    #[inline]
    #[must_use]
    pub fn get(&self, field: CBits<W>) -> W {
        field.extract(self.0)
    }

    /// Set the field to `value`, truncated to its width as C's assignment
    /// truncates; the other fields are unchanged.
    #[inline]
    pub fn set(&mut self, field: CBits<W>, value: W) {
        self.0 = field.insert(self.0, value);
    }

    /// Whether any of the field's bits is set: a one-bit field as a `bool`.
    #[inline]
    #[must_use]
    pub fn is_set(&self, field: CBits<W>) -> bool {
        self.get(field) != W::from_u64(0)
    }

    /// Set every bit of the field, or clear them.
    #[inline]
    pub fn set_flag(&mut self, field: CBits<W>, on: bool) {
        self.set(field, W::from_u64(if on { u64::MAX } else { 0 }));
    }
}

impl<W: CBitWord> fmt::Debug for CBitfield<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CBitfield({:#x})", self.0.to_u64())
    }
}

/// A C bitfield word that several threads update: the atomic of `W`, read
/// and written by field, each change one atomic read-modify-write of the
/// word. `#[repr(transparent)]` over the atomic, which has `W`'s size and
/// alignment. `W` is a [`CAtomicBitWord`]: `u64` is not one on i686, where
/// C aligns a `long long` member to 4 and `AtomicU64` is 8-aligned, so
/// `CAtomicBitfield<u64>` is not a type there, and a mirror naming it fails
/// to build (E0277), whether or not anything calls `new`.
///
/// Its counterparts are `AtomicU32::fetch_or`, `fetch_and`, `store` and
/// `swap`, over the field's bits. The orderings are std's, but for
/// [`set_flag`](Self::set_flag)'s shortcut, which it documents.
///
/// ```
/// use core::sync::atomic::Ordering::Relaxed;
/// use ffibox::bitfield::{CAtomicBitfield, CBits};
///
/// const ON_DISK: CBits<u32> = CBits::first(1);
/// const IGNORE_CASE: CBits<u32> = ON_DISK.next(1);
/// const DIRTY: CBits<u32> = IGNORE_CASE.next(1);
///
/// let bits = CAtomicBitfield::<u32>::new(0);
/// std::thread::scope(|s| {
///     s.spawn(|| bits.set_flag(IGNORE_CASE, true, Relaxed));
///     s.spawn(|| bits.set_flag(DIRTY, true, Relaxed));
/// });
/// // Neither thread lost the other's bit.
/// assert!(bits.is_set(IGNORE_CASE, Relaxed) && bits.is_set(DIRTY, Relaxed));
/// assert!(!bits.is_set(ON_DISK, Relaxed));
/// ```
#[repr(transparent)]
pub struct CAtomicBitfield<W: CAtomicBitWord>(W::Atomic);

impl<W: CAtomicBitWord> CAtomicBitfield<W> {
    /// The word `raw`.
    #[inline]
    #[must_use]
    pub fn new(raw: W) -> Self {
        Self(W::Atomic::new(raw))
    }

    /// The whole word.
    #[inline]
    #[must_use]
    pub fn load_word(&self, order: Ordering) -> W {
        self.0.load(order)
    }

    /// The whole word, taking the field apart.
    #[inline]
    #[must_use]
    pub fn into_inner(self) -> W {
        self.0.into_inner()
    }

    /// The field's value.
    #[inline]
    #[must_use]
    pub fn load(&self, field: CBits<W>, order: Ordering) -> W {
        field.extract(self.0.load(order))
    }

    /// Whether any of the field's bits is set.
    #[inline]
    #[must_use]
    pub fn is_set(&self, field: CBits<W>, order: Ordering) -> bool {
        self.load(field, order) != W::from_u64(0)
    }

    /// Set the field to `value`, truncated to its width, in one atomic
    /// read-modify-write that keeps the other fields.
    #[inline]
    pub fn store(&self, field: CBits<W>, value: W, order: Ordering) {
        let _ = self.swap(field, value, order);
    }

    /// As [`store`](Self::store), returning the field's previous value, as
    /// `AtomicU32::swap`. `order` is the read-modify-write's; its loads are
    /// `order` without its release part.
    pub fn swap(&self, field: CBits<W>, value: W, order: Ordering) -> W {
        let fetch = load_order(order);
        let mut cur = self.0.load(fetch);
        loop {
            match self
                .0
                .compare_exchange_weak(cur, field.insert(cur, value), order, fetch)
            {
                Ok(prev) => return field.extract(prev),
                Err(now) => cur = now,
            }
        }
    }

    /// Set every bit of the field, or clear them, with one `fetch_or` or
    /// `fetch_and`; whether any was set before.
    ///
    /// With an `order` that has no release part (`Relaxed`, `Acquire`), a
    /// field already as asked is not written at all, so that a read path C
    /// runs on several threads, setting a flag that is already set, leaves
    /// the word alone; the load then has `order`. With `Release`, `AcqRel` or
    /// `SeqCst` the read-modify-write always happens, so the release it
    /// promises is made even when the flag does not change.
    pub fn set_flag(&self, field: CBits<W>, on: bool, order: Ordering) -> bool {
        let releases = matches!(
            order,
            Ordering::Release | Ordering::AcqRel | Ordering::SeqCst
        );
        let cur = self.0.load(load_order(order));
        let mask = field.mask();
        let full = field.extract(cur) == W::from_u64(field.low_mask());
        let clear = field.extract(cur) == W::from_u64(0);
        if !releases && ((on && full) || (!on && clear)) {
            return !clear;
        }
        let prev = if on {
            self.0.fetch_or(mask, order)
        } else {
            self.0.fetch_and(
                W::from_u64(!mask.to_u64() & (u64::MAX >> (64 - W::BITS))),
                order,
            )
        };
        field.extract(prev) != W::from_u64(0)
    }
}

/// The load half of a read-modify-write's `order`.
#[inline]
fn load_order(order: Ordering) -> Ordering {
    match order {
        Ordering::Release => Ordering::Relaxed,
        Ordering::AcqRel => Ordering::Acquire,
        o => o,
    }
}

impl<W: CAtomicBitWord> Default for CAtomicBitfield<W> {
    #[inline]
    fn default() -> Self {
        Self::new(W::from_u64(0))
    }
}

impl<W: CAtomicBitWord> fmt::Debug for CAtomicBitfield<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CAtomicBitfield({:#x})",
            self.0.load(Ordering::Relaxed).to_u64()
        )
    }
}

const _: () = {
    use core::mem::{align_of, size_of};
    macro_rules! same_layout {
        ($($w:ty),*) => {$(
            assert!(size_of::<CBitfield<$w>>() == size_of::<$w>());
            assert!(align_of::<CBitfield<$w>>() == align_of::<$w>());
        )*};
    }
    same_layout!(u8, u16, u32, u64);
};
