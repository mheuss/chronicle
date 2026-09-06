// With `--features characterize` this compiles: the module and its frame type
// are public. Without the feature it must not, which is the proof that a
// default build carries none of the characterization path.
use chronicle_audio::characterize::CharacterizationFrame;

fn main() {
    let _ = std::mem::size_of::<CharacterizationFrame>();
}
