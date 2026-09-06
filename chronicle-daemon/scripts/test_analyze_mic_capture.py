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


# --- gates, candidates, report -------------------------------------------


def stereo(c0: np.ndarray, c1: np.ndarray) -> np.ndarray:
    return np.stack([c0, c1], axis=1)


def gate(native: np.ndarray, is_float32: bool = True) -> str | None:
    return amc.gate(native, RATE, is_float32)


def test_gate_0_fires_for_mono_input():
    assert gate(sine(1000, 0.5)[:, np.newaxis]).startswith("gate 0")


def test_gate_0_beats_gate_1_for_a_mono_integer_wav():
    assert gate(sine(1000, 0.5)[:, np.newaxis], is_float32=False).startswith("gate 0")


def test_gate_1_fires_for_a_stereo_wav_that_is_not_float32():
    native = stereo(sine(1000, 0.5), sine(1000, 0.5))
    assert gate(native, is_float32=False).startswith("gate 1")


def test_gate_2_fires_when_every_channel_is_silent():
    native = stereo(sine(1000, 0.001), sine(1000, 0.002))  # -63 and -57 dBFS, both under -50
    assert gate(native).startswith("gate 2")


def test_gate_2_does_not_fire_when_one_channel_is_healthy():
    assert gate(stereo(sine(1000, 0.5), sine(1000, 0.001))) is None


def test_gate_3_fires_when_every_channel_has_zero_variance():
    assert gate(stereo(np.full(RATE, 0.5), np.full(RATE, 0.5))).startswith("gate 3")


def test_gate_3_fires_when_a_channel_rms_is_not_finite():
    c0 = sine(1000, 0.5)
    c0[10] = np.nan  # one NaN sample makes that channel's RMS NaN
    assert gate(stereo(c0, sine(1000, 0.5))).startswith("gate 3")


def test_gate_2_takes_precedence_over_gate_3():
    # All-zero input satisfies both gates: every channel silent, every channel
    # zero variance. The design's order says gate 2.
    assert gate(stereo(np.zeros(RATE), np.zeros(RATE))).startswith("gate 2")


def test_candidates_are_the_four_pinned_mixes():
    c0 = np.array([1.0, 2.0])
    c1 = np.array([3.0, -2.0])
    cands = amc.candidates(stereo(c0, c1))
    np.testing.assert_allclose(cands["avg"], [2.0, 0.0])
    np.testing.assert_allclose(cands["diff"], [-1.0, 2.0])
    np.testing.assert_allclose(cands["c0"], c0)
    np.testing.assert_allclose(cands["c1"], c1)
    assert list(cands) == ["avg", "diff", "c0", "c1"]


def test_r_band_names():
    assert amc.r_band(0.9) == "high"
    assert amc.r_band(0.1) == "low"
    assert amc.r_band(-0.1) == "low"
    assert amc.r_band(-0.9) == "strongly negative"
    assert amc.r_band(0.5) == "partial"
    assert amc.r_band(-0.5) == "partial"
    assert amc.r_band(math.nan) == "undefined"


def wav(samples: np.ndarray, rate: int = RATE) -> "amc.Wav":
    if samples.ndim == 1:
        samples = samples[:, np.newaxis]
    return amc.Wav(rate=rate, samples=samples.astype(np.float64), dtype="float32")


def test_analyze_anti_correlated_stereo():
    c0 = sine(1000, 0.5)
    converted = sine(1000, 0.001)  # what a cancelling converter would emit
    report = amc.analyze(wav(stereo(c0, -c0)), wav(converted))

    assert report["gate"] is None
    assert report["pearson_r"] == pytest.approx(-1.0)
    assert report["r_band"] == "strongly negative"
    assert report["candidates"]["avg"]["rms_dbfs"] == -math.inf
    assert report["candidates"]["diff"]["rms_dbfs"] == pytest.approx(-9.03, abs=0.05)
    assert report["candidates"]["c0"]["rms_dbfs"] == pytest.approx(-9.03, abs=0.05)
    assert report["candidates"]["c0"]["clipped_samples"] == 0
    assert report["converted"]["rms_dbfs"] == pytest.approx(-63.0, abs=0.1)
    assert report["candidates"]["c0"]["transfer_gain_db"] == pytest.approx(-54.0, abs=0.2)


