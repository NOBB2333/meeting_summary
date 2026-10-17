#!/usr/bin/env python3
"""Xvfb + a synthetic PulseAudio sink: no personal desktop or microphone."""
import json
import math
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

build = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release").resolve()
output = Path("target/verify/linux-smoke").resolve()
output.mkdir(parents=True, exist_ok=True)
os.environ["DISPLAY"] = ":91"
os.environ["XDG_SESSION_TYPE"] = "x11"
os.environ["XDG_RUNTIME_DIR"] = str(output / "runtime")
Path(os.environ["XDG_RUNTIME_DIR"]).mkdir(mode=0o700, exist_ok=True)
os.environ["PULSE_SERVER"] = "unix:" + str(output / "pulse.sock")
processes = []


def run(*args):
    return subprocess.check_output([str(v) for v in args], text=True).strip()


def start(*args):
    process = subprocess.Popen([str(v) for v in args], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    processes.append(process)
    return process


def inspect(path, audio=False):
    if audio:
        raw = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "1", "-ar", "16000", "-"])
        samples = [v[0] for v in struct.iter_unpack("<f", raw)]
        assert len(samples) > 16000, f"Missing audio: {path}"
        rms = math.sqrt(sum(s * s for s in samples) / len(samples))
        assert rms > 0.01, f"Audio is silent (RMS {rms}): {path}"
        return {"file": str(path), "seconds": len(samples) / 16000, "rms": rms}
    raw = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(path), "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
    assert len(raw) == 640 * 360 * 3, "Unexpected captured dimensions"
    # xsetroot paints the virtual root red; verify real pixels were captured.
    red = sum(1 for i in range(0, len(raw), 3) if raw[i] > 180 and raw[i + 1] < 70 and raw[i + 2] < 70)
    assert red > 640 * 360 * 0.8, "X11 capture did not contain the synthetic desktop"
    return {"file": str(path), "red_pixels": red}


try:
    start("Xvfb", ":91", "-screen", "0", "800x600x24", "-nolisten", "tcp", "-ac")
    start("pulseaudio", "-n", "--daemonize=no", "--exit-idle-time=-1", "--disable-shm=yes", "--load=module-native-protocol-unix socket=" + str(output / "pulse.sock") + " auth-anonymous=1", "--load=module-null-sink sink_name=opencap_test rate=48000 channels=2")
    deadline = time.monotonic() + 15
    while True:
        try:
            run("pactl", "info")
            run("xsetroot", "-solid", "#f00000")
            break
        except subprocess.CalledProcessError:
            if time.monotonic() > deadline:
                raise RuntimeError("Synthetic desktop/audio services did not start")
            time.sleep(0.1)
    tone = output / "tone.wav"
    run("ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "40", "-ac", "2", tone)
    start("paplay", "--device=opencap_test", tone)
    print(run(build / "opencap-bench", "sources"), flush=True)
    print(run(build / "opencap-bench", "audio-probe", "--seconds", "4"), flush=True)
    results = []
    for name, arguments, audio in (
        ("screen", ["--no-audio", "--seconds", "4"], False),
        ("pause", ["--no-audio", "--seconds", "6", "--pause-at", "1", "--pause-for", "2"], False),
        ("audio", ["--audio-only", "--seconds", "4"], True),
    ):
        destination = output / name
        destination.mkdir(exist_ok=True)
        for old in destination.glob("*.mp4" if not audio else "*.m4a"):
            old.unlink()
        text = run(build / "opencap-bench", "record", "--region", "0,0,640,360", "--codec", "h264", "--encoder", "libx264", "--fps", "10", "--output", destination, *arguments)
        (destination / "stats.json").write_text(text + "\n")
        stats = json.loads(text)
        assert stats["state"] == "Finished", stats
        if name == "pause":
            assert 2.5 <= stats["duration_seconds"] < 5, stats
        paths = list(destination.glob("*.m4a" if audio else "*.mp4"))
        assert len(paths) == 1 and ".partial." not in paths[0].name, paths
        results.append(inspect(paths[0], audio))
    (output / "report.json").write_text(json.dumps(results, indent=2) + "\n")
    print("Linux X11 capture, cursor path, pause/resume, M4A and decoded audio checks passed", flush=True)
finally:
    for process in reversed(processes):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
