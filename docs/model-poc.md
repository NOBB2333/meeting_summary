# 流式模型对照实验（未验收）

两组必须处理同一份真实或获授权的媒体、相同机器和录屏设置。A 为现有 Zipformer/SenseVoice 加独立 Nemotron 3 Diarization；B 为 Nemotron 3.5 ASR Streaming 加同一 diarizer。当前默认后端不变。仓库内合成正弦素材只检查时序和管线，不能衡量文字或说话人准确率。新实验在解码前后记录并核对输入文件 SHA-256，完成事件记录解码器实际音轨；比较时还须核对各输出指纹和音轨一致。旧实验没有这些字段，不能仅凭路径证明同样本。开始和结束的两次哈希无法排除处理中被临时修改后又恢复的情况。

2026-11-14 对照的 NeMo-Speech.cpp 主分支为 `e790e463b2913cff9f989a3df224c587f14b4c5c`。本地源码副本位于 Git 忽略的 `target/opencap-poc/NeMo-Speech.cpp`。以下为待执行命令，尚无构建或模型评测结果；依赖、签名、许可证和打包均未验收。

```bash
git clone https://github.com/NVIDIA/NeMo-Speech.cpp.git target/opencap-poc/NeMo-Speech.cpp
git -C target/opencap-poc/NeMo-Speech.cpp checkout --detach e790e463b2913cff9f989a3df224c587f14b4c5c
cmake -S target/opencap-poc/NeMo-Speech.cpp -B target/opencap-poc/build \
  -DCMAKE_BUILD_TYPE=Release -DNEMO_SPEECH_BUILD_ASR=ON \
  -DNEMO_SPEECH_BUILD_DIAR=ON -DNEMO_SPEECH_BUILD_TTS=OFF \
  -DNEMO_SPEECH_BUILD_MIC_CAPTURE=OFF -DNEMO_SPEECH_BUILD_EXAMPLES=ON \
  -DBUILD_TESTING=OFF -DGGML_METAL=OFF -DGGML_VULKAN=OFF -DGGML_CUDA=OFF
cmake --build target/opencap-poc/build --parallel 4
cargo build -p opencap-asr --features nemo-poc --locked
```

源码已存在时跳过 `git clone`，先确认该目录实际 revision 和工作区状态，不覆盖已有内容。CMake 可能需要获取其固定依赖。CPU 是首个对照；Metal、Vulkan、CUDA 需要分别构建和实测，不能因为存在开关就标为通过。Windows 的文本 normalization 按该运行时文档尚不支持。

固定工件（下载至隔离目录，运行前校验实际文件，不能只依据文件名）：

| 模型 | 上游工件 revision | 字节 | SHA-256 |
| --- | --- | ---: | --- |
| Nemotron 3 Diarization q8_0 | `f667ed73aee57d40cc39428eb768b4fd87a0a29e` | 107012128 | `08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1` |
| Nemotron 3.5 ASR Streaming 0.6B q8_0 | `1c8deaecc64b91f034d73e08dd8b64625eb3395d` | 741548352 | `a5c435f294eea8f88ce68dd27b8c3bfea7f777cb2fbba04fcd30eaa555f429ae` |

NeMo 源码构建出的 CLI 可在设置 `NEMO_SPEECH_MODEL_DIR` 后按官方 `nemo-speech pull nemotron-3.5` 和 `nemo-speech pull nemotron-diar` 取得固定索引中的工件；实验应记录 CLI 实际解析的 revision、文件长度和哈希。首次下载可能需要接受上游许可。Rust 实验入口在加载说话人模型前核对上表的大小和 SHA-256，并记录动态库自身哈希；动态库的源码 revision 不能仅凭文件名证明，报告中必须另附构建记录。只加载自行构建且可信的本机原生库。

准备路径变量 `sample`、`diar_model`、`asr_model`、`nemo_library`，均使用实际绝对路径；输出使用新目录。基线模型需预先下载到独立的 `target/opencap-poc/sherpa-models`：

