//! Rebuildable local search index. The meeting files remain authoritative.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn recording_output_asset<'a>(
    base: &Path,
    manifest: &'a opencap_core::MeetingManifest,
    report: &serde_json::Value,
) -> Option<&'a opencap_core::MeetingAsset> {
    if report.get("meeting_id").and_then(|value| value.as_str())
        != Some(manifest.meeting_id.as_str())
    {
        return None;
    }
    let output = Path::new(report.pointer("/stats/output")?.as_str()?);
    let root = base.parent().unwrap_or(Path::new("."));
    let mut matches = manifest.assets.iter().filter(|asset| {
        asset.ownership == opencap_core::AssetOwnership::Managed
            && opencap_core::is_input_media_asset(asset)
            && root.join(&asset.path) == output
    });
    let first = matches.next()?;
    (matches.next().is_none()
        && !first.asset_id.is_empty()
        && manifest
            .assets
            .iter()
            .filter(|asset| asset.asset_id == first.asset_id)
            .take(2)
            .count()
            == 1)
        .then_some(first)
}

#[derive(Clone)]
pub(super) struct IndexEntry {
    pub base: PathBuf,
    pub title: String,
    /// This describes the metadata directory, not the source media.
    pub available: bool,
    pub media_available: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SearchFilters {
    pub date_by_recording: bool,
    pub updated_from_ms: Option<i64>,
    pub updated_before_ms: Option<i64>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub source: Option<String>,
    pub status: Option<String>,
    pub tag: Option<String>,
    pub speaker: Option<String>,
}
impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub(super) struct SearchHit {
    pub base: PathBuf,
    pub field: String,
    pub excerpt: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// Semantic timestamps are usable only while this indexed source is current.
    pub source_stamp: Option<(String, String)>,
    pub semantic_source: Option<SemanticSource>,
    pub stale: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SemanticSourceSegment {
    pub id: u64,
    pub revision: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text_sha256: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SemanticSource {
    pub run_id: Option<String>,
    pub meeting_id: Option<String>,
    pub asset_id: Option<String>,
    pub chunk_sha256: String,
    pub segments: Vec<SemanticSourceSegment>,
}

pub(super) fn source_is_current(base: &Path, source_stamp: &Option<(String, String)>) -> bool {
    source_stamp.as_ref().is_some_and(|(stamp, title)| {
        fingerprint(base, title).is_ok_and(|current| current.0 == *stamp)
    })
}

pub(super) const FILTER_SQL: &str = "(?3 IS NULL OR d.modified>=?3) AND (?4 IS NULL OR d.modified<?4) AND (?5 IS NULL OR d.duration_ms>=?5) AND (?6 IS NULL OR d.duration_ms<=?6) AND (?7 IS NULL OR d.source_kind=?7) AND (?8 IS NULL OR (?8='complete' AND d.complete=1) OR (?8='incomplete' AND (d.complete=0 OR d.complete IS NULL)) OR (?8='offline' AND (d.available=0 OR d.media_available=0))) AND (?9 IS NULL OR EXISTS (SELECT 1 FROM json_each(d.tags) AS tag WHERE lower(tag.value)=lower(?9))) AND (?10 IS NULL OR instr(lower(d.speakers),lower(?10))>0)";
pub(super) fn filter_sql(filters: &SearchFilters) -> String {
    if filters.date_by_recording {
        FILTER_SQL.replace("d.modified", "d.recording_started_ms")
    } else {
        FILTER_SQL.to_owned()
    }
}
#[derive(Debug)]
pub(super) struct SearchResult {
    pub query: String,
    pub page: usize,
    pub total: usize,
    pub hits: Vec<SearchHit>,
    pub filters: SearchFilters,
    pub semantic_model: Option<String>,
    /// A semantic request that returned only lexical results after an error.
    pub semantic_fallback: bool,
    pub notice: Option<String>,
}

pub(super) fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path).context("无法打开会议搜索索引")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version <= 4,
        "会议搜索索引由更新版本创建；未修改，请升级应用后再使用"
    );
    connection.execute_batch("\
        PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;\
        CREATE TABLE IF NOT EXISTS documents (\
          id INTEGER PRIMARY KEY, base TEXT UNIQUE NOT NULL, meeting_id TEXT, stamp TEXT NOT NULL,\
          title TEXT NOT NULL, transcript TEXT NOT NULL, summary TEXT NOT NULL, notes TEXT NOT NULL,\
          speakers TEXT NOT NULL, source_kind TEXT NOT NULL, duration_ms INTEGER,\
          complete INTEGER, available INTEGER NOT NULL, modified INTEGER NOT NULL,\
          tags TEXT NOT NULL DEFAULT '[]', media_available INTEGER\
        );\
        CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(\
          title, transcript, summary, notes, content='documents', content_rowid='id', tokenize='trigram'\
        );\
        CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN \
          INSERT INTO documents_fts(rowid,title,transcript,summary,notes) VALUES(new.id,new.title,new.transcript,new.summary,new.notes);\
        END;\
        CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN \
          INSERT INTO documents_fts(documents_fts,rowid,title,transcript,summary,notes) VALUES('delete',old.id,old.title,old.transcript,old.summary,old.notes);\
        END;\
        CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE OF title,transcript,summary,notes ON documents BEGIN \
          INSERT INTO documents_fts(documents_fts,rowid,title,transcript,summary,notes) VALUES('delete',old.id,old.title,old.transcript,old.summary,old.notes);\
          INSERT INTO documents_fts(rowid,title,transcript,summary,notes) VALUES(new.id,new.title,new.transcript,new.summary,new.notes);\
        END;\
        CREATE TABLE IF NOT EXISTS semantic_chunks (\
          base TEXT NOT NULL REFERENCES documents(base) ON UPDATE CASCADE ON DELETE CASCADE,\
          chunk_key TEXT NOT NULL, stamp TEXT NOT NULL, model_digest TEXT NOT NULL,\
          field TEXT NOT NULL, start_ms INTEGER, end_ms INTEGER, text TEXT NOT NULL, embedding BLOB NOT NULL, source_json TEXT,\
          PRIMARY KEY(base,chunk_key,model_digest)\
        );\
        CREATE INDEX IF NOT EXISTS semantic_model_index ON semantic_chunks(model_digest,base);")?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(version <= 4, "会议搜索索引已升级到更新版本；未继续修改");
    if version < 2 {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let locked_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            locked_version <= 4,
            "会议搜索索引已升级到更新版本；未继续修改"
        );
        if locked_version < 2 {
            let columns: Vec<String> = {
                let mut statement = connection.prepare("PRAGMA table_info(documents)")?;
                let rows = statement.query_map([], |row| row.get(1))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut upgraded = false;
            if !columns.iter().any(|column| column == "tags") {
                connection.execute(
                    "ALTER TABLE documents ADD COLUMN tags TEXT NOT NULL DEFAULT '[]'",
                    [],
                )?;
                upgraded = true;
            }
            if !columns.iter().any(|column| column == "media_available") {
                connection.execute(
                    "ALTER TABLE documents ADD COLUMN media_available INTEGER",
                    [],
                )?;
                upgraded = true;
            }
            if upgraded {
                connection.execute("UPDATE documents SET stamp=''", [])?;
            }
            connection.execute_batch("PRAGMA user_version=2")?;
        }
        connection.execute_batch("COMMIT")?;
    }
    if version < 3 {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let locked_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            locked_version <= 4,
            "会议搜索索引已升级到更新版本；未继续修改"
        );
        if locked_version < 3 {
            let columns: Vec<String> = {
                let mut statement = connection.prepare("PRAGMA table_info(semantic_chunks)")?;
                let rows = statement.query_map([], |row| row.get(1))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            if !columns.iter().any(|column| column == "source_json") {
                connection.execute(
                    "ALTER TABLE semantic_chunks ADD COLUMN source_json TEXT",
                    [],
                )?;
            }
            connection.execute_batch("PRAGMA user_version=3")?;
        }
        connection.execute_batch("COMMIT")?;
    }
    if version < 4 {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let locked_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            locked_version <= 4,
            "会议搜索索引已升级到更新版本；未继续修改"
        );
        if locked_version < 4 {
            let columns: Vec<String> = {
                let mut statement = connection.prepare("PRAGMA table_info(documents)")?;
                let rows = statement.query_map([], |row| row.get(1))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            if !columns
                .iter()
                .any(|column| column == "recording_started_ms")
            {
                connection.execute(
                    "ALTER TABLE documents ADD COLUMN recording_started_ms INTEGER",
                    [],
                )?;
                // Only the connection that actually adds the column invalidates
                // existing rows. A concurrent opener may have observed an older
                // user_version before waiting for this transaction.
                connection.execute("UPDATE documents SET stamp=''", [])?;
            }
            connection.execute_batch("PRAGMA user_version=4")?;
        }
        connection.execute_batch("COMMIT")?;
    }
    Ok(connection)
}

