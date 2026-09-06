# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "numpy>=2.0,<3",
#     "scipy>=1.14,<2",
# ]
# ///
"""Analyse a microphone characterization capture (HEU-650; read in HEU-651).

Run:
    uv run chronicle-daemon/scripts/analyze_mic_capture.py <capture-dir> [--out DIR] [--allow-invalid]

`<capture-dir>` is the directory `characterize_mic` wrote: `mic-native.wav`,
`mic-converted.wav` and `manifest.txt`. The script refuses a recording whose
manifest says `measurement_valid: false` unless `--allow-invalid` is given,
checks both WAVs against the manifest and the converted file's contract, then
prints every diagnostic the HEU-549 design asks for, writes `analysis.json`
into `--out` (default: the capture directory), and writes one mono 16 kHz float
WAV per candidate mix. Each candidate can be transcribed with:

    cargo run -p chronicle-transcription --example transcribe_wav -- <file>

Exit codes: 0 analysed, 1 bad input, 2 refused as invalid, 3 a precedence gate
fired (the report says which).

Everything is computed in float64. Formulas:

    rms(x)         = sqrt(mean(x^2))
    dBFS(v)        = 20 * log10(v); full scale is 1.0; v == 0 reports -inf
    pearson(a, b)  = sum(a'b') / sqrt(sum(a'^2) * sum(b'^2)), with a' = a - mean(a)
    xcorr(a, b, k) = sum(a'[n] b'[n + k]) / sqrt(sum(a'^2) * sum(b'^2)) for k in +-5 ms;
                     a positive k means b is DELAYED by k samples relative to a
    speech band    = 4th-order Butterworth band-pass 300-3400 Hz, zero phase
    candidates     = (c0+c1)/2, (c0-c1)/2, c0, c1 -- two-channel input only, never normalized
    transfer gain  = dBFS(converter output) - dBFS(candidate)

Nothing here normalizes or trims. The Yeti's defect is level collapse, so a
normalizer would erase the thing being measured, and trimming would move RMS
differently per channel. Nothing here aligns the two files either: the
converter holds back a filter tail, so they are not frame-aligned, and every
number is an aggregate over the whole recording.

Listen too. Per channel and per candidate. Thin, hollow or phasey audio is
audible long before a number shows it.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass
from math import gcd
from pathlib import Path

import numpy as np
import scipy
from scipy.io import wavfile
from scipy.signal import butter, resample_poly, sosfiltfilt

# Pinned analysis parameters (design "Analysis parameters, pinned").
SPEECH_BAND_HZ = (300.0, 3400.0)
# The design says "4th-order Butterworth band-pass". That is butter(N=4): the
# band-pass transform doubles the prototype order, so the filter has 8 poles in
# 4 biquad sections. Both readings are stated here so nobody changes N to 2 to
# "fix" it.
BUTTER_ORDER = 4
XCORR_WINDOW_S = 0.005
STRONG_PEAK = 0.7
MIN_STRONG_LAG_SAMPLES = 2
SILENCE_DBFS = -50.0
R_HIGH = 0.7
R_LOW = 0.3
R_STRONGLY_NEGATIVE = -0.7
TARGET_RATE = 16_000

PARAMETERS = {
    "speech_band_hz": list(SPEECH_BAND_HZ),
    "butterworth_order": BUTTER_ORDER,
    "cross_correlation_window_s": XCORR_WINDOW_S,
    "strong_peak": STRONG_PEAK,
    "min_strong_lag_samples": MIN_STRONG_LAG_SAMPLES,
    "silence_dbfs": SILENCE_DBFS,
    "r_high": R_HIGH,
    "r_low": R_LOW,
    "r_strongly_negative": R_STRONGLY_NEGATIVE,
    "candidate_rate_hz": TARGET_RATE,
    "silence_trimming": "none",
    "normalization": "none",
}


# --- WAV I/O ---------------------------------------------------------------


@dataclass
class Wav:
    rate: int
    samples: np.ndarray  # shape (frames, channels), float64
    dtype: str  # the stored sample type, e.g. "float32"

    @property
    def is_float32(self) -> bool:
        return self.dtype == "float32"

    @property
    def channels(self) -> int:
        return int(self.samples.shape[1])

    @property
    def frames(self) -> int:
        return int(self.samples.shape[0])


def read_wav(path: Path) -> Wav:
    """Read a WAV into float64 without judging it.

    The stored dtype is reported, not enforced, so the caller can decide what a
    wrong one means. Only exact float32 counts as the f32 the tap delivers;
    float64 does not. Integer WAVs come back at their stored integer scale, not
    +-1.0, so no level metric is meaningful on one until the caller has checked
    `is_float32`.
    """
    rate, data = wavfile.read(path)
    if data.ndim == 1:
        data = data[:, np.newaxis]
    return Wav(rate=int(rate), samples=data.astype(np.float64), dtype=str(data.dtype))


def write_float_wav(path: Path, rate: int, samples: np.ndarray) -> None:
    wavfile.write(path, rate, np.asarray(samples, dtype=np.float32))


# --- level -----------------------------------------------------------------


def rms(x: np.ndarray) -> float:
    if x.size == 0:
        return math.nan
    return float(np.sqrt(np.mean(np.square(x))))


def dbfs(value: float) -> float:
    if math.isnan(value):
        return math.nan
    if value <= 0.0:
        return -math.inf
    return 20.0 * math.log10(value)


def peak(x: np.ndarray) -> float:
    if x.size == 0:
        return math.nan
    return float(np.max(np.abs(x)))


def clipped_samples(x: np.ndarray) -> int:
    return int(np.count_nonzero(np.abs(x) >= 1.0))


# --- correlation -----------------------------------------------------------


def _centered(a: np.ndarray, b: np.ndarray) -> tuple[np.ndarray, np.ndarray, float]:
    a = a - a.mean()
    b = b - b.mean()
    den = math.sqrt(float(np.dot(a, a)) * float(np.dot(b, b)))
    return a, b, den


def pearson_r(a: np.ndarray, b: np.ndarray) -> float:
    if a.size == 0 or a.size != b.size:
        return math.nan
    a, b, den = _centered(a, b)
    if den == 0.0:
        return math.nan
    return float(np.dot(a, b) / den)


def cross_correlation_peak(a: np.ndarray, b: np.ndarray, rate: int) -> dict:
    """Largest |normalized cross-correlation| within +-XCORR_WINDOW_S.

    Returns lag_samples, lag_ms, the signed value at that lag, and whether the
    design calls it strong (|value| >= 0.7 at |lag| >= 2 samples). A positive
    lag means b is delayed relative to a.
    """
    empty = {"lag_samples": 0, "lag_ms": 0.0, "value": math.nan, "strong": False}
    if a.size == 0 or a.size != b.size:
        return empty
    a, b, den = _centered(a, b)
    if den == 0.0:
        return empty
    n = a.size
    max_lag = min(int(round(XCORR_WINDOW_S * rate)), n - 1)
    best_lag, best_value = 0, 0.0
    for lag in range(-max_lag, max_lag + 1):
        if lag >= 0:
            value = float(np.dot(a[: n - lag], b[lag:])) / den
        else:
            value = float(np.dot(a[-lag:], b[: n + lag])) / den
        if abs(value) > abs(best_value):
            best_lag, best_value = lag, value
    strong = abs(best_value) >= STRONG_PEAK and abs(best_lag) >= MIN_STRONG_LAG_SAMPLES
    return {
        "lag_samples": best_lag,
        "lag_ms": best_lag / rate * 1000.0,
        "value": best_value,
        "strong": strong,
    }


# --- band-pass and resampling --------------------------------------------


def speech_band(x: np.ndarray, rate: int) -> np.ndarray:
    sos = butter(BUTTER_ORDER, SPEECH_BAND_HZ, btype="bandpass", fs=rate, output="sos")
    return sosfiltfilt(sos, x)


def resample_to_16k(x: np.ndarray, rate: int) -> np.ndarray:
    if rate == TARGET_RATE:
        return x
    g = gcd(TARGET_RATE, rate)
    return resample_poly(x, TARGET_RATE // g, rate // g)


# --- gates -------------------------------------------------------------------


def gate(native: np.ndarray, rate: int, is_float32: bool) -> str | None:
    """The design's precedence gates, evaluated in its order: 0, 1, 2, 3.

    Gate 3 covers the *source* recording: it tests each native channel's RMS
    statistic for finiteness, which a NaN or infinite sample makes non-finite,
    and fires when every channel has zero variance. It does not cover the
    candidates: a candidate whose RMS is 0 (-inf dBFS) is a result the
    interpretation table reads, not an invalid measurement. An exactly silent
    native channel has RMS 0.0, which is finite; its -inf dBFS is reported.
    Returns None when the analysis may proceed.
    """
    channels = native.shape[1]
    if channels == 1:
        return "gate 0: native channel count is 1; no downmix exists, HEU-549 does not apply to this device"
    if not is_float32:
        return "gate 1: the native WAV is not 32-bit float; extraction returns None for this device, separate ticket"
    levels = [rms(native[:, ch]) for ch in range(channels)]
    # NaN compares false, so a NaN RMS never counts as silent here and falls
    # through to gate 3.
    if all(dbfs(level) < SILENCE_DBFS for level in levels):
        return (
            f"gate 2: every channel below {SILENCE_DBFS:g} dBFS while speaking; nothing was captured, "
            "the problem is upstream of the converter. Stop and revisit the design"
        )
    if any(not math.isfinite(level) for level in levels):
        return "gate 3: a per-channel RMS is not finite; invalid measurement, re-record"
    if all(float(np.var(native[:, ch])) == 0.0 for ch in range(channels)):
        return "gate 3: every channel has zero variance; invalid measurement, re-record"
    return None


# --- candidates and classification -----------------------------------------


def candidates(native: np.ndarray) -> dict[str, np.ndarray]:
    """The four pinned two-channel candidates, in the design's order."""
    c0 = native[:, 0]
    c1 = native[:, 1]
    return {
        "avg": (c0 + c1) / 2.0,
        "diff": (c0 - c1) / 2.0,
        "c0": c0,
        "c1": c1,
    }


