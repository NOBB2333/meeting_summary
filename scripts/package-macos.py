#!/usr/bin/env python3
"""Bundle native dylibs recursively; never require the recipient's Homebrew."""
import argparse
import os
import plistlib
import shutil
import subprocess
from pathlib import Path


def run(*args):
    return subprocess.check_output(args, text=True)


def dependencies(path):
    return [line.strip().split(" (", 1)[0] for line in run("otool", "-L", str(path)).splitlines()[1:]]


def rpaths(path):
    lines = run("otool", "-l", str(path)).splitlines()
    return [lines[i + 2].strip().split("path ", 1)[1].split(" (offset", 1)[0]
            for i, line in enumerate(lines) if line.strip() == "cmd LC_RPATH"]


def minimum_macos(path):
    versions = [(13, 0)]
    command = None
    for line in run("otool", "-l", str(path)).splitlines():
        fields = line.split()
        if fields[:1] == ["cmd"]:
            command = fields[1]
        if fields and ((command == "LC_BUILD_VERSION" and fields[0] == "minos") or
                       (command == "LC_VERSION_MIN_MACOSX" and fields[0] == "version")):
            versions.append(tuple(int(part) for part in fields[1].split(".")))
    return max(versions)


def package(profile, identity, build=True):
    root = Path(__file__).resolve().parent.parent
    destination = root / "release" / "OpenCap.app"
    executable_prefix = str(destination / "Contents/MacOS/opencap")
    for line in run("ps", "-axo", "pid=,command=").splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) != 2:
            continue
        process_command = fields[1]
        if (process_command == executable_prefix or
                process_command.startswith(executable_prefix + " ") or
                process_command.startswith(executable_prefix + ".bin ")):
            raise SystemExit("OpenCap is still running. Quit it from the app before packaging; replacing a running app leaves its process and macOS permission signature out of sync.")
    if build:
        command = [str(root / "scripts/dev.sh"), "build", "-p", "opencap"]
        if profile == "release":
            command.append("--release")
        subprocess.run(command, cwd=root, check=True)
    binary = root / "target" / profile / "opencap"
    if not binary.is_file():
        raise SystemExit(f"Build first: scripts/dev.sh build {'--release' if profile == 'release' else ''}")
    staging = root / "release" / ".OpenCap.app.build"
    if staging.exists():
        shutil.rmtree(staging)
    macos = staging / "Contents" / "MacOS"
    frameworks = staging / "Contents" / "Frameworks"
    resources = staging / "Contents" / "Resources"
    for directory in (macos, frameworks, resources):
        directory.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, macos / "opencap")
    shutil.copy2(root / "crates/app/resources/Info.plist", staging / "Contents/Info.plist")
    for icon in ("OpenCap.icns", "OpenCap-recording.icns"):
        source = binary.parent / "opencap-assets" / icon
        if not source.is_file():
            raise RuntimeError(f"Missing generated application icon: {source}; rebuild the application")
        shutil.copy2(source, resources / icon)
    for name in ("LICENSE", "THIRD_PARTY.md", "README.md"):
        shutil.copy2(root / name, resources / name)
    shutil.copytree(root / "LICENSES", resources / "LICENSES")
    (resources / "docs").mkdir()
    shutil.copy2(root / "docs/transcription-selection.md", resources / "docs/transcription-selection.md")
    copied = {}
    search = rpaths(binary)

    def resolve(name, source):
        if name.startswith("@rpath/"):
            suffix = name[len("@rpath/"):]
            for directory in rpaths(source) + search:
                candidate = Path(directory.replace("@loader_path", str(source.parent))) / suffix
                if candidate.is_file():
                    return candidate.resolve()
            raise RuntimeError(f"Cannot resolve {name} from {source}")
        if name.startswith("@loader_path/"):
            return (source.parent / name[len("@loader_path/"):]).resolve()
        return Path(name).resolve()

    def bundle(source, target):
        # Keep the signature load command while editing. Removing it first can
        # leave __LINKEDIT padding that older Xcode install_name_tool rejects.
        # Every edited Mach-O is signed again after dependency rewriting.
        for name in dependencies(source):
            if name.startswith(("/System/", "/usr/lib/")):
                continue
            dep = resolve(name, source)
            if dep == source:
                continue  # dylib install ID, not a dependency
            if dep not in copied:
                dest = frameworks / dep.name
                if dest.exists():
                    raise RuntimeError(f"Conflicting dylib filename: {dep.name}")
                copied[dep] = dest
                shutil.copy2(dep, dest)
                bundle(dep, dest)
                run("install_name_tool", "-id", f"@rpath/{dep.name}", str(dest))
            replacement = f"@rpath/{copied[dep].name}"
            if name != replacement:
                run("install_name_tool", "-change", name, replacement, str(target))
        for directory in rpaths(target):
            if directory.startswith("/"):
                run("install_name_tool", "-delete_rpath", directory, str(target))

    bundle(binary, macos / "opencap")
    # Homebrew bottles can require a newer OS than the native capture backend.
    # Advertise the actual maximum requirement, rather than a misleading 13.0.
    minimum = max(minimum_macos(path) for path in [macos / "opencap", *frameworks.iterdir()])
    minimum_text = ".".join(str(part) for part in minimum)
    plist_path = staging / "Contents/Info.plist"
    with plist_path.open("rb") as handle:
        info = plistlib.load(handle)
    info["LSMinimumSystemVersion"] = minimum_text
    if info.get("CFBundleIconFile") != "OpenCap.icns":
        raise RuntimeError("Application icon is missing from Info.plist")
    with plist_path.open("wb") as handle:
        plistlib.dump(info, handle)
    for library in frameworks.iterdir():
        run("codesign", "--force", "--sign", identity, str(library))
    run("codesign", "--force", "--sign", identity, str(staging))
    run("codesign", "--verify", "--deep", "--strict", str(staging))
    if destination.exists():
        shutil.rmtree(destination)
    staging.rename(destination)
    signing = "local ad-hoc; reauthorize after rebuilding" if identity == "-" else "certificate signature"
    print(f"{destination} ({len(copied)} bundled dylibs; macOS {minimum_text}+; {profile}; {signing})")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=("debug", "release"), default="release")
    parser.add_argument("--no-build", action="store_true", help="Package an existing binary without compiling; it may be older than the source")
    parser.add_argument("--identity", default=os.environ.get("OPENCAP_SIGNING_IDENTITY", "-"),
                        help="codesign identity; use Apple Development/Developer ID for stable permissions")
    args = parser.parse_args()
    package(args.profile, args.identity, not args.no_build)
