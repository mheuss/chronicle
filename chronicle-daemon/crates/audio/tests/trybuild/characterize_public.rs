// With `--features characterize` this compiles: the module and its frame type
// are public. Without the feature it must not: the module is not in the public
// API. This says nothing about what else a default build compiled.
use chronicle_audio::characterize::CharacterizationFrame;

fn main() {
    let _ = std::mem::size_of::<CharacterizationFrame>();
}
