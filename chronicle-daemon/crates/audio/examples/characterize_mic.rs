//! Record what the microphone tap receives and what the converter makes of it.
//!
//! ```text
//! cargo run -p chronicle-audio --features characterize --example characterize_mic -- \
//!     --device-label "Blue Yeti" --mode stereo --gain 50% \
//!     --phrase "the quick brown fox jumps over the lazy dog" --model-variant base \
//!     --seconds 10 --out ./yeti-take-1
//! ```
//!
//! Writes `mic-native.wav`, `mic-converted.wav` and `manifest.txt` into
//! `--out`, which must not exist yet or must be empty: a rerun into a used
//! directory could leave a fresh partial WAV next to an old manifest that
//! says `measurement_valid: true`.
//!
//! `--device-label` is a label for the manifest. It does not select or
//! verify the active input device. Set the default input in System Settings
//! first, and check it in Audio MIDI Setup before you trust the recording.
//!
//! Exit code 2 means the files were written but the recording is not a valid
//! measurement (a dropped frame or a conversion failure; see the manifest).
//! Exit code 1 is an error before or during recording. The first run prompts
//! for microphone permission.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chronicle_audio::characterize::{CharacterizationFrame, CharacterizationWriter, Manifest};
use chronicle_audio::{AudioDropCounters, MicrophoneCapture};

/// Same bound as the encoding channel in `engine.rs`, so a drop here means
/// what a drop there means.
const CHANNEL_CAPACITY: usize = 64;
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Mirrors `SUPPORTED_VARIANTS` in `chronicle-transcription`. Kept as a copy
/// because a dependency from this crate on the transcription crate would pull
/// whisper into every audio test build. If that list changes, change this one.
const MODEL_VARIANTS: &[&str] = &["base", "small", "medium"];

const USAGE: &str = "usage: characterize_mic --device-label TEXT --phrase TEXT [--mode TEXT] [--gain TEXT] [--model-variant base|small|medium] [--seconds N] [--out DIR]";

#[derive(Debug)]
struct Args {
    device_label: String,
    mode: String,
    gain: String,
    phrase: String,
    model_variant: String,
    seconds: u64,
    out: PathBuf,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut device_label = None;
    let mut mode = "unspecified".to_string();
    let mut gain = "unspecified".to_string();
    let mut phrase = None;
    let mut model_variant = "base".to_string();
    let mut seconds = 10;
    let mut out = None;

    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--device-label" => device_label = Some(value()?),
            "--mode" => mode = value()?,
            "--gain" => gain = value()?,
            "--phrase" => phrase = Some(value()?),
            "--model-variant" => model_variant = value()?,
            "--seconds" => {
                seconds = value()?
                    .parse()
                    .map_err(|e| format!("--seconds: {e}\n{USAGE}"))?;
            }
            "--out" => out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }

    let device_label = required("--device-label", device_label)?;
    let phrase = required("--phrase", phrase)?;
    if !MODEL_VARIANTS.contains(&model_variant.as_str()) {
        return Err(format!(
            "--model-variant {model_variant:?} is not one of base, small, medium"
        ));
    }
    if seconds == 0 {
        return Err("--seconds must be at least 1".into());
    }
    let out = out.unwrap_or_else(|| PathBuf::from(format!("mic-capture-{}", unix_secs())));
    Ok(Args {
        device_label,
        mode,
        gain,
        phrase,
        model_variant,
        seconds,
        out,
    })
}

fn required(flag: &str, value: Option<String>) -> Result<String, String> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("{flag} is required and must not be empty\n{USAGE}")),
    }
}