def test_analyze_stops_at_a_gate_before_any_filtering():
    # Two frames: sosfiltfilt would raise on input this short, so a report
    # with a gate proves the gate ran first.
    native = stereo(np.zeros(2), np.zeros(2))
    report = amc.analyze(wav(native), wav(np.zeros(2)))
    assert report["gate"].startswith("gate 2")
    assert "candidates" not in report
    assert "per_channel" not in report["native"]


# --- capture directory and CLI ----------------------------------------------


def write_capture(
    directory,
    native: np.ndarray,
    converted: np.ndarray,
    *,
    valid: bool = True,
    manifest_channels: int | None = None,
    native_rate: int = RATE,
    converted_rate: int = RATE,
    native_dtype=np.float32,
    converted_dtype=np.float32,
    native_frames: int | None = None,
    converted_frames: int | None = None,
):
    directory.mkdir(parents=True, exist_ok=True)
    wavfile.write(directory / "mic-native.wav", native_rate, native.astype(native_dtype))
    wavfile.write(directory / "mic-converted.wav", converted_rate, converted.astype(converted_dtype))
    channels = manifest_channels if manifest_channels is not None else native.shape[1]
    lines = [
        "device: test",
        f"native_channels: {channels}",
        f"native_sample_rate_hz: {native_rate}",
        f"measurement_valid: {'true' if valid else 'false'}",
    ]
    if native_frames is not None:
        lines.append(f"native_frames_written: {native_frames}")
    if converted_frames is not None:
        lines.append(f"converted_frames_written: {converted_frames}")
    (directory / "manifest.txt").write_text("\n".join(lines) + "\n")


def good_stereo(secs: float = 1.0):
    c0 = sine(1000, 0.5, secs=secs)
    return stereo(c0, 0.5 * c0), sine(1000, 0.3, secs=secs)


def test_main_writes_candidates_json_and_prints_a_report(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)

    code = amc.main([str(tmp_path)])

    assert code == 0
    for name in ["avg", "diff", "c0", "c1"]:
        rate, data = wavfile.read(tmp_path / f"candidate-{name}.wav")
        assert rate == 16_000, name
        assert data.dtype == np.float32, name
        assert data.ndim == 1, name
        assert len(data) == 16_000, name
    report = json.loads((tmp_path / "analysis.json").read_text())
    assert report["pearson_r"] == pytest.approx(1.0)
    assert report["candidates"]["c1"]["wav"].endswith("candidate-c1.wav")
    assert report["parameters"]["candidate_rate_hz"] == 16_000
    assert set(report["environment"]) >= {"python", "numpy", "scipy"}
    assert report["manifest"]["device"] == "test"
    printed = capsys.readouterr().out
    assert "pearson r" in printed
    assert "clipped" in printed
    assert "candidate-avg.wav" in printed


def test_main_refuses_an_invalid_measurement_unless_overridden(tmp_path):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    assert amc.main([str(tmp_path)]) == 0
    assert (tmp_path / "analysis.json").exists()

    write_capture(tmp_path, native, converted, valid=False)
    assert amc.main([str(tmp_path)]) == 2
    assert not (tmp_path / "candidate-avg.wav").exists(), "a refused take must not leave the earlier report behind"
    assert not (tmp_path / "analysis.json").exists()

    assert amc.main([str(tmp_path), "--allow-invalid"]) == 0
    assert (tmp_path / "candidate-avg.wav").exists()
    report = json.loads((tmp_path / "analysis.json").read_text())
    assert report["manifest"]["measurement_valid"] == "false"


def test_main_exits_3_when_a_gate_fires(tmp_path):
    mono = sine(1000, 0.5)[:, np.newaxis]
    write_capture(tmp_path, mono, sine(1000, 0.5))

    code = amc.main([str(tmp_path)])

    assert code == 3
    assert not (tmp_path / "candidate-avg.wav").exists()
    assert json.loads((tmp_path / "analysis.json").read_text())["gate"].startswith("gate 0")