/// Preserve the FTS row and its content while changing a meeting's location.
/// Re-indexing changed title/content still happens in the normal scan.
pub(super) fn relocate_index(path: &Path, moved: &opencap_core::RenamedMeeting) -> Result<()> {
    let mut connection = connect(path)?;
    let transaction = connection.transaction()?;
    let old = serde_json::to_string(&moved.old_base)?;
    let new = serde_json::to_string(&moved.new_base)?;
    if old != new {
        let old_row: Option<(i64, Option<String>)> = transaction
            .query_row(
                "SELECT id,meeting_id FROM documents WHERE base=?1",
                [&old],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let new_row: Option<(i64, Option<String>)> = transaction
            .query_row(
                "SELECT id,meeting_id FROM documents WHERE base=?1",
                [&new],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_id, old_id_value)) = old_row {
            ensure!(
                old_id_value
                    .as_deref()
                    .is_none_or(|id| id == moved.meeting_id.as_str()),
                "旧搜索条目的会议身份不匹配"
            );
            if let Some((new_id, new_id_value)) = new_row {
                ensure!(
                    new_id_value.as_deref() == Some(moved.meeting_id.as_str()),
                    "新路径已有其他会议的搜索条目"
                );
                // The destination may already have a stale row after an
                // interrupted migration. Force the normal scan to rebuild it.
                transaction.execute("UPDATE documents SET stamp='' WHERE id=?1", [new_id])?;
                transaction.execute("DELETE FROM documents WHERE id=?1", [old_id])?;
            } else {
                transaction.execute(
                    "UPDATE documents SET base=?1,meeting_id=?2,stamp='' WHERE id=?3",
                    params![new, moved.meeting_id, old_id],
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn source_bytes_cancellable(
    path: &Path,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Option<Vec<u8>>> {
    let before_open = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    ensure!(
        before_open.is_file() && before_open.len() <= MAX_SOURCE_BYTES,
        "索引来源不是普通文件或大于 64 MiB：{}",
        path.display()
    );
    let mut file = opencap_core::open_regular_file_for_read(path)?;
    let metadata = file.metadata()?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    ensure!(
        metadata.is_file()
            && metadata.len() <= MAX_SOURCE_BYTES
            && metadata.len() == before_open.len()
            && metadata.modified()? == before_open.modified()?,
        "索引文件不可读或大于 64 MiB：{}",
        path.display()
    );
    // The file can grow after metadata() returns. Bound the actual read too,
    // so a concurrent writer cannot bypass the indexer's memory limit.
    let mut bytes = Vec::new();
    let mut input = (&mut file).take(MAX_SOURCE_BYTES + 1);
    let mut buffer = [0u8; 128 * 1024];
    loop {
        ensure!(
            !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
            "索引更新已取消"
        );
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    ensure!(
        bytes.len() as u64 <= MAX_SOURCE_BYTES,
        "索引文件大于 64 MiB：{}",
        path.display()
    );
    let after = file.metadata()?;
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_file()
            && identity
                == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(path)?)?
            && metadata.len() == bytes.len() as u64
            && metadata.len() == after.len()
            && metadata.modified().ok() == after.modified().ok(),
        "索引文件在读取期间发生变化：{}",
        path.display()
    );
    Ok(Some(bytes))
}

pub(super) fn fingerprint(base: &Path, title: &str) -> Result<(String, i64)> {
    // Bump this prefix when index interpretation changes, so existing rows
    // are rebuilt even when their source files have not changed.
    // v6 also invalidates cached matches when a path is replaced with a
    // different file having the same size and modification time.
    let mut stamp = format!("index-v14|{title}");
    let mut latest = 0u64;
    for ext in [
        "meeting.json",
        "manifest.json",
        "recording.json",
        "transcript.json",
        "summary.history.json",
        "summary.live.json",
        "summary.md",
        "notes.txt",
    ] {
        let path = base.with_extension(ext);
        let before_open = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                stamp.push_str(&format!("|{ext}:-"));
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
        };
        ensure!(
            before_open.is_file(),
            "索引来源不是普通文件：{}",
            path.display()
        );
        match opencap_core::open_regular_file_for_read(&path)
            .and_then(|file| same_file::Handle::from_file(file).map_err(Into::into))
        {
            Ok(identity) => {
                let metadata = identity
                    .as_file()
                    .metadata()
                    .with_context(|| format!("读取索引来源元数据 {}", path.display()))?;
                ensure!(
                    metadata.is_file()
                        && metadata.len() == before_open.len()
                        && metadata.modified()? == before_open.modified()?,
                    "索引来源在打开前发生变化：{}",
                    path.display()
                );
                let current = opencap_core::open_regular_file_for_read(&path)
                    .and_then(|file| same_file::Handle::from_file(file).map_err(Into::into))
                    .with_context(|| format!("复核索引来源身份 {}", path.display()))?;
                let current_metadata = current.as_file().metadata()?;
                ensure!(
                    std::fs::symlink_metadata(&path)?.file_type().is_file()
                        && identity == current
                        && metadata.len() == current_metadata.len()
                        && metadata.modified().ok() == current_metadata.modified().ok(),
                    "索引来源在核对期间发生变化：{}",
                    path.display()
                );
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                identity.hash(&mut hash);
                let modified = metadata
                    .modified()
                    .unwrap_or(SystemTime::UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                latest = latest.max((modified / 1_000_000).min(i64::MAX as u128) as u64);
                stamp.push_str(&format!(
                    "|{ext}:{}:{modified}:{:016x}",
                    metadata.len(),
                    hash.finish()
                ));
            }
            Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
        }
    }
    Ok((stamp, latest as i64))
}

/// Upsert changed records only. Offline entries keep their last indexed text;
/// a transient read failure never replaces a good index row with empty data.
pub(super) fn update_index(
    path: &Path,
    entries: &[IndexEntry],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Vec<String>> {
    let mut connection = connect(path)?;
    let mut warnings = Vec::new();
    ensure!(
        !cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "索引更新已取消"
    );
    // Commit bounded batches. Cancellation rolls back only the current batch,
    // so a large library does not lose every preceding successful update.
    for batch in entries.chunks(25) {
        ensure!(
            !cancelled.load(std::sync::atomic::Ordering::Relaxed),
            "索引更新已取消；已完成的批次已保留"
        );
        let transaction = connection.transaction()?;
        for entry in batch {
            ensure!(
                !cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "索引更新已取消；已完成的批次已保留"
            );
            transaction.execute_batch("SAVEPOINT meeting_index_entry")?;
            let update = || -> Result<()> {
                let key = serde_json::to_string(&entry.base)?;
                if !entry.available {
                    transaction.execute("INSERT INTO documents(base,stamp,title,transcript,summary,notes,speakers,source_kind,available,modified) VALUES(?1,'',?2,'','','','','unknown',0,0) ON CONFLICT(base) DO UPDATE SET available=0,media_available=NULL,stamp=''", params![key, entry.title])?;
                    transaction
                        .execute("UPDATE semantic_chunks SET stamp='' WHERE base=?1", [&key])?;
                    return Ok(());
                }
                let (stamp, modified) = fingerprint(&entry.base, &entry.title)?;
                let existing: Option<(String, String)> = transaction
                    .query_row(
                        "SELECT stamp,source_kind FROM documents WHERE base=?1",
                        [&key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if existing.as_ref().is_some_and(|(old_stamp, kind)| {
                    old_stamp == &stamp
                        && kind != "unknown"
                        && !(kind == "managed" && entry.media_available.is_none())
                }) {
                    transaction.execute(
                        "UPDATE documents SET available=1,media_available=?2 WHERE base=?1",
                        params![key, entry.media_available.map(i64::from)],
                    )?;
                    return Ok(());
                }
                let transcript = source_bytes_cancellable(
                    &entry.base.with_extension("transcript.json"),
                    Some(cancelled),
                )?
                .map(|bytes| serde_json::from_slice::<opencap_asr::Transcript>(&bytes))
                .transpose()
                .context("逐字稿格式错误；保留旧索引")?;
                let words = transcript
                    .as_ref()
                    .map(|t| {
                        t.segments
                            .iter()
                            .filter(|s| s.is_confirmed())
                            .map(|s| s.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let speakers = transcript
                    .as_ref()
                    .map(|t| {
                        t.speaker_names
                            .values()
                            .map(|name| name.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let mut summary = if let Some(bytes) = source_bytes_cancellable(
                    &entry.base.with_extension("summary.history.json"),
                    Some(cancelled),
                )? {
                    serde_json::from_slice::<opencap_asr::MinutesHistory>(&bytes)
                        .context("纪要历史格式错误；保留旧索引")?
                        .latest()
                        .to_owned()
                } else {
                    source_bytes_cancellable(
                        &entry.base.with_extension("summary.md"),
                        Some(cancelled),
                    )?
                    .map(String::from_utf8)
                    .transpose()?
                    .unwrap_or_default()
                };
                if let Some(bytes) = source_bytes_cancellable(
                    &entry.base.with_extension("summary.live.json"),
                    Some(cancelled),
                )? {
                    let live: opencap_asr::LiveNotes =
                        serde_json::from_slice(&bytes).context("增量纪要格式错误；保留旧索引")?;
                    for item in live.items {
                        if !item.text.trim().is_empty() {
                            if !summary.is_empty() {
                                summary.push('\n');
                            }
                            summary.push_str(&item.text);
                        }
                    }
                }
                let notes = source_bytes_cancellable(
                    &entry.base.with_extension("notes.txt"),
                    Some(cancelled),
                )?
                .map(String::from_utf8)
                .transpose()?
                .unwrap_or_default();
                let tags = if let Some(bytes) = source_bytes_cancellable(
                    &entry.base.with_extension("meeting.json"),
                    Some(cancelled),
                )? {
                    let metadata: serde_json::Value = serde_json::from_slice(&bytes)
                        .context("会议名称或标签格式错误；保留旧索引")?;
                    ensure!(metadata.is_object(), "会议元数据不是对象；保留旧索引");
                    match metadata.get("tags") {
                        None => Vec::<String>::new(),
                        Some(value) => serde_json::from_value::<Vec<String>>(value.clone())
                            .context("会议标签格式错误；保留旧索引")?,
                    }
                } else {
                    Vec::new()
                };
                ensure!(
                    tags.len() <= 100 && tags.iter().all(|tag| tag.len() <= 512),
                    "会议标签过多或过长；保留旧索引"
                );
                let tags = serde_json::to_string(&tags)?;
                let manifest = opencap_core::meeting_manifest(&entry.base)?;
                if let Some(manifest) = &manifest
                    && let Some(recorded_id) = transcript
                        .as_ref()
                        .and_then(|transcript| transcript.run.as_ref())
                        .and_then(|run| run.meeting_id.as_deref())
                {
                    ensure!(
                        recorded_id == manifest.meeting_id,
                        "逐字稿与会议清单身份不一致；保留旧索引"
                    );
                }
                let meeting_id = manifest.as_ref().map(|m| m.meeting_id.as_str());
                let recording_report = source_bytes_cancellable(
                    &entry.base.with_extension("recording.json"),
                    Some(cancelled),
                )?
                .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes))
                .transpose()
                .context("录制报告格式错误；保留旧索引")?;
                ensure!(
                    recording_report
                        .as_ref()
                        .is_none_or(|report| report.is_object()),
                    "录制报告不是对象；保留旧索引"
                );
                let source_kind = if let Some(manifest) = &manifest {
                    let run = transcript
                        .as_ref()
                        .and_then(|transcript| transcript.run.as_ref());
                    let identity_mismatch = run
                        .and_then(|run| run.meeting_id.as_deref())
                        .is_some_and(|id| id != manifest.meeting_id);
                    let id = run.and_then(|run| run.input_asset_id.as_deref());
                    let report_asset = if id.is_none() {
                        recording_report
                            .as_ref()
                            .and_then(|report| {
                                recording_output_asset(&entry.base, manifest, report)
                            })
                            .map(|asset| asset.asset_id.as_str())
                    } else {
                        None
                    };
                    let mut candidates = manifest.assets.iter().filter(|asset| {
                        if let Some(id) = id.or(report_asset) {
                            asset.asset_id == id
                        } else {
                            opencap_core::is_input_media_asset(asset)
                        }
                    });
                    let first = candidates.next();
                    if identity_mismatch
                        || candidates.next().is_some()
                        || first.is_none_or(|asset| {
                            !opencap_core::is_input_media_asset(asset)
                                || opencap_core::resolve_asset_in_manifest(
                                    &entry.base,
                                    manifest,
                                    &asset.asset_id,
                                )
                                .ok()
                                .flatten()
                                .is_none()
                        })
                    {
                        "unknown"
                    } else if first.is_some_and(|asset| {
                        asset.ownership == opencap_core::AssetOwnership::External
                    }) {
                        "external"
                    } else {
                        "managed"
                    }
                } else {
                    "legacy"
                };
                let recording_started_ms = recording_report
                    .as_ref()
                    .filter(|report| {
                        meeting_id.is_some()
                            && report.get("meeting_id").and_then(|value| value.as_str())
                                == meeting_id
                    })
                    .filter(|report| {
                        report
                            .get("recording_start_basis")
                            .and_then(|value| value.as_str())
                            == Some("wall clock observed at media timeline origin")
                    })
                    .and_then(|report| report.get("recording_started_unix_ms"))
                    .and_then(|value| value.as_i64())
                    .filter(|value| *value > 0);
                let transcript_duration = transcript.as_ref().and_then(|t| t.source_duration_ms);
                let duration = if transcript_duration.is_some() {
                    transcript_duration
                } else if let Some(report) =
                    recording_report.as_ref().filter(|report| match meeting_id {
                        Some(id) => {
                            report.get("meeting_id").and_then(|value| value.as_str()) == Some(id)
                        }
                        None => report.get("meeting_id").is_none_or(|value| value.is_null()),
                    })
                {
                    report
                        .pointer("/stats/duration_seconds")
                        .and_then(|value| value.as_f64())
                        .filter(|seconds| seconds.is_finite() && *seconds >= 0.)
                        .map(|seconds| (seconds * 1000.).min(u64::MAX as f64) as u64)
                } else {
                    None
                }
                .map(|v| v.min(i64::MAX as u64) as i64);
                let complete = transcript
                    .as_ref()
                    .map(|t| if t.is_complete() { 1i64 } else { 0i64 });
                ensure!(
                    fingerprint(&entry.base, &entry.title)?.0 == stamp,
                    "索引读取期间会议文件发生变化；保留旧索引并在下次刷新重试"
                );
                transaction.execute("INSERT INTO documents(base,meeting_id,stamp,title,transcript,summary,notes,speakers,source_kind,duration_ms,complete,available,modified,tags,media_available,recording_started_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12,?13,?14,?15) ON CONFLICT(base) DO UPDATE SET meeting_id=excluded.meeting_id,stamp=excluded.stamp,title=excluded.title,transcript=excluded.transcript,summary=excluded.summary,notes=excluded.notes,speakers=excluded.speakers,source_kind=excluded.source_kind,duration_ms=excluded.duration_ms,complete=excluded.complete,available=1,modified=excluded.modified,tags=excluded.tags,media_available=excluded.media_available,recording_started_ms=excluded.recording_started_ms", params![key, meeting_id, stamp, entry.title, words, summary, notes, speakers, source_kind, duration, complete, modified, tags, entry.media_available.map(i64::from), recording_started_ms])?;
                Ok(())
            };
            if let Err(error) = update() {
                transaction.execute_batch(
                    "ROLLBACK TO meeting_index_entry; RELEASE meeting_index_entry",
                )?;
                let key = serde_json::to_string(&entry.base)?;
                transaction.execute("UPDATE documents SET stamp='' WHERE base=?1", [&key])?;
                transaction.execute("UPDATE semantic_chunks SET stamp='' WHERE base=?1", [&key])?;
                warnings.push(format!("{}：{error:#}", entry.base.display()));
            } else {
                transaction.execute_batch("RELEASE meeting_index_entry")?;
            }
        }
        ensure!(
            !cancelled.load(std::sync::atomic::Ordering::Relaxed),
            "索引更新已取消；已完成的批次已保留"
        );
        transaction.commit()?;
    }
    Ok(warnings)
}

/// Trigram search for three or more characters, parameterized exact substring
/// fallback for one/two characters. Both return the same hit/page semantics.
pub(super) fn search(
    path: &Path,
    query: &str,
    filters: &SearchFilters,
    page: usize,
    per_page: usize,
) -> Result<SearchResult> {
    let query = query.trim();
    ensure!(
        query.chars().count() <= 200
            && !query.contains('\0')
            && (!query.is_empty() || !filters.is_empty()),
        "请输入搜索词或选择筛选条件"
    );
    ensure!(
        filters
            .source
            .as_deref()
            .is_none_or(|value| matches!(value, "managed" | "external" | "legacy" | "unknown")),
        "无效来源筛选"
    );
    ensure!(
        filters
            .status
            .as_deref()
            .is_none_or(|value| matches!(value, "complete" | "incomplete" | "offline")),
        "无效状态筛选"
    );
    ensure!((1..=10_000).contains(&per_page), "无效分页大小");
    let connection = connect(path)?;
    // The index uses the trigram tokenizer, so a three-or-more-character
    // substring (including Chinese) can use the FTS prefilter. Shorter queries
    // cannot be indexed by trigrams and fall back to the exact instr predicate;
    // the predicate is authoritative either way.
    let phrase =
        (query.chars().count() >= 3).then(|| format!("\"{}\"", query.replace('"', "\"\"")));
    let prefilter = if phrase.is_some() {
        "d.id IN (SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?2) AND "
    } else {
        "?2 IS NULL AND "
    };
    let filter_sql = filter_sql(filters);
    let predicate = format!(
        "(?1='' OR instr(lower(d.title),lower(?1))>0 OR instr(lower(d.transcript),lower(?1))>0 OR instr(lower(d.summary),lower(?1))>0 OR instr(lower(d.notes),lower(?1))>0) AND {filter_sql}"
    );
    let count_sql = format!("SELECT count(*) FROM documents d WHERE {prefilter}{predicate}");
    let total: i64 = connection.query_row(
        &count_sql,
        params![
            query,
            phrase,
            filters.updated_from_ms,
            filters.updated_before_ms,
            filters.min_duration_ms,
            filters.max_duration_ms,
            filters.source.as_deref(),
            filters.status.as_deref(),
            filters.tag.as_deref(),
            filters.speaker.as_deref()
        ],
        |row| row.get(0),
    )?;
    let sql = format!(
        "SELECT d.base, CASE WHEN ?1='' THEN '筛选' WHEN instr(lower(d.title),lower(?1))>0 THEN '标题' WHEN instr(lower(d.transcript),lower(?1))>0 THEN '逐字稿' WHEN instr(lower(d.summary),lower(?1))>0 THEN '纪要' ELSE '笔记' END, CASE WHEN ?1='' THEN d.title WHEN instr(lower(d.title),lower(?1))>0 THEN substr(d.title,max(1,instr(lower(d.title),lower(?1))-24),120) WHEN instr(lower(d.transcript),lower(?1))>0 THEN substr(d.transcript,max(1,instr(lower(d.transcript),lower(?1))-24),120) WHEN instr(lower(d.summary),lower(?1))>0 THEN substr(d.summary,max(1,instr(lower(d.summary),lower(?1))-24),120) ELSE substr(d.notes,max(1,instr(lower(d.notes),lower(?1))-24),120) END, d.stamp, d.title, d.available FROM documents d WHERE {prefilter}{predicate} ORDER BY d.modified DESC,d.id DESC LIMIT ?11 OFFSET ?12"
    );
    let offset = page.saturating_mul(per_page).min(i64::MAX as usize) as i64;
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            query,
            phrase,
            filters.updated_from_ms,
            filters.updated_before_ms,
            filters.min_duration_ms,
            filters.max_duration_ms,
            filters.source.as_deref(),
            filters.status.as_deref(),
            filters.tag.as_deref(),
            filters.speaker.as_deref(),
            per_page as i64,
            offset
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let (base, field, excerpt, stamp, title, available) = row?;
        let base: PathBuf = serde_json::from_str(&base)?;
        let source_stamp = Some((stamp, title));
        let stale = available == 0 || !source_is_current(&base, &source_stamp);
        hits.push(SearchHit {
            base,
            field,
            excerpt,
            start_ms: None,
            end_ms: None,
            source_stamp,
            semantic_source: None,
            stale,
        });
    }
    Ok(SearchResult {
        query: query.into(),
        page,
        total: total.max(0) as usize,
        hits,
        filters: filters.clone(),
        semantic_model: None,
        semantic_fallback: false,
        notice: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "opencap-search-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn write_meeting(root: &Path, name: &str, text: &str, meeting_id: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join(name);
        let segment = opencap_asr::Segment {
            extra: Default::default(),
            id: 1,
            revision: 1,
            start_ms: 0,
            end_ms: 2000,
            text: text.into(),
            finalized: true,
            correcting: false,
            review: None,
        };
        let transcript = opencap_asr::Transcript {
            complete: true,
            segments: vec![segment],
            ..Default::default()
        };
        std::fs::write(
            base.with_extension("transcript.json"),
            serde_json::to_vec(&transcript).unwrap(),
        )
        .unwrap();
        std::fs::write(
            base.with_extension("summary.md"),
            format!("# {name}\n\n本周讨论退款流程。\n"),
        )
        .unwrap();
        std::fs::write(base.with_extension("notes.txt"), "合同与预算备注").unwrap();
        let manifest = opencap_core::MeetingManifest {
            schema_version: 2,
            meeting_id: meeting_id.into(),
            title: name.into(),
            managed_directory: PathBuf::from(name),
            assets: Vec::new(),
            transcription_runs: Vec::new(),
            extra: Default::default(),
        };
        std::fs::write(
            base.with_extension("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        base
    }

    fn index_meetings(root: &Path, entries: Vec<IndexEntry>) -> PathBuf {
        let db = root.join("index.sqlite3");
        let cancelled = AtomicBool::new(false);
        let warnings = update_index(&db, &entries, &cancelled).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        db
    }

    #[test]
    fn chinese_short_words_and_substrings_match_the_original_text() {
        let root = temp_root("zh");
        let base = write_meeting(&root, "会议一", "我们讨论退款和预算。合同下周签。", "m-1");
        let db = index_meetings(
            &root,
            vec![IndexEntry {
                base: base.clone(),
                title: "会议一".into(),
                available: true,
                media_available: None,
            }],
        );
        for query in ["退款", "预算", "合同", "退款和预算"] {
            let result = search(&db, query, &SearchFilters::default(), 0, 25).unwrap();
            assert_eq!(result.total, 1, "{query} 应命中");
            assert_eq!(result.hits[0].field, "逐字稿", "{query} 命中断字稿");
            assert_eq!(result.hits[0].base, base);
        }
        // A word that appears only in the summary is still found and labelled.
        let summary = search(&db, "流程", &SearchFilters::default(), 0, 25).unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.hits[0].field, "纪要");
        let missing = search(&db, "上线", &SearchFilters::default(), 0, 25).unwrap();
        assert_eq!(missing.total, 0);
        // Latin substrings inside a longer word must not be hidden by the FTS
        // prefilter either.
        let english = write_meeting(&root, "english", "The budgeting review is ready.", "m-2");
        let db = index_meetings(
            &root,
            vec![
                IndexEntry {
                    base,
                    title: "会议一".into(),
                    available: true,
                    media_available: None,
                },
                IndexEntry {
                    base: english,
                    title: "english".into(),
                    available: true,
                    media_available: None,
                },
            ],
        );
        let partial = search(&db, "budg", &SearchFilters::default(), 0, 25).unwrap();
        assert_eq!(partial.total, 1, "英文词内子串应命中");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pagination_total_is_not_truncated_to_the_first_page() {
        let root = temp_root("page");
        let mut entries = Vec::new();
        for index in 0..30 {
            let name = format!("会议{index:02}");
            let base = write_meeting(&root, &name, "共同讨论预算。", &format!("m-{index}"));
            entries.push(IndexEntry {
                base,
                title: name,
                available: true,
                media_available: None,
            });
        }
        let db = index_meetings(&root, entries);
        let first = search(&db, "预算", &SearchFilters::default(), 0, 10).unwrap();
        assert_eq!(first.total, 30);
        assert_eq!(first.hits.len(), 10);
        let third = search(&db, "预算", &SearchFilters::default(), 2, 10).unwrap();
        assert_eq!(third.total, 30);
        assert_eq!(third.hits.len(), 10);
        assert_ne!(first.hits[0].base, third.hits[0].base);
        let beyond = search(&db, "预算", &SearchFilters::default(), 3, 10).unwrap();
        assert_eq!(beyond.total, 30);
        assert!(beyond.hits.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
