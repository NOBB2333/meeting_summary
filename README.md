# OpenCap

Windows / macOS / Linux 本地开源录屏、转录与纪要工具。Rust + GPUI Kit + FFmpeg + sherpa-onnx；Windows 使用 Windows Graphics Capture / WASAPI，macOS 使用 ScreenCaptureKit / CoreAudio / VideoToolbox，Linux 使用 X11 / PulseAudio。本地 CPU 转录，无遥测；AI 纪要可选择本机模型或自行配置的远程服务。

## 去哪里看结果、怎样生成会议纪要

工作台左侧固定三个入口：**录制工作台 / 会议文件 / 设置**，窄窗时导航移到顶部。录制页将画面预览与声音、画质分为两栏，开始、暂停、停止始终固定在底部；选择录制目标时，网格使用整个内容区。录制停止后自动进入会议文件列表，重启后仍可继续查看。文档页的 **逐字稿 / AI 纪要 / 个人笔记** 标签固定，路径可展开“文件位置”查看。AI 页左侧对话、右侧文档，分隔线可拖动，预览可收起；窄窗在对话和文档之间切换。设置分为 **AI 服务 / 语音转录 / 录制偏好**。布局和模块说明见 [界面重构设计](docs/ui-redesign.md)。

**生成方式：逐字稿自动保存，AI 纪要手动生成。** 音频和带音轨视频走同一套转录、纪要与导出流程。开启实时转录并准备好模型后，录制中的语音自动识别；关闭实时转录或模型未就绪时，媒体仍正常保存，会议列表显示“待转录”，点击“转录并查看”即可补转。导入已有文件会自动开始本地转录；AI 纪要不会自动发起网络请求。

1. 在“设置 → 语音转录”开启实时转录。已下载文件转录模型时可直接逐句出字，下载流式模型可更早看到当前句；也可以在“会议文件”导入 WAV、M4A、MP3、MP4 等音视频或保存的 `.transcript.json`。
2. 转录完成，在“逐字稿”查看、搜索、高亮和拖选复制原文，TXT 在应用内预览，也可导出 Word。需要按人整理时，先点击“分析 / 更新说话人”。
3. 在“设置 / AI”配置服务；默认 Responses、DeepSeek 官方地址和 `deepseek-flash`，填写 API Key 后保存。密钥保存在 macOS 钥匙串 / Windows 凭据库，不写入设置 JSON。
4. 进入“AI 纪要”，选择正式纪要、阅读摘要或行动清单；默认只输出正文。点击“模板”可填写公司的章节、措辞要求，并选择 1–5 个历史 Word（.docx）、Markdown 或 UTF-8 TXT 作为内容结构参考。填写本次补充要求后点击“生成纪要”；生成后可继续提问、按要求修改，保留旧版本。
5. “个人笔记”由用户填写，本机自动保存 TXT，点击“导出笔记 Word”生成或更新独立文档。

| 内容 | 文件 | 生成方式 |
| --- | --- | --- |
| 原始转写 | `.transcript.txt` / `.transcript.md` / `.transcript.docx` | 转录完成后自动保存；旧稿可点击导出补齐 Word/TXT |
| 字幕与识别数据 | `.srt` / `.transcript.json` | 自动保存；JSON 支持重新打开 |
| AI 会议纪要 | `.summary.md` / `.summary.docx` | 手动生成，与原始转写独立 |
| AI 历史版本与对话 | `.summary.vNNN.md` / `.summary.vNNN.docx` / `.summary.history.json` | 每次生成、修改或提问后保存 |
| 个人笔记 | `.notes.txt` / `.notes.docx` | TXT 自动保存；Word 手动导出更新 |

实时录制的关联文档与媒体同目录，默认 `~/Movies/OpenCap`（macOS）；外部导入任务的产物位于保存根目录下独立的会议文件夹内。历史索引 `meetings.json` 只记录本地结果路径；删除或移动原文件后，不会复制、还原或猜测原文件。应用内不执行 AI 生成的脚本，Word 由 Rust 原生生成，接收方只需 Word/WPS/Pages。PDF 导出尚未实现。

“识别中”的整句会随后续语音替换；可选上下文校正在停顿后由独立线程复核整句，显示“整句校正中”，下一句继续出字；校正后原位替换。连续讲话不再按 20 秒强制重置识别上下文；校正积压或一句超过 30 秒时保留流式原文。关闭校正可降低 CPU 和内存。中文术语在设置中保存，下次流式识别生效；术语是偏置，不保证每个错字都能修好。转录中和完成后均可查看全部历史文字，向上滚动暂停跟随，点击“回到最新”恢复。队列积压会在停止后补转，取消不删除媒体或已确认文本。“试播文件，观察实时修正”按原速送入流式引擎，不播放声音。