def r_band(r: float) -> str:
    if math.isnan(r):
        return "undefined"
    if r > R_HIGH:
        return "high"
    if r < R_STRONGLY_NEGATIVE:
        return "strongly negative"
    if abs(r) <= R_LOW:
        return "low"
    return "partial"


def _channel_stats(x: np.ndarray, rate: int) -> dict:
    return {
        "rms_dbfs": dbfs(rms(x)),
        "peak": peak(x),
        "clipped_samples": clipped_samples(x),
        "speech_band_rms_dbfs": dbfs(rms(speech_band(x, rate))),
    }


def analyze(native_wav: Wav, converted_wav: Wav) -> dict:
    """Every diagnostic the design lists, as one JSON-ready dict.

    The gates run first, on nothing but RMS, so a recording that should stop
    stops before any filter sees it. Candidates are only computed for
    two-channel input; arrays and aggregate devices are outside the
    measurement (design "Channel counts above 2").
    """
    native, native_rate = native_wav.samples, native_wav.rate
    channels = native.shape[1]
    converted_mono = converted_wav.samples[:, 0]
    report = {
        "parameters": PARAMETERS,
        "environment": environment(),
        "native": {
            "rate_hz": native_rate,
            "channels": channels,
            "dtype": native_wav.dtype,
            "frames": int(native.shape[0]),
            "duration_s": native.shape[0] / native_rate,
        },
        "converted": {
            "rate_hz": converted_wav.rate,
            "dtype": converted_wav.dtype,
            "frames": int(converted_mono.size),
            "duration_s": converted_mono.size / converted_wav.rate,
        },
        "gate": gate(native, native_rate, native_wav.is_float32),
    }
    if report["gate"] is not None:
        return report

    report["native"]["per_channel"] = [_channel_stats(native[:, ch], native_rate) for ch in range(channels)]
    report["converted"].update(_channel_stats(converted_mono, converted_wav.rate))
    converted_dbfs = report["converted"]["rms_dbfs"]
    if channels == 2:
        r = pearson_r(native[:, 0], native[:, 1])
        report["pearson_r"] = r
        report["r_band"] = r_band(r)
        report["cross_correlation"] = cross_correlation_peak(native[:, 0], native[:, 1], native_rate)
        report["candidates"] = {}
        for name, samples in candidates(native).items():
            stats = _channel_stats(samples, native_rate)
            stats["transfer_gain_db"] = converted_dbfs - stats["rms_dbfs"]
            report["candidates"][name] = stats
    else:
        report["note"] = (
            f"{channels} channels: per-channel levels only. Candidate mixes are defined for two-channel "
            "input; an array or aggregate device needs its own measurement"
        )
    return report


