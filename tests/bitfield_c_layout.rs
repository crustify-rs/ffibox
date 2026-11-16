//! C bitfield layouts, from the C compiler, against the mirrors the
//! `bitfield` module's rules give.
//!
//! `tests/data/bitfield_layouts.txt` is clang's layout of each struct below,
//! for every target the module covers: `sizeof`, `_Alignof`, the `offsetof`
//! of the members after the bitfields, and each bitfield's byte image when it
//! alone is all ones. [`the_table_is_what_clang_computes`] rebuilds it with
//! clang and `llvm-objdump` (cross-compiling to an object file, no linking)
//! and checks it is unchanged; `FFIBOX_BLESS=1` rewrites it. Without clang
//! or `llvm-objdump` it passes with a note on stderr, **unless
//! `FFIBOX_REQUIRE_CLANG=1` is set, when it fails**: set it wherever the
//! test matrix runs (README, "Testing").
//!
//! [`mirrors_match_the_c_layout`] builds each struct's Rust mirror, sets each
//! field through its `CBits`, and compares with the table's row for the
//! target it runs on. It runs natively, and under Miri for the other targets
//! (`cargo +nightly miri test --target s390x-unknown-linux-gnu --test
//! bitfield_c_layout`, likewise `i686-unknown-linux-gnu`,
//! `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`).

#![allow(missing_docs, dead_code)]

use core::mem::{MaybeUninit, align_of, offset_of, size_of};

use ffibox::bitfield::{CAtomicBitfield, CBitWord, CBitfield, CBits};

/// The structs: name, C body, bitfields, members whose offset is checked.
const STRUCTS: &[(&str, &str, &[&str], &[&str])] = &[
    (
        "S1",
        "unsigned int seen:1, unint:1, topo:1, parsed:1, added:1, flags:4; \
         unsigned short in_degree; unsigned short out_degree;",
        &["seen", "flags"],
        &["in_degree"],
    ),
    ("S2", "unsigned int a:4; unsigned char c;", &["a"], &["c"]),
    ("S3", "unsigned int a:20, b:20;", &["a", "b"], &[]),
    (
        "S4",
        "unsigned long long a:30, b:10, c:24;",
        &["a", "b", "c"],
        &[],
    ),
    (
        "S5",
        "unsigned char a:4; unsigned short b:8; unsigned int c:12;",
        &["a", "b", "c"],
        &[],
    ),
    (
        "S6",
        "unsigned char a:3, b:5; unsigned char d;",
        &["a", "b"],
        &["d"],
    ),
    ("S8", "unsigned short a:7, b:9;", &["a", "b"], &[]),
    ("S9", "unsigned int x; unsigned long long a:1;", &["a"], &[]),
    (
        "S10",
        "unsigned char c; unsigned int a:4, b:24;",
        &["a", "b"],
        &[],
    ),
    (
        "S11",
        "unsigned int a:3; unsigned int :0; unsigned int b:3;",
        &["a", "b"],
        &[],
    ),
    (
        "S12",
        "unsigned short a:12; unsigned short b:8;",
        &["a", "b"],
        &[],
    ),
    (
        "T1",
        "unsigned int x; unsigned char c; unsigned long long a:40;",
        &["a"],
        &["c"],
    ),
    (
        "T2",
        "unsigned char c[3]; unsigned long long a:40;",
        &["a"],
        &[],
    ),
    (
        "S13",
        "int oid_type; unsigned int on_disk:1, ignore_case:1, distrust:1, \
         no_symlinks:1, dirty:1; void *tree;",
        &["on_disk", "dirty"],
        &["tree"],
    ),
];

/// The targets, by clang triple.
const TARGETS: &[&str] = &[
    "x86_64-linux-gnu",
    "aarch64-linux-gnu",
    "armv7-linux-gnueabihf",
    "i686-linux-gnu",
    "s390x-linux-gnu",
    "powerpc64-linux-gnu",
    "x86_64-pc-windows-msvc",
];

const TABLE: &str = include_str!("data/bitfield_layouts.txt");

