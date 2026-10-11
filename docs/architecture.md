# 架构与实现约束

UI → RecordingSession 命令队列 → 录制工作线程 → WGC/WASAPI → 帧策略/混音 → FFmpeg → fragmented MP4 → 无损 remux。

## 边界

`core` 不依赖 GPUI、FFmpeg 或 Windows。`CaptureBackend` / `AudioBackend` 管理原生后端生命周期；`VideoEncoder` 接受带时间戳的帧，返回零到多个 packet 并支持 flush。`Muxer<P>` 是后续分离输出容器的接口，当前 `MediaWriter` 直接管理视频与音频编码器及 MP4 输出。`RegionEditor` 也位于 `core`，只处理物理坐标中的创建、移动、八方向缩放和边界约束；Windows 的鼠标事件适配层及未来 macOS/Linux 后端共享这套编辑规则。后端初始化仍由 platform 的类型化函数负责。

Windows API 调用位于 platform，`CaptureTarget::Window` 仅保存本次运行所选 HWND 的整数标识，启动时强制重新选择以避免句柄复用。当前 `VideoFrame` 是拥有所有权的 BGRA CPU 缓冲区，`Arc` 支持静态帧复用。未来 GPU 缓冲区应增加独立枚举/生命周期协议，而不是向这个字段塞裸指针。未实现跨平台后端时显式编译失败。

## 界面与录制反馈

窗口选择使用 DWM thumbnail，将源 HWND 合成到主窗口。GPUI prepaint 提供布局、DPI 和滚动裁剪边界，按可用宽度计算网格列数；默认完整展开，收起模式才启用列表滚动。映射目标矩形与源裁剪矩形；选择器关闭时隐藏并注销。不会为每个缩略图启动 WGC/FFmpeg。

范围边框为独立分层工具窗口，设置 NOACTIVATE、TRANSPARENT 和 EXCLUDEFROMCAPTURE；区域/全屏范围未变时不重复刷新，窗口模式随源窗口边界更新，并在目标最小化、隐藏或 DWM cloaked 时隐藏。边框策略是常亮、启动短暂显示或关闭。WGC 初始化会请求 `Borderless` 权限移除系统捕获边框；失败时保留系统行为并上报警告。控制浮窗默认关闭，首次创建显式设置物理尺寸，避免绕过 GPUI 隐藏窗口的延后 placement 后显示为默认大窗口。主界面底部控制固定，内容独立滚动。

录制预览只消费将要送入编码器的帧：有界槽位缩小到最大 640×360，并限制发布到 5 FPS。它不会启动第二个 WGC 会话或编码器，关闭预览会立即释放该槽位的图像缓冲。

## 时间轴和队列

- 统一单调时钟，扣除暂停时间。WGC/WASAPI QPC 时间转换为此时间轴。
- 试编码完成后才设定录制起点，不把探测时间录进视频。
- 采集队列最多两帧，队满驱逐旧帧。音频队列有固定容量，超出时该路报错并停止，避免无界延迟。
- 策略对 80×45 亮度样本计算差异；保留最高帧率上限，静态时每秒保留帧，动态恢复。没有 OCR 或文本密度检测。
- MP4 使用 1/90000 视频时间基。延迟一包计算真实 sample duration，停止时补齐尾帧时长。B 帧关闭以降低延迟并简化实时时序。
- WASAPI 共享音频引擎转换到 48 kHz float stereo；混音按时间戳投放到有界时间窗口，缺失补零，迟到丢弃，输出限幅。不是 AEC；扬声器声音被麦克风再次拾取时，建议耳机。
- `SetAudio(AudioRouting)` 在录制线程应用，只有设备或开关改变的路会停止/重开，另一条路保留。混音缓冲按来源分开，切换清除该路未编码样本。GUI 预留 AAC 音轨支持初始全静音后开启，CLI 无音轨请求保持不变；切换记录保存在录制报告 `audio_changes`。
- 窗口尺寸变化时维持首帧输出尺寸，等比缩放留黑边。

## 失败和发布

开始失败会保留错误详情；录制中写入/编码错误保留 partial 文件。原生采集目标关闭、锁屏会走正常停止流程。恢复和普通输出均拒绝覆盖已有目标，原始恢复源保持不变。

FFmpeg ABI、GPUI 版本及 Rust 依赖由固定版本与 Cargo.lock 管理。发布构建必须使用同一套 FFmpeg DLL，禁止 `--enable-nonfree`。不承诺已有本机 FFmpeg 构建与打包 DLL 可互换。

## 后续优化接口

GPU texture 生命周期与 D3D11/QSV/NVENC 互操作、各编码器可验证的动态重配置、真实设备时钟平滑重采样、GPU 引擎利用率计数器，均须经过独立 Benchmark 后启用。当前会优先尝试采集显示器所属厂商的编码器，但未建立共享 GPU 设备上下文。macOS 用 ScreenCaptureKit/CoreAudio，Linux 用 PipeWire；不要让一期 Windows 工作等待这些后端。
