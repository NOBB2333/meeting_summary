#!/usr/bin/env python3
"""Bundle native dylibs recursively; never require the recipient's Homebrew."""
import argparse
from datetime import datetime
import os
import plistlib
import shutil
import subprocess
import tempfile
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


def running_pids(destination, process_table=None):
    """Only match the app binary itself, not a command merely mentioning it."""
    executable = str(destination / "Contents/MacOS/opencap")
    if process_table is None:
        process_table = run("ps", "-axo", "pid=,command=")
    found = []
    for line in process_table.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) != 2:
            continue
        command = fields[1]
        if any(command == prefix or command.startswith(prefix + " ")
               for prefix in (executable, executable + ".bin")):
            found.append(fields[0])
    return found


def reserve_build_path(root, label="build"):
    builds = root / "release" / "builds"
    builds.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.mkdtemp(prefix=f"{label}-{stamp}-", dir=builds)) / "OpenCap.app"


def select_destination(root, output=None):
    default = root / "release" / "OpenCap.app"
    destination = Path(os.path.abspath(os.path.expanduser(output))) if output else default
    if destination.suffix.lower() != ".app":
        raise SystemExit("The output path must end in .app")
    if destination.is_symlink():
        raise SystemExit(f"Refusing to replace a symlink: {destination}")
    if destination.exists() and destination != default:
        raise SystemExit(f"Output already exists; choose a new path: {destination}")
    pids = running_pids(destination)
    if pids and output:
        raise SystemExit(
            f"OpenCap is running from {destination} (PID {', '.join(pids)}). "
            "Quit it before replacing this exact app, or run scripts/package.sh "
            "without --output to create a separate build."
        )
    if pids:
        destination = reserve_build_path(root)
        print(f"OpenCap is running (PID {', '.join(pids)}); building a separate app at {destination}", flush=True)
    return destination


def build_bundle(root, binary, staging, identity):
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
    for name in ("transcription-selection.md", "asr-models.md", "performance-review.md",
                 "technical-current.md", "asr-improvement-plan.md", "low-power-capture-plan.md",
                 "architecture.md", "build-and-linux.md",
                 "ui-workspace-layout.md", "ui-redesign.md"):
        shutil.copy2(root / "docs" / name, resources / "docs" / name)
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
    return len(copied), minimum_text


def package(profile, identity, build=True, output=None):
    root = Path(__file__).resolve().parent.parent
    destination = select_destination(root, output)
    if build:
        command = [str(root / "scripts/dev.sh"), "build", "-p", "opencap"]
        if profile == "release":
            command.append("--release")
        subprocess.run(command, cwd=root, check=True)
    binary = root / "target" / profile / "opencap"
    if not binary.is_file():
        raise SystemExit(f"Build first: scripts/dev.sh build {'--release' if profile == 'release' else ''}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".OpenCap-staging-", dir=destination.parent) as temp:
        staging = Path(temp) / "OpenCap.app"
        count, minimum_text = build_bundle(root, binary, staging, identity)
        # A user may have opened the canonical app while the build was running.
        if running_pids(destination):
            if output:
                raise SystemExit(f"OpenCap started while building; existing app is untouched: {destination}")
            destination = reserve_build_path(root)
            print(f"OpenCap started while building; using separate app at {destination}", flush=True)
        default = root / "release" / "OpenCap.app"
        if destination.is_symlink() or (destination.exists() and destination != default):
            raise SystemExit(f"Output appeared while building; existing path is untouched: {destination}")
        previous = None
        if destination.exists():
            previous = reserve_build_path(root, "previous")
            destination.rename(previous)
        try:
            staging.rename(destination)
        except OSError:
            if previous is not None:
                previous.rename(destination)
            raise
        if previous is not None:
            print(f"Previous app retained at {previous}")
    signing = "local ad-hoc; reauthorize after rebuilding" if identity == "-" else "certificate signature"
    print(f"{destination} ({count} bundled dylibs; macOS {minimum_text}+; {profile}; {signing})")


def build_parser():
    parser = argparse.ArgumentParser()
    profiles = parser.add_mutually_exclusive_group()
    profiles.add_argument("--profile", choices=("debug", "release"))
    profiles.add_argument("--release", action="store_const", const="release", dest="profile",
                          help="Alias for --profile release")
    parser.set_defaults(profile="release")
    parser.add_argument("--no-build", action="store_true", help="Package an existing binary without compiling; it may be older than the source")
    parser.add_argument("--output", help="Write to this new .app path; an existing non-default path is never overwritten")
    parser.add_argument("--identity", default=os.environ.get("OPENCAP_SIGNING_IDENTITY", "-"),
                        help="codesign identity; use Apple Development/Developer ID for stable permissions")
    return parser


if __name__ == "__main__":
    args = build_parser().parse_args()
    package(args.profile, args.identity, not args.no_build, args.output)
