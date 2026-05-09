//! Compile-fail tests for `AudioPipeline` borrow invariants.
//!
//! The token-alive-during-stop test verifies that the borrow checker
//! refuses to allow `AudioPipeline::stop(&mut self)` while an
//! `AudioHandlerToken<'_>` is outstanding.

#[test]
fn borrow_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/stop_while_token_alive.rs");
}
