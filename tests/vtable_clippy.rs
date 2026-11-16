//! `impl_cvtable!` emits the binding's own `unsafe impl`, so clippy's
//! `undocumented_unsafe_blocks` asks the binding for its `SAFETY` comment:
//! clippy runs over a fixture crate holding one invocation with the comment
//! and one without, and must flag the second only.

#![allow(missing_docs)]

use std::path::Path;
use std::process::Command;

const MANIFEST: &str = r#"[package]
name = "vtable-clippy-fixture"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
ffibox = { path = "FFIBOX" }

[lints.clippy]
undocumented_unsafe_blocks = "warn"

[workspace]
"#;

const LIB: &str = r#"
use core::ffi::c_int;

mod ffi {
    use core::ffi::c_int;
    #[repr(C)]
    #[derive(Default)]
    pub struct ops {
        pub get: Option<unsafe extern "C" fn(*mut ops) -> c_int>,
    }
}

ffibox::define_ctype!(Documented, DocumentedRef, DocumentedMut, ffi::ops);
ffibox::define_ctype!(Undocumented, UndocumentedRef, UndocumentedMut, ffi::ops);

ffibox::impl_cvtable! {
    // SAFETY: C passes `get` its table, its only argument.
    unsafe impl CVtable<u8> for Documented {}
    base = ffi::ops::default();
    fn get(@t) -> c_int { on panic: -1, on busy: -2, |s: &u8| c_int::from(*s) }
}

ffibox::impl_cvtable! {
    unsafe impl CVtable<u8> for Undocumented {}
    base = ffi::ops::default();
    fn get(@t) -> c_int { on panic: -1, on busy: -2, |s: &u8| c_int::from(*s) }
}
"#;

#[test]
#[cfg_attr(miri, ignore = "runs cargo clippy")]
fn clippy_asks_for_the_impls_safety_comment() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let has_clippy = Command::new(&cargo)
        .args(["clippy", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_clippy {
        eprintln!("skipped: no cargo clippy");
        return;
    }

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vtable-clippy-fixture");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let ffibox = env!("CARGO_MANIFEST_DIR");
    std::fs::write(root.join("Cargo.toml"), MANIFEST.replace("FFIBOX", ffibox)).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();

    let out = Command::new(&cargo)
        .args(["clippy", "--quiet", "--message-format=short"])
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");

    let flagged: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("unsafe impl missing a safety comment"))
        .collect();
    // `--message-format=short` prints `src/lib.rs:<line>:<col>: warning: ...`;
    // the line is that of the undocumented impl.
    let undocumented = LIB
        .lines()
        .position(|l| l.contains("for Undocumented"))
        .unwrap();
    assert_eq!(flagged.len(), 1, "{stderr}");
    assert!(
        flagged[0].starts_with(&format!("src/lib.rs:{}:", undocumented + 1)),
        "{stderr}"
    );
}
