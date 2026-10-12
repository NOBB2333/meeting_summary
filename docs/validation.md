# 验收规范

## 自动检查

`scripts/dev.ps1 test`：暂停时间扣除、帧调度、显式编码器选择、混音对齐/限幅、VFR 尾帧与音轨时长、无损恢复拒绝覆盖、窗口尺寸改变时保持输出尺寸。

CI/发布前还必须在一台安装 Windows SDK Shader Tools 的机器上执行一次干净 release 构建；GPUI 0.2.2 的 Windows backend 在 release 构建时使用 `fxc.exe` 编译着色器，debug 则在运行时编译。

`bench probe`：逐一尝试硬件和软件编码器，要求输出可解码，记录失败原因。

`bench record --seconds 12 --pause-at 4 --pause-for 2`：输出应约 10 秒；使用 ffprobe 检查 packet 时间戳单调、音视频轨、duration，完整解码检查错误。音画感知同步另用声画标记判断，不能仅依赖 duration。

## 基准测试

1080p 原尺寸、相同素材，比较 H.264/HEVC/AV1 的可用实现、自适应与固定 15 FPS。先 5 分钟筛选，再 1 小时验证。每次保存原始命令、配置、驱动与设备、编码器、profile/preset/GOP/码控、FPS、丢帧、文件大小、CPU/内存和可用 GPU 指标。

MB = 1,000,000 bytes。低动态 PPT/文档以 ≤300 MB/h 为目标，≤200 MB/h 是优化目标。不能以强制模糊文字、降低源尺寸或让播放加速获得通过。

文字样本在 12/14/16 px 下，原尺寸观看并抽取固定时间点人工辨认；至少 95% 样本文本正确。基准未做人工检查时必须保持 `quality_target_passed: null`。

合成 `motion` 场景只覆盖移动图形压力。真实 1080p 视频播放需要用户拥有使用权的参考片段；不可将合成结果标作真实视频质量。

## 实机手工清单

- 全屏/窗口/区域，100%/125%/150% DPI，多屏不同缩放，窗口最小化/恢复/关闭。
- 无音轨、系统声、麦克风、双路混音、拔出设备、默认设备变化。
- 暂停/继续/停止、托盘操作、快捷键被占用、主窗关闭、退出时收尾。
- HDR 拒绝、显示器移除、锁屏、编码器不可用、磁盘写满、强制结束进程后恢复。
- 一小时音画标记偏移 ≤150 ms、无持续增长的队列/内存；H.264 在 Windows 常用播放器播放，HEVC/AV1 在支持其解码的播放器播放。

实测记录在本文件后续追加，未运行的项目不可标为通过。

## 2026-10-12 macOS 一阶段补齐

本机：Apple M1 / macOS 26.5.2 / FFmpeg 8.0.1 / Rust 1.97.1。新增 ScreenCaptureKit、CoreAudio、VideoToolbox 后端和本地 `.app` 打包。没有修改二阶段 Python 项目或接入转录服务。

- Debug / Release workspace 构建通过，30 项测试通过；全 targets / 全 features Clippy `-D warnings`、Rust 格式检查通过。新增回归覆盖任意设备回调长度、双声道波形重建、末尾非整块采样以及音频时间戳抖动。一次并行测试暴露临时目录名碰撞，改用进程内原子序号后复测通过。
- H.264 / HEVC VideoToolbox 试编码和解码探测通过。实际 GUI 录制自动选择 `h264_videotoolbox`，不是软件编码。
- 系统日志明确记录旧授权签名与当前二进制 cdhash 不匹配。仅清理 OpenCap 的旧屏幕录制授权，重新添加当时的应用包后，显示器与窗口列表恢复，实际录制成功。未修改 TCC 数据库、关闭系统保护或扩大其他应用权限。
- GUI 全屏录制、暂停、继续、停止、独立音频导出通过。连续播放原创 440 Hz / 48 kHz 测试音 50 秒，解码后第 5–54 秒有连续信号，中间没有整秒静音，内部 RMS 最低约 0.0623。MP4 / M4A 完整解码通过；录制媒体时长约 82.0054 秒，轨尾差约 -0.0042 ms。轨尾一致不证明感知声画同步。
- 窗口列表显示真实快照，19 个目标按 3 列排列；选中系统设置窗口录制和保存通过，文件完整解码通过。
- 区域选择实际拖出 640×360，再移动选框、拖动右下角，保存选择为 `(150,150,720,420)`，采集文件尺寸为 720×420。测试中强制退出该录制，原 partial 保留，恢复副本完整解码通过，视频与音频 packet DTS 均严格递增。VFR 解码使用 `-fps_mode passthrough -enc_time_base demux`，避免 null 输出端重采样时间基产生假告警。
- 上述首次采集用于验证录制流程和音频连续性。随后修正 Retina 屏幕为 CGDisplayMode 物理像素尺寸，窗口采用 `SCContentFilter.pointPixelScale`，区域坐标按像素与点转换；最终包的 Retina 尺寸与位置复验仍待桌面可见和当前包授权后完成。
- Release 应用包递归捆绑 15 个动态库，约 38 MB，签名完整性检查通过。捆绑的 Homebrew 库要求 macOS 26，打包脚本会据实际 dylib 的最低版本设置 `LSMinimumSystemVersion`。后端源码最低为 macOS 13，不能把本机包标作兼容 macOS 13。

