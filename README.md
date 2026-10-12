# OpenCap

Windows / macOS 本地开源录屏工具。Rust + GPUI + FFmpeg；Windows 使用 Windows Graphics Capture / WASAPI，macOS 使用 ScreenCaptureKit / CoreAudio / VideoToolbox。完全本地运行，无账号、无云服务、无遥测。

目标不是做第二个 OBS，而是一个**点一下就能录**的轻量工具：全屏 / 窗口 / 区域录制，系统声音和麦克风，暂停继续，托盘和全局快捷键，输出 MP4（可选同名 M4A）。针对 PPT、文档、浏览器、IDE 这类桌面内容做了低码率优化，同时支持录制游戏和视频等高动态画面。

---

## 一、直接使用（不想编译）

1. 拿到 `OpenCap-windows-x64.zip`。
2. **解压整个压缩包**，不要只把 `opencap.exe` 拖出来。
3. 双击解压后 `OpenCap` 文件夹里的 `opencap.exe`。

FFmpeg 的 DLL 已经放在 `opencap.exe` 旁边，不需要另外安装 FFmpeg，也不需要装运行库。

要求：Windows 11 x64（Windows 10 较新版本也可）。

**macOS**：直接打开 `release/OpenCap.app`，FFmpeg 动态库已放在应用包内，无需安装虚拟声卡。源码后端支持 macOS 13+，应用包的最低版本同时取决于捆绑库；本机 Homebrew 库要求 macOS 26，当前本机包因此要求 macOS 26+。首次使用在「系统设置 → 隐私与安全性 → 屏幕与系统音频录制」允许 OpenCap；录制麦克风还需要独立的麦克风权限。macOS 的默认输出位置是 `~/Movies/OpenCap`。

**设置已允许但仍无法获取目标**：开发包使用临时签名，重新构建后系统可能仍保留旧签名的授权。先退出应用，在录屏权限列表移除旧 OpenCap，再添加**当前的** `OpenCap.app` 并重新打开。只关掉再打开权限开关可能无法修复旧签名。应用会显示 ScreenCaptureKit 的实际错误，并提供「打开权限设置」「刷新录制目标」。使用固定 Apple Development / Developer ID 签名可避免每次构建改变授权身份，见下面的 macOS 构建说明。

第一次使用建议：

- 在“设置”里确认录制范围边框选的是「开始显示 3 秒」或「关闭」。
- 想确认麦克风到底有没有声音，打开「电平检测」（见下面的“声音”）。

---

## 二、怎么录

### 录制目标

- **全屏**：录制选定显示器的全部画面。
- **窗口**：列表可展开/收起，列数随窗口宽度自适应。Windows 使用实时缩略图；macOS 14+ 使用窗口快照，刷新列表可更新；macOS 13 使用窗口名称。选中后只录这个窗口，窗口移动、缩放都会跟随；不可见时保留最后画面。
- **区域**：进入后拖动框选，**松开鼠标即可继续调整**，不需要按 Enter：

  - 拖动选框内部：移动位置
  - 拖动四边或四角：缩放
  - 点工具栏「开始录制」：确认并开始
  - `Esc` 或右键：取消
  - 已经有区域时，点「调整区域」可以再进来改

### 录制操作

- 开始 / 停止：主界面按钮、`Ctrl+Alt+F9`、或托盘右键菜单
- 暂停 / 继续：主界面按钮、`Ctrl+Alt+F10`、或托盘右键菜单
- 关闭主窗口 = 最小化到托盘，不会退出录制；从托盘选「退出」会先停止并保存
- 录制中主界面左侧显示**实时录制预览**（复用编码帧，不额外占用一次采集）

### 文件

默认保存到系统「视频」目录下的 `OpenCap` 文件夹，文件名按开始时间：

```
OpenCap_2026-10-09_12-44-26.mp4
OpenCap_2026-10-09_12-44-26.m4a     # 可选，独立音频
OpenCap_2026-10-09_12-44-26.recording.json
```

`.m4a` 直接从 MP4 的混合音轨无损重封装，不会再采集一次，方便以后做录音转写。可在设置里关掉。

录制过程中写的是 `*.partial.mp4` 碎片文件，正常停止后整理成上面的正式文件。如果程序异常退出，主界面会出现「恢复未完成录制」。