```bash
mkdir -p target/opencap-poc/results
# A：两个进程使用相同音频；需要分别记录耗时和资源，不能把它当成同时录屏的最终性能。
target/debug/opencap-transcribe baseline-asr-poc "$sample" \
  --model-root "$PWD/target/opencap-poc/sherpa-models" \
  --output "$PWD/target/opencap-poc/results/route-a-asr.jsonl"
target/debug/opencap-transcribe nemo-diarize-poc "$sample" \
  --library "$nemo_library" --model "$diar_model" \
  --output "$PWD/target/opencap-poc/results/route-a-diar.jsonl"
# B：同一实时块输入接口；说话人使用相同的独立模型和样本。
target/debug/opencap-transcribe nemo-asr-poc "$sample" \
  --library "$nemo_library" --model "$asr_model" \
  --output "$PWD/target/opencap-poc/results/route-b-asr.jsonl"
target/debug/opencap-transcribe nemo-diarize-poc "$sample" \
  --library "$nemo_library" --model "$diar_model" \
  --output "$PWD/target/opencap-poc/results/route-b-diar.jsonl"
```

说话人 POC 持续向单个 C ABI stream 送入 PCM，按原媒体时间输出可修订的 slot 区间；遇到超过容差的缺口报错，不压缩时间轴。输出是区间快照，不表示逐词归属、真实姓名或已验证的跨会话身份。当前每秒读取完整区间快照，仅用于短样本实验；长会议内存、运算成本和积压未验收。八个 slot 不是整场无限人数。

两种 ASR 已有统一的 PCM push/finish 实验接口；基线输出为句段修订，NeMo 输出为当前语句的假设，不能直接把它们当作同一稳定延迟口径。固定源码的 `fire_eou` 会在发布最终语句后清空该语句的文本，流结束再返回尾部；评分时必须按顺序拼接所有最终事件，不能只取最后一个。上述 ASR 与 diarizer 分进程顺序重放，只适合功能和初步延迟对照；尚未证明两者同进程并行或同时录屏的性能。首字、稳定延迟与准确率必须另依事件和人工标注计算。基线准备在工作线程异步执行，其模型冷加载耗时尚未单独测出。

两组输出保留后端实际返回的时间证据：基线仅保存可用的原始 token 起点，NeMo 实验请求并记录解码器返回的词起止时间；NeMo 的时间来自模型发射帧网格，不是强制对齐。当前实验不会把这些结果自动贴到修改后的展示文字，也未评估时间误差或覆盖率。应用中的 token 起点跳播仅在来源运行、句段身份及时间范围、原文、版本、哈希、顺序和完整文本覆盖均吻合时启用；旧证据缺少来源绑定时只保留，不启用跳播。它不代表已获得词结束时间或逐词高亮。

词级播放所用的 `word_alignment_v1` 还要求任务记录实际媒体指纹、音轨编号和资产身份，并与识别运行中前后核对一致的媒体及音轨匹配。逐字稿页增加了主动启动的 NeMo 重识别实验：使用用户选择的可信原生库与固定 ASR 模型处理同一媒体和音轨，只在最终模型词与当前句段文字完整一致、词时间处于该句段内时保存其原始起止时间；每条记录包含模型、库哈希及时间口径。实验不修改默认 ASR，也不是强制对齐器；运行、准确率和可用性尚未验收。旧数据缺少来源字段时保留，但不能作为词级播放依据。

说话人评分另报告相邻编号切换次数：仅计连续评分区间均恰有一位参考说话人和一位输出说话人、参考身份不变而输出槽位改变的情况；静音、缺失、重叠和排除区间会断开统计。这是明确限定口径的辅助指标，不是通用 ID-switch 标准，也不能代替 cpCER、DER 或人工核验。

`scripts/score-cpcer-poc.py` 提供独立、待验证的 cpCER 评分：人工准备参考和两条路线的说话人归属文本，按每人时间顺序拼接，并在 JSON 中声明 `schema: opencap-poc-speaker-text-v1`、`sample_id`、`sample_fingerprint`、`audio_stream_index`、`coverage: entire_decoded_audio`、`authorization`、`version`、`provenance` 和 `speakers: [{id, text}]`。三份输入必须指向相同媒体和音轨；两份路线文本各需另加 `source_replay_fingerprint`，绑定对应完整重放文件的 SHA-256 和大小。命令接收 `--sample`、`--sample-id`、`--audio-stream-index`、`--reference`、`--route-a`、`--route-b`、`--replay-a`、`--replay-b` 和不存在的 `--output`。评分按全局最小字符编辑距离匹配说话人，未匹配的人与空文本比较；每侧最多 10 人，总编辑距离计算量受限。绑定重放文件不证明人工整理的内容完全来自该文件；来源、顺序、归属、覆盖和授权均为人工声明，该实现及真实样本尚未验证。