转录任务在文档页顶部显示进度条、大号百分比、已用时间与预计剩余时间；剩余时间根据实际进度计算，准备模型和缺少稳定进度时不显示虚假估计。说话人分析和 AI 整理显示处理阶段与已用时间，当前模型接口不能提供可靠的总百分比。新任务的耗时保存到识别数据或纪要历史，旧文件没有的计时不能补算。

完整逐字稿使用可变高度虚拟列表，只布局可见句段及附近缓冲，长句自然换行；内容和说话人标签在稿件变化时更新，滚动不重新处理整稿。性能问题与 Windows 本地采样步骤见 [性能排查](docs/performance-review.md)。

“分析 / 更新说话人”使用本机 CPU 的 Pyannote INT8 + 中文 3D-Speaker ERes2Net；首次下载约 41 MB 模型。默认双线程，可选单线程省资源或四线程。人数 0 自动，或填写已知人数 1–20；已知 9 人就填 9，不需要调整自动阈值。自动阈值默认 0.9，远场、串音、短发言可能过分拆分同一人。编号进入逐字稿、字幕和 Word；AI 正文默认不附说话人索引，可在内容格式中选择包含来源。未分析时明确标注“尚未分析”，不虚构编号或姓名。横跨多人的句子标交替或重叠；当前仍为会后分析，实时字幕尚不带说话人。最长支持 2 小时。

分析完成后，姓名和分组会同步到逐字稿 JSON、TXT、字幕与 Word。某个文件被占用或不可写时，结果仍保留在界面，并显示失败清单；点击“重试同步文件”即可重试，无需重新识别。已有 AI 正文不会自动重写，可在对话中要求按新姓名更新。

说话人可以“仅本场保存”姓名，或“保存并记住声音”。声纹记忆保存在本机配置目录的 `speaker-memory.json`，不上传录音；只使用至少 3 秒独立发言，复用已有 ERes2Net 模型，无需新模型。后续会后分析会尝试匹配姓名，不确定或有歧义时保留编号；自动姓名标注“声纹匹配待核对”。同名登记会补充到已有记忆，最多保留 5 份声纹样本；可在说话人面板点击“忘记”，既有会议姓名保留。旧逐字稿需要重新分析才能提取可登记的声纹。

所有 AI 任务在后台运行，设置随时可打开。使用远程 AI 时，主动生成、修改或提问才发送转录文字和所选格式示例正文片段，不上传音视频。每个参考文件最多使用前 3000 字；示例仅用于结构和措辞，不作为本场会议事实。默认正文包含必要的主题、议题、决定、后续工作和待确认事项，没有的字段省略。保留 Chat Completions、Anthropic/Claude、本机 Ollama；支持 Responses 后台轮询与取消（需服务支持）；DeepSeek 使用本机后台任务，默认关闭深度思考以优先速度，可手动开启。

“整理来源”指读取逐字稿并提取会议事实，不是重新识别音频。普通长度的 DeepSeek 稿件直接生成一次；超过来源预算的长稿才分段提取简短事实后汇总。只提问不会改写当前 Word；失败和取消保留旧稿。

识别模型在应用配置目录的 `models` 缓存，之后离线使用，可通过 `OPENCAP_MODEL_DIR` 指定缓存目录。默认流式中文模型为 Zipformer Chinese 14M 2023-02-23 INT8，单线程 CPU + modified beam search；它不是英文模型。选型和跨平台边界见 [转录与 UI 选型方案](docs/transcription-selection.md)。GPUI Kit 0.7.3 已替代直接依赖 GPUI 0.2.2；GPUI Fast 本轮未开启。

独立 CLI（也可在 Windows 开发脚本准备 FFmpeg DLL 后使用）：

```bash
cargo build -p opencap-asr
target/debug/opencap-transcribe prepare streaming
target/debug/opencap-transcribe transcribe meeting.m4a --output recordings/meeting --model offline
target/debug/opencap-transcribe transcribe meeting.wav --output recordings/live-demo --model streaming --realtime
target/debug/opencap-transcribe summary recordings/meeting.transcript.json --output recordings/meeting --model qwen2.5:7b
```

首次构建会取得固定版本 sherpa-onnx 原生静态运行库；无需 Python、GPU、Metal 推理或网络转录服务。Linux 使用独立的 X11 / PulseAudio 平台层；Wayland 屏幕采集暂未实现。

目标不是做第二个 OBS，而是一个**点一下就能录**的轻量工具：全屏 / 窗口 / 区域 / 仅录音，系统声音和麦克风，暂停继续，托盘和全局快捷键，输出 MP4（可选同名 M4A），仅录音直接输出 M4A。针对 PPT、文档、浏览器、IDE 这类桌面内容做了低码率优化，同时支持录制游戏和视频等高动态画面。

---