/// Create `dir` if missing; refuse it if it holds anything.
fn ensure_empty_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if std::fs::read_dir(dir)?.next().is_some() {
        return Err(std::io::Error::other(format!(
            "{} is not empty; use a new directory per take",
            dir.display()
        )));
    }
    Ok(())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn macos_version() -> String {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
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
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Returns whether the recording is a valid measurement.
fn run(args: &Args) -> Result<bool, Box<dyn std::error::Error>> {
    ensure_empty_dir(&args.out)?;
    let native_wav = args.out.join("mic-native.wav");
    let converted_wav = args.out.join("mic-converted.wav");
    let manifest_path = args.out.join("manifest.txt");

    let (tx, rx) = sync_channel::<CharacterizationFrame>(CHANNEL_CAPACITY);
    let counters = Arc::new(AudioDropCounters::default());

    let (capture, format) =
        MicrophoneCapture::new_characterizing(tx.clone(), Arc::clone(&counters))?;
    println!(
        "native format: {} ch, {} Hz, interleaved={}, {}",
        format.channels, format.sample_rate, format.interleaved, format.common_format
    );

    let writer = CharacterizationWriter::spawn(rx, &format, &native_wav, &converted_wav)?;
    let recorded_at_unix_secs = unix_secs();

    capture.start()?;
    println!(
        "recording for {} s. read this, once: {}",
        args.seconds, args.phrase
    );
    std::thread::sleep(Duration::from_secs(args.seconds));
    capture.stop()?;

    // `stop()` halts the engine, but the tap block keeps its sender until the
    // capture is dropped. Drop the capture first, then our own sender, and
    // only then is the writer's channel closed.
    drop(capture);
    drop(tx);

    let report = writer.finish(FINISH_TIMEOUT)?;
    let manifest = Manifest {
        device: &args.device_label,
        mode: &args.mode,
        gain: &args.gain,
        phrase: &args.phrase,
        model_variant: &args.model_variant,
        macos_version: &macos_version(),
        recorded_at_unix_secs,
        requested_secs: args.seconds,
        format: &format,
        native_wav: &native_wav,
        converted_wav: &converted_wav,
        report: &report,
        drops: counters.snapshot(),
    };
    let text = manifest.render();
    std::fs::write(&manifest_path, &text)?;
    print!("{text}");

    let valid = manifest.measurement_valid();
    if valid {
        println!("measurement valid. wrote {}", args.out.display());
    } else {
        println!(
            "MEASUREMENT INVALID: re-record. see {}",
            manifest_path.display()
        );
    }
    Ok(valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const FULL: &[&str] = &[
        "--device-label",
        "Blue Yeti",
        "--mode",
        "stereo",
        "--gain",
        "50%",
        "--phrase",
        "the quick brown fox",
        "--model-variant",
        "small",
        "--seconds",
        "7",
        "--out",
        "/tmp/take",
    ];

    #[test]
    fn parses_every_flag() {
        let a = parse_args(&args(FULL)).unwrap();
        assert_eq!(a.device_label, "Blue Yeti");
        assert_eq!(a.mode, "stereo");
        assert_eq!(a.gain, "50%");
        assert_eq!(a.phrase, "the quick brown fox");
        assert_eq!(a.model_variant, "small");
        assert_eq!(a.seconds, 7);
        assert_eq!(a.out, PathBuf::from("/tmp/take"));
    }

    #[test]
    fn defaults_apply_when_a_flag_is_absent() {
        let a = parse_args(&args(&["--device-label", "Yeti", "--phrase", "fox"])).unwrap();
        assert_eq!(a.mode, "unspecified");
        assert_eq!(a.gain, "unspecified");
        assert_eq!(a.model_variant, "base");
        assert_eq!(a.seconds, 10);
        assert!(a.out.to_string_lossy().starts_with("mic-capture-"));
    }

    #[test]
    fn device_label_and_phrase_are_required_and_non_empty() {
        assert!(
            parse_args(&args(&["--phrase", "fox"]))
                .unwrap_err()
                .contains("--device-label")
        );
        assert!(
            parse_args(&args(&["--device-label", "Yeti"]))
                .unwrap_err()
                .contains("--phrase")
        );
        assert!(
            parse_args(&args(&["--device-label", "  ", "--phrase", "fox"]))
                .unwrap_err()
                .contains("--device-label")
        );
        assert!(
            parse_args(&args(&["--device-label", "Yeti", "--phrase", ""]))
                .unwrap_err()
                .contains("--phrase")
        );
    }

    #[test]
    fn model_variant_must_be_on_the_allow_list() {
        let err = parse_args(&args(&[
            "--device-label",
            "Yeti",
            "--phrase",
            "fox",
            "--model-variant",
            "tiny",
        ]))
        .unwrap_err();
        assert!(err.contains("tiny"), "{err}");
        assert!(err.contains("base, small, medium"), "{err}");
    }

    #[test]
    fn seconds_must_be_a_positive_integer() {
        assert!(
            parse_args(&args(&[
                "--device-label",
                "Yeti",
                "--phrase",
                "fox",
                "--seconds",
                "0"
            ]))
            .is_err()
        );
        assert!(
            parse_args(&args(&[
                "--device-label",
                "Yeti",
                "--phrase",
                "fox",
                "--seconds",
                "ten"
            ]))
            .is_err()
        );
    }

    #[test]
    fn unknown_flags_and_missing_values_are_errors() {
        assert!(
            parse_args(&args(&[
                "--device-label",
                "Yeti",
                "--phrase",
                "fox",
                "--bogus"
            ]))
            .is_err()
        );
        assert!(parse_args(&args(&["--device-label", "Yeti", "--phrase"])).is_err());
    }

    /// `MODEL_VARIANTS` is a copy of the transcription crate's list. Read the
    /// sibling at test time and anchor each variant inside the one line that
    /// defines it, so a change on either side fails here.
    #[test]
    fn model_variants_match_the_transcription_crate() {
        let sibling =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../transcription/src/lib.rs");
        let source = std::fs::read_to_string(&sibling).expect("transcription lib.rs readable");
        let line = source
            .lines()
            .find(|l| l.contains("pub const SUPPORTED_VARIANTS"))
            .expect("SUPPORTED_VARIANTS line present");
        for variant in MODEL_VARIANTS {
            assert!(
                line.contains(&format!("\"{variant}\"")),
                "{variant} missing from: {line}"
            );
        }
        let quoted = line.matches('"').count() / 2;
        assert_eq!(
            quoted,
            MODEL_VARIANTS.len(),
            "the sibling list has a variant this copy lacks: {line}"
        );
    }

    #[test]
    fn output_directory_must_be_new_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            ensure_empty_dir(dir.path()).is_ok(),
            "an empty directory is fine"
        );
        let fresh = dir.path().join("new");
        assert!(
            ensure_empty_dir(&fresh).is_ok(),
            "a missing directory is created"
        );
        std::fs::write(fresh.join("manifest.txt"), "old").unwrap();
        let err = ensure_empty_dir(&fresh).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }
}
