# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "numpy>=2.0,<3",
#     "scipy>=1.14,<2",
#     "pytest>=8,<9",
# ]
# ///
"""Tests for analyze_mic_capture.py.

Run:  uv run chronicle-daemon/scripts/test_analyze_mic_capture.py
"""

import json
import math
import sys
from pathlib import Path

import numpy as np
import pytest
from scipy.io import wavfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import analyze_mic_capture as amc  # noqa: E402

RATE = 48_000


def sine(freq_hz: float, amp: float, secs: float = 1.0, rate: int = RATE) -> np.ndarray:
    t = np.arange(int(secs * rate)) / rate
    return amp * np.sin(2 * np.pi * freq_hz * t)


def noise(secs: float = 1.0, seed: int = 1, rate: int = RATE) -> np.ndarray:
    return np.random.default_rng(seed).standard_normal(int(secs * rate)) * 0.1


# --- level ---------------------------------------------------------------


def test_rms_of_a_sine_is_amplitude_over_root_two():
    assert amc.rms(sine(1000, 0.5)) == pytest.approx(0.5 / math.sqrt(2), rel=1e-3)


def test_dbfs_of_full_scale_sine_is_minus_three():
    assert amc.dbfs(amc.rms(sine(1000, 1.0))) == pytest.approx(-3.01, abs=0.02)


def test_dbfs_of_silence_is_negative_infinity_and_nan_stays_nan():
    assert amc.dbfs(0.0) == -math.inf
    assert math.isnan(amc.dbfs(math.nan))


def test_rms_of_empty_is_nan():
    assert math.isnan(amc.rms(np.zeros(0)))


def test_peak_and_clipping_count():
    x = np.array([0.1, -0.9, 1.0, -1.0, 0.5])
    assert amc.peak(x) == 1.0
    assert amc.clipped_samples(x) == 2


# --- correlation ---------------------------------------------------------


def test_pearson_identical_is_one_and_inverted_is_minus_one():
    a = noise()
    assert amc.pearson_r(a, a) == pytest.approx(1.0)
    assert amc.pearson_r(a, -a) == pytest.approx(-1.0)


def test_pearson_of_a_constant_channel_is_nan():
    assert math.isnan(amc.pearson_r(np.ones(100), noise(secs=100 / RATE)))


def test_pearson_ignores_dc_offset():
    a = noise()
    assert amc.pearson_r(a, a + 0.3) == pytest.approx(1.0)


def test_cross_correlation_finds_a_seven_sample_delay():
    a = noise()
    b = np.roll(a, 7)  # b is a delayed by 7 samples
    result = amc.cross_correlation_peak(a, b, RATE)
    assert result["lag_samples"] == 7
    assert result["lag_ms"] == pytest.approx(7 / RATE * 1000)
    assert result["value"] == pytest.approx(1.0, abs=1e-3)
    assert result["strong"] is True


def test_cross_correlation_at_zero_lag_is_not_strong():
    a = noise()
    result = amc.cross_correlation_peak(a, a, RATE)
    assert result["lag_samples"] == 0
    assert result["strong"] is False, "a strong peak needs a lag of at least 2 samples"


def test_cross_correlation_of_inverted_delayed_copy_reports_negative_value():
    a = noise()
    b = -np.roll(a, 5)
    result = amc.cross_correlation_peak(a, b, RATE)
    assert result["lag_samples"] == 5
    assert result["value"] == pytest.approx(-1.0, abs=1e-3)


def test_cross_correlation_of_independent_noise_is_weak():
    result = amc.cross_correlation_peak(noise(seed=1), noise(seed=2), RATE)
    assert abs(result["value"]) < 0.1
    assert result["strong"] is False


def test_cross_correlation_window_is_five_milliseconds():
    a = noise()
    inside = amc.cross_correlation_peak(a, np.roll(a, 200), RATE)  # 4.17 ms
    assert inside["lag_samples"] == 200
    outside = amc.cross_correlation_peak(a, np.roll(a, 400), RATE)  # 8.33 ms
    assert abs(outside["lag_samples"]) <= 240
    assert abs(outside["value"]) < 0.1, "a delay past the window must not be found"
    assert outside["strong"] is False


