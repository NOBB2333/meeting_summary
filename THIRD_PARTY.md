# Third-party software

- GPUI 0.2.2: Apache-2.0, https://github.com/zed-industries/zed/tree/main/crates/gpui
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
