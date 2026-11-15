//! Explicit, local-only semantic indexing and hybrid retrieval.
//! A local Ollama server must report that cloud features are disabled before
//! any meeting text is sent. No model is downloaded by this module.
use crate::search::{
    self, SearchFilters, SearchHit, SearchResult, SemanticSource, SemanticSourceSegment,
};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::{Client, Response};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const ENDPOINT: &str = "http://127.0.0.1:11434";
const MAX_BODY: u64 = 16 * 1024 * 1024;
const CHARS_PER_CHUNK: usize = 200;
const MAX_CHUNKS_PER_MEETING: usize = 20_000;

#[derive(Debug)]
pub(super) struct IndexReport {
    pub meetings: usize,
    pub chunks_created: usize,
    pub chunks_reused: usize,
    pub skipped: Vec<String>,
    pub source_unavailable: usize,
    pub source_pending: usize,
    pub eligible: usize,
    pub cancelled: bool,
}
pub(super) enum SemanticEvent {
    Progress(String),
    Finished(Result<IndexReport>),
}
struct LocalModel {
    name: String,
    digest: String,
}
struct Chunk {
    key: String,
    field: &'static str,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    text: String,
    embedding: Option<Vec<u8>>,
    source: Option<SemanticSource>,
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(60))
        .build()?)
}
fn json_body(response: Response) -> Result<serde_json::Value> {
    let status = response.status();
    ensure!(status.is_success(), "本地嵌入服务返回 HTTP {status}");
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_BODY),
        "本地嵌入服务返回内容过大"
    );
    let mut bytes = Vec::new();
    response.take(MAX_BODY + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BODY, "本地嵌入服务返回内容过大");
    Ok(serde_json::from_slice(&bytes)?)
}
fn verify_local_model(client: &Client, requested: &str) -> Result<LocalModel> {
    let requested = requested.trim();
    ensure!(
        !requested.is_empty()
            && requested.len() <= 128
            && requested
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':')),
        "请输入已安装的本地嵌入模型名称"
    );
    ensure!(
        !requested.to_ascii_lowercase().ends_with(":cloud"),
        "语义索引只支持本地模型"
    );
    let status = json_body(
        client
            .get(format!("{ENDPOINT}/api/status"))
            .send()
            .context("无法连接本机 Ollama")?,
    )?;
    ensure!(
        status
            .pointer("/cloud/disabled")
            .and_then(|value| value.as_bool())
            == Some(true),
        "请先在本机 Ollama 禁用云功能，再使用本地语义搜索"
    );
    let tags = json_body(client.get(format!("{ENDPOINT}/api/tags")).send()?)?;
    let models = tags
        .get("models")
        .and_then(|value| value.as_array())
        .context("本地模型列表格式错误")?;
    let latest = format!("{requested}:latest");
    let model = models
        .iter()
        .find(|model| {
            model
                .get("name")
                .and_then(|value| value.as_str())
                .is_some_and(|name| name == requested || name == latest)
        })
        .context("找不到该本地模型；请先自行安装嵌入模型")?;
    for field in ["remote_host", "remote_model"] {
        ensure!(
            model
                .get(field)
                .and_then(|value| value.as_str())
                .is_none_or(str::is_empty),
            "所选模型指向远程服务，不能用于本地语义索引"
        );
    }
    let name = model
        .get("name")
        .and_then(|value| value.as_str())
        .context("模型名称缺失")?
        .to_owned();
    let digest = model
        .get("digest")
        .and_then(|value| value.as_str())
        .context("本地模型没有内容指纹")?;
    ensure!(
        digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
        "本地模型指纹无效"
    );
    Ok(LocalModel {
        name,
        digest: digest.to_ascii_lowercase(),
    })
}

