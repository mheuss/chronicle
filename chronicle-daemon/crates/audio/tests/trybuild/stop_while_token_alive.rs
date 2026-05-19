use chronicle_audio::{AudioConfig, AudioPipeline};

fn main() {
    let (mut pipeline, _rx) = AudioPipeline::create(AudioConfig::default()).unwrap();
    let token = pipeline.token(48_000, 1).unwrap();
    pipeline.stop().unwrap(); //~ ERROR cannot borrow `pipeline` as mutable
    // Keep `token` borrow live past `stop()` so NLL doesn't release it early.
    let _ = token.sample_rate;
}
