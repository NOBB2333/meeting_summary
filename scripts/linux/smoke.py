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


def run(*args, **kwargs):
    return subprocess.check_output([str(v) for v in args], text=True, timeout=60, **kwargs).strip()


def start(*args):
    # Host services use their own libraries; only OpenCap and its helper programs
    # should load the application's private runtime.
    environment = os.environ.copy()
    environment.pop("LD_LIBRARY_PATH", None)
    environment.pop("FONTCONFIG_FILE", None)
    process = subprocess.Popen([str(v) for v in args], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    processes.append(process)
    return process


def decoded_frame(path, last=False):
    # The distributed FFmpeg deliberately enables a small set of encoders.
    # BMP is included; rawvideo is not needed by the application.
    filters = ["-vf", "reverse"] if last else []
    data = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(path), *filters, "-frames:v", "1", "-c:v", "bmp", "-pix_fmt", "bgr24", "-f", "image2pipe", "-"])
    assert data[:2] == b"BM"
    offset = struct.unpack_from("<I", data, 10)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    assert struct.unpack_from("<H", data, 28)[0] == 24
    stride = (width * 3 + 3) // 4 * 4
    pixels = b"".join(data[offset + row * stride:offset + row * stride + width * 3] for row in range(abs(height)))
    return width, abs(height), pixels


def inspect(path, audio=False):
    if audio:
        raw = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "1", "-ar", "16000", "-"])
        samples = [v[0] for v in struct.iter_unpack("<f", raw)]
        assert len(samples) > 16000, f"Missing audio: {path}"
        rms = math.sqrt(sum(s * s for s in samples) / len(samples))
        assert rms > 0.01, f"Audio is silent (RMS {rms}): {path}"
        return {"file": str(path), "seconds": len(samples) / 16000, "rms": rms}
    width, height, raw = decoded_frame(path)
    assert (width, height, len(raw)) == (640, 360, 640 * 360 * 3), "Unexpected captured dimensions"
    # xsetroot paints the virtual root red; verify real pixels were captured.
    red = sum(1 for i in range(0, len(raw), 3) if raw[i + 2] > 180 and raw[i + 1] < 70 and raw[i] < 70)
    assert red > 640 * 360 * 0.8, "X11 capture did not contain the synthetic desktop"
    return {"file": str(path), "red_pixels": red}


