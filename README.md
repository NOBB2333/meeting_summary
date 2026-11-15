# OpenCap

**本地优先的会议录制、实时转写、说话人整理与 AI 纪要工作台。**<br>
Rust + GPUI + FFmpeg，原生桌面应用；录制、识别、回放和文件索引都在本机完成。AI 纪要由你主动选择的服务生成，只发送文字。

[![Release](https://img.shields.io/badge/release-v3.0--rc.1-2f6fed)](https://github.com/NOBB2333/meeting_summary/releases)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-555)](docs/build-and-linux.md)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-b31b1b)](LICENSE)

![录制工作台](docs/assets/screenshots/recording.png)

## 能做什么

- **录制**：全屏、窗口、区域或仅录音；系统声与麦克风可独立开关，支持暂停、托盘和全局快捷键。
- **实时转写**：本机 CPU 流式识别，边说边出字；停顿后可用文件模型逐句复核，提高最终稿件质量。
- **文件转写**：导入 WAV、M4A、MP3、MP4 等，生成 JSON、TXT、Markdown、Word 和 SRT。
- **说话人**：会后本机分析、命名、合并/拆分与声纹记忆；在线说话人仍是默认关闭的实验功能。
- **AI 纪要**：连接 Anthropic Messages 或 OpenAI Responses 兼容服务，按转写生成结构化纪要；支持会中增量纪要、来源回跳和版本历史。
- **回放与定位**：内置音视频播放器、波形、句段高亮、点击跳播、选区播放/循环、倍速和音量；快速 seek 不泄露旧缓冲。
- **会议库与检索**：按月份折叠浏览、全文/两字中文检索、筛选、详情和文件重定位。
- **本地优先**：无遥测，不上传音视频；API Key 存在系统凭据库，会议原文与模型结果保存在本地文件夹。

## 界面

| 会议库（按月份） | 播放器与逐字稿 |
| --- | --- |
| ![会议库](docs/assets/screenshots/library.png) | ![播放器](docs/assets/screenshots/player.png) |

![AI 服务设置](docs/assets/screenshots/ai-settings.png)

## 快速开始

1. 从 [Releases](https://github.com/NOBB2333/meeting_summary/releases) 下载对应平台包，或按下方“从源码构建”编译。
2. 打开「录制工作台」，选择画面和声音，点击开始。停止后自动进入「会议文件」。
3. 首次实时识别前，在「设置 → 识别与语言」下载实时模型；文件转写默认使用 SenseVoice。
4. 如需 AI 纪要，在「设置 → AI 服务」选择 Anthropic Messages 或 OpenAI Responses，填写兼容地址和 API Key，再输入模型名或点箭头获取列表，最后保存。
5. 在「会议文件」打开逐字稿、回放、整理说话人或生成纪要。

新配置默认开启麦克风、系统声音和录制前画面预览；已有配置保留你的选择。预览本身不采集音频或写入录制文件，可在「设置 → 录制偏好」关闭。

## 识别模型

| 场景 | 默认 | 说明 |
| --- | --- | --- |
| 实时转写 | 自动 → 轻量 Zipformer 14M | 约 30 MB，CPU 实时；可换标准/增强模型 |
| 逐句复核 | 所选文件模型（默认 SenseVoice） | 停顿后重新识别整句，替换流式文字；会增加 CPU |
| 文件转写 | SenseVoice INT8 | 中英日韩粤，约 229 MB；可换 Paraformer 轻/标准 |
| 标点 | 可选 CT-Transformer | 未安装时只补句末标点 |
| 说话人 | 会后 Pyannote + ERes2Net | 本机 CPU；人数可自动或指定 |

实时与文件转写都在本机运行，不需要显卡、Python 或 Ollama。模型在设置页显式下载，缓存在应用配置目录，可用 `OPENCAP_MODEL_DIR` 指定目录。

新配置默认开启「断句校正」：流式模型先显示文字，SenseVoice/Paraformer 在停顿后复核整句。电脑跟不上时，可在录制页选择「轻量实时」或关闭校正。

## AI 服务

只支持两种协议：

- **Anthropic Messages**（默认）
- **OpenAI Responses**

DeepSeek、OpenAI、Anthropic 官方或其他兼容网关都可以填写对应的服务根地址和模型。示例：

```text
Anthropic 官方          https://api.anthropic.com/v1
DeepSeek Anthropic 兼容 https://api.deepseek.com/anthropic
OpenAI Responses        https://api.openai.com/v1
```

密钥保存在系统凭据库；使用远程 AI 时只发送转录文字，音视频仍留在本机。AI 服务失败不会影响录制、逐字稿和已有纪要。

模型名称右侧的箭头会向当前服务请求模型列表，不发送会议内容。服务不提供兼容列表时，仍可手动输入模型 ID。DeepSeek 的 Anthropic 兼容地址使用其官方模型列表端点。

## 数据与隐私

每场会议一个文件夹，名称默认为本地开始时间：

```text
OpenCap/
  2026-11-15_10-30-00/
    2026-11-15_10-30-00.mp4
    2026-11-15_10-30-00.m4a
    2026-11-15_10-30-00.transcript.json/.txt/.md/.docx/.srt
    2026-11-15_10-30-00.summary.md/.docx
    2026-11-15_10-30-00.summary.v001.md
    2026-11-15_10-30-00.notes.txt
    2026-11-15_10-30-00.manifest.json
```

- 原始媒体、逐字稿、纪要和笔记是权威数据；SQLite 只保存可重建的搜索索引。
- 重命名可同时移动受管理目录、媒体和文稿，操作有恢复日志；外部原件不会被误改。
- 文件被删除或移动后，会议库会标记缺失，可逐场或整库重新定位，不会静默删除索引。
- 历史识别配置保存当时的模型、音轨、媒体指纹和参数；缺失时显示“历史版本未记录”，不拿当前设置补造。

## 从源码构建

需要 Rust 1.90+（仓库固定 1.97.1）、FFmpeg 9 开发库和平台构建工具。详见 [构建与 Linux](docs/build-and-linux.md)。

```bash
scripts/setup.sh              # macOS / Linux
scripts/dev.sh build
scripts/dev.sh run            # 开发版：独立配置和会议库
scripts/dev.sh test
scripts/dev.sh lint -- -D warnings
```

默认开发运行复用 `target/debug` 的依赖缓存；首次构建或工具链、依赖变更后仍会重新编译相关部分。开发版的配置和会议库与正式版分开；`scripts/dev.sh run --release` 的构建输出仍使用独立目录。`setup.sh` 只检查或准备依赖，不会预先编译应用。

Windows：

```powershell
scripts/setup.ps1
scripts/dev.ps1 build
scripts/dev.ps1 run
```

macOS 固定身份打包（保留录屏权限）：

```bash
python3 scripts/package-macos.py --release --identity "OpenCap Local"
```

Linux 打包和 Debian 依赖说明见 [构建与 Linux](docs/build-and-linux.md)。发布包由 GitHub Actions 在 Windows/macOS/Linux 上构建并校验；标签版本必须与 `Cargo.toml`、`Cargo.lock` 和 macOS `Info.plist` 一致。

## 命令行与诊断

```bash
# 查看已保存状态，不读取逐字稿或密钥
opencap-transcribe status --meeting /path/to/meeting

# 文件转写
opencap-transcribe transcribe input.m4a --output /path/to/meeting

# 诊断回放（可注入丢包等故障）
opencap-transcribe diagnose-replay input.m4a --report report.json --fault drop-packet
```

更多诊断和固定素材见 [诊断与回放](docs/diagnostics.md)。

## 已知限制

- 在线说话人、流式联合模型和 GPU 直通仍属实验；默认使用稳定的本机流式 ASR + 会后说话人分析。
- Windows/Linux 的真实设备、低配机器和部分录屏权限行为仍需各平台实测。
- 语义检索不默认启用；当前版本先保证精确/短词检索，模糊与向量方案见 [搜索评估](docs/search-evaluation.md)。
- Word 导出、录音恢复、长会议和超大会议库的故障场景需要持续回归。

## 文档

- [当前技术路径](docs/technical-current.md)
- [工作台布局](docs/ui-workspace-layout.md) · [界面重构](docs/ui-redesign.md)
- [识别模型](docs/asr-models.md) · [模型对照实验](docs/model-poc.md) · [实时说话人方案](docs/realtime-speaker-plan.md)
- [性能与诊断](docs/performance-review.md) · [验收记录](docs/validation.md) · [升级实施记录](docs/upgrade-progress.md)
- [第三方依赖](THIRD_PARTY.md)

## 许可

GPL-3.0-or-later。GPUI 使用 Apache-2.0；FFmpeg 构建包含 GPL 组件。发布二进制时附带许可证、源码与构建记录，详见 [THIRD_PARTY.md](THIRD_PARTY.md)。
