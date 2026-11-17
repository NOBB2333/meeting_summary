# 脚本入口

在仓库根目录执行。Windows 需要 Rust、Visual Studio C++ 构建工具、Windows SDK（含 `fxc.exe`）和命令行可用的 7-Zip；首次准备会下载项目固定版本的 FFmpeg。

## Windows 本机打包

```powershell
.\scripts\setup.ps1
.\scripts\dev.ps1 build -Release
.\scripts\package.ps1 -Release
```

便携目录为 `release\OpenCap`，压缩包为 `release\OpenCap-windows-x64.zip`。默认是本机测试包；公开发行时需按 `THIRD_PARTY.md` 提供对应源码，使用 `-CorrespondingSourceDir <目录>`。开发运行使用 `.\scripts\dev.ps1`，测试和静态检查分别使用 `.\scripts\dev.ps1 test`、`.\scripts\dev.ps1 lint -- -D warnings`。

## macOS / Linux

```sh
./scripts/setup.sh
./scripts/dev.sh build --release
./scripts/package.sh
```

开发运行使用 `./scripts/dev.sh`。macOS/Linux 的默认调试运行复用 `target/debug`；Windows 开发运行以及带 `--release` 的开发运行使用 `target/opencap-dev`，正式构建使用 `target/release`。不同目标目录首次构建都会编译依赖。

## 其他脚本

- `runtime.ps1`、`toolchain.ps1`、`test-runtime.ps1`：Windows 依赖、工具链和运行库检查，由开发、打包和 CI 调用。
- `benchmark.ps1`、`diagnose-windows.ps1`：Windows 性能测量和诊断。
- `package-macos.py`、`package-linux.py`、`release.py`、`build-ffmpeg.sh`、`linux/`：跨平台打包及 GitHub Actions 发行工具。
- `create-upgrade-fixtures.sh`、`score-*-poc.py`：合成测试素材及模型评估工具；`tests/` 覆盖打包和发布。