def test_main_rejects_a_manifest_that_disagrees_with_the_wav(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted, manifest_channels=4)
    assert amc.main([str(tmp_path)]) == 1
    assert "native_channels" in capsys.readouterr().err


def test_main_rejects_a_converted_file_off_contract(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, stereo(converted, converted))
    assert amc.main([str(tmp_path)]) == 1
    assert "mic-converted.wav" in capsys.readouterr().err


def test_main_rejects_a_float64_native_as_gate_1(tmp_path):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted, native_dtype=np.float64)
    assert amc.main([str(tmp_path)]) == 3
    assert json.loads((tmp_path / "analysis.json").read_text())["gate"].startswith("gate 1")


def test_main_rejects_a_wav_whose_frame_count_disagrees_with_the_manifest(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted, native_frames=native.shape[0], converted_frames=len(converted))
    assert amc.main([str(tmp_path)]) == 0

    write_capture(tmp_path, native, converted, native_frames=native.shape[0] - 1, converted_frames=len(converted))
    assert amc.main([str(tmp_path)]) == 1
    assert "native_frames_written" in capsys.readouterr().err

    write_capture(tmp_path, native, converted, native_frames=native.shape[0], converted_frames=len(converted) + 480)
    assert amc.main([str(tmp_path)]) == 1
    assert "converted_frames_written" in capsys.readouterr().err


def test_analysis_json_is_strict_json_even_with_silent_and_dead_channels(tmp_path):
    c0 = sine(1000, 0.5)
    write_capture(tmp_path, stereo(c0, -c0), sine(1000, 0.001))  # avg cancels to -inf dBFS
    assert amc.main([str(tmp_path)]) == 0
    text = (tmp_path / "analysis.json").read_text()

    def refuse(token):
        raise AssertionError(f"non-finite token {token} in analysis.json")

    report = json.loads(text, parse_constant=refuse)
    assert report["candidates"]["avg"]["rms_dbfs"] == "-inf"
    assert report["candidates"]["avg"]["transfer_gain_db"] == "inf"

    write_capture(tmp_path, stereo(c0, np.full(RATE, 0.3)), sine(1000, 0.3))  # DC channel: r is NaN
    assert amc.main([str(tmp_path)]) == 0
    report = json.loads((tmp_path / "analysis.json").read_text(), parse_constant=refuse)
    assert report["pearson_r"] == "NaN"
    assert report["r_band"] == "undefined"


def test_analyze_and_main_handle_more_than_two_channels(tmp_path, capsys):
    four = np.stack([sine(1000, 0.5), sine(1000, 0.4), sine(1000, 0.3), sine(1000, 0.2)], axis=1)
    report = amc.analyze(wav(four), wav(sine(1000, 0.3)))
    assert report["gate"] is None
    assert len(report["native"]["per_channel"]) == 4
    assert "candidates" not in report and "pearson_r" not in report
    assert report["note"].startswith("4 channels")

    write_capture(tmp_path, four, sine(1000, 0.3))
    assert amc.main([str(tmp_path)]) == 0
    assert not any(tmp_path.glob("candidate-*.wav"))
    assert "4 channels" in capsys.readouterr().out
    assert "note" in json.loads((tmp_path / "analysis.json").read_text())


