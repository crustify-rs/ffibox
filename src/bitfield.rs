//! C bitfields: [`CBits<W>`] names a field of a bitfield word `W` once, in
//! C's declaration order, and [`CBitfield<W>`] / [`CAtomicBitfield<W>`] are
//! the word, read and written by field.
//!
//! ```c
//! struct node {
//!     unsigned short seen:1, uninteresting:1, flags:4;
//!     unsigned short in_degree;
//! };
//! ```
//!
//! Rust has no bitfields, so a mirror holds the storage unit as an integer
//! and reads each field through a shift and a mask. Which bits a field has
//! is the ABI's: on the System V and AAPCS ABIs, the first field declared
//! takes the least significant bits on a little-endian target and the most
//! significant on a big-endian one. [`CBits::first`] and [`CBits::next`]
//! follow the declaration and compute the bits from the target, so the
//! binding writes the C's order and widths, not masks:
//!
//! ```
//! use ffibox::bitfield::{CBitfield, CBits};
//!
//! #[repr(C)]
//! pub struct Node {
//!     pub bits: CBitfield<u16>,
//!     pub in_degree: u16,
//! }
//! pub const SEEN: CBits<u16> = CBits::first(1);
//! pub const UNINTERESTING: CBits<u16> = SEEN.next(1);
//! pub const FLAGS: CBits<u16> = UNINTERESTING.next(4);
//!
//! let mut n = Node { bits: CBitfield::new(0), in_degree: 0 };
//! n.bits.set_flag(UNINTERESTING, true);
//! n.bits.set(FLAGS, 0b1_0110);           // truncated to 4 bits, as C does
//! assert!(!n.bits.is_set(SEEN));
//! assert!(n.bits.is_set(UNINTERESTING));
//! assert_eq!(n.bits.get(FLAGS), 0b0110);
//! # #[cfg(target_endian = "little")]
//! assert_eq!(n.bits.into_inner(), 0b0110_10);
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
//! `int` bitfields are signed or not as the implementation chooses. The
//! storage unit `W` is the type the fields are declared with, as a
//! layout test against the C compiler confirms; a field does not straddle
//! two units.

use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// An unsigned integer that holds C bitfields: `u8`, `u16`, `u32`, `u64`.
/// Sealed.
pub trait CBitWord: Copy + Eq + fmt::Debug + sealed::Sealed {
    /// Its width in bits.
    const BITS: u32;
    /// The atomic of the same width, for [`CAtomicBitfield`].
    type Atomic: CAtomicWord<Word = Self>;
    #[doc(hidden)]
    fn to_u64(self) -> u64;
    #[doc(hidden)]
    fn from_u64(v: u64) -> Self;
}

/// The atomic of a [`CBitWord`]. Sealed.
pub trait CAtomicWord: sealed::Sealed {
    /// The word it holds.
    type Word: CBitWord;
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
    ($($w:ty => $a:ty),*) => {$(
        impl sealed::Sealed for $w {}
        impl sealed::Sealed for $a {}
        impl CBitWord for $w {
            const BITS: u32 = <$w>::BITS;
            type Atomic = $a;
            #[inline(always)]
            fn to_u64(self) -> u64 {
                self as u64
            }
            #[inline(always)]
            fn from_u64(v: u64) -> Self {
                v as $w
            }
        }
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
bit_word!(u8 => AtomicU8, u16 => AtomicU16, u32 => AtomicU32, u64 => AtomicU64);

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
/// word. `#[repr(transparent)]` over the atomic, which has `W`'s size, and
/// its alignment but for `u64` on a target where `u64` is less aligned than
/// its atomic (i686): there the field's offset is checked against C's.
///
/// Its counterparts are `AtomicU32::fetch_or`, `fetch_and` and
/// `fetch_update`, over the field's bits. The orderings are std's.
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
pub struct CAtomicBitfield<W: CBitWord>(W::Atomic);

impl<W: CBitWord> CAtomicBitfield<W> {
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
    /// read-modify-write that keeps the other fields; the field's previous
    /// value. `order` is the read-modify-write's, as for `fetch_update`'s
    /// `set_order`; its loads are `order` without its release part.
    pub fn store(&self, field: CBits<W>, value: W, order: Ordering) -> W {
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
    /// `fetch_and`; whether any was set before. A field already so is not
    /// written at all, so that a read path that C runs on several threads
    /// and that sets a flag already set leaves the word alone.
    pub fn set_flag(&self, field: CBits<W>, on: bool, order: Ordering) -> bool {
        let cur = self.0.load(load_order(order));
        let mask = field.mask();
        let full = field.extract(cur) == W::from_u64(field.low_mask());
        let clear = field.extract(cur) == W::from_u64(0);
        if (on && full) || (!on && clear) {
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

impl<W: CBitWord> Default for CAtomicBitfield<W> {
    #[inline]
    fn default() -> Self {
        Self::new(W::from_u64(0))
    }
}

impl<W: CBitWord> fmt::Debug for CAtomicBitfield<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CAtomicBitfield({:#x})",
            self.0.load(Ordering::Relaxed).to_u64()
        )
    }
}

const _: () = {
    assert!(core::mem::size_of::<CBitfield<u16>>() == 2);
    assert!(core::mem::size_of::<CAtomicBitfield<u32>>() == 4);
};
