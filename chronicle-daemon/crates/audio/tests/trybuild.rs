//! trybuild tests for what `chronicle-audio` lets a dependent crate write.
//!
//! The token-alive-during-stop test verifies that the borrow checker
//! refuses to allow `AudioPipeline::stop(&mut self)` while an
//! `AudioHandlerToken<'_>` is outstanding. The characterize tests verify that
//! the `characterize` module is absent without its feature and public with it.

#[test]
fn borrow_invariants() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/stop_while_token_alive.rs");
}

/// Without the feature, `chronicle_audio::characterize` does not exist in the
/// public API. That is what this proves: the module is absent, so an example
/// crate cannot name its types. It does not inspect what else was compiled.
#[cfg(not(feature = "characterize"))]
#[test]
fn characterize_module_is_absent_by_default() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/characterize_public.rs");
}

/// The mirror image: with the feature on, an example crate can name the
/// frame type through the public API.
#[cfg(feature = "characterize")]
#[test]
fn characterization_is_public_with_the_feature() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/characterize_public.rs");
}