def test_main_refuses_a_directory_at_a_candidate_name(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    (tmp_path / "candidate-avg.wav").mkdir()
    assert amc.main([str(tmp_path)]) == 1
    assert "candidate-avg.wav" in capsys.readouterr().err


def test_main_refuses_a_symlink_at_an_owned_name(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()  # the target's parent exists, so a write through the link would succeed
    for name in amc.OWNED_OUTPUTS:
        link = tmp_path / name
        victim = elsewhere / f"victim-{name}"
        link.symlink_to(victim)  # dangling
        assert amc.main([str(tmp_path)]) == 1, name
        assert name in capsys.readouterr().err, name
        assert not victim.exists(), f"{name}: nothing may be written through a symlink"
        assert link.is_symlink(), f"{name}: the link must be left alone"
        link.unlink()
    assert list(elsewhere.iterdir()) == []


def test_main_rejects_a_native_rate_the_speech_band_cannot_use(tmp_path, capsys):
    c0 = sine(1000, 0.5, rate=6000)
    write_capture(tmp_path, stereo(c0, c0), sine(1000, 0.3), native_rate=6000)
    assert amc.main([str(tmp_path)]) == 1
    assert "speech band" in capsys.readouterr().err


def test_main_names_the_manifest_key_that_is_not_a_whole_number(tmp_path, capsys):
    native, converted = good_stereo()
    frames = native.shape[0]
    # `frames + 0.5` would truncate to a matching count if the whole-number check were missing.
    for bad in ["lots", "inf", "-inf", "1e400", "nan", "2.5", f"{frames}.5"]:
        write_capture(tmp_path, native, converted)
        with (tmp_path / "manifest.txt").open("a") as f:
            f.write(f"native_frames_written: {bad}\n")
        assert amc.main([str(tmp_path)]) == 1, bad
        assert "native_frames_written" in capsys.readouterr().err, bad
    write_capture(tmp_path, native, converted, native_frames=frames)
    with (tmp_path / "manifest.txt").open("a") as f:
        f.write(f"native_frames_written: {frames}.0\n")  # a decimal spelling of a whole number is accepted
    assert amc.main([str(tmp_path)]) == 0


def test_main_names_an_unlistable_out_directory(tmp_path, capsys):
    import os

    if os.geteuid() == 0:
        pytest.skip("root ignores directory permissions")
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    out = tmp_path / "dropbox"
    out.mkdir()
    out.chmod(0o333)  # write and search, no read
    try:
        assert amc.main([str(tmp_path), "--out", str(out)]) == 1
        assert "read permission" in capsys.readouterr().err
    finally:
        out.chmod(0o700)


def test_main_reports_a_failed_write_as_an_output_error(tmp_path, capsys):
    import os

    if os.geteuid() == 0:
        pytest.skip("root ignores directory permissions")
    # Mono stops at gate 0, so no candidate is written and the failing write
    # is the report itself.
    write_capture(tmp_path, sine(1000, 0.5)[:, np.newaxis], sine(1000, 0.5))
    out = tmp_path / "out"
    out.mkdir()
    out.chmod(0o500)
    try:
        assert amc.main([str(tmp_path), "--out", str(out)]) == 1
        assert "output error" in capsys.readouterr().err
    finally:
        out.chmod(0o700)


def test_main_rejects_a_recording_shorter_than_one_second(tmp_path, capsys):
    native, converted = good_stereo(secs=0.5)
    write_capture(tmp_path, native, converted)
    assert amc.main([str(tmp_path)]) == 1
    assert "shorter than" in capsys.readouterr().err


def test_main_rejects_a_short_converted_file(tmp_path, capsys):
    native, _ = good_stereo()
    write_capture(tmp_path, native, sine(1000, 0.3, secs=0.5))
    assert amc.main([str(tmp_path)]) == 1
    err = capsys.readouterr().err
    assert "mic-converted.wav" in err and "shorter than" in err


def test_main_owns_its_names_in_out_and_nothing_else(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    out = tmp_path / "out"
    out.mkdir()

    (out / "analysis.json").write_text("not json")
    assert amc.main([str(tmp_path), "--out", str(out)]) == 1
    assert "analysis.json" in capsys.readouterr().err
    assert (out / "analysis.json").read_text() == "not json"

    for foreign in [{"something": "else"}, {"parameters": {"lr": 0.01}, "results": []}]:
        (out / "analysis.json").write_text(json.dumps(foreign))
        assert amc.main([str(tmp_path), "--out", str(out)]) == 1, foreign
        assert (out / "analysis.json").read_text() == json.dumps(foreign)
    (out / "analysis.json").unlink()

    (out / "analysis.json").mkdir()
    assert amc.main([str(tmp_path), "--out", str(out)]) == 1
    (out / "analysis.json").rmdir()

    other = out / "candidate-mine.wav"
    other.write_bytes(b"mine")
    owned = out / "candidate-avg.wav"
    owned.write_bytes(b"whatever was here is the script's to replace")
    assert amc.main([str(tmp_path), "--out", str(out)]) == 0
    assert other.read_bytes() == b"mine"
    assert owned.read_bytes() != b"whatever was here is the script's to replace"
    assert sorted(p.name for p in out.iterdir()) == sorted(
        ["candidate-mine.wav", "analysis.json", "candidate-avg.wav", "candidate-diff.wav", "candidate-c0.wav", "candidate-c1.wav"]
    )


def test_the_temp_report_is_an_owned_name(tmp_path, capsys):
    assert "analysis.json.tmp" in amc.OWNED_OUTPUTS
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)

    stale = tmp_path / "analysis.json.tmp"
    stale.write_text("half written {")
    assert amc.main([str(tmp_path)]) == 0
    assert not stale.exists(), "a leftover temp report is cleaned up"

    (tmp_path / "elsewhere").mkdir()
    victim = tmp_path / "elsewhere" / "report.json"
    stale.symlink_to(victim)  # dangling, parent exists
    assert amc.main([str(tmp_path)]) == 1
    assert "analysis.json.tmp" in capsys.readouterr().err
    assert not victim.exists(), "nothing may be written through a symlink at the temp name"


def test_main_refuses_a_case_variant_of_an_owned_name(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    users = tmp_path / "Candidate-AVG.wav"
    users.write_bytes(b"not ours")
    case_insensitive = (tmp_path / "candidate-avg.wav").exists()

    code = amc.main([str(tmp_path)])

    assert users.read_bytes() == b"not ours", "a user's file must never be replaced"
    if case_insensitive:
        assert code == 1
        assert "different-case" in capsys.readouterr().err
    else:
        assert code == 0
        pytest.skip("case-sensitive filesystem: the different-case guard cannot fire here")


def test_main_still_cleans_up_after_the_directory_is_renamed(tmp_path):
    native, converted = good_stereo()
    take = tmp_path / "take1"
    write_capture(take, native, converted)
    assert amc.main([str(take)]) == 0
    renamed = tmp_path / "take1-renamed"
    take.rename(renamed)

    write_capture(renamed, native[:, :1], sine(1000, 0.5))  # mono now: gate 0
    assert amc.main([str(renamed)]) == 3
    assert not (renamed / "candidate-avg.wav").exists(), "the earlier run's candidates must go"
    assert json.loads((renamed / "analysis.json").read_text())["gate"].startswith("gate 0")


def test_main_rejects_a_manifest_that_is_not_utf8(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    with (tmp_path / "manifest.txt").open("ab") as f:
        f.write(b"phrase: caf\xe9\n")  # Latin-1 e-acute
    assert amc.main([str(tmp_path)]) == 1
    err = capsys.readouterr().err
    assert "manifest.txt" in err and "UTF-8" in err


def test_main_rejects_a_missing_manifest(tmp_path, capsys):
    native, converted = good_stereo()
    write_capture(tmp_path, native, converted)
    (tmp_path / "manifest.txt").unlink()
    assert amc.main([str(tmp_path)]) == 1
    assert "manifest.txt" in capsys.readouterr().err


def test_main_removes_its_own_stale_outputs_first(tmp_path):
    mono = sine(1000, 0.5)[:, np.newaxis]
    write_capture(tmp_path, mono, sine(1000, 0.5))
    (tmp_path / "candidate-avg.wav").write_bytes(b"stale")
    earlier = {key: {} for key in amc.REPORT_KEYS} | {"parameters": amc.PARAMETERS, "gate": "gate 0: earlier take"}
    (tmp_path / "analysis.json").write_text(json.dumps(earlier))

    assert amc.main([str(tmp_path)]) == 3

    assert not (tmp_path / "candidate-avg.wav").exists(), "a stale candidate must not survive a gated run"
    assert json.loads((tmp_path / "analysis.json").read_text())["gate"].startswith("gate 0")


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-q"]))
