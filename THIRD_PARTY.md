# Third-party software

- GPUI Kit 0.7.3 / GPUI Base / GPUI Component: Apache-2.0, https://github.com/longbridge/gpui-kit (with pinned gpui-pre 0.3.9, Apache-2.0). GPUI Fast is not enabled.
- gpui-pre-wgpu 0.3.9: the Apache-2.0 snapshot is vendored in `vendor/gpui-pre-wgpu`, retaining its license. The local adapter-selection patch prefers OpenGL for CPU window presentation, avoiding a verified blank-window failure in Mesa software Vulkan; hardware and headless priorities are preserved. Changes are listed in `PATCHES.txt`; the license is included in `LICENSES/gpui-pre-wgpu.txt`.
- sherpa-onnx / Rust C API wrapper 1.13.8: Apache-2.0, https://github.com/k2-fsa/sherpa-onnx. Uses the official static native archive, including ONNX Runtime and the archive's third-party libraries. Include their notices and corresponding sources where required when distributing.
- ASR weights are downloaded separately: streaming Zipformer Chinese 14M INT8 2023-02-23 and SenseVoiceSmall INT8 2024-07-17; Silero VAD is used for offline segmentation. Model weights and training-data terms are independent of the Rust wrapper's license; inspect the specific upstream model cards before publishing a model bundle. Local caches are not packaged into the application.
- Optional speaker models: sherpa-onnx Pyannote segmentation 3.0 INT8 (archive includes CNRS MIT LICENSE), plus 3D-Speaker ERes2Net-base Chinese ONNX (3D-Speaker toolkit Apache-2.0; verify the specific weight/model-card terms before redistribution). Models are downloaded to local cache, never bundled with the app. Sources: https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/models.html and https://github.com/modelscope/3D-Speaker
- windows-capture 2.0.1: MIT, https://github.com/NiiightmareXD/windows-capture
- wasapi 0.25.0: MIT, https://github.com/HEnquist/wasapi-rs
- ffmpeg-next / ffmpeg-sys-next 9.0.0: WTFPL, https://github.com/zmwangx/rust-ffmpeg
- FFmpeg 9.0.2 shared distribution: https://www.gyan.dev/ffmpeg/builds/
- macOS development builds: locally installed Homebrew FFmpeg (this machine: 8.0.1), with bundled dylib dependencies. Apple frameworks are supplied by macOS. The small Swift bridge is project source.

The pinned FFmpeg archive SHA256 is `4d2060a8b34a940aa47d785142055bb92a63053781e55f2ace4546edd519a8f5`.
The build includes GPL components such as x264/x265 and is used under GPL-compatible terms. No nonfree build is permitted in release packaging.

Record the exact output of `ffmpeg -version`, `ffmpeg -buildconf`, and library versions in each release. A release must include the exact FFmpeg source, patches, build scripts, and corresponding sources for enabled external libraries, together with their notices. The upstream FFmpeg tarball alone is not the complete corresponding source for a full third-party build.

The development dependency download is not itself a complete release source bundle. The macOS packager produces a local review bundle; it does not collect complete corresponding sources or notarize/publish the app. Public distributions must include the exact sources and licenses for the Homebrew libraries actually bundled, including their build configuration and patches. `scripts/package.ps1` supports a corresponding-source directory and does not publish or upload releases. A publisher must verify that this directory covers the actual bundled build.

Rust transitive dependencies are recorded in Cargo.lock; use `cargo vendor --locked` to provide their exact sources with published distributions. Do not package local recordings, settings, absolute-path logs, or private test fixtures in public releases.

- Native Word export: docx-rs 0.4.22 (MIT), pulldown-cmark 0.13.4 (MIT); exact dependencies including zip are in Cargo.lock. Exported DOCX files contain ordinary OOXML, no macros, executable scripts, downloaded fonts, or external image dependencies.

- Linux build: FFmpeg 9.0.2 from https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz (SHA256 `8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e`), GPL with Debian x264. The build script, version/configuration output and source tarball are recorded. NASM 2.16.03 is a build tool, pinned and verified separately; SIMD remains enabled.
- Linux private C++ runtime: conda-forge libstdcxx-ng 12.2.0, GPL-3.0 with GCC Runtime Library Exception 3.1; https://anaconda.org/conda-forge/libstdcxx-ng. Exact architecture-specific package hashes are pinned in `scripts/linux/cxx-runtime.sh`; its recipe, package metadata and license exception accompany the test bundle. This library is private to OpenCap, never installed over the OS copy. Runtime dependencies are recorded in `runtime-provenance.json`; all packaged ELF files are audited for glibc <= 2.28.
- Linux bundled Noto CJK and DejaVu fonts retain their Debian copyright/license files. X11/PulseAudio and desktop/font library notices are collected per package; the test bundle still requires complete corresponding sources before a formal public release, as described above.

- Windows packages include application-local `vcruntime140.dll` and `vcruntime140_1.dll` from the installed Visual Studio x64 redistributable directory. These remain subject to Microsoft license terms; see `LICENSES/Microsoft-Visual-Cpp-Runtime.txt`. They are not copied from system directories. Clean-PATH validation checks that the software-codec fixture loads the packaged FFmpeg and VC runtime, and launches the transcription CLI.