/// The table's rows for `target`: `key value`.
fn rows(target: &str) -> std::collections::BTreeMap<String, String> {
    TABLE
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let (t, k, v) = (it.next()?, it.next()?, it.next()?);
            (t == target).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The C oracle
// ---------------------------------------------------------------------------

/// The C source: one section per measurement, so that the object file
/// carries each value as a section size (`sizeof`, `_Alignof`, `offsetof`
/// + 1) or contents (a struct with one field all ones).
fn c_source() -> String {
    let mut out = String::new();
    for (s, body, fields, members) in STRUCTS {
        out += &format!("struct {s} {{ {body} }};\n");
        let sec = |name: &str, decl: &str| {
            format!("__attribute__((section(\"z{name}\"), used)) {decl};\n")
        };
        out += &sec(
            &format!("sz_{s}"),
            &format!("unsigned char sz_{s}[sizeof(struct {s})] = {{1}}"),
        );
        out += &sec(
            &format!("al_{s}"),
            &format!("unsigned char al_{s}[_Alignof(struct {s})] = {{1}}"),
        );
        for m in *members {
            out += &sec(
                &format!("of_{s}_{m}"),
                &format!(
                    "unsigned char of_{s}_{m}[__builtin_offsetof(struct {s}, {m}) + 1] = {{1}}"
                ),
            );
        }
        for f in *fields {
            out += &sec(
                &format!("bf_{s}_{f}"),
                &format!("struct {s} bf_{s}_{f} = {{ .{f} = -1 }}"),
            );
        }
    }
    out
}

/// The tool `name`, if it runs.
fn tool(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| {
        std::process::Command::new(n)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| (*n).to_owned())
    })
}

/// clang's table, for every target.
fn clang_table(clang: &str, objdump: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ffibox-bitfields-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("layouts.c");
    std::fs::write(&src, c_source()).unwrap();
    let mut table = String::new();
    for target in TARGETS {
        let obj = dir.join(format!("{target}.o"));
        let st = std::process::Command::new(clang)
            .args(["-c", "-O0", "-w"])
            .arg(format!("--target={target}"))
            .arg(&src)
            .arg("-o")
            .arg(&obj)
            .status()
            .unwrap();
        assert!(st.success(), "clang --target={target}");
        let run = |flag: &str| {
            let o = std::process::Command::new(objdump)
                .arg(flag)
                .arg(&obj)
                .output()
                .unwrap();
            assert!(o.status.success());
            String::from_utf8(o.stdout).unwrap()
        };
        // Section sizes.
        let mut sizes = std::collections::BTreeMap::new();
        for line in run("-h").lines() {
            let p: Vec<_> = line.split_whitespace().collect();
            if p.len() >= 3 && p[1].starts_with('z') {
                sizes.insert(
                    p[1][1..].to_owned(),
                    usize::from_str_radix(p[2], 16).unwrap(),
                );
            }
        }
        // Section contents.
        let mut data = std::collections::BTreeMap::<String, String>::new();
        let mut cur = None;
        for line in run("-s").lines() {
            if let Some(rest) = line.strip_prefix("Contents of section ") {
                cur = rest
                    .trim_end_matches(':')
                    .strip_prefix('z')
                    .map(str::to_owned);
                continue;
            }
            if let (Some(c), Some(body)) = (&cur, line.strip_prefix(' ')) {
                // ` 0000 01020304 05060708  ........`
                let hex: String = body
                    .split("  ")
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .skip(1)
                    .collect();
                *data.entry(c.clone()).or_default() += &hex;
            }
        }
        for (k, n) in &sizes {
            let line = if let Some(f) = k.strip_prefix("bf_") {
                format!("{target} {f} {}\n", &data[k][..n * 2])
            } else if k.starts_with("of_") {
                format!("{target} {k} {}\n", n - 1)
            } else {
                format!("{target} {k} {n}\n")
            };
            table += &line;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    table
}

#[test]
#[cfg_attr(miri, ignore = "runs clang")]
fn the_table_is_what_clang_computes() {
    let (Some(clang), Some(objdump)) = (
        tool(&["clang", "clang-18"]),
        tool(&["llvm-objdump", "llvm-objdump-18"]),
    ) else {
        assert!(
            std::env::var_os("FFIBOX_REQUIRE_CLANG").is_none(),
            "clang and llvm-objdump are required"
        );
        eprintln!("skipped: clang or llvm-objdump not found");
        return;
    };
    let table = clang_table(&clang, &objdump);
    if std::env::var_os("FFIBOX_BLESS").is_some() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/bitfield_layouts.txt"
        );
        std::fs::write(path, &table).unwrap();
        return;
    }
    assert_eq!(
        table, TABLE,
        "clang's layouts changed: rerun with FFIBOX_BLESS=1 and review the diff"
    );
}