会议卡片支持重命名，直接预览纪要正文或逐字稿开头，并显示录音时长、文件大小和最近更新时间。名称保存在本地元数据中，原录音路径保持稳定；导出的 Word 使用会议名称。

AI 纪要使用“正文 + 对话修改”：普通请求实时显示服务返回的文字；继续提出修改要求会修订当前正文，切换“只问问题”则只回复问题。可添加历史格式文件，也可手动编写、修改正文；点击“保存正文与 Word”统一更新文档。未保存的编辑草稿会在本机恢复。外部 Word / Markdown / TXT 改好后，点击“同步文件修改…”选择该文件，正文与新修订版本会一起更新。每次修订保留历史，可以恢复旧版；全部仍是本地文件，无数据库。左侧保留全部对话，输入框固定在底部；生成的正文显示在右侧，可点击对话中的版本卡片预览。拖动分隔线调整宽度；窄窗口使用“打开文档 / 只看对话”切换。Enter 发送，Shift+Enter 换行。

AI 设置显示当前服务的密钥保存状态，载入后可用小眼睛显示 / 隐藏。密钥仍存放在系统凭据库。服务端后台模式完成后显示整篇；忽略流式参数的兼容服务也可能一次返回完整文本。

## 一、直接使用（不想编译）

1. 拿到 `OpenCap-windows-x64.zip`。
2. **解压整个压缩包**，不要只把 `opencap.exe` 拖出来。
3. 双击解压后 `OpenCap` 文件夹里的 `opencap.exe`。

FFmpeg 的 DLL 已经放在 `opencap.exe` 旁边，不需要另外安装 FFmpeg，也不需要装运行库。

推荐：Windows 11 x64。新版已通过 Windows CI 构建、软件编码与私有运行库检查；Kit / ASR / AI 交互和实际录制性能仍需实机回归。

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
OpenCap/
  OpenCap_2026-10-09_12-44-26/
    OpenCap_2026-10-09_12-44-26.mp4
    OpenCap_2026-10-09_12-44-26.m4a              # 可选，独立音频
    OpenCap_2026-10-09_12-44-26.recording.json
    OpenCap_2026-10-09_12-44-26.transcript.docx
    OpenCap_2026-10-09_12-44-26.summary.docx
    OpenCap_2026-10-09_12-44-26.notes.txt