fn embed(client: &Client, model: &LocalModel, texts: &[String]) -> Result<Vec<Vec<u8>>> {
    let current = verify_local_model(client, &model.name)?;
    ensure!(
        current.digest == model.digest,
        "本地嵌入模型已变化，请重新开始"
    );
    let response = json_body(client.post(format!("{ENDPOINT}/api/embed"))
        .json(&serde_json::json!({ "model": model.name.as_str(), "input": texts, "truncate": false }))
        .send().context("本地嵌入请求失败")?)?;
    let after = verify_local_model(client, &model.name)?;
    ensure!(
        after.digest == model.digest,
        "生成嵌入期间本地模型已变化，请重新开始"
    );
    let vectors = response
        .get("embeddings")
        .and_then(|value| value.as_array())
        .context("嵌入服务没有返回向量")?;
    ensure!(vectors.len() == texts.len(), "嵌入向量数量与输入不符");
    let mut result = Vec::with_capacity(vectors.len());
    let mut dimension = None;
    for vector in vectors {
        let vector = vector.as_array().context("嵌入向量格式错误")?;
        ensure!((1..=4096).contains(&vector.len()), "嵌入向量维度无效");
        if let Some(previous) = dimension {
            ensure!(previous == vector.len(), "嵌入向量维度不一致");
        }
        dimension = Some(vector.len());
        let values: Vec<f32> = vector
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .map(|v| v as f32)
                    .context("嵌入向量包含无效数字")
            })
            .collect::<Result<_>>()?;
        let norm = values
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        ensure!(norm.is_finite() && norm > 1e-12, "嵌入向量为空");
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&((f64::from(value) / norm) as f32).to_le_bytes());
        }
        result.push(bytes);
    }
    Ok(result)
}
fn vector(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        !bytes.is_empty() && bytes.len().is_multiple_of(4) && bytes.len() <= 4096 * 4,
        "语义索引向量损坏"
    );
    let values: Vec<_> = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    ensure!(
        values.iter().all(|value| value.is_finite()),
        "语义索引向量包含无效数字"
    );
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(
        norm.is_finite() && (norm - 1.).abs() <= 0.001,
        "语义索引向量未归一化或已损坏"
    );
    Ok(values)
}
fn split_text(text: &str, cancelled: &AtomicBool) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
        let remaining = &text[start..];
        let mut end = remaining.len();
        let mut boundary = None;
        for (count, (offset, ch)) in remaining.char_indices().enumerate() {
            if count == CHARS_PER_CHUNK {
                end = boundary.unwrap_or(offset);
                break;
            }
            // Prefer a complete sentence or paragraph, but avoid producing
            // tiny fragments when a longer context fits in the same chunk.
            if count + 1 >= CHARS_PER_CHUNK / 2
                && matches!(ch, '。' | '！' | '？' | '；' | '!' | '?' | ';' | '\n')
            {
                boundary = Some(offset + ch.len_utf8());
            }
        }
        let part = remaining[..end].trim();
        if !part.is_empty() {
            parts.push(part.to_owned());
            ensure!(
                parts.len() <= MAX_CHUNKS_PER_MEETING,
                "会议文本过长，语义索引已跳过"
            );
        }
        start += end;
    }
    Ok(parts)
}
fn push_chunk(
    chunks: &mut Vec<Chunk>,
    field: &'static str,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    text: String,
    source: Option<SemanticSource>,
) -> Result<()> {
    let mut hash = Sha256::new();
    hash.update(field.as_bytes());
    hash.update(start_ms.unwrap_or(u64::MAX).to_le_bytes());
    hash.update(end_ms.unwrap_or(u64::MAX).to_le_bytes());
    hash.update(text.as_bytes());
    if let Some(source) = &source {
        hash.update(serde_json::to_vec(source)?);
    }
    chunks.push(Chunk {
        key: format!("{:x}", hash.finalize()),
        field,
        start_ms,
        end_ms,
        text,
        embedding: None,
        source,
    });
    Ok(())
}
fn push_transcript_chunk(
    chunks: &mut Vec<Chunk>,
    text: String,
    segments: Vec<SemanticSourceSegment>,
    run: Option<&opencap_asr::TranscriptionRun>,
    start: Option<u64>,
    end: u64,
    valid: bool,
) -> Result<()> {
    let source = SemanticSource {
        run_id: run.map(|run| run.run_id.clone()),
        meeting_id: run.and_then(|run| run.meeting_id.clone()),
        asset_id: run.and_then(|run| run.input_asset_id.clone()),
        chunk_sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        segments,
    };
    push_chunk(
        chunks,
        "逐字稿",
        if valid { start } else { None },
        valid.then_some(end),
        text,
        Some(source),
    )
}
fn collect_chunks(
    base: &Path,
    title: &str,
    summary: &str,
    notes: &str,
    cancelled: &AtomicBool,
) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    for part in split_text(title, cancelled)? {
        push_chunk(&mut chunks, "标题", None, None, part, None)?;
    }
    if let Some(bytes) =
        search::source_bytes_cancellable(&base.with_extension("transcript.json"), Some(cancelled))?
    {
        let transcript: opencap_asr::Transcript =
            serde_json::from_slice(&bytes).context("逐字稿无法读取")?;
        if let Some(manifest) = opencap_core::meeting_manifest(base)?
            && let Some(id) = transcript
                .run
                .as_ref()
                .and_then(|run| run.meeting_id.as_deref())
        {
            ensure!(
                id == manifest.meeting_id,
                "逐字稿与会议清单身份不一致，未生成语义索引"
            );
        }
        let mut text = String::new();
        let mut start = None;
        let mut end = 0u64;
        let mut valid_times = true;
        let mut references = Vec::new();
        for segment in transcript
            .segments
            .iter()
            .filter(|segment| segment.is_confirmed() && !segment.text.trim().is_empty())
        {
            ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
            ensure!(
                chunks.len() <= MAX_CHUNKS_PER_MEETING,
                "会议文本过长，语义索引已跳过"
            );
            let parts = split_text(&segment.text, cancelled)?;
            let segment_hash = format!("{:x}", Sha256::digest(segment.text.as_bytes()));
            let mut cursor = 0;
            for part in parts {
                let offset = segment.text[cursor..]
                    .find(&part)
                    .context("逐字稿片段无法映射到源文本")?;
                let start_byte = cursor + offset;
                let end_byte = start_byte + part.len();
                cursor = end_byte;
                if !text.is_empty()
                    && text.chars().count() + part.chars().count() + 1 > CHARS_PER_CHUNK
                {
                    push_transcript_chunk(
                        &mut chunks,
                        std::mem::take(&mut text),
                        std::mem::take(&mut references),
                        transcript.run.as_ref(),
                        start,
                        end,
                        valid_times,
                    )?;
                    start = None;
                    end = 0;
                    valid_times = true;
                }
                valid_times &= segment.end_ms > segment.start_ms;
                if text.is_empty() {
                    start = Some(segment.start_ms);
                } else {
                    text.push('\n');
                    start = Some(start.unwrap_or(segment.start_ms).min(segment.start_ms));
                }
                text.push_str(&part);
                references.push(SemanticSourceSegment {
                    id: segment.id,
                    revision: segment.revision,
                    start_ms: segment.start_ms,
                    end_ms: segment.end_ms,
                    text_sha256: segment_hash.clone(),
                    start_byte,
                    end_byte,
                });
                end = end.max(segment.end_ms);
            }
        }
        if !text.is_empty() {
            push_transcript_chunk(
                &mut chunks,
                text,
                references,
                transcript.run.as_ref(),
                start,
                end,
                valid_times,
            )?;
        }
    }
    for part in split_text(summary, cancelled)? {
        push_chunk(&mut chunks, "纪要", None, None, part, None)?;
    }
    for part in split_text(notes, cancelled)? {
        push_chunk(&mut chunks, "笔记", None, None, part, None)?;
    }
    ensure!(
        chunks.len() <= MAX_CHUNKS_PER_MEETING,
        "会议文本过长，语义索引已跳过"
    );
    let mut unique = std::collections::HashSet::new();
    chunks.retain(|chunk| unique.insert(chunk.key.clone()));
    Ok(chunks)
}

