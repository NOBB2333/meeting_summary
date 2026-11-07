# 本地识别模型

在「设置 → 识别模型」分别选择实时模型和文件转录 / 断句校正模型。录制实时识别不会在录制路径中自动下载：明确选择的实时模型未安装时提示下载，录制仍可正常保存。自动模式优先已安装的轻量实时模型，否则使用已安装的所选文件模型逐句识别。文件转录如果缺少所选模型，会先下载它。原有设置默认使用自动实时模型和 SenseVoice，不会因为更新而切换到较大的模型。

| 用途 | 选择 | 特点与体积 |
| --- | --- | --- |
| 实时 | Zipformer 轻量 | 原有中文 14M INT8，约 30 MB；适合资源有限的电脑。 |
| 实时 | Zipformer 标准 | 中文模型，约 76 MB。 |
| 实时 | Zipformer 增强 | 中文模型，约 167 MB。 |
| 实时 | 文件模型逐句 | 使用下面选中的文件模型，停顿或最长约 15 秒后出字。 |
| 文件 / 校正 | SenseVoice 多语言 | 原有 INT8 模型，约 239 MB；支持中文、英文、日语、韩语和粤语，接受语言提示。 |
| 文件 / 校正 | Paraformer 轻量中英 | INT8 模型，约 82 MB；支持中文、英文及部分方言。 |
| 文件 / 校正 | Paraformer 标准中英 | INT8 模型，约 227 MB；支持中文、英文及部分方言。 |

体积是模型文件的近似十进制大小，不等于运行内存或各平台的最终下载量。三个实时模型以中文为主，不保证英文表现。文件识别由 Silero VAD 切句，字幕时间为整句范围；Paraformer 在本应用中不使用语言提示或实时热词，输出标点可能较少。SenseVoice 同样不使用实时热词。实时热词仅用在流式模型中，不属于当前模型词表的词会被跳过并提示，不会导致转录启动失败。

「断句校正」是可选的第二次识别：流式模型先显示文字，停顿后所选文件模型在后台复核该句。它会增加 CPU 和内存占用，结果也不保证一定更好；电脑卡顿时先关闭校正或关闭实时转录，在录制结束后转录文件。队列积压或连续讲话过长时，程序保留流式原文。不要把参数规模或文件大小当成准确率保证。

如需对比，在会议列表为同一段录音点击「用当前模型另存重转」；新结果保存到独立会议目录，原录音、逐字稿和纪要不覆盖，会议卡片显示主识别模型。可先用自己的几段典型录音比较人名、数字、专业词、中英混说和安静 / 嘈杂场景。清晰收音、缩短说话人与麦克风的距离、减少回声和多人同时发言，通常也有帮助；当前录制混音没有声学回声消除。

VAD 按段识别时，为每段补回最多约 250 毫秒起始音频，相邻段不会重复使用同一段音频；遇到时间线边界会重置。合成中文语音的单次测试中，原来三种文件模型均漏掉开头的「大」，修改后均能识别「大家好」。两段合成语音测试中没有重复拼接；标准实时模型接 Paraformer 轻量校正的样本中，「陆瓶」被校正为「录屏」。这些只验证具体样本和流程，不能推断真实会议的通用准确率或 Windows 老电脑上的速度。

首次模型下载需要网络。新增模型权重使用发布者 Hugging Face 仓库的固定版本，并在安装前检查长度和 SHA-256；原有模型与 VAD 使用 GitHub 来源或复用本地已有的 VAD。模型下载后，识别音视频不需要网络，音视频不会因本地识别上传。应用的 AI 纪要或全文校正若配置远程服务，仍按其各自的明确操作发送文本。

模型发布者资料：

- [流式 Zipformer 模型](https://k2-fsa.github.io/sherpa/onnx/pretrained_models/online-transducer/zipformer-transducer-models.html)
- [SenseVoice](https://k2-fsa.github.io/sherpa/onnx/sense-voice/pretrained.html)
- [Paraformer 中文模型](https://k2-fsa.github.io/sherpa/onnx/pretrained_models/offline-paraformer/paraformer-models.html)
- [标准实时权重](https://huggingface.co/k2-fsa/sherpa-onnx-streaming-zipformer-multi-zh-hans-2023-12-12)、[增强实时权重](https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30)
- [Paraformer 轻量权重](https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09)、[Paraformer 标准权重](https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09)