def environment() -> dict:
    """Recorded in analysis.json so a number can be traced to the code that made it."""
    return {
        "python": sys.version.split()[0],
        "numpy": np.__version__,
        "scipy": scipy.__version__,
    }


# --- capture directory ------------------------------------------------------

MANIFEST = "manifest.txt"
NATIVE_WAV = "mic-native.wav"
CONVERTED_WAV = "mic-converted.wav"
ANALYSIS_JSON = "analysis.json"
CANDIDATE_GLOB = "candidate-*.wav"
CONVERTED_RATE = 48_000
MIN_SECONDS = 1.0


class InputError(Exception):
    """The directory does not hold the recording the manifest describes."""


class InvalidMeasurement(Exception):
    """The manifest says the recording is not a valid measurement."""


def read_manifest(path: Path) -> dict[str, str]:
    if not path.is_file():
        raise InputError(f"{path}: no manifest.txt; is this a characterize_mic output directory?")
    manifest: dict[str, str] = {}
    for line in path.read_text().splitlines():
        key, sep, value = line.partition(":")
        if sep:
            manifest[key.strip()] = value.strip()
    return manifest


def load_capture(capture_dir: Path, allow_invalid: bool) -> tuple[dict[str, str], Wav, Wav]:
    """Read and validate one capture. Every failure here means "wrong file"."""
    manifest = read_manifest(capture_dir / MANIFEST)
    if manifest.get("measurement_valid") != "true" and not allow_invalid:
        raise InvalidMeasurement(
            f"{capture_dir / MANIFEST} says measurement_valid: {manifest.get('measurement_valid', 'missing')}; "
            "re-record, or pass --allow-invalid to look anyway"
        )
    native = read_wav(capture_dir / NATIVE_WAV)
    converted = read_wav(capture_dir / CONVERTED_WAV)

    problems = []
    want_channels = manifest.get("native_channels")
    if want_channels is not None and int(want_channels) != native.channels:
        problems.append(f"manifest native_channels {want_channels} but {NATIVE_WAV} has {native.channels}")
    want_rate = manifest.get("native_sample_rate_hz")
    if want_rate is not None and int(float(want_rate)) != native.rate:
        problems.append(f"manifest native_sample_rate_hz {want_rate} but {NATIVE_WAV} is {native.rate} Hz")
    if converted.channels != 1 or converted.rate != CONVERTED_RATE or not converted.is_float32:
        problems.append(
            f"{CONVERTED_WAV} must be 1 ch, {CONVERTED_RATE} Hz, float32; "
            f"got {converted.channels} ch, {converted.rate} Hz, {converted.dtype}"
        )
    if native.frames < MIN_SECONDS * native.rate:
        problems.append(
            f"{NATIVE_WAV} is shorter than {MIN_SECONDS:g} s ({native.frames} frames); too short to analyse"
        )
    if problems:
        raise InputError("; ".join(problems))
    return manifest, native, converted