路线归属文本的规范化字符集合及其出现次数必须与对应重放的最终文本一致，未能归属的文字也须保留在一个单独槽位。该检查可发现字符增删，但无法证明顺序、说话人归属或完整音轨覆盖正确；跨拆分边界的 Unicode 组合可能导致规范化不一致，需按原文调整拆分边界。

cpCER 还需通过 `--reference-text` 和 `--reference-meta` 提供 CER 使用的完整人工参考文本及清单。说话人参考 JSON 的 `source_reference_fingerprint` 须绑定该文本，拆分后的规范化字符及次数须一致；此校验仍不能证明归属或顺序准确。

每个 `speakers` 项还须包含 `source_spans: [[start, end], ...]`，偏移为各自完整来源文本规范化后的 Unicode 码点半开范围。每个人的范围必须依来源顺序排列，所有人的范围合计恰好覆盖来源一次，拼接内容须与该人的规范化 `text` 一致。参考范围对应完整人工参考，路线范围对应相应重放最终文本；未确定归属可使用单独槽位。此检查核对文本拆分，不能证明来源的时间顺序或说话人标签正确。

固定源码中缓存流向解码器传递全流帧偏移，语句端点只清除语句状态；实验因此以输入媒体起点平移模型时间，并拒绝回退或超出已输入音频的词时间，同时保留识别文字和错误标记。NeMo ASR 与说话人实验均要求输入 PCM 时间在 1 毫秒容差内连续；较大的缺口会终止实验，不能压缩时间轴。这些校验和精度尚未通过真实音频验证。

记录样本 ID、授权、语言、人工参考及说话人数、重叠比例；实际代码与权重哈希、provider 和硬件；冷/热加载、首字和稳定文字/说话人 P50/P95、CER/cpCER、DER 口径、时间戳误差、积压、RSS/显存和同时录屏帧损失。缺少标注或设备时填写“未测”，不推导数值。两组在完整真实样本对照前，不决定新的默认后端。

完成两条 ASR 重放并准备同片段的人工参考纯文本后，可生成初步 CER 报告（输出必须是不存在的新文件）：

```bash
python3 scripts/score-model-poc.py --sample "$sample" --sample-id "授权样本编号" \
  --reference "$reference" --reference-meta "$reference_meta" \
  --route-a "$PWD/target/opencap-poc/results/route-a-asr.jsonl" \
  --route-b "$PWD/target/opencap-poc/results/route-b-asr.jsonl" \
  --output "$PWD/target/opencap-poc/results/cer-report.json"
```

`reference_meta` 是人工准备的 JSON 清单，示例中的指纹需替换为实际文件 SHA-256 和字节数，音轨编号取自重放完成事件：

```json
{
  "schema": "opencap-poc-reference-v1",
  "sample_id": "授权样本编号",
  "sample_fingerprint": {"sha256": "媒体实际哈希", "size_bytes": 123},
  "reference_fingerprint": {"sha256": "参考文本实际哈希", "size_bytes": 123},
  "audio_stream_index": 0,
  "coverage": "entire_decoded_audio",
  "reference_version": "人工标注版本",
  "language": "普通话、中英混合或其他实际语言",
  "authorization": "记录授权依据，不包含个人敏感信息",
  "speaker_count": null,
  "overlap_ratio": null,
  "recording_conditions": null
}
```

清单声明参考覆盖所选媒体的整个解码音轨；对长会议应先准备独立的短媒体片段和与该片段对应的参考。清单中的语言、授权、说话人数和重叠比例由实验者提供，未知值按允许的 `null` 标记，脚本不会核实授权或标注真实性。

脚本要求两条重放均有完成事件、前后媒体指纹一致且实际音轨与解码映射一致，并核对当前媒体和参考文件；旧格式或未完成结果会被拒绝。CER 按 Unicode 码点进行全片段编辑距离，先做 NFKC、大小写折叠，移除空白和 Unicode 标点，保留数字和符号；相同口径适用于两条路线。路线 A 使用最终逐字稿，路线 B 按固定源码的语句端点行为拼接全部最终事件和流尾；拼接行为仍需用真实模型核验，报告只作为初步结果，不等于验收。计算限制为单份文本最多 5 万码点、参考与假设长度乘积不超过 2500 万，长会议须使用与参考严格对应的较短片段。首个非空事件的重放墙钟时间不是首字延迟；DER、cpCER、稳定延迟、时间戳误差、资源和同时录屏影响仍需单独评测。人工参考必须有授权且与同一片段对应，脚本无法验证标注质量。当前脚本尚未运行验证。