def test_cross_correlation_peak_of_one_half_is_not_strong():
    a = noise(seed=1)
    # Adding independent noise at three times the variance scales the
    # normalized peak to 1 / sqrt(1 + 3) = 0.5, below the 0.7 threshold.
    b = np.roll(a, 5) + math.sqrt(3) * noise(seed=2)
    result = amc.cross_correlation_peak(a, b, RATE)
    assert result["lag_samples"] == 5
    assert result["value"] == pytest.approx(0.5, abs=0.03)
    assert result["strong"] is False


# --- band-pass and resampling -------------------------------------------


def test_speech_band_keeps_one_khz_and_removes_fifty_hz():
    one_k = sine(1000, 1.0)
    fifty = sine(50, 1.0)
    assert amc.rms(amc.speech_band(one_k, RATE)) == pytest.approx(amc.rms(one_k), rel=0.05)
    assert amc.rms(amc.speech_band(fifty, RATE)) < 0.05 * amc.rms(fifty)


def test_speech_band_is_fourth_order():
    # One octave below the low edge: order 2 leaves 4.5% of the level, order 4
    # leaves 0.3%. The threshold sits between them.
    probe = sine(150, 1.0)
    assert amc.rms(amc.speech_band(probe, RATE)) < 0.01 * amc.rms(probe)


def test_speech_band_edges_are_at_300_and_3400_hz():
    # Zero-phase filtering squares the magnitude response, so each corner
    # sits at -6 dB, half the input level.
    for edge in (300.0, 3400.0):
        probe = sine(edge, 1.0)
        assert amc.rms(amc.speech_band(probe, RATE)) == pytest.approx(0.5 * amc.rms(probe), rel=0.02), edge


def test_resample_to_16k_keeps_length_ratio_and_level():
    x = sine(1000, 0.5)
    y = amc.resample_to_16k(x, RATE)
    assert len(y) == 16_000
    assert amc.rms(y) == pytest.approx(amc.rms(x), rel=0.02)


def test_resample_to_16k_is_identity_at_16k():
    x = sine(1000, 0.5, rate=16_000)
    assert amc.resample_to_16k(x, 16_000) is x


# --- WAV I/O -------------------------------------------------------------


def test_read_wav_returns_frames_by_channels_in_float64(tmp_path):
    path = tmp_path / "stereo.wav"
    data = np.stack([sine(1000, 0.5), sine(1000, 0.25)], axis=1).astype(np.float32)
    wavfile.write(path, RATE, data)

    wav = amc.read_wav(path)

    assert wav.rate == RATE
    assert wav.samples.shape == data.shape
    assert wav.samples.dtype == np.float64
    assert wav.dtype == "float32"
    assert wav.is_float32 is True
    assert wav.channels == 2
    assert wav.frames == RATE
    np.testing.assert_allclose(wav.samples, data, rtol=1e-6)


def test_read_wav_promotes_mono_to_one_column(tmp_path):
    path = tmp_path / "mono.wav"
    wavfile.write(path, RATE, sine(1000, 0.5).astype(np.float32))
    assert amc.read_wav(path).samples.shape == (RATE, 1)


def test_read_wav_reports_the_stored_dtype_without_rejecting_it(tmp_path):
    int_path = tmp_path / "int16.wav"
    wavfile.write(int_path, RATE, (sine(1000, 0.5) * 32767).astype(np.int16))
    wav = amc.read_wav(int_path)
    assert wav.dtype == "int16"
    assert wav.is_float32 is False
    assert wav.samples.dtype == np.float64

    f64_path = tmp_path / "float64.wav"
    wavfile.write(f64_path, RATE, sine(1000, 0.5).astype(np.float64))
    wav = amc.read_wav(f64_path)
    assert wav.dtype == "float64"
    assert wav.is_float32 is False, "float64 is not the f32 the tap delivers"


def test_write_float_wav_round_trips(tmp_path):
    path = tmp_path / "out.wav"
    x = sine(440, 0.3, rate=16_000)
    amc.write_float_wav(path, 16_000, x)
    rate, back = wavfile.read(path)
    assert rate == 16_000
    assert back.dtype == np.float32
    np.testing.assert_allclose(back, x, rtol=1e-6)


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-q"]))
