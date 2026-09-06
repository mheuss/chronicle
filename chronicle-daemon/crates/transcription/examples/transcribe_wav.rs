//! Transcribe a mono 16 kHz 32-bit float WAV through the production engine.
//!
//! ```text
//! cargo run -p chronicle-transcription --example transcribe_wav -- \
//!     candidate-avg.wav [--variant base|small|medium] [--base-dir DIR]
//! ```
//!
//! `--base-dir` is the Chronicle data directory holding `models/`. Defaults to
//! `$CHRONICLE_TEST_BASE_DIR`, then `~/Library/Application Support/Chronicle`,
//! the same rule the crate's ignored real-model tests use. The input must be
//! one channel, 16000 Hz, 32-bit float. Chronicle's only other decode path is
//! Ogg/Opus, which is why this exists.

use std::path::PathBuf;
use std::process::ExitCode;

use chronicle_transcription::{
    DEFAULT_VARIANT, ModelVariant, Transcriber, TranscriptionEngine, model_path, model_present,
    parse_variant,
};

const USAGE: &str =
    "usage: transcribe_wav <mono-16k-float.wav> [--variant base|small|medium] [--base-dir DIR]";

#[derive(Debug)]
struct Args {
    wav: PathBuf,
    variant: ModelVariant,
    base_dir: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut wav = None;
    let mut variant = DEFAULT_VARIANT;
    let mut base_dir = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--variant" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| format!("--variant needs a value\n{USAGE}"))?;
                variant = parse_variant(raw)
                    .ok_or_else(|| format!("unknown variant {raw:?}; use base, small or medium"))?;
            }
            "--base-dir" => {
                let raw = iter
                    .next()
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("--base-dir needs a value\n{USAGE}"))?;
                base_dir = Some(PathBuf::from(raw));
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown argument {other}\n{USAGE}"));
            }
            positional => {
                if wav.replace(PathBuf::from(positional)).is_some() {
                    return Err(format!("only one WAV path is accepted\n{USAGE}"));
                }
            }
        }
    }

    let wav = wav.ok_or_else(|| format!("a WAV path is required\n{USAGE}"))?;
    Ok(Args {
        wav,
        variant,
        base_dir,
    })
}

fn default_base_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CHRONICLE_TEST_BASE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Library/Application Support/Chronicle")
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = args.base_dir.clone().unwrap_or_else(default_base_dir);

    let mut reader = hound::WavReader::open(&args.wav)?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != 16_000
        || spec.sample_format != hound::SampleFormat::Float
        || spec.bits_per_sample != 32
    {
        return Err(format!(
            "{} must be mono, 16000 Hz, 32-bit float; got {} ch, {} Hz, {:?} {}-bit",
            args.wav.display(),
            spec.channels,
            spec.sample_rate,
            spec.sample_format,
            spec.bits_per_sample
        )
        .into());
    }
    let pcm: Vec<f32> = reader.samples::<f32>().collect::<Result<_, _>>()?;
    println!(
        "{}: {} samples ({:.2} s)",
        args.wav.display(),
        pcm.len(),
        pcm.len() as f64 / 16_000.0
    );

    if !model_present(&base_dir, args.variant) {
        return Err(format!(
            "no {} model at {}; run chronicle-daemon/scripts/fetch-whisper-model.sh {} or pass --base-dir",
            args.variant,
            model_path(&base_dir, args.variant).display(),
            args.variant
        )
        .into());
    }

    let engine = TranscriptionEngine::load(&base_dir, args.variant)?;
    let transcript = engine.transcribe(&pcm)?;

    println!("model: {}", args.variant);
    println!(
        "language: {}",
        transcript.language.as_deref().unwrap_or("undetected")
    );
    if transcript.text.is_empty() {
        println!("text: (empty transcript)");
    } else {
        println!("text: {}", transcript.text);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_every_flag() {
        let a = parse_args(&args(&[
            "take.wav",
            "--variant",
            "small",
            "--base-dir",
            "/tmp/chronicle",
        ]))
        .unwrap();
        assert_eq!(a.wav, PathBuf::from("take.wav"));
        assert_eq!(a.variant, parse_variant("small").unwrap());
        assert_eq!(a.base_dir, Some(PathBuf::from("/tmp/chronicle")));
    }

    #[test]
    fn defaults_apply_when_a_flag_is_absent() {
        let a = parse_args(&args(&["take.wav"])).unwrap();
        assert_eq!(a.variant, DEFAULT_VARIANT);
        assert_eq!(a.base_dir, None);
    }

    #[test]
    fn flags_may_come_before_the_path() {
        let a = parse_args(&args(&["--variant", "medium", "take.wav"])).unwrap();
        assert_eq!(a.wav, PathBuf::from("take.wav"));
        assert_eq!(a.variant, parse_variant("medium").unwrap());
    }

    #[test]
    fn the_wav_path_is_required_and_single() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["--variant", "base"])).is_err());
        assert!(parse_args(&args(&["a.wav", "b.wav"])).is_err());
    }

    #[test]
    fn variant_must_be_on_the_allow_list() {
        let err = parse_args(&args(&["take.wav", "--variant", "../evil"])).unwrap_err();
        assert!(err.contains("unknown variant"), "{err}");
        assert!(parse_args(&args(&["take.wav", "--variant", "tiny"])).is_err());
    }

    #[test]
    fn unknown_flags_and_missing_values_are_errors() {
        assert!(parse_args(&args(&["take.wav", "--bogus"])).is_err());
        assert!(parse_args(&args(&["take.wav", "--variant"])).is_err());
        assert!(parse_args(&args(&["take.wav", "--base-dir"])).is_err());
        let err = parse_args(&args(&["--base-dir", "--variant", "take.wav"])).unwrap_err();
        assert!(err.contains("--base-dir needs a value"), "{err}");
    }
}