try:
    desktop = start("Xvfb", ":91", "-screen", "0", "1920x1080x24", "-nolisten", "tcp", "-ac", "-noreset")
    start("pulseaudio", "-n", "--daemonize=no", "--exit-idle-time=-1", "--disable-shm=yes", "--load=module-native-protocol-unix socket=" + str(output / "pulse.sock") + " auth-anonymous=1", "--load=module-null-sink sink_name=opencap_test rate=48000 channels=2")
    deadline = time.monotonic() + 15
    while True:
        if desktop.poll() is not None:
            raise RuntimeError("Cannot start the dedicated virtual desktop; refusing to use another display")
        try:
            run("pactl", "info")
            run("xsetroot", "-solid", "#f00000")
            break
        except subprocess.CalledProcessError:
            if time.monotonic() > deadline:
                raise RuntimeError("Synthetic desktop/audio services did not start")
            time.sleep(0.1)
    tone = output / "tone.wav"
    run("ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "60", "-ac", "2", tone)
    start("paplay", "--device=opencap_test", tone)
    print(run(build / "opencap-bench", "sources"), flush=True)
    print(run(build / "opencap-bench", "audio-probe", "--seconds", "4"), flush=True)
    results = []
    cases = (
        ("screen", ["--no-audio", "--seconds", "4"], False),
        ("screen-audio", ["--seconds", "4"], False),
        ("pause", ["--no-audio", "--seconds", "6", "--pause-at", "1", "--pause-for", "2"], False),
        ("audio", ["--audio-only", "--seconds", "4"], True),
        ("audio-pause", ["--audio-only", "--seconds", "6", "--pause-at", "1", "--pause-for", "2"], True),
    )
    for name, arguments, audio in (() if "--gui-only" in sys.argv else cases):
        destination = output / name
        destination.mkdir(exist_ok=True)
        for old in destination.glob("*.mp4" if not audio else "*.m4a"):
            old.unlink()
        recording_env = os.environ.copy()
        if audio:
            recording_env.pop("DISPLAY", None)
            recording_env.pop("WAYLAND_DISPLAY", None)
        began = time.monotonic()
        text = run(build / "opencap-bench", "record", "--region", "0,0,640,360", "--codec", "h264", "--encoder", "libx264", "--fps", "10", "--output", destination, *arguments, env=recording_env)
        elapsed = time.monotonic() - began
        (destination / "stats.json").write_text(text + "\n")
        stats = json.loads(text)
        assert stats["state"] == "Finished", stats
        requested = int(arguments[arguments.index("--seconds") + 1])
        assert stats["duration_seconds"] >= requested - (2 if "pause" in name else 0) - 0.75, stats
        if "pause" in name:
            assert stats["duration_seconds"] <= elapsed - 1.5, stats
        path = Path(stats["output"])
        assert path.is_file() and ".partial." not in path.name, path
        assert path.suffix == (".m4a" if audio else ".mp4"), path
        results.append(inspect(path, audio))
        if name == "screen-audio":
            results.append(inspect(path, audio=True))
    if "--skip-gui" not in sys.argv:
        # Run the actual application against an isolated home, then capture its
        # rendered X11 window. CLI-only checks cannot detect graphics startup errors.
        manager = start("openbox", "--sm-disable")
        time.sleep(0.5)
        assert manager.poll() is None, "The virtual desktop window manager did not start"
        gui_env = os.environ.copy()
        gui_env["OPENCAP_LOG"] = "debug"
        if os.environ.get("OPENCAP_TEST_PRESENT_MODE") == "immediate":
            gui_env["MESA_VK_WSI_PRESENT_MODE"] = "immediate"
        for variable, name in (("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data")):
            directory = output / name
            directory.mkdir(exist_ok=True)
            gui_env[variable] = str(directory)
        with (output / "gui.log").open("w") as log:
            gui = subprocess.Popen([str(build / "opencap")], env=gui_env, stdout=log, stderr=log)
            processes.append(gui)
            deadline = time.monotonic() + 20
            window = None
            while time.monotonic() < deadline:
                if gui.poll() is not None:
                    raise RuntimeError("GUI exited during startup; see target/verify/linux-smoke/gui.log")
                sources = run(build / "opencap-bench", "sources")
                for line in sources.splitlines():
                    if line.startswith("Window(") and "\tOpenCap\t" in line:
                        window = line.split(")", 1)[0][7:]
                        break
                if window:
                    break
                time.sleep(0.2)
            assert window, "GUI did not create its X11 window"
            destination = output / "gui"
            destination.mkdir(exist_ok=True)
            # A mapped window can precede the first software-GPU frame. Poll for
            # painted content and inspect the last frame, with a bounded deadline.
            deadline = time.monotonic() + 25
            rendered = False
            while time.monotonic() < deadline:
                if gui.poll() is not None:
                    raise RuntimeError("GUI exited before drawing its contents; see gui.log")
                text = run(build / "opencap-bench", "record", "--window", window, "--no-audio", "--seconds", "2", "--fps", "2", "--codec", "h264", "--encoder", "libx264", "--output", destination)
                (destination / "stats.json").write_text(text + "\n")
                path = Path(json.loads(text)["output"])
                width, height, raw = decoded_frame(path, last=True)
                run("ffmpeg", "-v", "error", "-y", "-i", path, "-vf", "reverse", "-frames:v", "1", output / "gui.png")
                red = sum(1 for i in range(0, len(raw), 3) if raw[i + 2] > 180 and raw[i + 1] < 70 and raw[i] < 70)
                colors = len({raw[i:i + 3] for i in range(0, len(raw) - 2, 30)})
                print("GUI content: red background {:.1%}, {} sampled colors".format(red / (width * height), colors), flush=True)
                rendered = red < width * height * 0.5 and colors > 64
                if rendered:
                    break
            if not rendered:
                with (output / "gui-threads.log").open("w") as diagnostic:
                    subprocess.run(["gdb", "-batch", "-ex", "set pagination off", "-ex", "attach {}".format(gui.pid), "-ex", "thread apply all bt 12", "-ex", "detach"], stdout=diagnostic, stderr=diagnostic, timeout=45)
                with (output / "gui-window.log").open("w") as diagnostic:
                    for command in (["xwininfo", "-all", "-id", window], ["xprop", "-id", window], ["xrandr", "--current"]):
                        subprocess.run(command, stdout=diagnostic, stderr=diagnostic, timeout=5)
                # An explicit unmap/map helps diagnose missed initial visibility
                # events. A failure remains a failure even if this wakes it up.
                run("xdotool", "windowunmap", window)
                time.sleep(0.2)
                run("xdotool", "windowmap", window)
                time.sleep(3)
                text = run(build / "opencap-bench", "record", "--window", window, "--no-audio", "--seconds", "2", "--fps", "2", "--codec", "h264", "--encoder", "libx264", "--output", destination)
                run("ffmpeg", "-v", "error", "-y", "-i", json.loads(text)["output"], "-vf", "reverse", "-frames:v", "1", output / "gui-remapped.png")
                text = run(build / "opencap-bench", "record", "--region", "0,0,1920,1080", "--no-audio", "--seconds", "2", "--fps", "2", "--codec", "h264", "--encoder", "libx264", "--output", destination)
                run("ffmpeg", "-v", "error", "-y", "-i", json.loads(text)["output"], "-vf", "reverse", "-frames:v", "1", output / "gui-desktop.png")
                run("xdotool", "windowactivate", "--sync", window)
                run("xdotool", "key", "F13")
                time.sleep(1)
                text = run(build / "opencap-bench", "record", "--window", window, "--no-audio", "--seconds", "2", "--fps", "2", "--codec", "h264", "--encoder", "libx264", "--output", destination)
                run("ffmpeg", "-v", "error", "-y", "-i", json.loads(text)["output"], "-vf", "reverse", "-frames:v", "1", output / "gui-key-event.png")
                run("xdotool", "windowsize", window, "1210", "830")
                time.sleep(1)
                text = run(build / "opencap-bench", "record", "--window", window, "--no-audio", "--seconds", "2", "--fps", "2", "--codec", "h264", "--encoder", "libx264", "--output", destination)
                run("ffmpeg", "-v", "error", "-y", "-i", json.loads(text)["output"], "-vf", "reverse", "-frames:v", "1", output / "gui-resized.png")
            assert rendered, "GUI did not draw its contents within 25 seconds; see gui.png and gui.log"
            results.append({"gui_started": True, "gui_rendered": True})
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