新的 ASR 重放还在完成前复核模型文件，路线 B 同时复核动态库；CER 和 cpCER 评分拒绝缺少该核对标记的旧重放。前后指纹一致仍不能排除中途短暂替换，也不能证明识别质量。

基线最终事件同时保存工作线程报告的主模型、标点实际方式、指纹核对状态、provider 和线程数，评分报告保留这些信息；缺少实际运行配置的旧基线重放需重新生成。标点模型可用状态和文件在重放前后核对，实际使用时评分还核对工作线程加载时的指纹；中途短暂替换仍无法排除。

说话人重放同样在完成前复核模型及动态库，DER 评分拒绝缺少核对标记的旧结果；文件读取期间还检查文件身份和元数据变化。

说话人区间可另用人工标注计算初步 DER。先准备独立 JSON，样本指纹和音轨必须与两次说话人重放一致；`scoring_regions` 由人工指定已完整标注的区域，区域外不评分，区域内未列出的时间视为无人说话。说话人标识使用匿名代号，时间以媒体时间轴的整数毫秒表示，区间为左闭右开：

```json
{
  "schema": "opencap-poc-diar-reference-v1",
  "sample_id": "授权样本编号",
  "sample_fingerprint": {"sha256": "媒体实际哈希", "size_bytes": 123},
  "audio_stream_index": 0,
  "reference_version": "人工区间标注版本",
  "language": "实际语言",
  "authorization": "授权依据，不包含个人敏感信息",
  "scoring_regions": [{"start_ms": 0, "end_ms": 10000}],
  "segments": [
    {"start_ms": 1000, "end_ms": 4000, "speaker": "S1"},
    {"start_ms": 3500, "end_ms": 6000, "speaker": "S2"}
  ]
}
```

```bash
python3 scripts/score-diarization-poc.py --sample "$sample" --sample-id "授权样本编号" \
  --reference "$diar_reference" --route-a "$PWD/target/opencap-poc/results/route-a-diar.jsonl" \
  --route-b "$PWD/target/opencap-poc/results/route-b-diar.jsonl" \
  --collar-ms 250 --include-overlap --output "$PWD/target/opencap-poc/results/der-report.json"
```

上例在每个人工说话人区间的起止边界两侧各排除 250 毫秒，并计入参考重叠；可改用 `--exclude-overlap`，报告会记录口径。脚本按全局最大共同活动时间建立一对一匿名槽映射，再计算漏检、误检和说话人混淆的总时长，以参考说话人总时长为分母。两次重放须为相同说话人模型、动态库和音轨；该报告**不代表两路模型同时运行的性能**。每侧最多 12 个说话人，超过时需分片或使用专业评分工具。此实现尚未运行，也未与权威 DER 实现交叉验证；标注覆盖和授权仍需人工核实，cpCER、ID 切换和延迟等仍未测。

词时间可使用单独的人工 JSON 标注进行初步测量：`schema` 为 `opencap-word-reference-v1`，包含 `sample_id`、`sample_fingerprint`（`sha256` 和 `size_bytes`）、`audio_stream_index`、`reference_version`、`authorization`、`coverage: "entire_decoded_audio"` 和 `words` 数组；每个词包含原文 `text`、`start_ms`、`end_ms`。时间使用原媒体坐标。运行：

```bash
python3 scripts/score-word-timing-poc.py --sample "$sample" --reference "$word_reference" \
  --route-b "$PWD/target/opencap-poc/results/route-b-asr.jsonl" \
  --output "$PWD/target/opencap-poc/results/word-timing-report.json"
```

脚本要求后端最终词序列与人工词序列逐项完全一致，计算起点和终点绝对误差的均值、中位数、P95 和最大值；缺词、文字不一致、后端时间异常时拒绝评分。另对每条最终语句检查词列表是否覆盖文字：此项检查使用 NFKC、大小写折叠并忽略空白及标点，但人工词与后端词的逐项比较仍须完全一致。它不对不同文字做强制对齐，不能据此评价识别错误的词或路线 A；后端提供的是解码发射帧而非强制对齐。脚本和模型输出尚未实测，标注授权与质量需要人工核实。

## 2026-11-15 本机构建与首轮实测（macOS arm64）

以下是在 Apple M1 / 16 GiB / macOS 27.2.1 上实际执行的结果。机器没有 GPU 加速配置，全部为 CPU。样本是 NeMo-Speech.cpp 仓库自带的公开样例，不是中文会议，也不是同时录屏场景；因此只能证明管线可用和有初步延迟数据，不能据此替换默认后端。