// ---------------------------------------------------------------------------
// The mirrors
// ---------------------------------------------------------------------------

/// The byte image of a zeroed `S` after `set` writes into it, padding
/// included: the bytes are zeroed in place, on the heap, and never copied
/// (a by-value `MaybeUninit<S>` would not keep its padding bytes).
fn image<S>(set: impl FnOnce(*mut S)) -> String {
    let mut m: Box<MaybeUninit<S>> = Box::new_zeroed();
    set(m.as_mut_ptr());
    // SAFETY: every byte is initialised: zeroed, then written by `set`.
    let bytes = unsafe { core::slice::from_raw_parts(m.as_ptr().cast::<u8>(), size_of::<S>()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Set every bit of `f` in the plain unit at `u`.
fn ones<W: CBitWord>(u: *mut CBitfield<W>, f: CBits<W>) {
    // SAFETY: `u` is a field of a live, zeroed mirror, which nothing else
    // reaches.
    unsafe { (*u).set_flag(f, true) }
}

/// The field `$f` of the mirror at `$p`, as a raw pointer.
macro_rules! fld {
    ($p:ident, $f:ident) => {
        // SAFETY: `$p` is the zeroed mirror `image` passes; the projection
        // stays in it and forms no reference.
        unsafe { &raw mut (*$p).$f }
    };
}

/// What the mirrors give: `key value`, as the table.
type Rows = Vec<(String, String)>;

fn push(r: &mut Rows, k: &str, v: impl ToString) {
    r.push((k.to_owned(), v.to_string()));
}

/// The mirrors for the System V and AAPCS ABIs, little- or big-endian: each
/// `W` covers the bytes its run occupies.
#[cfg(not(target_env = "msvc"))]
fn mirrors() -> Rows {
    let mut r = Rows::new();

    // S1: 9 bits in 2 bytes; `in_degree` at 2; the `unsigned`'s alignment.
    #[repr(C, align(4))]
    struct S1 {
        u: CBitfield<u16>,
        in_degree: u16,
        out_degree: u16,
    }
    const SEEN: CBits<u16> = CBits::first(1);
    const FLAGS: CBits<u16> = SEEN.next(1).next(1).next(1).next(1).next(4);
    push(&mut r, "sz_S1", size_of::<S1>());
    push(&mut r, "al_S1", align_of::<S1>());
    push(&mut r, "of_S1_in_degree", offset_of!(S1, in_degree));
    push(&mut r, "S1_seen", image::<S1>(|p| ones(fld!(p, u), SEEN)));
    push(&mut r, "S1_flags", image::<S1>(|p| ones(fld!(p, u), FLAGS)));

    // S2: `c` inside the `unsigned`'s size.
    #[repr(C, align(4))]
    struct S2 {
        u: CBitfield<u8>,
        c: u8,
    }
    push(&mut r, "sz_S2", size_of::<S2>());
    push(&mut r, "al_S2", align_of::<S2>());
    push(&mut r, "of_S2_c", offset_of!(S2, c));
    push(
        &mut r,
        "S2_a",
        image::<S2>(|p| ones(fld!(p, u), CBits::first(4))),
    );

    // S3: `b` would cross 32 bits, so it starts the next unit.
    #[repr(C)]
    struct S3 {
        u0: CBitfield<u32>,
        u1: CBitfield<u32>,
    }
    push(&mut r, "sz_S3", size_of::<S3>());
    push(&mut r, "al_S3", align_of::<S3>());
    push(
        &mut r,
        "S3_a",
        image::<S3>(|p| ones(fld!(p, u0), CBits::first(20))),
    );
    push(
        &mut r,
        "S3_b",
        image::<S3>(|p| ones(fld!(p, u1), CBits::first(20))),
    );

    // S4: one 64-bit run crossing bit 32.
    #[repr(C)]
    struct S4 {
        u: CBitfield<u64>,
    }
    const S4A: CBits<u64> = CBits::first(30);
    const S4B: CBits<u64> = S4A.next(10);
    const S4C: CBits<u64> = S4B.next(24);
    push(&mut r, "sz_S4", size_of::<S4>());
    push(&mut r, "al_S4", align_of::<S4>());
    for (k, f) in [("S4_a", S4A), ("S4_b", S4B), ("S4_c", S4C)] {
        push(&mut r, k, image::<S4>(|p| ones(fld!(p, u), f)));
    }

    // S5: three declared types packed into one run of 24 bits.
    #[repr(C)]
    struct S5 {
        u: CBitfield<u32>,
    }
    const S5A: CBits<u32> = CBits::first(4);
    const S5B: CBits<u32> = S5A.next(8);
    const S5C: CBits<u32> = S5B.next(12);
    push(&mut r, "sz_S5", size_of::<S5>());
    push(&mut r, "al_S5", align_of::<S5>());
    for (k, f) in [("S5_a", S5A), ("S5_b", S5B), ("S5_c", S5C)] {
        push(&mut r, k, image::<S5>(|p| ones(fld!(p, u), f)));
    }

    // S6.
    #[repr(C)]
    struct S6 {
        u: CBitfield<u8>,
        d: u8,
    }
    const S6A: CBits<u8> = CBits::first(3);
    push(&mut r, "sz_S6", size_of::<S6>());
    push(&mut r, "al_S6", align_of::<S6>());
    push(&mut r, "of_S6_d", offset_of!(S6, d));
    push(&mut r, "S6_a", image::<S6>(|p| ones(fld!(p, u), S6A)));
    push(
        &mut r,
        "S6_b",
        image::<S6>(|p| ones(fld!(p, u), S6A.next(5))),
    );

    // S8.
    #[repr(C)]
    struct S8 {
        u: CBitfield<u16>,
    }
    const S8A: CBits<u16> = CBits::first(7);
    push(&mut r, "sz_S8", size_of::<S8>());
    push(&mut r, "al_S8", align_of::<S8>());
    push(&mut r, "S8_a", image::<S8>(|p| ones(fld!(p, u), S8A)));
    push(
        &mut r,
        "S8_b",
        image::<S8>(|p| ones(fld!(p, u), S8A.next(9))),
    );

    // S9: `a` sits in the `long long` unit that holds `x`, at bit 32: byte
    // 4, and the struct is 8 bytes, aligned as a `long long` member is (8,
    // but 4 on i686).
    #[cfg_attr(not(target_arch = "x86"), repr(C, align(8)))]
    #[cfg_attr(target_arch = "x86", repr(C))]
    struct S9 {
        x: u32,
        u: CBitfield<u32>,
    }
    push(&mut r, "sz_S9", size_of::<S9>());
    push(&mut r, "al_S9", align_of::<S9>());
    push(
        &mut r,
        "S9_a",
        image::<S9>(|p| ones(fld!(p, u), CBits::first(1))),
    );

    // S10: `a` starts in byte 1, inside `c`'s unit; `b` would cross 32 bits.
    #[repr(C, align(4))]
    struct S10 {
        c: u8,
        ua: CBitfield<u8>,
        ub: CBitfield<u32>,
    }
    push(&mut r, "sz_S10", size_of::<S10>());
    push(&mut r, "al_S10", align_of::<S10>());
    push(
        &mut r,
        "S10_a",
        image::<S10>(|p| ones(fld!(p, ua), CBits::first(4))),
    );
    push(
        &mut r,
        "S10_b",
        image::<S10>(|p| ones(fld!(p, ub), CBits::first(24))),
    );

    // S11: `:0` moves on to the next unit.
    #[repr(C)]
    struct S11 {
        u0: CBitfield<u32>,
        u1: CBitfield<u32>,
    }
    push(&mut r, "sz_S11", size_of::<S11>());
    push(&mut r, "al_S11", align_of::<S11>());
    push(
        &mut r,
        "S11_a",
        image::<S11>(|p| ones(fld!(p, u0), CBits::first(3))),
    );
    push(
        &mut r,
        "S11_b",
        image::<S11>(|p| ones(fld!(p, u1), CBits::first(3))),
    );

    // S12: `b` would cross 16 bits.
    #[repr(C)]
    struct S12 {
        u0: CBitfield<u16>,
        u1: CBitfield<u16>,
    }
    push(&mut r, "sz_S12", size_of::<S12>());
    push(&mut r, "al_S12", align_of::<S12>());
    push(
        &mut r,
        "S12_a",
        image::<S12>(|p| ones(fld!(p, u0), CBits::first(12))),
    );
    push(
        &mut r,
        "S12_b",
        image::<S12>(|p| ones(fld!(p, u1), CBits::first(8))),
    );

    t_runs(&mut r);
    s13(&mut r);
    r
}

/// The mirrors for Microsoft's ABI: each run of one declared type is a unit
/// of that type.
#[cfg(target_env = "msvc")]
fn mirrors() -> Rows {
    let mut r = Rows::new();

    #[repr(C)]
    struct S1 {
        u: CBitfield<u32>,
        in_degree: u16,
        out_degree: u16,
    }
    const SEEN: CBits<u32> = CBits::first(1);
    const FLAGS: CBits<u32> = SEEN.next(1).next(1).next(1).next(1).next(4);
    push(&mut r, "sz_S1", size_of::<S1>());
    push(&mut r, "al_S1", align_of::<S1>());
    push(&mut r, "of_S1_in_degree", offset_of!(S1, in_degree));
    push(&mut r, "S1_seen", image::<S1>(|p| ones(fld!(p, u), SEEN)));
    push(&mut r, "S1_flags", image::<S1>(|p| ones(fld!(p, u), FLAGS)));

    #[repr(C)]
    struct S2 {
        u: CBitfield<u32>,
        c: u8,
    }
    push(&mut r, "sz_S2", size_of::<S2>());
    push(&mut r, "al_S2", align_of::<S2>());
    push(&mut r, "of_S2_c", offset_of!(S2, c));
    push(
        &mut r,
        "S2_a",
        image::<S2>(|p| ones(fld!(p, u), CBits::first(4))),
    );

    #[repr(C)]
    struct S3 {
        u0: CBitfield<u32>,
        u1: CBitfield<u32>,
    }
    push(&mut r, "sz_S3", size_of::<S3>());
    push(&mut r, "al_S3", align_of::<S3>());
    push(
        &mut r,
        "S3_a",
        image::<S3>(|p| ones(fld!(p, u0), CBits::first(20))),
    );
    push(
        &mut r,
        "S3_b",
        image::<S3>(|p| ones(fld!(p, u1), CBits::first(20))),
    );

    #[repr(C)]
    struct S4 {
        u: CBitfield<u64>,
    }
    const S4A: CBits<u64> = CBits::first(30);
    const S4B: CBits<u64> = S4A.next(10);
    const S4C: CBits<u64> = S4B.next(24);
    push(&mut r, "sz_S4", size_of::<S4>());
    push(&mut r, "al_S4", align_of::<S4>());
    for (k, f) in [("S4_a", S4A), ("S4_b", S4B), ("S4_c", S4C)] {
        push(&mut r, k, image::<S4>(|p| ones(fld!(p, u), f)));
    }

    // S5: a unit per declared type.
    #[repr(C)]
    struct S5 {
        a: CBitfield<u8>,
        b: CBitfield<u16>,
        c: CBitfield<u32>,
    }
    push(&mut r, "sz_S5", size_of::<S5>());
    push(&mut r, "al_S5", align_of::<S5>());
    push(
        &mut r,
        "S5_a",
        image::<S5>(|p| ones(fld!(p, a), CBits::first(4))),
    );
    push(
        &mut r,
        "S5_b",
        image::<S5>(|p| ones(fld!(p, b), CBits::first(8))),
    );
    push(
        &mut r,
        "S5_c",
        image::<S5>(|p| ones(fld!(p, c), CBits::first(12))),
    );

    #[repr(C)]
    struct S6 {
        u: CBitfield<u8>,
        d: u8,
    }
    const S6A: CBits<u8> = CBits::first(3);
    push(&mut r, "sz_S6", size_of::<S6>());
    push(&mut r, "al_S6", align_of::<S6>());
    push(&mut r, "of_S6_d", offset_of!(S6, d));
    push(&mut r, "S6_a", image::<S6>(|p| ones(fld!(p, u), S6A)));
    push(
        &mut r,
        "S6_b",
        image::<S6>(|p| ones(fld!(p, u), S6A.next(5))),
    );

    #[repr(C)]
    struct S8 {
        u: CBitfield<u16>,
    }
    const S8A: CBits<u16> = CBits::first(7);
    push(&mut r, "sz_S8", size_of::<S8>());
    push(&mut r, "al_S8", align_of::<S8>());
    push(&mut r, "S8_a", image::<S8>(|p| ones(fld!(p, u), S8A)));
    push(
        &mut r,
        "S8_b",
        image::<S8>(|p| ones(fld!(p, u), S8A.next(9))),
    );

    #[repr(C)]
    struct S9 {
        x: u32,
        u: CBitfield<u64>,
    }
    push(&mut r, "sz_S9", size_of::<S9>());
    push(&mut r, "al_S9", align_of::<S9>());
    push(
        &mut r,
        "S9_a",
        image::<S9>(|p| ones(fld!(p, u), CBits::first(1))),
    );

    #[repr(C)]
    struct S10 {
        c: u8,
        u: CBitfield<u32>,
    }
    const S10A: CBits<u32> = CBits::first(4);
    push(&mut r, "sz_S10", size_of::<S10>());
    push(&mut r, "al_S10", align_of::<S10>());
    push(&mut r, "S10_a", image::<S10>(|p| ones(fld!(p, u), S10A)));
    push(
        &mut r,
        "S10_b",
        image::<S10>(|p| ones(fld!(p, u), S10A.next(24))),
    );

    #[repr(C)]
    struct S11 {
        u0: CBitfield<u32>,
        u1: CBitfield<u32>,
    }
    push(&mut r, "sz_S11", size_of::<S11>());
    push(&mut r, "al_S11", align_of::<S11>());
    push(
        &mut r,
        "S11_a",
        image::<S11>(|p| ones(fld!(p, u0), CBits::first(3))),
    );
    push(
        &mut r,
        "S11_b",
        image::<S11>(|p| ones(fld!(p, u1), CBits::first(3))),
    );

    #[repr(C)]
    struct S12 {
        u0: CBitfield<u16>,
        u1: CBitfield<u16>,
    }
    push(&mut r, "sz_S12", size_of::<S12>());
    push(&mut r, "al_S12", align_of::<S12>());
    push(
        &mut r,
        "S12_a",
        image::<S12>(|p| ones(fld!(p, u0), CBits::first(12))),
    );
    push(
        &mut r,
        "S12_b",
        image::<S12>(|p| ones(fld!(p, u1), CBits::first(8))),
    );

    t_runs(&mut r);
    s13(&mut r);
    r
}

/// T1 and T2: a `long long` run after 5 and 3 bytes. Where no aligned
/// integer covers the run's bytes without covering another member (T1 on
/// i686, whose `long long` alignment of 4 keeps `a` at bit 40; T2 on System
/// V and AAPCS), the run is mirrored as bytes, set by hand.
fn t_runs(r: &mut Rows) {
    // T1: a unit of its own at 8, except on i686, where `a` stays in bytes
    // 5-9, behind `c`.
    #[cfg(not(all(target_arch = "x86", not(target_env = "msvc"))))]
    {
        #[repr(C)]
        struct T1 {
            x: u32,
            c: u8,
            u: CBitfield<u64>,
        }
        push(r, "sz_T1", size_of::<T1>());
        push(r, "al_T1", align_of::<T1>());
        push(r, "of_T1_c", offset_of!(T1, c));
        push(
            r,
            "T1_a",
            image::<T1>(|p| ones(fld!(p, u), CBits::first(40))),
        );
    }
    #[cfg(all(target_arch = "x86", not(target_env = "msvc")))]
    {
        #[repr(C)]
        struct T1 {
            x: u32,
            c: u8,
            /// `a`, bytes 5-9: no aligned integer covers them without `c`.
            a: [u8; 7],
        }
        push(r, "sz_T1", size_of::<T1>());
        push(r, "al_T1", align_of::<T1>());
        push(r, "of_T1_c", offset_of!(T1, c));
        push(
            r,
            "T1_a",
            image::<T1>(|p| {
                // SAFETY: the zeroed mirror; 40 bits are its first 5 bytes.
                unsafe { (*p).a[..5].fill(0xff) }
            }),
        );
    }
    // T2: bytes 3-7 on System V and AAPCS (as bytes); a unit of its own at 8
    // on Microsoft's ABI.
    #[cfg(not(target_env = "msvc"))]
    {
        #[cfg_attr(not(target_arch = "x86"), repr(C, align(8)))]
        #[cfg_attr(target_arch = "x86", repr(C, align(4)))]
        struct T2 {
            c: [u8; 3],
            /// `a`, bytes 3-7: a `u64` over them would cover `c`.
            a: [u8; 5],
        }
        push(r, "sz_T2", size_of::<T2>());
        push(r, "al_T2", align_of::<T2>());
        push(
            r,
            "T2_a",
            image::<T2>(|p| {
                // SAFETY: the zeroed mirror; 40 bits are all 5 bytes.
                unsafe { (*p).a.fill(0xff) }
            }),
        );
    }
    #[cfg(target_env = "msvc")]
    {
        #[repr(C)]
        struct T2 {
            c: [u8; 3],
            u: CBitfield<u64>,
        }
        push(r, "sz_T2", size_of::<T2>());
        push(r, "al_T2", align_of::<T2>());
        push(
            r,
            "T2_a",
            image::<T2>(|p| ones(fld!(p, u), CBits::first(40))),
        );
    }
}

/// S13, `git_index`'s word, the same on every covered ABI: a `u32` unit,
/// whose bytes after the run are padding, as an atomic word.
fn s13(r: &mut Rows) {
    #[repr(C)]
    struct S13 {
        oid_type: i32,
        u: CAtomicBitfield<u32>,
        tree: *mut (),
    }
    const ON_DISK: CBits<u32> = CBits::first(1);
    const DIRTY: CBits<u32> = ON_DISK.next(1).next(1).next(1).next(1);
    let set = |f: CBits<u32>| {
        image::<S13>(|p| {
            // SAFETY: a field of a live, zeroed mirror.
            unsafe {
                (*p).u
                    .set_flag(f, true, core::sync::atomic::Ordering::Relaxed)
            };
        })
    };
    push(r, "sz_S13", size_of::<S13>());
    push(r, "al_S13", align_of::<S13>());
    push(r, "of_S13_tree", offset_of!(S13, tree));
    push(r, "S13_on_disk", set(ON_DISK));
    push(r, "S13_dirty", set(DIRTY));
}

/// The table's name for the target this runs on.
fn this_target() -> Option<&'static str> {
    let t = if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-linux-gnu"
    } else if cfg!(all(target_arch = "arm", target_os = "linux")) {
        "armv7-linux-gnueabihf"
    } else if cfg!(all(target_arch = "x86", target_os = "linux")) {
        "i686-linux-gnu"
    } else if cfg!(all(target_arch = "s390x", target_os = "linux")) {
        "s390x-linux-gnu"
    } else if cfg!(all(
        target_arch = "powerpc64",
        target_endian = "big",
        target_os = "linux"
    )) {
        "powerpc64-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_env = "msvc")) {
        "x86_64-pc-windows-msvc"
    } else {
        return None;
    };
    Some(t)
}

#[test]
fn mirrors_match_the_c_layout() {
    let Some(target) = this_target() else {
        eprintln!("skipped: no C layouts for this target");
        return;
    };
    let want = rows(target);
    assert!(!want.is_empty(), "no rows for {target}");
    let got = mirrors();
    let mut bad = Vec::new();
    for (k, v) in &got {
        match want.get(k) {
            Some(w) if w == v => {}
            other => bad.push(format!("{k}: mirror {v}, C {other:?}")),
        }
    }
    for k in want.keys() {
        if !got.iter().any(|(g, _)| g == k) {
            bad.push(format!("{k}: not mirrored"));
        }
    }
    assert!(bad.is_empty(), "{target}:\n{}", bad.join("\n"));
}