### 托盘

Windows 托盘图标是两张卡通图：**未录制是绿色，录制中是橙红色**。macOS 使用菜单栏录制图标，颜色随状态改变。鼠标悬停提示只显示 `OpenCap`。

---

## 三、声音

### 系统声音和麦克风

- 两路可以独立开关，**录制中和暂停中都能随时改**，也能换设备，切换时只有改动的那一路会短暂静音。
- 界面录制始终预留一条 AAC 音轨，所以两路都关着开始录，中途再打开也不影响文件结构。
- 使用「系统默认设备」时，在开启该路的瞬间解析当前默认设备；换了 Windows 默认输出后，关闭再开启该路即可切换。macOS 系统声音由 ScreenCaptureKit 采集所有输出设备的声音，麦克风可单独选择。

### 麦克风降噪

设置里的「麦克风降噪」（默认开启）是一条实时人声链：

- 100 Hz 高通：去掉直流偏移、低频轰鸣和桌面震动
- 512 点 STFT 频域降噪：按频段学习环境噪声底，再用 Wiener 增益压掉宽带嘶嘶声；合成稳态噪声测试下降超过 12 dB，实际效果随环境和设备变化
- 滑动最小包络噪声门：安静时把残余底噪再压低约 24 dB，说话时立即打开；不会硬切静音，轻声说话不会被切掉
- 3:1 压缩 + 3 dB 补偿：把人声抬起来，接近会议软件的听感
- 慢速 AGC（约 2 秒释放 / 100 毫秒压制）：把偏小的麦克风人声拉到稳定电平，同时不会把安静时的环境噪声抽上来
- 混音末级是软限幅而不是硬削波：系统声加麦克风超过 0 dB 时平滑压回，不会把波峰削平产生咔嗒声

如果声音仍然像“电流麦”一样噼啪响，先做两步排查：

1. 用 `opencap-bench.exe audio-probe --seconds 8 --microphone`。如果 `digital_steps` 或 `nonfinite` 明显不为 0，说明是设备/驱动侧的问题（常见于开启了“音频增强/AGC”的阵列麦克风），不是软件降噪。
2. 在 Windows「声音设置 → 输入设备 → 属性 → 高级」里关闭该麦克风的音频增强，或在「设备属性 → 附加设备属性 → 增强」里禁用全部音效，再重新录一段对比。

不想做降噪时可以在设置里关掉，关掉后写入的是麦克风原始采样。降噪同时用于录制和空闲时的「电平检测」，所以电平条显示的就是实际会写入的声音。

音频按采样数维护连续时间轴，抵消回调时间抖动；暂停继续后重建采集起点。降噪延迟约 10.7 ms，会在暂停和停止时补齐尾部。队列暂时积压时发出告警并继续采集，避免一次拥塞导致后续声音全部丢失。系统声保持原始采样，不经过麦克风降噪。

### 系统声音录不到时

Windows 的系统声音走扬声器设备的 WASAPI 回环。如果换了默认输出（例如切到 HDMI 或蓝牙耳机）后才开始录，回环会绑定当时解析到的设备。排查顺序：

1. 打开「电平检测」，播放一段声音，确认「系统声音」电平条是否跳动。
2. 不跳就关掉「系统声音」再打开，让它重新解析当前默认设备；或在设置里手动选择正确的扬声器。
3. 仍然不跳，用命令行诊断（需要发布包里的 FFmpeg DLL 在 PATH 上或与 exe 同目录）：
   `opencap-bench.exe audio-probe --seconds 8 --device "<设备ID>"`。`peak` 为 0 说明这个端点当前没有回环数据。

录制过程中如果系统声音或麦克风连续 12 秒没有任何信号，录制结束的统计里会出现对应告警。

### 实时电平检测

设置里或声音区域右上角的「电平检测」开关。打开后，在**没有录制**的情况下也会临时打开所选设备，只显示系统声音和麦克风的实时电平，**不写入任何文件**，关闭即停止。这样你能在开录前确认设备选对了、麦克风是不是真的有声音。

电平条按 -60 dBFS 起算，安静时读数为空，不再出现“什么都没干也有一半进度条”的情况。

---