构建与固定工件：

- NeMo-Speech.cpp 源码固定为 `e790e463b2913cff9f989a3df224c587f14b4c5c`；SentencePiece 静态依赖使用上游脚本固定提交 `31646a467d2051eb904e0b45de3a73e91fe1c1e3`。`llama.cpp` 子模块按该提交初始化。
- CMake：`Release`，ASR/Diarization/Examples 开启，TTS/麦克风采集/测试关闭，`GGML_METAL`、`GGML_VULKAN`、`GGML_CUDA` 关闭，`NEMO_SPEECH_WITH_NORM` 未开启。产物位于 Git 忽略的 `target/opencap-poc/build/bin`。
- 动态库 SHA-256：`libnemo_speech_asr_c.2.dylib` = `94666226ba512aaa44657e889aaec866729f227a1ca36e744cf235b8328dd9bf`；`libnemo_speech_asr.dylib` = `1965796a60fe4b04e69c07a61c228acd84dddacd93ea2db69e75cacb25dbbea8`。这些哈希只证明本次加载的文件，不证明源码 revision；构建记录为本节命令。
- 模型均通过 `nemo-speech model pull` 下载并核对固定大小和 SHA-256：
  - `Nemotron-3-Diarization.q8_0.gguf`：102.1 MiB，`08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1`。
  - `nemotron-3.5-asr-streaming-0.6b.q8_0.gguf`：741548352 字节，`a5c435f294eea8f88ce68dd27b8c3bfea7f777cb2fbba04fcd30eaa555f429ae`。
- 构建命令（已执行）：先 `scripts/build_sentencepiece_static.sh`，再按 `docs/install.md` 的 CMake 参数配置并构建；本机额外安装了 `ninja` 供 SentencePiece 构建使用。未执行 `nemo-speech doctor` 的设备矩阵探测。

实时说话人线路 A/B 共用同一 diarizer 的重放（同一 60 秒 AMI EN2002d 片段，16 kHz 单声道，英文，4 位参考说话人，其中一人仅少量发言）：

| 项目 | 结果 |
| --- | --- |
| 模型冷加载 | 3583–3633 ms（两次运行） |
| 持续流重放 | 实时节奏重放 60.0 秒；共 59 个修订快照、25 个区间，使用后端槽 1/2/3 |
| 首个说话人区间出现 | 重放墙钟约 2.05 秒（单样本，不是 P95） |
| 官方 `bench diarize`（单并发，60 秒） | RTFx 7.74，平均延迟约 7750 ms/60 秒音频，25 个片段；预热约 8015 ms |
| DER（250 ms collar，排除重叠） | 10.79%（miss 3552 ms、false alarm 10 ms、confusion 0 ms） |
| DER（无 collar，包含重叠） | 25.19%（miss 6188 ms、FA 4298 ms、confusion 1240 ms） |

DER 使用本仓库 `scripts/score-diarization-poc.py` 对 AMI `only_words` RTTM 打分；人工标注覆盖和授权是本地声明，评分器未与专业实现对照。参考有 4 位说话人，输出 3 位，符合样例中第四人发言很短的描述；这不等于未识别的人被静默合并。重叠计分下误差明显上升，说明当前结果不能用于“重叠时逐词归属”。

Nemotron 3.5 ASR 线路 B 重放（同一仓库自带 `jfk.wav`，英文）：

| 项目 | 结果 |
| --- | --- |
| 冷加载 | 24667 ms |
| 首个非最终文本 | 输入 1088 ms 时出现，墙钟约 1184 ms |
| 最终文本 | 与样例内容一致（大小写和句读由模型给出） |
| 词时间 | 后端发射帧网格，`method = nemo_decoder_emission_frames_not_forced_alignment`；不是强制对齐 |
| 官方 `bench asr --mode stream`（单并发，11 秒音频） | RTFx 3.07，平均延迟约 3584 ms，预热约 4430 ms |

线路 A 用现有 Zipformer 中文模型重放了同一段英文样本；输出为无意义中文，说明语言不匹配时不能用这段样本比较两条路线。当前机器没有任何已授权中文会议音频，因此：
- 不报告中文 CER、cpCER 或说话人归属 CER。
- 不在没有同样本数据时改变默认 ASR 或默认说话人后端。
- 不把英文 AMI 的 DER、英文 JFK 的首字延迟外推为中文会议质量。

