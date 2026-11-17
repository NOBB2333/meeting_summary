# Cap 采集路径对照

本机参考源码：`/Users/wong/Code/_Github_my/Cap`，检视提交 `6af4ec9`。Cap 的 `scap-*` 系列为 MIT，其余多数代码为 AGPLv3；这里仅记录设计差异，没有复制实现。

- Windows：Cap 的 `crates/scap-direct3d/src/lib.rs` 复用 D3D11 staging 纹理池。OpenCap 的 `crates/platform/src/windows_impl/capture.rs` 复用单个 staging 纹理；`async_capture.rs` 已有多槽非阻塞读回和有界队列，可在录制偏好中试用，并在启动失败时回退。二者都仍需按各自编码器和驱动条件测量，不能仅凭路径长度判断性能。
- macOS：两者均使用 ScreenCaptureKit。OpenCap 当前将像素缓冲读回 CPU 后交给 FFmpeg；Cap 的实际输出链取决于录制模式。GPU 直通需要设备、格式、颜色和编码器兼容测试，不能直接将另一项目的管线替换进来。
- 声音：Cap 包含 RNNoise；OpenCap 已有针对麦克风的降噪和自动增益，并单独提供系统声音增益。两者的具体音质及 CPU 占用尚未进行同设备对照；系统声音采集路径不能代替回声消除。

建议在 Windows 真机使用相同分辨率、帧率、编码器和录制时长，对比默认路径与“Windows 异步采集（实验）”，查看录制报告中的实际 `capture_path`、丢帧、读回耗时以及 CPU/GPU 占用。该实验尚未证明可作为所有驱动的默认值。