## 四、从源码构建（Windows）

要求：

- Windows 11 x64
- Rust 1.97.1（`rust-toolchain.toml` 已固定）
- Visual Studio C++ Build Tools
- Windows SDK，**含 Shader Tools（fxc.exe）**，GPUI 的 release 构建需要
- LLVM / Clang（bindgen 需要；找不到时设置 `LIBCLANG_PATH` 指向 LLVM 的 `bin`）
- 7-Zip（`scoop install 7zip`）

```powershell
./scripts/setup.ps1          # 下载固定版本 FFmpeg 并校验 SHA256
./scripts/dev.ps1 build      # 编译 debug
./scripts/dev.ps1 run        # 编译并运行
./scripts/dev.ps1 test       # 跑测试
./scripts/dev.ps1 build -Release   # 编译 release
```

`dev.ps1` 会自动准备缺失的 FFmpeg 开发库和 DLL，只影响当前进程的 PATH，不改系统环境；构建完成后把匹配版本的运行库复制到 `target/debug` 或 `target/release`，所以可以直接双击 EXE。

**关于图标**：仓库里没有 `.ico`，只有两张 128×128 的 JPG：

```
crates/app/resources/opencap-idle.jpg        未录制（绿色）
crates/app/resources/opencap-recording.jpg   录制中（橙红色）
```

构建时 `crates/app/build.rs` 会把它们缩放成 16/20/24/32/40/48/64/128 多尺寸图标并嵌入 EXE，
同时用于托盘和窗口图标。换图标只需要替换这两张 JPG 再重新构建，不需要手工生成或维护 `.ico`。
建议用正方形图片，非正方形会按比例居中裁切。

**关于 FFmpeg DLL**：
`avcodec-63.dll`、`avformat-63.dll`、`avutil-61.dll`、`swresample-7.dll`、`swscale-10.dll` 是第三方构建产物，不提交 Git。仓库里只有源码；第一次构建会自动下载 FFmpeg 9.0.2 shared（gyan.dev）并校验 SHA256。所以从 Git 克隆后按上面的命令构建，不会出现“找不到 avcodec-63.dll”。

---

## 五、打包与发布

先构建，再打包：

```powershell
./scripts/dev.ps1 build -Release
./scripts/package.ps1 -Release
```

产物固定在 `release/`：

```
release/OpenCap/                    便携目录，双击 opencap.exe 即可运行
release/OpenCap-windows-x64.zip     可直接发给别人的压缩包
```

`package.ps1` 的行为：

- 不带参数也能跑，默认打包 `target/debug`；加 `-Release` 打包 `target/release`
- 不带 `-CorrespondingSourceDir` 时生成**测试包**，并打印一行提醒。测试包功能完整、可以放心转发
- 已经存在的 `release/OpenCap` 和 ZIP 会直接替换，重新打包不用手动删
- 打包过程中会用一个「只有系统 PATH」的干净环境启动一次程序，确认 DLL 确实是从 EXE 同目录加载的

正式公开分发（例如上传到 GitHub Release）需要带上匹配的第三方源码：

```powershell
./scripts/package.ps1 -Release -CorrespondingSourceDir <FFmpeg 等对应源码目录>
```

这会在包内附带 `source/`、`rust-vendor/` 和 `corresponding-source/`。原因见 `THIRD_PARTY.md`：本项目以 GPL-3.0 发布，所捆绑的 FFmpeg 构建包含 GPL 组件。

也可以用 `-BuildDirectory <目录>` 指定别的构建输出目录，用 `-OutputDirectory <目录>` 改交付目录。

---

## 六、目录说明

| 目录 | 用途 | 是否提交 |
| --- | --- | --- |
| `crates/` | Rust 源码：core / platform / media / policy / app / bench | 是 |
| `scripts/` | 构建、打包、验收脚本 | 是 |
| `docs/` | 架构与验收说明 | 是 |
| `.deps/` | 自动下载的 FFmpeg 开发库与 DLL | 否 |
| `target/` | Cargo 构建缓存，以及 `target/verify/` 下的测试输出 | 否 |
| `release/` | 唯一的交付目录（便携文件夹 + ZIP） | 否 |

