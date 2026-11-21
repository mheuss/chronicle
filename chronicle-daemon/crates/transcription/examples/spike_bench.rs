//! CHR-173 spike harness. Not for main.
//!
//! ```text
//! spike_bench stats <file.opus>...            # path, seconds, rms dBFS, peak dBFS
//! spike_bench wav <outdir> <file.opus>...     # decode to mono 16 kHz f32 WAV
//! spike_bench whisper <base-dir> <variant> <runs> <file.wav>...
//! ```
//!
//! `whisper` loads the engine once, then transcribes each file `runs` times and
//! prints one tab-separated line per file: path, audio seconds, min ms, median
//! ms, language, text.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chronicle_transcription::{
    Transcriber, TranscriptionEngine, decode_opus_16k_mono, parse_variant,
};

fn db(x: f32) -> f32 {
    if x <= 0.0 { -120.0 } else { 20.0 * x.log10() }
}

fn read_wav(path: &Path) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let mut r = hound::WavReader::open(path)?;
    let s = r.spec();
    if s.channels != 1 || s.sample_rate != 16_000 || s.sample_format != hound::SampleFormat::Float {
        return Err(format!("{} is not mono 16 kHz float", path.display()).into());
    }
    Ok(r.samples::<f32>().collect::<Result<_, _>>()?)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn bench(
    engine: &dyn Transcriber,
    runs: usize,
    files: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    for p in files {
        let pcm = read_wav(Path::new(p))?;
        let mut times = Vec::with_capacity(runs);
        let mut last = None;
        for _ in 0..runs {
            let t = Instant::now();
            let tr = engine.transcribe(&pcm)?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            last = Some(tr);
        }
        let tr = last.unwrap();
        let min = times.iter().cloned().fold(f64::MAX, f64::min);
        println!(
            "{p}\t{:.2}\t{min:.0}\t{:.0}\t{}\t{}",
            pcm.len() as f64 / 16_000.0,
            median(&mut times),
            tr.language.as_deref().unwrap_or("-"),
            tr.text.replace(['\t', '\n'], " ")
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("stats") => {
            for p in &argv[1..] {
                match decode_opus_16k_mono(Path::new(p)) {
                    Ok(pcm) => {
                        let n = pcm.len().max(1) as f32;
                        let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / n).sqrt();
                        let peak = pcm.iter().fold(0f32, |m, x| m.max(x.abs()));
                        println!(
                            "{p}\t{:.2}\t{:.1}\t{:.1}",
                            pcm.len() as f64 / 16_000.0,
                            db(rms),
                            db(peak)
                        );
                    }
                    Err(e) => println!("{p}\tERR\t{e}"),
                }
            }
        }
        Some("wav") => {
            let out = PathBuf::from(&argv[1]);
            std::fs::create_dir_all(&out)?;
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            for p in &argv[2..] {
                let pcm = decode_opus_16k_mono(Path::new(p))?;
                let stem = Path::new(p)
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let mut w = hound::WavWriter::create(out.join(format!("{stem}.wav")), spec)?;
                for s in pcm {
                    w.write_sample(s)?;
                }
                w.finalize()?;
            }
        }
        Some("encode") => {
            // encode <in-48k-mono-f32.wav> <outdir>: 30 s Ogg/Opus segments, as the mic pipeline writes them.
            let mut r = hound::WavReader::open(&argv[1])?;
            let s = r.spec();
            if s.channels != 1
                || s.sample_rate != 48_000
                || s.sample_format != hound::SampleFormat::Float
            {
                return Err("input must be mono 48 kHz float".into());
            }
            let pcm: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>()?;
            let out = PathBuf::from(&argv[2]);
            std::fs::create_dir_all(&out)?;
            let enc = chronicle_audio::OggOpusEncoder::new(1, 64_000, opus::Application::Voip);
            for (i, chunk) in pcm.chunks(48_000 * 30).enumerate() {
                let p = out.join(format!("take_{i:02}.opus"));
                enc.encode_to_file(chunk, &p)?;
                println!("{}\t{:.2}", p.display(), chunk.len() as f64 / 48_000.0);
            }
        }
        Some("whisper") => {
            let base = PathBuf::from(&argv[1]);
            let variant = parse_variant(&argv[2]).ok_or("bad variant")?;
            let runs: usize = argv[3].parse()?;
            let t = Instant::now();
            let engine = TranscriptionEngine::load(&base, variant)?;
            eprintln!("load_ms\t{:.0}", t.elapsed().as_secs_f64() * 1000.0);
            bench(&engine, runs, &argv[4..])?;
        }
        #[cfg(feature = "parakeet")]
        Some("parakeet") => {
            // parakeet <encoder> <decoder> <joiner> <tokens> <provider> <threads> <runs> <file.wav>...
            use chronicle_transcription::parakeet::{ParakeetConfig, ParakeetEngine};
            let cfg = ParakeetConfig {
                encoder: argv[1].clone().into(),
                decoder: argv[2].clone().into(),
                joiner: argv[3].clone().into(),
                tokens: argv[4].clone().into(),
                provider: argv[5].clone(),
                num_threads: argv[6].parse()?,
                name: "parakeet".into(),
            };
            let runs: usize = argv[7].parse()?;
            let t = Instant::now();
            let engine = ParakeetEngine::load(&cfg)?;
            eprintln!("load_ms\t{:.0}", t.elapsed().as_secs_f64() * 1000.0);
            bench(&engine, runs, &argv[8..])?;
        }
        _ => return Err("usage: spike_bench stats|wav|whisper|parakeet ...".into()),
    }
    Ok(())
}
