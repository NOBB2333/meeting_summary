#!/usr/bin/env python3
"""Check versioned builds locally; prepare and publish one verified Release run."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import tarfile
import time
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PACKAGES = (
    "windows-x64.zip", "macos-arm64.zip", "macos-x64.zip",
    "linux-x64.tar.gz", "linux-x64.deb",
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check_version(tag, root=ROOT):
    match = re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)(?:\.(0|[1-9]\d*))?", tag)
    require(match, "Use a version tag such as v1.2 or v1.2.1")
    version = ".".join(part or "0" for part in match.groups())
    cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
    section = re.search(r"(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)", cargo)
    actual = re.search(r'^version\s*=\s*"([^"]+)"', section[1], re.M) if section else None
    require(actual and actual[1] == version, f"Cargo.toml must contain version {version}")
    for block in (root / "Cargo.lock").read_text(encoding="utf-8").split("[[package]]")[1:]:
        package = re.search(r'^name = "(opencap(?:-[^"]+)?)"', block, re.M)
        if package:
            locked = re.search(r'^version = "([^"]+)"', block, re.M)
            require(locked and locked[1] == version, f"Cargo.lock: {package[1]} must be {version}")
    info = plistlib.loads((root / "crates/app/resources/Info.plist").read_bytes())
    require(info["CFBundleShortVersionString"] == info["CFBundleVersion"] == version,
            f"Both macOS bundle versions must be {version}")
    require((root / "docs/releases" / f"{tag}.md").is_file(), f"Missing release notes for {tag}")
    return version


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_packages(paths, version, report):
    checks = {}
    with zipfile.ZipFile(paths["windows-x64.zip"]) as archive:
        names = archive.namelist()
        binaries = [name for name in names if name.endswith("/opencap.exe")]
        require(len(binaries) == 1, "Windows package must contain one opencap.exe")
        binary = archive.read(binaries[0])
        require(binary[:2] == b"MZ", "Windows executable is not PE")
        offset = struct.unpack_from("<I", binary, 0x3c)[0]
        require(binary[offset:offset + 4] == b"PE\0\0" and
                struct.unpack_from("<H", binary, offset + 4)[0] == 0x8664,
                "Windows executable must be x64")
        require(f"OpenCap {version}".encode() in binary, "Windows binary version differs")
        dlls = [name for name in names if name.lower().endswith(".dll")]
        require(len(dlls) >= 7 and sum(name.endswith(".exe") for name in names) == 3,
                "Windows runtime dependencies are missing")
        checks["windows"] = {"architecture": "x86_64", "version": version, "dlls": len(dlls)}
    for arch, cpu in (("arm64", 0x0100000c), ("x64", 0x01000007)):
        with zipfile.ZipFile(paths[f"macos-{arch}.zip"]) as archive:
            info = plistlib.loads(archive.read("OpenCap.app/Contents/Info.plist"))
            require(info["CFBundleShortVersionString"] == info["CFBundleVersion"] == version,
                    f"macOS {arch} bundle version differs")
            binary = archive.read("OpenCap.app/Contents/MacOS/opencap")
            require(binary[:4] == b"\xcf\xfa\xed\xfe" and
                    struct.unpack_from("<I", binary, 4)[0] == cpu,
                    f"macOS {arch} binary architecture differs")
            require(f"OpenCap {version}".encode() in binary, f"macOS {arch} binary version differs")
            require(archive.getinfo("OpenCap.app/Contents/Resources/OpenCap.icns").file_size > 0,
                    f"macOS {arch} icon is missing")
            libraries = [name for name in archive.namelist()
                         if name.startswith("OpenCap.app/Contents/Frameworks/") and name.endswith(".dylib")]
            require(libraries, f"macOS {arch} libraries are missing")
            checks[f"macos_{arch}"] = {
                "architecture": arch, "version": version,
                "minimum_macos": info["LSMinimumSystemVersion"], "bundled_dylibs": len(libraries),
            }
    with tarfile.open(paths["linux-x64.tar.gz"]) as archive:
        binary = archive.extractfile("OpenCap/usr/bin/opencap").read()
        require(binary[:5] == b"\x7fELF\x02" and struct.unpack_from("<H", binary, 18)[0] == 62,
                "Linux binary must be x86_64 ELF")
        require(f"OpenCap {version}".encode() in binary, "Linux binary version differs")
        audit = json.load(archive.extractfile("OpenCap/elf-audit.json"))
        maximum = max(tuple(map(int, entry["glibc"].split("."))) for entry in audit)
        require(maximum <= (2, 28), "Linux package exceeds glibc 2.28")
        fonts = [name for name in archive.getnames()
                 if name.startswith("OpenCap/fonts/") and name.endswith((".ttf", ".ttc"))]
        require(len(fonts) >= 3, "Linux bundled fonts are missing")
        checks["linux"] = {"architecture": "x86_64", "version": version,
                           "maximum_glibc": ".".join(map(str, maximum)), "elf_files_audited": len(audit)}
    deb = str(paths["linux-x64.deb"])
    members = subprocess.check_output(["ar", "t", deb], text=True).splitlines()
    control = next(name for name in members if name.startswith("control.tar."))
    payload = subprocess.check_output(["ar", "p", deb, control])
    with tarfile.open(fileobj=io.BytesIO(payload)) as archive:
        name = next(name for name in archive.getnames() if name in ("control", "./control"))
        text = archive.extractfile(name).read().decode()
        require(f"Version: {version}\n" in text and "Architecture: amd64\n" in text,
                "Debian package version or architecture differs")
    runtime = json.loads(report.read_text(encoding="utf-8"))
    require(any(row.get("gui_started") and row.get("gui_rendered") for row in runtime),
            "Linux GUI did not render")
    media = [row for row in runtime if "file" in row]
    cases = sorted({Path(row["file"]).parent.parent.name for row in media})
    require(cases == ["audio", "audio-pause", "pause", "screen", "screen-audio"],
            "Linux recording smoke cases are incomplete")
    require(all(row.get("red_pixels", 1) > 0 and row.get("rms", 1) > 0.001 for row in media),
            "Linux recording smoke test detected empty media")
    checks["linux"].update(runtime_gui_rendered=True, runtime_media_cases=cases)
    return checks


def current_commit():
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    require(os.environ.get("GITHUB_SHA", commit) == commit, "Checkout and workflow commits differ")
    return commit


def prepare(tag, artifacts, output):
    version = check_version(tag)
    paths = {}
    for suffix in PACKAGES:
        matches = list(artifacts.rglob(f"OpenCap-{suffix}"))
        require(len(matches) == 1, f"Expected exactly one OpenCap-{suffix}, found {len(matches)}")
        paths[suffix] = matches[0]
    reports = list(artifacts.rglob("linux-smoke/report.json"))
    require(len(reports) == 1, "Expected one Linux runtime report from this run")
    checks = verify_packages(paths, version, reports[0])
    results = json.loads(os.environ.get("BUILD_RESULTS", "{}"))
    if os.environ.get("GITHUB_ACTIONS") == "true":
        require(set(results) == {"windows", "macos", "linux"} and
                all(job["result"] == "success" for job in results.values()),
                "All three platform builds must succeed in this Release run")
        require(os.environ.get("GITHUB_REF") == f"refs/tags/{tag}", "Release must run on its version tag")
    output.mkdir(parents=True, exist_ok=True)
    records = []
    for suffix, source in paths.items():
        destination = output / f"OpenCap-{version}-{suffix}"
        shutil.copy2(source, destination)
        records.append({"name": destination.name, "size": destination.stat().st_size,
                        "sha256": sha256(destination)})
    repository = os.environ.get("GITHUB_REPOSITORY", "NOBB2333/meeting_summary")
    run_id = os.environ.get("GITHUB_RUN_ID")
    run_url = f"https://github.com/{repository}/actions/runs/{run_id}" if run_id else None
    manifest = {"tag": tag, "version": version, "commit": current_commit(), "run_id": run_id,
                "run_url": run_url, "build_results": results, "checks": checks, "assets": records}
    (output / "build-validation.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    sums = [f'{row["sha256"]}  {row["name"]}' for row in records]
    sums.append(f'{sha256(output / "build-validation.json")}  build-validation.json')
    (output / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8")
    notes = (ROOT / "docs/releases" / f"{tag}.md").read_text(encoding="utf-8")
    if run_url:
        notes += f'\n构建来源：[OpenCap {tag}]({run_url})，提交 `{manifest["commit"]}`。\n'
    notes += (f'\n已验证 Mac 最低版本：Apple Silicon {checks["macos_arm64"]["minimum_macos"]}，'
              f'Intel {checks["macos_x64"]["minimum_macos"]}；Linux glibc {checks["linux"]["maximum_glibc"]}+。\n')
    (output / "release-notes.md").write_text(notes, encoding="utf-8")
    print(f"Verified OpenCap {version}: {len(records)} packages; manifest and SHA256SUMS ready")


def api(path, method="GET", payload=None):
    args = ["gh", "api", path, "--method", method, "-H", "Cache-Control: no-cache"]
    if payload is not None:
        args += ["--input", "-"]
    return json.loads(subprocess.check_output(
        args, input=json.dumps(payload) if payload is not None else None, text=True))


def verify_uploaded(release, paths):
    assets = {row["name"]: row for row in release["assets"]}
    require(set(assets) == {path.name for path in paths}, "Remote release assets differ")
    for path in paths:
        remote = assets[path.name]
        require(remote["state"] == "uploaded" and remote["size"] == path.stat().st_size and
                remote.get("digest") == f"sha256:{sha256(path)}", f"Remote checksum differs: {path.name}")


def publish(tag, output):
    version = check_version(tag)
    manifest = json.loads((output / "build-validation.json").read_text(encoding="utf-8"))
    require(manifest["tag"] == tag and manifest["version"] == version and
            manifest["commit"] == current_commit(), "Prepared release does not match this checkout")
    require(manifest["run_id"] and manifest["run_id"] == os.environ.get("GITHUB_RUN_ID"),
            "Publish only packages prepared by this Release run")
    expected = {f"OpenCap-{version}-{suffix}" for suffix in PACKAGES}
    require({row["name"] for row in manifest["assets"]} == expected, "Prepared package set differs")
    for row in manifest["assets"]:
        path = output / row["name"]
        require(path.stat().st_size == row["size"] and sha256(path) == row["sha256"],
                f"Prepared package changed: {path.name}")
    paths = [output / row["name"] for row in manifest["assets"]]
    paths += [output / "build-validation.json", output / "SHA256SUMS"]
    require((output / "SHA256SUMS").read_text(encoding="utf-8") == "".join(
        f"{sha256(path)}  {path.name}\n" for path in paths[:-1]), "SHA256SUMS differs")
    repository = os.environ["GH_REPO"]
    require(re.fullmatch(r"[\w.-]+/[\w.-]+", repository), "Invalid repository")
    endpoint = f"repos/{repository}"
    ref = api(f"{endpoint}/git/ref/tags/{tag}")["object"]
    if ref["type"] == "tag":
        ref = api(f'{endpoint}/git/tags/{ref["sha"]}')["object"]
    require(ref["type"] == "commit" and ref["sha"] == manifest["commit"], "Remote tag points elsewhere")
    matches = [row for row in api(f"{endpoint}/releases?per_page=100") if row["tag_name"] == tag]
    require(len(matches) <= 1, "Multiple releases use this tag")
    release = matches[0] if matches else None
    if release and not release["draft"]:
        verify_uploaded(release, paths)
        print(release["html_url"])
        return
    details = {"tag_name": tag, "target_commitish": manifest["commit"], "name": f"OpenCap {tag}",
               "body": (output / "release-notes.md").read_text(encoding="utf-8"),
               "draft": True, "prerelease": False}
    if release:
        require({row["name"] for row in release["assets"]} <= {path.name for path in paths},
                "Draft contains unexpected assets; refusing to overwrite them")
        release = api(f'{endpoint}/releases/{release["id"]}', "PATCH", details)
    else:
        release = api(f"{endpoint}/releases", "POST", details)
    subprocess.run(["gh", "release", "upload", tag, "--repo", repository, "--clobber",
                    *map(str, paths)], check=True)
    for attempt in range(6):
        release = api(f'{endpoint}/releases/{release["id"]}')
        try:
            verify_uploaded(release, paths)
            break
        except ValueError:
            if attempt == 5:
                raise
            time.sleep(5)
    release = api(f'{endpoint}/releases/{release["id"]}', "PATCH", {"draft": False, "make_latest": "true"})
    require(not release["draft"], "Release remains a draft")
    print(release["html_url"])
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as summary:
            summary.write(f'Published [OpenCap {tag}]({release["html_url"]}) with five verified packages.\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check-version", "prepare", "publish"))
    parser.add_argument("--tag", required=True)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.command == "check-version":
        print(f"{args.tag}: application and macOS bundle version {check_version(args.tag)}")
    elif args.command == "prepare":
        require(args.artifacts and args.output, "prepare needs --artifacts and --output")
        prepare(args.tag, args.artifacts, args.output)
    else:
        require(args.output, "publish needs --output")
        publish(args.tag, args.output)


if __name__ == "__main__":
    main()