def remove_stale_outputs(out: Path) -> None:
    """Nothing from an earlier run may survive into this one."""
    for path in list(out.glob(CANDIDATE_GLOB)) + [out / ANALYSIS_JSON]:
        if path.is_file():
            path.unlink()


# --- output ----------------------------------------------------------------


def _fmt(value) -> str:
    if isinstance(value, float):
        if math.isnan(value):
            return "NaN"
        if math.isinf(value):
            return "-inf" if value < 0 else "inf"
        return f"{value:.2f}"
    return str(value)


def render(report: dict) -> str:
    lines = []
    env = report["environment"]
    lines.append(f"analyzer: python {env['python']}, numpy {env['numpy']}, scipy {env['scipy']}")
    n = report["native"]
    lines.append(f"native: {n['channels']} ch, {n['rate_hz']} Hz, {n['dtype']}, {n['frames']} frames ({n['duration_s']:.2f} s)")
    if report["gate"] is not None:
        lines.append(f"STOP: {report['gate']}")
        return "\n".join(lines) + "\n"
    for ch, stats in enumerate(n["per_channel"]):
        lines.append(
            f"  c{ch}: rms {_fmt(stats['rms_dbfs'])} dBFS, speech-band rms {_fmt(stats['speech_band_rms_dbfs'])} dBFS, "
            f"peak {_fmt(stats['peak'])}, clipped {stats['clipped_samples']}"
        )
    c = report["converted"]
    lines.append(
        f"converter output: {c['rate_hz']} Hz, {c['frames']} frames ({c['duration_s']:.2f} s), "
        f"rms {_fmt(c['rms_dbfs'])} dBFS, speech-band rms {_fmt(c['speech_band_rms_dbfs'])} dBFS, "
        f"peak {_fmt(c['peak'])}, clipped {c['clipped_samples']}"
    )
    if "pearson_r" in report:
        x = report["cross_correlation"]
        lines.append(f"pearson r (zero lag): {_fmt(report['pearson_r'])} ({report['r_band']})")
        lines.append(
            f"cross-correlation peak: {_fmt(x['value'])} at lag {x['lag_samples']} samples "
            f"({x['lag_ms']:.3f} ms), strong={x['strong']}"
        )
        lines.append("candidates (never normalized; listen to each one as well):")
        for name, stats in report["candidates"].items():
            lines.append(
                f"  {name:<4} rms {_fmt(stats['rms_dbfs'])} dBFS, speech-band rms {_fmt(stats['speech_band_rms_dbfs'])} dBFS, "
                f"transfer gain {_fmt(stats['transfer_gain_db'])} dB, peak {_fmt(stats['peak'])}, "
                f"clipped {stats['clipped_samples']}"
                + (f"  -> {stats['wav']}" if "wav" in stats else "")
            )
    if "note" in report:
        lines.append(report["note"])
    return "\n".join(lines) + "\n"