pub(super) fn build_index(
    path: &Path,
    model_name: &str,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Result<IndexReport> {
    let client = client()?;
    let model = verify_local_model(&client, model_name)?;
    ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
    progress("正在核对本地嵌入模型的向量维度…".into());
    // A fixed, non-meeting probe lets an incremental rebuild reject old
    // vectors with a wrong dimension even when every chunk is reusable.
    let model_dimension =
        vector(&embed(&client, &model, &["OpenCap".to_owned()])?.remove(0))?.len();
    ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
    let mut connection = search::connect(path)?;
    let (source_unavailable, source_pending): (i64, i64) = connection.query_row(
        "SELECT COUNT(*) FILTER (WHERE available=0), COUNT(*) FILTER (WHERE available=1 AND stamp='') FROM documents",
        [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let documents: Vec<(String, String, String)> = {
        let mut statement = connection.prepare(
            "SELECT base,stamp,title FROM documents WHERE available=1 AND stamp<>'' ORDER BY modified DESC",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut report = IndexReport {
        meetings: 0,
        chunks_created: 0,
        chunks_reused: 0,
        skipped: Vec::new(),
        source_unavailable: source_unavailable.max(0) as usize,
        source_pending: source_pending.max(0) as usize,
        eligible: documents.len(),
        cancelled: false,
    };
    for (index, (key, stamp, title)) in documents.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        progress(format!(
            "语义索引 · 第 {} / {} 场",
            index + 1,
            documents.len()
        ));
        let outcome = (|| -> Result<(usize, usize)> {
            let base: PathBuf = serde_json::from_str(key)?;
            ensure!(
                search::fingerprint(&base, title)?.0 == *stamp,
                "源文件已变化，请先刷新会议库"
            );
            let (summary, notes): (String, String) = connection.query_row(
                "SELECT summary,notes FROM documents WHERE base=?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut chunks = collect_chunks(&base, title, &summary, &notes, cancelled)?;
            let existing: HashMap<String, Vec<u8>> = {
                let mut statement = connection.prepare(
                    "SELECT text,embedding FROM semantic_chunks WHERE base=?1 AND model_digest=?2",
                )?;
                let rows = statement.query_map(params![key, model.digest], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let mut reused = 0;
            for chunk in &mut chunks {
                if let Some(bytes) = existing.get(&chunk.text)
                    && vector(bytes).is_ok_and(|values| values.len() == model_dimension)
                {
                    chunk.embedding = Some(bytes.clone());
                    reused += 1;
                }
            }
            let pending: Vec<_> = chunks
                .iter()
                .enumerate()
                .filter_map(|(index, chunk)| chunk.embedding.is_none().then_some(index))
                .collect();
            for batch in pending.chunks(8) {
                ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
                let texts: Vec<_> = batch
                    .iter()
                    .map(|index| chunks[*index].text.clone())
                    .collect();
                for (index, embedding) in batch.iter().zip(embed(&client, &model, &texts)?) {
                    chunks[*index].embedding = Some(embedding);
                }
            }
            // Reused chunks and separate requests must share one dimension;
            // otherwise a partially rebuilt meeting cannot be searched safely.
            for chunk in &chunks {
                let values = vector(chunk.embedding.as_deref().context("嵌入向量缺失")?)?;
                ensure!(
                    values.len() == model_dimension,
                    "嵌入向量维度与当前模型不一致；保留旧索引"
                );
            }
            ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
            ensure!(
                verify_local_model(&client, &model.name)?.digest == model.digest,
                "本地嵌入模型已变化，请重新开始"
            );
            ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
            ensure!(
                search::fingerprint(&base, title)?.0 == *stamp,
                "生成期间会议文件已变化，请刷新后重试"
            );
            let transaction = connection.transaction()?;
            let current: Option<String> = transaction
                .query_row(
                    "SELECT stamp FROM documents WHERE base=?1 AND available=1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                current.as_deref() == Some(stamp.as_str()),
                "会议索引已变化，请重试"
            );
            transaction.execute(
                "DELETE FROM semantic_chunks WHERE base=?1 AND model_digest=?2",
                params![key, model.digest],
            )?;
            for chunk in &chunks {
                ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
                let source_json = chunk
                    .source
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;
                transaction.execute("INSERT INTO semantic_chunks(base,chunk_key,stamp,model_digest,field,start_ms,end_ms,text,embedding,source_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![key, chunk.key, stamp, model.digest, chunk.field, chunk.start_ms.map(|v| v.min(i64::MAX as u64) as i64), chunk.end_ms.map(|v| v.min(i64::MAX as u64) as i64), chunk.text, chunk.embedding.as_ref().context("嵌入向量缺失")?, source_json])?;
            }
            ensure!(!cancelled.load(Ordering::Relaxed), "语义索引已取消");
            transaction.commit()?;
            Ok((chunks.len().saturating_sub(reused), reused))
        })();
        match outcome {
            Ok((created, reused)) => {
                report.meetings += 1;
                report.chunks_created += created;
                report.chunks_reused += reused;
            }
            Err(_) if cancelled.load(Ordering::Relaxed) => {
                report.cancelled = true;
                break;
            }
            Err(error) => {
                connection.execute("UPDATE semantic_chunks SET stamp='' WHERE base=?1 AND model_digest=?2 AND stamp=?3",
                    params![key, model.digest, stamp])?;
                report.skipped.push(format!("{title}：{error:#}"));
            }
        }
    }
    Ok(report)
}

/// Fuse exact full-text matches with the 500 highest-scoring semantic meetings.
/// Vectors whose source files have changed are ignored until re-indexed.
pub(super) fn search_hybrid(
    path: &Path,
    model_name: &str,
    query: &str,
    filters: &SearchFilters,
    page: usize,
    per_page: usize,
    cancelled: &AtomicBool,
) -> Result<SearchResult> {
    let query = query.trim();
    ensure!(!query.is_empty(), "语义搜索需要输入文字");
    ensure!(!cancelled.load(Ordering::Relaxed), "搜索已取消");
    let lexical = search::search(path, query, filters, 0, 10_000)?;
    ensure!(lexical.total <= 10_000, "关键词命中过多，请增加筛选条件");
    let client = client()?;
    let model = verify_local_model(&client, model_name)?;
    let query_vector = vector(&embed(&client, &model, &[query.to_owned()])?.remove(0))?;
    ensure!(!cancelled.load(Ordering::Relaxed), "搜索已取消");
    let connection = search::connect(path)?;
    let indexed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_chunks c JOIN documents d ON d.base=c.base AND d.stamp=c.stamp WHERE c.model_digest=?1 AND d.available=1 AND d.stamp<>'')",
        [&model.digest],
        |row| row.get(0),
    )?;
    ensure!(indexed, "没有此模型的语义索引；请先创建或更新索引");
    let sql = format!(
        "SELECT d.base,d.stamp,d.title,c.field,c.start_ms,c.end_ms,c.text,c.embedding,c.source_json FROM semantic_chunks c JOIN documents d ON d.base=c.base AND d.stamp=c.stamp WHERE d.available=1 AND d.stamp<>'' AND ?1='' AND ?2 IS NULL AND {} AND c.model_digest=?11",
        search::filter_sql(filters)
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params![
        "",
        Option::<String>::None,
        filters.updated_from_ms,
        filters.updated_before_ms,
        filters.min_duration_ms,
        filters.max_duration_ms,
        filters.source.as_deref(),
        filters.status.as_deref(),
        filters.tag.as_deref(),
        filters.speaker.as_deref(),
        model.digest
    ])?;
    let mut best = HashMap::<PathBuf, (f64, SearchHit)>::new();
    let mut fresh = HashMap::<String, bool>::new();
    let mut invalid_meetings = HashSet::<PathBuf>::new();
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        count += 1;
        if count.is_multiple_of(1000) {
            ensure!(!cancelled.load(Ordering::Relaxed), "搜索已取消");
        }
        ensure!(count <= 500_000, "语义候选过多，请增加筛选条件");
        let key: String = row.get(0)?;
        let stamp: String = row.get(1)?;
        let title: String = row.get(2)?;
        let base: PathBuf = serde_json::from_str(&key)?;
        if invalid_meetings.contains(&base) {
            continue;
        }
        let is_fresh = *fresh.entry(key).or_insert_with(|| {
            search::fingerprint(&base, &title).is_ok_and(|value| value.0 == stamp)
        });
        if !is_fresh {
            continue;
        }
        let field: String = row.get(3)?;
        let start: Option<i64> = row.get(4)?;
        let end: Option<i64> = row.get(5)?;
        let text: String = row.get(6)?;
        let source_json: Option<String> = row.get(8)?;
        let semantic_source = source_json
            .and_then(|json| serde_json::from_str::<SemanticSource>(&json).ok())
            .filter(|source| {
                !source.segments.is_empty()
                    && source.chunk_sha256 == format!("{:x}", Sha256::digest(text.as_bytes()))
            });
        let candidate = row
            .get::<_, Vec<u8>>(7)
            .ok()
            .and_then(|bytes| vector(&bytes).ok());
        let Some(candidate) = candidate.filter(|candidate| candidate.len() == query_vector.len())
        else {
            // Drop every semantic hit for this meeting, including any valid
            // chunk seen earlier; the partial ranking would be misleading.
            best.remove(&base);
            invalid_meetings.insert(base);
            continue;
        };
        let score = candidate
            .iter()
            .zip(&query_vector)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum::<f64>();
        let range = start
            .zip(end)
            .filter(|_| field == "逐字稿" && semantic_source.is_some())
            .and_then(|(start, end)| {
                let start = u64::try_from(start).ok()?;
                let end = u64::try_from(end).ok()?;
                (end > start).then_some((start, end))
            });
        let hit = SearchHit {
            base: base.clone(),
            field: format!("语义·{field}"),
            excerpt: text.chars().take(120).collect(),
            start_ms: range.map(|(start, _)| start),
            end_ms: range.map(|(_, end)| end),
            source_stamp: Some((stamp, title)),
            semantic_source,
            stale: false,
        };
        best.entry(base)
            .and_modify(|old| {
                if score > old.0 {
                    *old = (score, hit.clone());
                }
            })
            .or_insert((score, hit));
    }
    let mut notice = if !invalid_meetings.is_empty() {
        Some(format!(
            "{} 场会议的语义向量损坏或维度不一致，已跳过其语义结果；请更新索引。关键词结果仍可用。",
            invalid_meetings.len()
        ))
    } else if best.is_empty() {
        Some(
            if count > 0 {
                "语义索引已过期；请更新索引，本次仅显示关键词命中。"
            } else {
                "当前筛选范围没有可用的语义候选。"
            }
            .to_owned(),
        )
    } else {
        None
    };
    let mut ranked: Vec<_> = best.into_values().collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.base.cmp(&right.1.base))
    });
    let unverified = ranked
        .iter()
        .take(500)
        .filter(|(_, hit)| hit.field == "语义·逐字稿" && hit.semantic_source.is_none())
        .count();
    if unverified > 0 {
        let warning = format!("{unverified} 场语义命中缺少可验证来源，更新索引后才能定位或回听。");
        notice = Some(match notice {
            Some(existing) => format!("{existing} {warning}"),
            None => warning,
        });
    }
    let mut fused = BTreeMap::<PathBuf, (f64, SearchHit)>::new();
    for (rank, (_, hit)) in ranked.into_iter().take(500).enumerate() {
        fused.insert(hit.base.clone(), (1. / (61. + rank as f64), hit));
    }
    for (rank, hit) in lexical.hits.into_iter().enumerate() {
        let value = 1. / (61. + rank as f64);
        fused
            .entry(hit.base.clone())
            .and_modify(|entry| {
                entry.0 += value;
                // A result found by both methods should display the exact
                // keyword match; its semantic score still contributes to rank.
                entry.1 = hit.clone();
            })
            .or_insert((value, hit));
    }
    let mut hits: Vec<_> = fused.into_values().collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.base.cmp(&b.1.base)));
    let total = hits.len();
    let hits = hits
        .into_iter()
        .skip(page.saturating_mul(per_page))
        .take(per_page)
        .map(|(_, hit)| hit)
        .collect();
    Ok(SearchResult {
        query: query.into(),
        page,
        total,
        hits,
        filters: filters.clone(),
        semantic_model: Some(model_name.trim().to_owned()),
        semantic_fallback: false,
        notice,
    })
}