后续同口径评测仍需：用户授权的真实中文会议音频、人工参考文本和说话人标注、30/60/120 分钟长会话、录屏同时运行的资源测量，以及 Windows/Linux 的 CPU 路径。以上结果对应的结果文件在 Git 忽略的 `target/opencap-poc/results`。

复现命令（路径为本次实际使用；输出文件必须不存在）：

```bash
cd target/opencap-poc/NeMo-Speech.cpp
JOBS=4 bash scripts/build_sentencepiece_static.sh
cmake -S . -B ../build -DCMAKE_BUILD_TYPE=Release -DNEMO_SPEECH_BUILD_ASR=ON \
  -DNEMO_SPEECH_BUILD_DIAR=ON -DNEMO_SPEECH_BUILD_TTS=OFF \
  -DNEMO_SPEECH_BUILD_MIC_CAPTURE=OFF -DNEMO_SPEECH_BUILD_EXAMPLES=ON \
  -DBUILD_TESTING=OFF -DGGML_METAL=OFF -DGGML_VULKAN=OFF -DGGML_CUDA=OFF
cmake --build ../build --parallel 4

cd /Users/wong/Code/_Github_my/meeting_summary
NEMO_SPEECH_MODEL_DIR="$PWD/target/opencap-poc/models" \
  target/opencap-poc/build/bin/nemo-speech model pull nemotron-diar
NEMO_SPEECH_MODEL_DIR="$PWD/target/opencap-poc/models" \
  target/opencap-poc/build/bin/nemo-speech model pull nemotron-3.5

cargo run -q -p opencap-asr --features nemo-poc --bin opencap-transcribe -- \
  nemo-diarize-poc target/opencap-poc/NeMo-Speech.cpp/test_files/diar/ami_en2002d_2132.wav \
  --library target/opencap-poc/build/bin/libnemo_speech_asr_c.2.dylib \
  --model target/opencap-poc/models/nvidia/Nemotron-3-Diarization/f667ed73aee57d40cc39428eb768b4fd87a0a29e/Nemotron-3-Diarization.q8_0.gguf \
  --output target/opencap-poc/results/diar-a.jsonl

cargo run -q -p opencap-asr --features nemo-poc --bin opencap-transcribe -- \
  nemo-asr-poc target/opencap-poc/NeMo-Speech.cpp/test_files/asr/wav/test/jfk.wav \
  --library target/opencap-poc/build/bin/libnemo_speech_asr_c.2.dylib \
  --model target/opencap-poc/models/nvidia/nemotron-3.5-asr-streaming-0.6b/1c8deaecc64b91f034d73e08dd8b64625eb3395d/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf \
  --output target/opencap-poc/results/asr-b-jfk.jsonl
```

### 真实中文会议片段（用户授权样本）

样本：用户授权的 20 分钟中文业务会议录音 `output2.m4a`，截取 300–360 秒并转为 16 kHz 单声道 WAV。原始文件只读，测试副本位于 `target/e2e`。没有人工标注的时间轴和逐字参考，以下只能判断可运行性和相对表现，不能作为准确率验收。

| 项目 | 实测 |
| --- | --- |
| Nemotron 3 Diarization | 60 秒内 25 个区间、5 个匿名槽；首个区间约 2.05 秒墙钟；模型冷加载约 3.9 秒；未有人工标注，无 DER |
| Nemotron 3.5 ASR，官方 CLI `--stream` | 60 秒音频耗时 291.88 秒（约 4.86 倍慢于实时），平均约 17.7 个 CPU 核；有标点但不完整 |
| Nemotron 3.5 ASR，Rust POC 重放 | 60 秒音频耗时 507.85 秒；与 CLI 文本基本一致，说明慢在模型/分块路径而非仅采集 |
| 与 SenseVoice 对比 | 归一化字符相似度约 0.569（不是 CER）；出现“填你发票信息→田里发票信息”“把那个→帮那个”等同音/近音错误 |
| 现有 Sherpa 流式模型 | 同一片段可实时送帧，文字更粗糙；SenseVoice 离线在 20 分钟整段上约 7.5 倍实时 |

结论：Nemotron 3.5 ASR 在本机 Apple M1 CPU 上处理中文会议时明显慢于实时，且该片段质量低于现有 SenseVoice；Nemotron diarizer 能持续产生匿名区间，但 60 秒内出现 5 个槽，是否有过度切分需人工标注确认。不更换默认 ASR 或说话人后端；若继续该路线，应优先优化分块/线程/平台后端，并取得人工标注后重新对照。
