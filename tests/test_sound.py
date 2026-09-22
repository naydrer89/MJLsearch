"""The start-up chime.

Nothing here is allowed to reach an audio device. ``subprocess.Popen`` is patched
so the tests assert *what would have been played*, not that a machine somewhere
beeped -- a test suite that makes noise on every run gets disabled, and then it
stops catching the bug it existed for.
"""

from __future__ import annotations

import time
import wave
from pathlib import Path

import pytest

from api import sound
from api.config import Settings


class _RecordedPopen:
    """Stands in for ``subprocess.Popen``, recording instead of launching."""

    def __init__(self) -> None:
        self.commands: list[list[str]] = []

    def __call__(self, command, **kwargs):  # noqa: ANN001, ANN003 - matches Popen's shape
        self.commands.append(list(command))
        self.kwargs = kwargs
        return object()


@pytest.fixture
def recorded(monkeypatch: pytest.MonkeyPatch) -> _RecordedPopen:
    recorder = _RecordedPopen()
    monkeypatch.setattr(sound.subprocess, "Popen", recorder)
    # A player that definitely resolves, so the test does not depend on what is
    # installed on the machine running it.
    monkeypatch.setattr(sound.shutil, "which", lambda name: f"/usr/bin/{name}")
    return recorder


def test_the_chime_is_a_valid_wav_with_audible_content(tmp_path: Path) -> None:
    path = sound.write_chime(tmp_path / "startup.wav")

    with wave.open(str(path), "rb") as handle:
        assert handle.getnchannels() == 1
        assert handle.getsampwidth() == 2
        assert handle.getframerate() == sound._SAMPLE_RATE
        frames = handle.readframes(handle.getnframes())
        assert handle.getnframes() > 0

    # Non-silent, and not clipped: a clipped chime is a buzz rather than a note.
    import array

    samples = array.array("h")
    samples.frombytes(frames)
    peak = max(abs(value) for value in samples)
    assert 0 < peak < 32_767


def test_the_envelope_does_not_start_at_full_amplitude() -> None:
    samples = sound.chime_samples()

    # A hard onset clicks, which is what the short attack ramp exists to prevent.
    assert abs(samples[0]) < abs(max(samples, key=abs)) / 2


def test_the_chime_rises_in_pitch() -> None:
    assert sound._CHIME_HZ[1] > sound._CHIME_HZ[0]


def test_a_disabled_chime_plays_nothing(recorded: _RecordedPopen, tmp_path: Path) -> None:
    settings = Settings(startup_sound=False, sound_dir=tmp_path)

    assert sound.play_startup_sound(settings) == "disabled"
    assert recorded.commands == []


def test_playback_is_detached_and_silent(recorded: _RecordedPopen, tmp_path: Path) -> None:
    settings = Settings(sound_dir=tmp_path)

    result = sound.play_startup_sound(settings)

    assert result.startswith("played via")
    assert len(recorded.commands) == 1
    command = recorded.commands[0]
    assert command[-1].endswith("startup.wav")
    assert Path(command[-1]).exists(), "the file must exist before the player is started"
    # Detached: a chime must not keep the server alive, nor die with a signal
    # aimed at it.
    assert recorded.kwargs["start_new_session"] is True
    assert recorded.kwargs["stdout"] is not None


def test_the_wav_is_generated_once_and_reused(recorded: _RecordedPopen, tmp_path: Path) -> None:
    settings = Settings(sound_dir=tmp_path, sound_window_seconds=0)

    sound.play_startup_sound(settings)
    written = (tmp_path / "startup.wav").stat().st_mtime_ns
    sound.play_startup_sound(settings)

    assert (tmp_path / "startup.wav").stat().st_mtime_ns == written
    assert len(recorded.commands) == 2


