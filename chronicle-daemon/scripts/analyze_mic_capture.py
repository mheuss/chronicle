# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "numpy>=2.0,<3",
#     "scipy>=1.14,<2",
# ]
# ///
"""Metrics for a microphone characterization capture (HEU-650; read in HEU-651).

The functions here take the two WAVs `characterize_mic` writes, `mic-native.wav`
and `mic-converted.wav`, and compute the level, correlation and band-pass
diagnostics the HEU-549 design asks for. Everything is computed in float64.
Formulas:

    rms(x)         = sqrt(mean(x^2))
    dBFS(v)        = 20 * log10(v); full scale is 1.0; v == 0 reports -inf
    pearson(a, b)  = sum(a'b') / sqrt(sum(a'^2) * sum(b'^2)), with a' = a - mean(a)
    xcorr(a, b, k) = sum(a'[n] b'[n + k]) / sqrt(sum(a'^2) * sum(b'^2)) for k in +-5 ms;
                     a positive k means b is DELAYED by k samples relative to a
    speech band    = 4th-order Butterworth band-pass 300-3400 Hz, zero phase

Nothing here normalizes or trims. The Yeti's defect is level collapse, so a
normalizer would erase the thing being measured, and trimming would move RMS
differently per channel. Nothing here aligns the two files either: the
converter holds back a filter tail, so they are not frame-aligned, and every
number is an aggregate over the whole recording.
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
    dtype: str  # the stored sample type, e.g. "float32"; gate 1 reads it

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

    The stored dtype is reported, not enforced, so the precedence gates can
    run in the design's order: a mono integer file is gate 0, not gate 1. Only
    exact float32 counts as the f32 the tap delivers; float64 does not.
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