待验证：当前包的麦克风实录、长时间声画标记、多屏不同缩放、锁屏、设备拔出、快捷键与菜单栏交互，以及完整 `--ui-smoke` 场景。麦克风首次授权窗口需要用户点击允许；最终阶段电脑操作工具返回 `cgWindowNotFound`，无法继续操作可见窗口。不要将这些项目标为通过。新增 macOS CI，修正 Windows CI 的 FFmpeg 环境传递；远程 CI 和 Windows 实机尚未在本轮执行。Linux 后端仍未实现。

本地声音检查数据在 `target/verify/macos/fullscreen-audio-check.json`，恢复副本在 `target/verify/macos/region-recovered.mp4`。`target`、录制文件和本机应用包不提交到 Git。

## 2026-10-08 本机实测

Windows 11 / Intel Iris Xe + NVIDIA MX450 / FFmpeg 9.0.2 / Rust 1.97.1。

- Debug 与 Release workspace 构建通过，9 项自动测试通过，Clippy `-D warnings` 通过。
- QSV H.264/HEVC 编解码探测通过；AV1 QSV 不支持；当前 NVENC API/驱动版本不匹配；AMF 无运行时。x264、x265、SVT-AV1 软件编码可用。
- 2560×1600 全屏 H.264，8 秒墙钟录制中暂停 2 秒，输出 6.402 秒。包含 CLI 轮询误差。
- 1920×1080 区域 HEVC QSV + 系统声 + 麦克风，12 秒墙钟录制中暂停 2 秒，输出约 10.212 秒，音视频轨尾时差约 -0.013 ms。该值不证明感知音画同步。
- OpenCap 自身窗口录制通过，已提取画面检查中文界面。Windows Computer Use 原生管道不可用，因此托盘点击、区域拖拽及快捷键冲突的自动交互验证尚未完成。
- 强制终止独立测试录制后恢复约 51.594 秒 H.264，原 partial 文件保留，完整解码无错误。
- 五类 300 秒 HEVC QSV 素材完整解码无错误。下表为早期 fixture v1，音频每秒 80 ms 音调、其余静音，不能外推成真实会议或真实电影；文字检查区重叠问题已在 v2 修复，v1 不用于文字验收。

| 素材 v1 | 实际 FPS | 文件大小 MB | 按时长折算 MB/h |
|---|---:|---:|---:|
| PPT | 1.14 | 0.77 | 9.26 |
| 文档 | 2.45 | 1.39 | 16.69 |
| IDE | 1.14 | 0.88 | 10.56 |
| 滚动 | 15.03 | 16.61 | 199.31 |
| 高动态图形 | 30.00 | 81.27 | 975.28 |

结果说明高动态内容不应受 200 MB/h 的硬限制。所有 `quality_target_passed` 保持 null，待原尺寸人工逐字检查；一小时真实桌面、连续语音同步、不同 DPI/多显示器、磁盘写满仍需实机验收。

### 长时合成与参考视频

fixture v2 的 3600 秒 PPT 与文档 HEVC QSV 已编码完成，分别 8.79 MB 和 16.45 MB。PPT 完整解码无错误，容器和两条流时长为 3600.021 秒（包含 AAC 补齐）。文档尚未做完整解码。二者都是加速生成的合成媒体，音轨绝大部分为静音，不是墙钟一小时的真实会议或设备同步验收。

用户提供视频的转码入口以本地原创 10 秒 VFR 文档视频验证：x264 转码输出 10.021 秒，与输入视频轨时长一致，完整解码无错误；该比较刻意不含音轨。

### 启动卡顿修复

用户报告启动半分钟后桌面和鼠标明显卡顿。当时预览 UI 与离线 QSV 基准同时运行，已全部停止。发现 UI 空闲每 200 ms 无条件通知重绘，隐藏浮窗也被通知；已改为状态、计时或统计变化时才刷新，隐藏浮窗不刷新。Benchmark 默认改为实时节奏，加速编码须显式 `--fast`。

修复后的 Release 独立空闲监测 61 秒（BelowNormal 进程优先级、未录制）：工作集从 96.24 MB 到 95.36 MB，私有内存从 266.36 MB 到 265.55 MB，句柄 592 到 591；CPU 大多低于单核 1%，两次 GPU 引擎采样均为 0%。该测试未见持续资源增长，但用户侧鼠标卡顿仍需复测确认。原始采样在本地 `artifacts/idle-after-fix.json`。