def write_candidate_wavs(report: dict, native: np.ndarray, native_rate: int, out: Path) -> None:
    """One mono 16 kHz float WAV per candidate, in the design's order."""
    for name, samples in candidates(native).items():
        path = out / f"candidate-{name}.wav"
        write_float_wav(path, TARGET_RATE, resample_to_16k(samples, native_rate))
        report["candidates"][name]["wav"] = str(path)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("capture_dir", type=Path, help="directory characterize_mic wrote: mic-native.wav, mic-converted.wav, manifest.txt")
    parser.add_argument("--out", type=Path, default=None, help="directory for analysis.json and candidate WAVs (default: the capture directory)")
    parser.add_argument("--allow-invalid", action="store_true", help="analyse even if the manifest says measurement_valid: false")
    args = parser.parse_args(argv)

    try:
        manifest, native_wav, converted_wav = load_capture(args.capture_dir, args.allow_invalid)
    except InvalidMeasurement as e:
        sys.stderr.write(f"refused: {e}\n")
        return 2
    except (InputError, FileNotFoundError, ValueError) as e:
        sys.stderr.write(f"input error: {e}\n")
        return 1

    out = args.out if args.out is not None else args.capture_dir
    out.mkdir(parents=True, exist_ok=True)
    remove_stale_outputs(out)

    report = analyze(native_wav, converted_wav)
    report["manifest"] = manifest
    report["native"]["path"] = str(args.capture_dir / NATIVE_WAV)
    report["converted"]["path"] = str(args.capture_dir / CONVERTED_WAV)

    if report["gate"] is None and "candidates" in report:
        write_candidate_wavs(report, native_wav.samples, native_wav.rate, out)

    (out / ANALYSIS_JSON).write_text(json.dumps(report, indent=2))
    sys.stdout.write(render(report))
    sys.stdout.write(f"wrote {out / ANALYSIS_JSON}\n")
    return 3 if report["gate"] is not None else 0


if __name__ == "__main__":
    sys.exit(main())
