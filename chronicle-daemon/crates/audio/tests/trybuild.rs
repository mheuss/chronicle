//! trybuild tests for what `chronicle-audio` lets a dependent crate write.
//!
//! The `.stderr` snapshots pin rustc's diagnostic wording. A red result right
//! after a toolchain bump may only need `TRYBUILD=overwrite` and a re-read.

#[test]
fn borrow_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/stop_while_token_alive.rs");
}

/// Proves the module is not public without the feature, and nothing more.
#[cfg(not(feature = "characterize"))]
#[test]
fn characterize_module_is_absent_by_default() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/characterize_public.rs");
}

#[cfg(feature = "characterize")]
#[test]
fn characterization_is_public_with_the_feature() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/characterize_public.rs");
}