```

每场新会议使用独立文件夹，逐字稿、字幕、纪要版本和笔记使用同一文件前缀。新导入也集中保存转写产物；导入原音视频保留在原位置，文档页提供独立的原文件位置入口。历史平铺文件仍可从会议库打开，不会自动搬移。

录屏附带的 `.m4a` 直接从 MP4 的混合音轨无损重封装，不会再采集一次，方便以后做录音转写。可在设置里关掉。

“录制方式 → 仅录音”直接保存 AAC/M4A，不创建视频采集或视频编码器。两路声音可以混合，支持暂停继续和实时文字。macOS 录系统声音仍需系统音频权限；只录麦克风无需屏幕授权。

录制过程中写的是 `*.partial.mp4` / `*.partial.m4a` 碎片文件，正常停止后整理成上面的正式文件。如果程序异常退出，主界面会出现「恢复未完成录制」。

### 实时文字与导出

录制页显示可独立滚动的完整历史，不再只显示最后一句，也不截断为最近 40 段。开始录制后实时区排到设置卡前面；默认跟随最新，向上翻阅时暂停跟随，滚动到底部或点击“回到最新”继续跟随。长稿仍采用虚拟列表，只布局可见段落。

“打开文档”打开本机已有文件；“导出 Word…”使用系统保存对话框选择副本位置，保留会议目录中的自动保存稿。侧栏“保存根目录”和文档页“会议文件夹”是不同入口，路径可以复制。

### 托盘

Windows 任务栏窗口图标与右下角托盘使用同一套状态：**未录制是绿色，启动录制、录制、暂停及停止保存期间是橙红色，完成或失败后恢复绿色**。窗口大小图标均更新，且仅在状态切换时刷新。macOS 使用菜单栏录制图标，颜色随状态改变。鼠标悬停提示只显示 `OpenCap`。

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
- 平滑向下扩展：残余底噪最多再压低约 15 dB，发言后保持约 200 ms，再以约 450 ms 的响应释放；配合软过渡减少短停顿时的音量突变
- 3:1 压缩 + 3 dB 补偿：把人声抬起来，接近会议软件的听感
- 慢速 AGC（约 2 秒释放 / 100 毫秒压制）：把偏小的麦克风人声拉到稳定电平，只在判断为发言的区间调整增益；初始化时跳过频谱缓冲零值，减少背景声被误抬高的问题
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
| `crates/` | Rust 源码：core / platform / media / policy / app / bench / asr | 是 |
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

`package.sh` 默认先编译 release，再递归捆绑 FFmpeg 和其依赖 `.dylib`、重写加载路径并检查签名。`--profile debug` 可构建并打包调试版，`--no-build` 仅打包已有二进制。应用仍在运行时会拒绝覆盖包，须从应用中退出后再打包，避免运行进程与授权签名不一致。

macOS 的 `scripts/dev.sh` 默认构建 Debug 原生应用包并打开 `release/OpenCap.app`；`scripts/dev.sh run --release` 使用优化版本。两者都可录屏，Debug 更适合调试代码，Release 用于录制和性能测试。给应用包的授权不一定适用于裸 `target/debug/opencap`，因此开发启动也统一走应用包。重复字体和 `block` 依赖未来兼容性警告不代表脚本启动失败。

默认临时签名供本机评审；二进制或包内容改变后，Debug / Release 都可能需要重新授权。有有效签名证书时使用 `scripts/package.sh --identity "Apple Development: ..."` 或环境变量 `OPENCAP_SIGNING_IDENTITY`，持续使用同一个有效证书身份以保持更新时的代码身份。本机打包不会发布或上传，也没有执行 notarization；公开分发还需准备对应源码和第三方许可证（见 `THIRD_PARTY.md`）。

命令行 `target/release/opencap-bench` 支持同样的探测和录制命令。裸命令行进程的系统授权可能与应用包不同，实机操作优先使用 `OpenCap.app`。

Linux 首先支持 Intel / x86_64 的统信 UOS：X11 显示器、窗口和区域录制，PulseAudio 或 PipeWire-Pulse 的系统声与麦克风，以及仅录音、暂停恢复和文稿功能。Wayland 暂仅支持音频和文稿。X11 不支持排除本应用，暂不提供窗口缩略预览、录制边框或托盘；关闭主窗会退出，快捷键与悬浮控制仍可用；Linux 暂未实现锁屏自动停止，请先停止录制再锁屏。窗口最小化或关闭时结束采集并保存，窗口内容被遮挡时建议改录完整屏幕。

Actions 提供 Windows x64、Mac Apple Silicon / Intel 和 Linux x64 测试包。Linux 在 Debian 10 / glibc 2.28 基线上构建，私有捆绑 FFmpeg、C++、字体等依赖并审计 ELF 符号版本，不替换系统 glibc 或显卡驱动。图形界面使用宿主 Vulkan / OpenGL 驱动；Linux 当前提供软件 H.264 / AAC 路径，UOS 实机性能仍需按设备验证。

当前 Actions 的 Mac Apple Silicon 包要求 macOS 14+，Intel 包要求 macOS 15+；本机 Homebrew 打包的最低版本单独计算，不能把本机 macOS 26 的要求套到云端包。Linux 的 CPU 窗口渲染优先选择 OpenGL，以避开已复现的软件 Vulkan 空白窗口；硬件 GPU 和无窗口计算保留原排序。

Linux 下载 `OpenCap-linux-x64.deb` 安装，或解压 `OpenCap-linux-x64.tar.gz` 后运行 `OpenCap/AppRun`，保留整个目录。构建和兼容检查见 [docs/build-and-linux.md](docs/build-and-linux.md)。Mac 图标由同一套绿色 / 红色图源生成 ICNS 并在签名前装入应用包，Dock 随录制状态切换；系统权限列表使用应用包图标。

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


### Windows 周期性卡顿诊断

录制前可分别关闭“实时转录”和“画面预览”对照；只关“断句校正”仍会保留实时文字。新配置默认不启用二次校正，已有设置保留。录制和文件试播均先显示流式文字，停顿后在独立队列校正，避免校正挡住后续识别。文件转录模型和流式模型共用模型目录但格式不同：已有文件模型可直接通过 VAD 逐句实时识别，停顿或最长约 15 秒断句后出字；流式模型不依赖可选校正模型才能启动。

针对约三分钟的卡顿，20 秒采样不足。先设置对应场景，再在 Windows PowerShell 运行（默认 660 秒，覆盖三个周期）：

```powershell
./scripts/diagnose-windows.ps1 -Scenario recording
# 再关闭转录与预览，重复录制同样内容：
./scripts/diagnose-windows.ps1 -Scenario no-asr-preview
```

输出在 `target/verify/windows-performance`，不会上传。每次录制已有的 `.recording.json` 另含最后 30 分钟的一秒采样：进程 CPU、内存峰值、帧率、丢帧、转录队列积压、录制循环/编码/预览/音频写入耗时峰值。这些不是鼠标或 UI 延迟，也不能单独证明显卡驱动问题。Windows 后端使用 WGC，当前仍有 GPU 回读与 CPU 色彩转换，并非全程 GPU 零拷贝或外接采集卡。Debug 的 DirectX 验证开销与 Release 有差异；Release 也不能保证所有机器没有性能问题。
