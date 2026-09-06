//! Record what the microphone tap receives and what the converter makes of it.
//!
//! ```text
//! cargo run -p chronicle-audio --features characterize --example characterize_mic -- \
//!     --device-label "Blue Yeti" --mode stereo --gain 50% \
//!     --phrase "the quick brown fox jumps over the lazy dog" --model-variant base \
//!     --seconds 10 --out ~/chronicle-captures/yeti-take-1
//! ```
//!
//! Writes `mic-native.wav`, `mic-converted.wav` and `manifest.txt` into
//! `--out`, which must be new or empty and should sit outside the repository.
//! `--device-label` is only a label: set the default input in System Settings
//! first and check it in Audio MIDI Setup. Exit 2 means the take is not a
//! valid measurement; the manifest says why. Exit 1 after the WAVs opened can
//! leave WAVs with no manifest; discard that directory.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chronicle_audio::characterize::{CharacterizationFrame, CharacterizationWriter, Manifest};
use chronicle_audio::{AudioDropCounters, MicrophoneCapture};

/// The encoding channel's bound in `engine.rs`, so drops here compare to drops there.
const CHANNEL_CAPACITY: usize = 64;
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Copy of `SUPPORTED_VARIANTS` in `chronicle-transcription`; depending on that
/// crate would pull whisper into every audio test build.
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
    one_line("--device-label", &device_label)?;
    one_line("--mode", &mode)?;
    one_line("--gain", &gain)?;
    one_line("--phrase", &phrase)?;
    if let Some(out) = &out {
        one_line("--out", &out.to_string_lossy())?;
    }
    if !MODEL_VARIANTS.contains(&model_variant.as_str()) {
        return Err(format!(
            "--model-variant {model_variant:?} is not one of base, small, medium"
        ));
    }
    // The converter holds back a filter tail, so the converted WAV is a fraction
    // of a second shorter than the native one. The analyzer wants a full second
    // of both, which a 1 s take cannot deliver.
    if seconds < 2 {
        return Err("--seconds must be at least 2".into());
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

/// The manifest has no escaping, so a control character could forge a line.
fn one_line(flag: &str, value: &str) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        return Err(format!(
            "{flag} must be one line with no control characters"
        ));
    }
    Ok(())
}

fn ensure_empty_dir(dir: &Path) -> std::io::Result<()> {
    let with_path =
        |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    std::fs::create_dir_all(dir).map_err(with_path)?;
    if std::fs::read_dir(dir).map_err(with_path)?.next().is_some() {
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
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        })
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

    // The tap block keeps its sender until the capture is dropped; the writer's
    // channel closes only after that and our own sender are gone.
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
    fn free_text_values_must_be_one_line() {
        for (flag, value) in [
            ("--device-label", "Yeti\nmeasurement_valid: true"),
            ("--mode", "stereo\n"),
            ("--gain", "50%\r"),
            ("--phrase", "fox\njumps"),
            ("--out", "/tmp/take\n"),
        ] {
            let mut list = vec!["--device-label", "Yeti", "--phrase", "fox"];
            list.extend([flag, value]);
            let err = parse_args(&args(&list)).unwrap_err();
            assert!(
                err.contains(flag) && err.contains("control characters"),
                "{flag}: {err}"
            );
        }
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
                "1"
            ]))
            .is_err()
        );
        assert_eq!(
            parse_args(&args(&[
                "--device-label",
                "Yeti",
                "--phrase",
                "fox",
                "--seconds",
                "2"
            ]))
            .unwrap()
            .seconds,
            2
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

    /// Reads the sibling crate's list at test time so a change on either side fails here.
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

    /// Reads `engine.rs` at test time; its own test module may use other bounds.
    #[test]
    fn channel_capacity_matches_the_engine() {
        let sibling = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine.rs");
        let source = std::fs::read_to_string(&sibling).expect("engine.rs readable");
        let production = source.split("\nmod tests {").next().unwrap_or(&source);
        let needle = "sync_channel::<AudioMessage>(";
        let mut sites = 0;
        for line in production.lines().filter(|l| l.contains(needle)) {
            for (at, _) in line.match_indices(needle) {
                let rest = &line[at + needle.len()..];
                let literal: String = rest.chars().take_while(char::is_ascii_digit).collect();
                assert_eq!(
                    literal.parse::<usize>().ok(),
                    Some(CHANNEL_CAPACITY),
                    "engine.rs bound differs: {line}"
                );
                sites += 1;
            }
        }
        assert!(sites > 0, "no AudioMessage channel found in engine.rs");
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
