#!/usr/bin/env python3
"""Create a portable Linux test bundle and a UOS/Debian package on glibc 2.28."""
import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = (2, 28)
# The loader, libc family and graphics drivers must come from the host OS.
HOST_LIBS = re.compile(r"^(linux-vdso|ld-linux|lib(c|m|pthread|dl|rt|resolv|util|anl|nss_.*)\.so|lib(GL|EGL|GLES|GLX|GLdispatch|vulkan)\b)")


def run(*args, **kwargs):
    return subprocess.check_output([str(a) for a in args], text=True, **kwargs).strip()


def dependencies(path):
    output = run("ldd", path)
    if "not found" in output:
        raise RuntimeError(f"Unresolved build dependency of {path}:\n{output}")
    result = []
    for line in output.splitlines():
        match = re.match(r"\s*(\S+) => (/\S+)", line)
        if match and not HOST_LIBS.match(match[1]):
            result.append((match[1], Path(match[2])))
    return result


def audit(directory):
    report = []
    for path in sorted(directory.rglob("*")):
        if not path.is_file():
            continue
        with path.open("rb") as handle:
            if handle.read(4) != b"\x7fELF":
                continue
        output = run("readelf", "--version-info", "--wide", path)
        versions = {tuple(map(int, v.split("."))) for v in re.findall(r"\bGLIBC_(\d+\.\d+(?:\.\d+)?)\b", output)}
        maximum = max(versions, default=(0, 0))
        if maximum > BASELINE:
            raise RuntimeError(f"{path.name} requires glibc {maximum}; expected <= {BASELINE}")
        needed = run("patchelf", "--print-needed", path).splitlines()
        for library in needed:
            if not HOST_LIBS.match(library) and not (directory / "usr/lib" / library).is_file():
                raise RuntimeError(f"Missing bundled dependency: {path.name} -> {library}")
        report.append({"file": str(path.relative_to(directory)), "glibc": ".".join(map(str, maximum)), "needed": needed})
    if not report:
        raise RuntimeError("No Linux executables were packaged")
    return report