便携包另以默认 Normal 优先级独立运行 35 秒，无外部 FFmpeg DLL PATH 配置：启动后工作集稳定在约 96.1 MB，私有内存约 266.4 MB，句柄 594–597；10–35 秒各采样区间 CPU 为单核 0–0.31%。测试结束后程序已关闭，并确认没有 OpenCap、Benchmark 或 FFmpeg 进程残留。原始采样在本地 `artifacts/portable-idle-normal-priority.json`。这验证了修复包的空闲资源表现，不代替用户侧鼠标操作复测。

### 运行时依赖修复

复现原始 `target/release/opencap-bench.exe` 在仅包含 Windows 系统目录的 PATH 下启动退出码 `0xC0000135`：构建输出旁未复制 FFmpeg DLL，之前开发脚本的 PATH 掩盖了这个问题。构建与打包现在共用运行时清单，包含 avcodec、avformat、avutil、swscale 及 avcodec 间接依赖的 swresample；成功构建后自动复制到 EXE 同目录。CI 与打包均检查干净 PATH 下的启动，ZIP 保留完整 OpenCap 目录。

修复后 Release 使用默认 Normal 优先级、仅 Windows 系统 PATH、独立工作目录实测：命令行正常启动，主窗口持续响应 60 秒，5 个 FFmpeg DLL 均从 EXE 同目录加载；工作集约 90.4–91.5 MB，CPU 采样为单核 0–0.62%。随后 640×360 原生区域短录制生成约 3.5 秒 MP4，停止封装及完整解码转码均通过。测试自动关闭全部所启动的进程。记录在本地 `artifacts/runtime-fix-release/`。

## 2026-10-09 交互改进验证

- 12 项自动测试通过，新增静音清除单路缓存、初始全静音后写入音频的 AAC 解码验证、缩略图缩放和滚动裁剪；另单独运行 Windows 桌面边框测试通过，检查穿透、排除捕获、暂停状态、隐藏及销毁。
- 可选 `ui-smoke` 开发 feature 运行真实 GPUI 窗口、23 个真实 HWND 缩略图与实际 WGC 窗口录制。最大化窗口验证自适应为 7 列，窄窗口验证响应式布局；默认展开时可访问最后一个目标，收起后才使用滚动。还检查编码帧复用预览、固定底部按钮、设备列表和浮窗。测试调用界面状态方法，不伪装为真实鼠标点击验收，不改写用户设置。构建：`./scripts/dev.ps1 build --features opencap/ui-smoke`；运行：`target/debug/opencap.exe --ui-smoke <绝对输出目录>`。正式包不包含此 feature。
- 界面路径完成开始、开系统声/麦克风、暂停、切换设备/关麦、继续和停止，输出约 3.1 秒，报告无采集警告；检查默认没有显示控制浮窗。
- 原生区域 640×360、9 秒单进程声音切换检查执行 7 次路由修改（从全静音启用两路、切设备、分别静音和重新启用），输出 9.150 秒，最终两路关闭，无警告，完整音视频解码通过。原始记录位于本地 `artifacts/audio-live-switch/`。
- Windows Computer Use 报 native pipe unavailable，因此鼠标拖拽区域、多屏不同 DPI 的视觉边框、独占全屏游戏和设备拔插等实机交互尚未全面验收。
- 最终浮窗尺寸与完整按钮经区域录制提帧确认，记录在 `artifacts/ui-smoke-toolbar-sized/`；控制条使用明确的紧凑布局尺寸，避免隐藏窗口初始 viewport 导致按钮落到窗口之外。
- 最终普通 Release（不含 ui-smoke）构建、fmt、全 features Clippy `-D warnings` 通过；清理 PATH 后启动 45 秒，末段工作集约 98.9 MB、单核 CPU 0–0.31%，主窗口持续响应；短录制和解码通过。前段存在界面交互/渲染波动，不将其记录为全程静止基准。记录在 `artifacts/ui-update-release-runtime/`。
- 本轮响应式 UI smoke 的结构化结果在 `artifacts/ui-smoke-responsive-20261009/ui-smoke.json`：`passed=true`、`maximized_columns=7`。同时覆盖最小化目标窗口自动隐藏边框、常亮/启动 3 秒/关闭三种边框策略，以及开始、暂停、继续、停止和录制中声音开关与设备切换。截图为 `maximized.png`、`narrow.png` 和 `finished.png`。区域编辑的鼠标拖动、不同 DPI 和独占全屏游戏仍需要人工实机验证。
- 本轮普通 Release 使用隔离 Cargo 输出目录构建，以保留正在运行的旧版实例；不包含 `ui-smoke` feature。干净 PATH 下 GUI 持续响应 10 秒，工作集约 105–106 MB、单核 CPU 约 0.31%，5 个 FFmpeg DLL 均从 EXE 目录加载；原生短录制和完整解码通过。记录在 `artifacts/runtime-responsive-release-20261009/`。这是短启动回归，不代替长期录制性能验收。
