# 架构与实现约束

UI → RecordingSession 命令队列 → 录制工作线程 → 平台采集 → 帧策略/混音 → FFmpeg → fragmented MP4 → 无损 remux。Windows 使用 WGC/WASAPI，macOS 使用 ScreenCaptureKit/CoreAudio。

## 边界

`core` 不依赖 GPUI、FFmpeg 或平台 API。`CaptureBackend` / `AudioBackend` 管理原生后端生命周期；`VideoEncoder` 接受带时间戳的帧，返回零到多个 packet 并支持 flush。`Muxer<P>` 是后续分离输出容器的接口，当前 `MediaWriter` 直接管理视频与音频编码器及 MP4 输出。`RegionEditor` 位于 `core`，处理物理坐标中的编辑约束；macOS 原生选择器在 AppKit 点坐标中编辑，再转换为物理像素。后端初始化由 platform 的类型化函数负责。

平台 API 位于 platform。`CaptureTarget::Window` 保存 HWND（Windows）或 CGWindowID（macOS），启动时要求重新选择以避免 ID 复用。GPUI 的 AppKit 原始句柄是 NSView，不能直接作为 CGWindowID。当前 `VideoFrame` 是拥有所有权的 BGRA CPU 缓冲区，`Arc` 支持静态帧复用。未来 GPU 缓冲区应增加独立枚举/生命周期协议。Linux 后端未实现，仍显式编译失败。

macOS 的 Swift 桥接层通过 C ABI 接收配置和同步回调。屏幕采集与系统声音分别使用 SCStream，系统声音流只输出音频并采用 2×2 / 1 FPS 配置；麦克风使用 AVAudioEngine / AVAudioConverter。Rust 在回调返回前复制媒体数据。停止时关闭回调入口、停止流并排空队列，再释放 Rust 上下文，避免异步回调访问已释放指针。AppKit 菜单、区域选择及范围提示在主线程操作；会触发 GPUI 回调的窗口操作延后到下一轮主线程执行。

录屏权限以 ScreenCaptureKit 的实际结果判定。临时签名的 designated requirement 绑定当前二进制哈希，每次重建可能使旧授权失效；打包支持固定证书身份，默认临时签名仅用于本机评审。动态库递归捆绑并重写为应用内 rpath，独立应用不依赖 Homebrew 安装位置。

## 界面与录制反馈

窗口选择使用 DWM thumbnail，将源 HWND 合成到主窗口。GPUI prepaint 提供布局、DPI 和滚动裁剪边界，按可用宽度计算网格列数；默认完整展开，收起模式才启用列表滚动。映射目标矩形与源裁剪矩形；选择器关闭时隐藏并注销。不会为每个缩略图启动 WGC/FFmpeg。

macOS 14+ 的窗口列表使用 SCScreenshotManager 快照，macOS 13 仅显示窗口名称；每次刷新重建快照，不启动持续采集流。显示器尺寸采用 CGDisplayMode 的像素尺寸；窗口使用 SCContentFilter 的 pointPixelScale（macOS 13 根据主要相交显示器计算），区域 sourceRect 转换为点坐标，输出保持物理像素尺寸。

范围边框为独立分层工具窗口，设置 NOACTIVATE、TRANSPARENT 和 EXCLUDEFROMCAPTURE；区域/全屏范围未变时不重复刷新，窗口模式随源窗口边界更新，并在目标最小化、隐藏或 DWM cloaked 时隐藏。边框策略是常亮、启动短暂显示或关闭。WGC 初始化会请求 `Borderless` 权限移除系统捕获边框；失败时保留系统行为并上报警告。控制浮窗默认关闭，首次创建显式设置物理尺寸，避免绕过 GPUI 隐藏窗口的延后 placement 后显示为默认大窗口。主界面底部控制固定，内容独立滚动。

录制预览只消费将要送入编码器的帧：有界槽位缩小到最大 640×360，并限制发布到 5 FPS。它不会启动第二个 WGC 会话或编码器，关闭预览会立即释放该槽位的图像缓冲。

## 时间轴和队列

- 统一单调时钟，扣除暂停时间。平台原生时间转换为此时间轴；AudioTimeline 以 48 kHz 采样数串联回调，吸收短时回调抖动，暂停继续和真实中断时建立新起点。
- 试编码完成后才设定录制起点，不把探测时间录进视频。
- 采集队列最多两帧，队满驱逐旧帧。音频队列有固定容量，超出时告警并丢弃该包，后续采集继续；控制命令单独使用无界队列，停止命令不会被媒体积压丢弃。
- 策略对 80×45 亮度样本计算差异；保留最高帧率上限，静态时每秒保留帧，动态恢复。没有 OCR 或文本密度检测。
- MP4 使用 1/90000 视频时间基。延迟一包计算真实 sample duration，停止时补齐尾帧时长。B 帧关闭以降低延迟并简化实时时序。
- 音频统一为 48 kHz float stereo；混音按时间戳投放到有界时间窗口，缺失补零，迟到丢弃，输出限幅。起点之前的音频裁去对应采样，避免时间戳直接归零造成重叠。STFT 采用 sqrt-Hann、50% overlap-add，跨设备回调保留剩余采样，约 10.7 ms 延迟通过时间戳扣除，并在暂停、降噪切换和停止时排出尾部。系统声不经过麦克风降噪；两路静音告警独立统计。没有 AEC，使用扬声器时建议耳机。
- `SetAudio(AudioRouting)` 在录制线程应用，只有设备或开关改变的路会停止/重开，另一条路保留。混音缓冲按来源分开，切换清除该路未编码样本。GUI 预留 AAC 音轨支持初始全静音后开启，CLI 无音轨请求保持不变；切换记录保存在录制报告 `audio_changes`。
- 窗口尺寸变化时维持首帧输出尺寸，等比缩放留黑边。

## 失败和发布

开始失败会保留错误详情；录制中写入/编码错误保留 partial 文件。原生采集目标关闭、锁屏会走正常停止流程。恢复和普通输出均拒绝覆盖已有目标，原始恢复源保持不变。

FFmpeg ABI、GPUI 版本及 Rust 依赖由固定版本与 Cargo.lock 管理。发布构建必须使用同一套 FFmpeg DLL，禁止 `--enable-nonfree`。不承诺已有本机 FFmpeg 构建与打包 DLL 可互换。

## 后续优化接口

GPU texture 生命周期与硬件编码器互操作、动态重配置、真实设备时钟平滑重采样、GPU 引擎利用率计数器，均须经过独立 Benchmark 后启用。Windows 优先尝试采集显示器所属厂商的编码器，macOS 优先尝试 VideoToolbox，失败后使用软件编码。端到端 GPU 零拷贝及 Linux PipeWire 后端尚未实现。