def package(build_dir, output):
    metadata = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", cwd=ROOT))
    package_version = next(package["version"] for package in metadata["packages"] if package["name"] == "opencap")
    arch = {"x86_64": ("x64", "amd64"), "aarch64": ("arm64", "arm64")}.get(platform.machine())
    if arch is None:
        raise RuntimeError(f"Unsupported package architecture: {platform.machine()}")
    output.mkdir(parents=True, exist_ok=True)
    stage = output / f".OpenCap-linux-{arch[0]}.build"
    if stage.exists():
        shutil.rmtree(stage)
    bin_dir, lib_dir = stage / "usr/bin", stage / "usr/lib"
    for directory in (bin_dir, lib_dir, stage / "licenses", stage / "fonts"):
        directory.mkdir(parents=True, exist_ok=True)
    copied, provenance = {}, []

    def bundle(source, destination):
        source = source.resolve()
        shutil.copy2(source, destination)
        for name, dependency in dependencies(source):
            if name not in copied:
                copied[name] = dependency.resolve()
                bundle(dependency, lib_dir / name)
            elif copied[name] != dependency.resolve():
                raise RuntimeError(f"Conflicting library {name}: {copied[name]} / {dependency}")
        run("patchelf", "--set-rpath", "$ORIGIN/../lib" if destination.parent == bin_dir else "$ORIGIN", destination)

    for name in ("opencap", "opencap-bench", "opencap-transcribe"):
        source = build_dir / name
        if not source.is_file():
            raise RuntimeError(f"Missing executable: {source}; run scripts/dev.sh build --release")
        bundle(source, bin_dir / name)
    for name in ("ffmpeg", "pactl"):
        source = shutil.which(name)
        if not source:
            raise RuntimeError(f"Missing runtime program: {name}")
        bundle(Path(source), bin_dir / name)
    # These are opened at runtime by the window/font and TLS integrations.
    available = run("ldconfig", "-p")
    for name in ("libxkbcommon.so.0", "libxkbcommon-x11.so.0", "libX11.so.6", "libXcursor.so.1", "libwayland-client.so.0", "libfontconfig.so.1", "libfreetype.so.6"):
        if name in copied:
            continue
        match = re.search(r"\s" + re.escape(name) + r" .* => (\S+)", available)
        if not match:
            raise RuntimeError(f"Missing desktop runtime library: {name}")
        copied[name] = Path(match[1]).resolve()
        bundle(Path(match[1]), lib_dir / name)
    for name in ("LICENSE", "THIRD_PARTY.md", "README.md"):
        shutil.copy2(ROOT / name, stage / name)
    (stage / "build-scripts").mkdir()
    for name in ("build-ffmpeg.sh", "package-linux.py", "linux/Dockerfile", "linux/cxx-runtime.sh"):
        shutil.copy2(ROOT / "scripts" / name, stage / "build-scripts" / Path(name).name)
    shutil.copytree(ROOT / "LICENSES", stage / "licenses/project")
    for name, source in sorted(copied.items()):
        owner = subprocess.run(["dpkg-query", "-S", str(source)], capture_output=True, text=True)
        package_name = owner.stdout.split(": ", 1)[0] if owner.returncode == 0 else None
        version = run("dpkg-query", "-W", "-f=${Version}", package_name) if package_name else None
        if package_name:
            copyright_file = Path("/usr/share/doc") / package_name.split(":")[0] / "copyright"
            if copyright_file.is_file():
                shutil.copy2(copyright_file, stage / "licenses" / f"{package_name.replace(':', '-')}.txt")
        provenance.append({"file": name, "source": str(source), "package": package_name, "version": version})
    fonts = [Path("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"), Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"), Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")]
    for font in fonts:
        shutil.copy2(font, stage / "fonts" / font.name)
    for name in ("fonts-noto-cjk", "fonts-dejavu-core"):
        shutil.copy2(Path("/usr/share/doc") / name / "copyright", stage / "licenses" / f"{name}.txt")
    shutil.copy2(build_dir / "opencap-assets/OpenCap.png", stage / "OpenCap.png")
    (stage / "fonts.conf").write_text('''<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig><include ignore_missing="yes">/etc/fonts/fonts.conf</include><dir prefix="relative">fonts</dir><cachedir prefix="xdg">opencap/fontconfig</cachedir></fontconfig>
''')
    launcher = stage / "AppRun"
    launcher.write_text('''#!/bin/sh
set -eu
app_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
export PATH="$app_dir/usr/bin:$PATH"
export FONTCONFIG_FILE="$app_dir/fonts.conf"
exec "$app_dir/usr/bin/opencap" "$@"
''')
    launcher.chmod(0o755)
    (stage / "OpenCap.desktop").write_text('''[Desktop Entry]
Type=Application
Name=OpenCap
Comment=Screen recording and meeting notes
Exec=/opt/OpenCap/AppRun
Icon=/opt/OpenCap/OpenCap.png
Terminal=false
StartupWMClass=OpenCap
Categories=AudioVideo;Recorder;
''')
    source_dir = Path("/opt/ffmpeg/share/opencap-source")
    if source_dir.is_dir():
        shutil.copytree(source_dir, stage / "ffmpeg-source")
    if Path("/opt/compat/info").is_dir():
        shutil.copytree("/opt/compat/info", stage / "licenses/cxx-runtime-build")
        shutil.copytree("/opt/compat/share/licenses", stage / "licenses/cxx-runtime")
        shutil.copytree("/opt/compat/source", stage / "cxx-runtime-package")
    (stage / "ffmpeg-version.txt").write_text(run("ffmpeg", "-version") + "\n")
    (stage / "ffmpeg-buildconf.txt").write_text(run("ffmpeg", "-buildconf") + "\n")
    (stage / "runtime-provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    (stage / "elf-audit.json").write_text(json.dumps(audit(stage), indent=2) + "\n")
    (stage / "TEST-PACKAGE.txt").write_text("OpenCap Linux 测试包。解压后运行 AppRun，或安装同架构 .deb。\n支持 glibc 2.28+、X11 录屏、PulseAudio / PipeWire-Pulse 音频。\nWayland 暂仅支持音频和文稿；图形界面需要可用的 Vulkan / OpenGL 驱动。\n保留整个目录；不覆盖系统 glibc、图形驱动。\n正式公开发布需按 THIRD_PARTY.md 补齐所有所附库的对应源码。\n")
    archive = output / f"OpenCap-linux-{arch[0]}.tar.gz"
    with tarfile.open(archive, "w:gz") as handle:
        handle.add(stage, arcname="OpenCap")
    deb = output / f".deb-{arch[1]}"
    if deb.exists():
        shutil.rmtree(deb)
    shutil.copytree(stage, deb / "opt/OpenCap")
    desktop = deb / "usr/share/applications"
    desktop.mkdir(parents=True)
    shutil.copy2(stage / "OpenCap.desktop", desktop / "OpenCap.desktop")
    control = deb / "DEBIAN"
    control.mkdir()
    (control / "control").write_text(f"Package: opencap\nVersion: {package_version}\nArchitecture: {arch[1]}\nMaintainer: OpenCap contributors\nDepends: libc6 (>= 2.28), libvulkan1, xdg-utils\nSection: video\nPriority: optional\nDescription: Native screen recording and meeting notes\n")
    subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(deb), str(output / f"OpenCap-linux-{arch[0]}.deb")], check=True)
    shutil.rmtree(deb)
    destination = output / f"OpenCap-linux-{arch[0]}"
    if destination.exists():
        shutil.rmtree(destination)
    stage.rename(destination)
    print(f"{archive}\n{destination}\nELF dependencies verified for glibc <= 2.28")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-directory", type=Path, default=ROOT / "target/release")
    parser.add_argument("--output-directory", type=Path, default=ROOT / "release")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Run in the Linux build container")
    package(args.build_directory.resolve(), args.output_directory.resolve())
