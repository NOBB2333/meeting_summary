# 会议搜索能力评估（v3.0）

本文只评估能力与代价，**本轮不修改 `search.rs` 的检索行为**。目标是先决定“模糊搜索/向量搜索做到哪一层”，再动手实现。

## 1. 当前实现事实

- 词法检索：`instr(lower(title/transcript/summary/notes), lower(query))` 精确子串，按修改时间排序（`crates/app/src/search.rs`）。
- `documents_fts` 使用 FTS5 **trigram**，只在查询 ≥3 个字符时作为预筛选；1–2 字中文直接扫描。
- 没有拼写容错、前缀、同义词、拼音、相关度排序或高亮排序；命中字段和摘录是确定的。
- 现有“语义搜索”强制本机 Ollama（`semantic.rs:19,75-195`），向量以 BLOB 存储并在 Rust 中全量 cosine；不是 sqlite-vec，也没有 ANN。
- 本机真实索引当前 19 条、约 20k 字符；该目录中的测试会议已被删除，条目处于 stale/不可访问状态（与“删除了还显示”的问题一致）。

## 2. 分层结论

| 层级 | 能做到什么 | 额外依赖 | 中文表现 | 主要成本 |
| --- | --- | --- | --- | --- |
| L0 精确 | 当前子串/trigram 预筛选 | 无 | 3+ 字良好；2 字靠扫描 | 低 |
| L1 模糊 | 标题/摘录候选 + 编辑距离/子序列评分 | `strsim` 或 `nucleo-matcher`（纯 Rust，无模型） | 英文/数字好；中文错字只能对候选做字符编辑距离，无法保证无候选的错字 | 小二进制、CPU 毫秒级；需限制候选 |
| L2 本地语义 | 语义召回 + 关键词 RRF | 本地 embedding 模型（如 fastembed + bge-small-zh，约百 MB） | 中英语义较强 | 模型下载、内存、索引时间、打包体积 |
| L2b 远程 embedding | 语义召回，无需本地模型 | 第三方 embedding API | 取决于服务 | 发送会议文本，费用/隐私；DeepSeek 目前没有 embedding 端点 |
| L3 ANN 索引 | 大规模向量 KNN | `sqlite-vec`（当前 0.1.10-alpha，需静态 C）或 BLOB+cosine | 语言无关 | 打包/版本风险；先实测是否必要 |
| Rerank | 对已召回候选重排 | 交叉编码器或 chat 模型 | 改善 top-k | 额外模型/延迟；不能替代召回 |

### SQLite 能做什么

- FTS5 trigram 可做子串匹配，但 1–2 字查询无法使用 trigram；`spellfix1` 是可选扩展、按词表做编辑距离，对没有空格分词的中文收益有限。
- SQLite 可以存 BLOB 向量；用 Rust 计算 cosine 不需要扩展。`sqlite-vec` 可提供 vec0/KNN，但其 Rust crate 仍为 pre-v1 alpha，静态编译和跨平台打包需要单独验证。
- 因此“SQLite 能不能模糊/向量”答案是：**能存向量、能做候选与 KNN；但模糊中文需要自定义候选，向量必须有 embedding 模型**。

### DeepSeek 能不能做 embedding

不能。已核对 DeepSeek `/models` 只有文本生成模型（deepseek-flash / v4-pro），没有 embedding 端点；Anthropic 也没有。DeepSeek 或其他 chat 模型可以用于查询扩展或对 top-N 候选重排，但会发送片段并产生 token 成本，不能替代 embedding 召回。Rank/交叉编码器也只能重排，不能单独提供向量。

## 3. 规模与代价估算

假设每场 30k 字符、200 字符 chunk、512 维 float32：

| 规模 | chunk 数 | 向量存储 | 首次索引 | 全量 cosine/查询 |
| --- | ---: | ---: | ---: | ---: |
| 当前 19 场 | ~2,850 | ~5.8 MB | 约 30–60 秒 CPU | 约 1–5 ms |
| 100 场 | ~15,000 | ~31 MB | 几分钟 | 约 10–30 ms |
| 1,000 场 | ~150,000 | ~307 MB（含元数据 350–450 MB） | 约 25–50 分钟 | 约 100–300 ms，需实测 |

以上是量级估算，不是本机实测。索引可增量构建；删除/重转要按 stamp/revision 失效旧向量。embedding 模型可选 fastembed 的 `BGESmallZHV15`（中文小模型，约百 MB 级）或 multilingual-e5；具体大小、许可证、ONNX Runtime/CPU 依赖需在实现前固定版本并写 SHA-256。

## 4. 推荐路线

1. **先做 L1，不动模型**：精确结果优先；当精确命中很少时，对标题和当前页/摘要候选做受控模糊评分。中文按字符编辑距离，短词仍回退精确；不承诺无候选的中文错字。用合成多语言库和当前 19 条索引做只读基准。
2. **L2 作为可选开关**：用户明确同意下载模型后再启用本地 bge-small-zh 向量；先 BLOB + Rust cosine，只有实测 p95 超过门槛（例如 150 ms @1000 场）才评估 sqlite-vec。索引后台增量、可取消、可重建；不上传会议文本。
3. **不做 Ollama 依赖**：现有 Ollama-only 语义入口从主搜索 UI 隐藏/标记实验，避免误导。
4. **Rerank 最后做**：只有混合召回证明不足时再评估交叉编码器；DeepSeek chat 重排不计为向量方案。

## 5. 需要用户确认的决策门槛

- 是否接受 L1 的“候选内模糊”，不承诺任意中文错字？
- 是否接受为 L2 下载约百 MB 的本地 embedding 模型，并承担首次索引时间/内存？
- 是否允许使用远程 embedding（会发送会议文本）？
- 没有明确同意前，不引入模型、不建向量索引、不改变现有搜索行为。