除 `release/` 之外，所有中间产物和测试输出都在 `target/` 下，`cargo clean` 或直接删掉都可以，重新构建会自己生成。

---

## 七、命令行与 Benchmark

```powershell
./scripts/dev.ps1 bench sources
./scripts/dev.ps1 bench probe --output target/verify/capabilities.json
./scripts/dev.ps1 bench record --seconds 20 --output target/verify/recordings
./scripts/dev.ps1 bench record --seconds 20 --microphone --pause-at 5 --pause-for 3 --output target/verify/audio
./scripts/dev.ps1 bench record --region 100,100,1280,720 --seconds 10 --no-audio --output target/verify/region
./scripts/dev.ps1 bench fixture --scene ppt --seconds 300 --encoder hevc_qsv --output target/verify/ppt
./scripts/dev.ps1 bench encode --input reference.mp4 --encoder libx264 --output target/verify/reference
./scripts/benchmark.ps1 -Seconds 300
```

`fixture` 提供原创的 PPT、文档、IDE、滚动和高动态动画素材，是可重复的合成测试，**不等于真实 Office / IDE / 浏览器 / 视频播放验收**。`record` 用来做真实应用的端到端测试。正式验收先跑 5 分钟方案，再跑 1 小时素材，文字可读性需要人工确认。

正常启动 OpenCap 不会执行 Benchmark，也不会采集或编码屏幕；空闲状态只轮询托盘事件。

编码、体积和参数细节见 `docs/architecture.md` 与 `docs/validation.md`。

---

## 八、macOS / Linux

macOS 后端已实现显示器 / 窗口 / 物理像素区域采集、系统声音、麦克风及设备切换、区域编辑、菜单栏控制、范围提示和 VideoToolbox 编码。采集和系统交互使用一个薄的 Swift C ABI 桥接层，录制状态、时间轴、降噪、混音、编码和文件保存都在 Rust 中。无需 Python 或虚拟音频驱动。

构建要求：macOS 13+、Rust、完整 Xcode（含 Metal 编译器）、Homebrew FFmpeg 开发库和 pkgconf：

```bash
brew install ffmpeg pkgconf
# 若 xcrun 找不到 metal：
xcodebuild -downloadComponent MetalToolchain
scripts/setup.sh
scripts/dev.sh test
scripts/dev.sh build --release
scripts/package.sh
open release/OpenCap.app
```

`package.sh` 默认打包 release，递归捆绑 FFmpeg 和其依赖 `.dylib`，重写加载路径并检查签名。`--profile debug` 可打包调试版。默认临时签名供本机评审；有签名证书时使用 `scripts/package.sh --identity "Apple Development: ..."` 或环境变量 `OPENCAP_SIGNING_IDENTITY`。持续使用同一签名身份可以保留跨版本权限。本机打包不会发布或上传，也没有执行 notarization；公开分发还需准备对应源码和第三方许可证（见 `THIRD_PARTY.md`）。

命令行 `target/release/opencap-bench` 支持同样的探测和录制命令。裸命令行进程的系统授权可能与应用包不同，实机操作优先使用 `OpenCap.app`。

Linux / PipeWire 后端尚未实现，当前仍会显式编译失败。

---

## 九、已知限制

- 单个显示器、单个窗口或单屏区域；不拼接多显示器，不做游戏注入捕获
- 仅 SDR；HDR 显示器需要先切到 SDR
- 受保护内容是否允许捕获由操作系统决定
- 音频设备不可用时给出提示并补静音，不会静默换到别的麦克风
- 降噪是本地的高通 + STFT 频域降噪 + 噪声门 + 压缩，不做回声消除，也不使用神经网络降噪模型
- 当前采集路径为原生采集 → CPU BGRA → FFmpeg，硬件编码器接收 CPU 上传帧，端到端 GPU 零拷贝尚未启用
- 「200–300 MB/小时」是 1080p 低动态桌面场景的优化目标，不是普遍保证；游戏、视频、4K 会明显更大
- GPU 占用无法采集时报告为 `null`

---

## 十、许可

GPL-3.0-or-later。GPUI 使用 Apache-2.0；FFmpeg 构建包含 GPL 组件。发布二进制时附带许可证、源码及构建记录，详见 [THIRD_PARTY.md](THIRD_PARTY.md)。
