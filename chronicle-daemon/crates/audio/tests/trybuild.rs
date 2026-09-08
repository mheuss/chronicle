//! trybuild tests for what `chronicle-audio` lets a dependent crate write.
//!
//! The `.stderr` snapshots pin rustc's diagnostic wording. A red result right
//! after a toolchain bump may only need `TRYBUILD=overwrite` and a re-read.

#[test]
fn borrow_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/stop_while_token_alive.rs");
}
