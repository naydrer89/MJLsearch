"""An audible start-up chime.

Three constraints shape this module, and all three argue for keeping it tiny:

* **No new dependency.** The tone is synthesised with the standard library's
  :mod:`wave` and :mod:`math`, so nothing is added to the environment for what is
  a convenience feature.
* **It must never break start-up.** Playback is best effort. A missing audio
  device, a headless container, or a failed decode is a debug log line, never an
  exception. A search server that refuses to boot because it could not beep would
  be absurd.
* **It must not block.** ``Popen`` rather than ``run``, so start-up latency is
  unaffected by however long the audio device takes to come up.

The chime is generated once and cached on disk, which keeps start-up cheap on
every run after the first.
"""

from __future__ import annotations

import array
import logging
import math
import os
import shutil
import struct
import subprocess
import sys
import time
import wave
from pathlib import Path

from api.config import Settings

try:  # pragma: no cover - platform dependent
    import fcntl

except ImportError:  # pragma: no cover - Windows
    fcntl = None  # type: ignore[assignment]

logger = logging.getLogger(__name__)

# Two rising notes a fifth apart: recognisable as an alert, short enough that it
# is not irritating when restarts are frequent.
_CHIME_HZ = (587.33, 880.0)
_NOTE_SECONDS = 0.16
_SAMPLE_RATE = 44_100
_AMPLITUDE = 0.28

# First match wins. Every one of these is a plain playback client that takes a
# filename, so no format negotiation is needed. `ffplay` and `afplay` are last
# because they are the heaviest and the most platform-specific.
_PLAYERS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("pw-play", ()),
    ("paplay", ()),
    ("aplay", ("-q",)),
    ("ffplay", ("-nodisp", "-autoexit", "-loglevel", "quiet")),
    ("afplay", ()),
    ("play", ("-q",)),
)


def _envelope(index: int, total: int) -> float:
    """Exponential decay, with a short attack to avoid a click on note onset.

    A raw sine that starts and stops at full amplitude produces an audible click,
    and a chime made of clicks sounds broken.
    """
    attack = max(1, total // 20)
    position = index / total
    decay = math.exp(-4.5 * position)
    ramp = min(1.0, index / attack)
    return decay * ramp


def chime_samples() -> array.array:
    """Synthesises the chime as 16-bit mono PCM samples.

    Notes are concatenated rather than mixed: mixing two sines needs stereo or a
    carefully chosen amplitude and buys nothing for two short sequential tones.
    """
    samples = array.array("h")
    per_note = int(_SAMPLE_RATE * _NOTE_SECONDS)

    for frequency in _CHIME_HZ:
        for index in range(per_note):
            value = math.sin(2 * math.pi * frequency * index / _SAMPLE_RATE)
            samples.append(int(value * _envelope(index, per_note) * _AMPLITUDE * 32_767))

    return samples


def write_chime(path: Path) -> Path:
    """Writes the chime to `path`, creating parent directories as needed."""
    path.parent.mkdir(parents=True, exist_ok=True)
    samples = chime_samples()

    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(_SAMPLE_RATE)
        # `array` is little-endian on every platform this runs on; the explicit
        # byte order is stated rather than assumed, since a wrong-endian WAV is
        # silence or noise rather than an error.
        handle.writeframes(struct.pack(f"<{len(samples)}h", *samples))

    return path


def _player_command(sound_file: Path) -> list[str] | None:
    """Returns the command that plays `sound_file`, or None if nothing can."""
    for name, arguments in _PLAYERS:
        executable = shutil.which(name)
        if executable is not None:
            return [executable, *arguments, str(sound_file)]
    return None


def _recently_announced(stamp: Path, window_seconds: float) -> bool:
    """Whether another process announced a start this recently.

    uvicorn with ``--workers N`` imports the application in every worker, so
    without this the sound would play once per worker. The window is short enough
    that a genuine restart minutes later is still audible.
    """
    try:
        age = time.time() - stamp.stat().st_mtime
    except OSError:
        return False
    return age < window_seconds


def _claim_announcement(stamp: Path, window_seconds: float) -> bool:
    """Atomically claims the right to announce this start.

    The check and the stamp have to be one operation. Reading the timestamp,
    deciding to play, and only then writing the file leaves a window in which two
    workers starting together both read "no recent announcement" and both play --
    which is exactly what happens with ``uvicorn --workers 2``, because the
    workers are forked within milliseconds of each other.

    The lock lives in its own file rather than on the stamp, and that separation
    is the whole trick: opening the stamp to lock it *creates* it, and a file that
    was just created looks exactly like a file another worker just wrote, so the
    freshest possible claim would read as "already announced" and the very first
    chime would be swallowed. The lock file's own timestamp is never read, so
    creating it is harmless.
    """
    # Fast path: somebody announced recently, and no lock is needed to see that.
    if _recently_announced(stamp, window_seconds):
        return False

    lock_path = stamp.with_name(f"{stamp.name}.lock")
    try:
        stamp.parent.mkdir(parents=True, exist_ok=True)
        lock = open(lock_path, "a+")  # noqa: SIM115 - closed below; the lock lives on the fd
    except OSError:
        # An unwritable directory must not silence a machine that could beep.
        return True

    try:
        if fcntl is not None:
            try:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                # Another process is announcing right now.
                return False

        # Re-checked under the lock, which is what makes this authoritative: any
        # worker that got here first has already written its stamp by now.
        if _recently_announced(stamp, window_seconds):
            return False

        stamp.write_text(f"{os.getpid()} {time.time():.3f}\n", encoding="utf-8")
        return True
    except OSError:
        return True
    finally:
        lock.close()


def play_startup_sound(settings: Settings) -> str | None:
    """Plays the chime, returning a short description of what happened.

    The return value exists for the log line and for tests; callers are not
    expected to branch on it. Nothing here raises.
    """
    if not settings.startup_sound:
        return "disabled"

    # Claimed before the WAV is written, not merely before it is played. Two
    # workers writing the same file at once can interleave and leave a truncated
    # WAV, which is a screech rather than a chime.
    stamp = settings.sound_dir / ".last-announce"
    if not _claim_announcement(stamp, settings.sound_window_seconds):
        return "already announced by another worker"

    sound_file = settings.sound_dir / "startup.wav"
    command: list[str] | None = None

    try:
        if not sound_file.exists():
            write_chime(sound_file)

        command = _player_command(sound_file)
        if command is None:
            return _release(stamp, "no audio player found")

        # Detached and silent: the chime must not keep the process alive, must
        # not inherit the server's stdout, and must not be killed by a signal
        # aimed at the server.
        subprocess.Popen(  # noqa: S603 - the command is built from a resolved path
            command,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    except Exception as error:  # noqa: BLE001 - a chime must never break start-up
        logger.debug("startup sound failed", extra={"detail": str(error)})
        return _release(stamp, f"failed: {error}")

    return f"played via {Path(command[0]).name}"


def _release(stamp: Path, outcome: str) -> str:
    """Gives back an unused claim.

    Without this, a machine with no audio device would stamp every start as
    "announced" and would swallow the announcement for the next few seconds, so a
    container that later gained a sound device would stay silent.
    """
    try:
        stamp.unlink()
    except OSError:
        pass
    return outcome


def main() -> int:
    """Plays the chime on demand: ``python -m api.sound``."""
    logging.basicConfig(level=logging.INFO, stream=sys.stderr)
    from api.config import get_settings

    result = play_startup_sound(get_settings())
    print(result, file=sys.stderr)
    return 0 if result != "no audio player found" else 1


if __name__ == "__main__":
    raise SystemExit(main())