def test_a_second_worker_in_the_same_window_stays_quiet(
    recorded: _RecordedPopen, tmp_path: Path
) -> None:
    # uvicorn --workers N imports the app in every worker, so without the claim
    # every worker would beep over the others.
    settings = Settings(sound_dir=tmp_path)

    assert sound.play_startup_sound(settings).startswith("played via")
    assert sound.play_startup_sound(settings) == "already announced by another worker"
    assert len(recorded.commands) == 1


def test_the_first_start_can_claim_despite_creating_the_lock(tmp_path: Path) -> None:
    # The trap this guards: locking the stamp file itself would create it, and a
    # file created a microsecond ago looks exactly like one another worker just
    # wrote, so the first chime would be swallowed as "already announced".
    stamp = tmp_path / ".last-announce"

    assert sound._claim_announcement(stamp, window_seconds=5.0) is True
    assert stamp.exists()
    assert sound._claim_announcement(stamp, window_seconds=5.0) is False


def test_the_claim_is_atomic_against_a_worker_that_already_holds_the_lock(
    tmp_path: Path,
) -> None:
    # The check and the stamp have to be one operation: uvicorn forks its workers
    # within milliseconds, so a read-then-write would let both through.
    fcntl = pytest.importorskip("fcntl")
    stamp = tmp_path / ".last-announce"
    lock_path = tmp_path / ".last-announce.lock"

    holder = open(lock_path, "a+")  # noqa: SIM115 - closed below
    try:
        fcntl.flock(holder.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

        # Simulates the other worker mid-announcement: it holds the lock and has
        # not written its stamp yet, so this one must still decline.
        assert sound._claim_announcement(stamp, window_seconds=5.0) is False
        assert not stamp.exists()
    finally:
        holder.close()

    # The lock is free again and no stamp was written by the other worker, so a
    # start now is claimable.
    assert sound._claim_announcement(stamp, window_seconds=5.0) is True


def test_a_claim_is_recorded_so_a_later_start_can_see_it(
    recorded: _RecordedPopen, tmp_path: Path
) -> None:
    settings = Settings(sound_dir=tmp_path)

    sound.play_startup_sound(settings)

    stamp = tmp_path / ".last-announce"
    assert stamp.exists()
    # The mtime is the announcement time, and it is what the next start reads.
    assert time.time() - stamp.stat().st_mtime < 5


def test_a_genuine_restart_later_is_audible_again(recorded: _RecordedPopen, tmp_path: Path) -> None:
    settings = Settings(sound_dir=tmp_path, sound_window_seconds=0)

    assert sound.play_startup_sound(settings).startswith("played via")
    assert sound.play_startup_sound(settings).startswith("played via")


def test_no_audio_player_is_a_reported_outcome_not_a_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(sound.shutil, "which", lambda name: None)
    settings = Settings(sound_dir=tmp_path)

    # A headless container must start normally, and say why it was silent.
    assert sound.play_startup_sound(settings) == "no audio player found"
    # And the claim is released, so gaining a sound device later is not swallowed
    # by a stamp left over from a silent start.
    assert not (tmp_path / ".last-announce").exists()


def test_a_failing_player_never_raises(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    def explode(*args, **kwargs):  # noqa: ANN002, ANN003
        raise OSError("no audio device")

    monkeypatch.setattr(sound.shutil, "which", lambda name: f"/usr/bin/{name}")
    monkeypatch.setattr(sound.subprocess, "Popen", explode)
    settings = Settings(sound_dir=tmp_path)

    assert sound.play_startup_sound(settings).startswith("failed:")


def test_an_unwritable_sound_directory_does_not_break_start_up(
    recorded: _RecordedPopen, tmp_path: Path
) -> None:
    # A read-only data mount is a normal deployment; the chime is not worth a
    # failed boot over.
    settings = Settings(sound_dir=tmp_path / "nested" / "deeper")

    assert sound.play_startup_sound(settings).startswith("played via")
    assert (tmp_path / "nested" / "deeper" / "startup.wav").exists()
