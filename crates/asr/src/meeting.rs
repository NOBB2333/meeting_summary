//! Small local metadata; renaming a meeting never moves its source media.
use anyhow::{Context, Result};
use std::path::Path;

fn metadata_snapshot(
    path: &Path,
) -> Result<Option<(Vec<u8>, same_file::Handle, opencap_core::MediaFingerprint)>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            anyhow::ensure!(metadata.file_type().is_file(), "会议元数据不是普通文件");
            Ok(Some(
                opencap_core::read_title_source(path, 4 * 1024 * 1024)?
                    .context("会议元数据超过 4 MiB")?,
            ))
        }
    }
}

fn save_metadata(
    path: &Path,
    value: &serde_json::Value,
    snapshot: Option<(Vec<u8>, same_file::Handle, opencap_core::MediaFingerprint)>,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if let Some((_, identity, fingerprint)) = snapshot {
        opencap_core::verify_title_source(path, &identity, &fingerprint)?;
        crate::atomic_write(path, &bytes)
    } else {
        opencap_core::atomic_create_bytes(path, &bytes)
    }
}

pub fn meeting_name(base: &Path) -> Option<String> {
    meeting_name_checked(base).ok().flatten()
}

pub fn meeting_name_checked(base: &Path) -> Result<Option<String>> {
    let Some((bytes, _, _)) = metadata_snapshot(&base.with_extension("meeting.json"))? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).context("会议元数据无法读取")?;
    anyhow::ensure!(value.is_object(), "会议元数据不是 JSON 对象");
    Ok(value["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned))
}

pub fn meeting_tags(base: &Path) -> Result<Vec<String>> {
    let path = base.with_extension("meeting.json");
    let Some((bytes, _, _)) = metadata_snapshot(&path)? else {
        return Ok(Vec::new());
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).context("会议元数据无法读取")?;
    anyhow::ensure!(value.is_object(), "会议元数据不是 JSON 对象");
    value
        .get("tags")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("会议标签格式错误")
        .map(|tags| tags.unwrap_or_default())
}

pub fn save_meeting_tags(base: &Path, text: &str) -> Result<Vec<String>> {
    save_meeting_tags_checked(base, text, None)
}

pub fn save_meeting_tags_checked(
    base: &Path,
    text: &str,
    expected_tags: Option<&[String]>,
) -> Result<Vec<String>> {
    let (tags, warning) = save_meeting_tags_for_identity(base, text, expected_tags, None)?;
    if let Some(warning) = warning {
        anyhow::bail!("标签已保存，但资产登记失败：{warning}");
    }
    Ok(tags)
}

pub fn save_meeting_tags_for_identity(
    base: &Path,
    text: &str,
    expected_tags: Option<&[String]>,
    expected_meeting: Option<Option<&str>>,
) -> Result<(Vec<String>, Option<String>)> {
    let _meeting_activity = opencap_core::managed_meeting_activity(base)?;
    let _metadata_guard = opencap_core::meeting_metadata_guard(base)?;
    if let Some(expected) = expected_meeting {
        let current = opencap_core::meeting_manifest(base)?;
        anyhow::ensure!(
            current
                .as_ref()
                .map(|manifest| manifest.meeting_id.as_str())
                == expected,
            "会议身份已变化，请重新打开标签编辑器"
        );
    }
    let mut tags = Vec::<String>::new();
    for tag in text
        .split([',', '，', '\n'])
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
    {
        anyhow::ensure!(
            tag.chars().count() <= 32 && !tag.chars().any(char::is_control),
            "每个标签最多 32 字，不能包含控制字符"
        );
        if !tags
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(tag))
        {
            tags.push(tag.to_owned());
        }
    }
    anyhow::ensure!(tags.len() <= 20, "每场会议最多 20 个标签");
    let path = base.with_extension("meeting.json");
    let snapshot = metadata_snapshot(&path)?;
    let mut value: serde_json::Value = match snapshot.as_ref() {
        Some((bytes, _, _)) => {
            serde_json::from_slice(bytes).context("会议元数据无法读取，未覆盖")?
        }
        None => serde_json::json!({}),
    };
    anyhow::ensure!(value.is_object(), "会议元数据不是 JSON 对象");
    let existing: Vec<String> = value
        .get("tags")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("已有标签格式错误，未覆盖")?
        .unwrap_or_default();
    anyhow::ensure!(
        expected_tags.is_none_or(|expected| expected == existing),
        "标签在编辑期间已变化，请重新载入后编辑"
    );
    value["tags"] = serde_json::to_value(&tags)?;
    if let Some(expected) = expected_meeting {
        let current = opencap_core::meeting_manifest(base)?;
        anyhow::ensure!(
            current
                .as_ref()
                .map(|manifest| manifest.meeting_id.as_str())
                == expected,
            "保存前会议身份已变化，标签未写入"
        );
    }
    save_metadata(&path, &value, snapshot)?;
    let warning = if let Some(name) = path.file_name() {
        opencap_core::register_meeting_asset_checked(
            base,
            "meeting.json",
            opencap_core::AssetOwnership::Managed,
            Path::new(name),
            expected_meeting.flatten(),
        )
        .err()
        .map(|error| format!("{error:#}"))
    } else {
        None
    };
    Ok((tags, warning))
}

pub fn rename_meeting(base: &Path, title: &str) -> Result<()> {
    rename_meeting_checked(base, title, None)
}

pub fn rename_meeting_checked(
    base: &Path,
    title: &str,
    expected_title: Option<Option<&str>>,
) -> Result<()> {
    rename_meeting_for_identity(base, title, expected_title, None)
}

pub fn rename_meeting_for_identity(
    base: &Path,
    title: &str,
    expected_title: Option<Option<&str>>,
    expected_meeting: Option<Option<&str>>,
) -> Result<()> {
    let _meeting_activity = opencap_core::managed_meeting_activity(base)?;
    let _metadata_guard = opencap_core::meeting_metadata_guard(base)?;
    if let Some(expected) = expected_meeting {
        let current = opencap_core::meeting_manifest(base)?;
        anyhow::ensure!(
            current
                .as_ref()
                .map(|manifest| manifest.meeting_id.as_str())
                == expected,
            "会议身份已变化，请重新打开编辑器"
        );
    }
    let title = title.trim();
    anyhow::ensure!(!title.is_empty(), "会议名称不能为空");
    anyhow::ensure!(title.chars().count() <= 100, "会议名称最多 100 个字");
    anyhow::ensure!(
        !title.chars().any(char::is_control),
        "会议名称不能包含换行或控制字符"
    );
    let path = base.with_extension("meeting.json");
    let snapshot = metadata_snapshot(&path)?;
    let mut value = if let Some((bytes, _, _)) = &snapshot {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .context("会议名称文件无法读取，未覆盖原文件")?
    } else {
        serde_json::json!({})
    };
    anyhow::ensure!(value.is_object(), "会议名称文件格式错误");
    if let Some(expected) = expected_title {
        let current = value
            .get("title")
            .and_then(|value| value.as_str())
            .filter(|title| !title.trim().is_empty());
        anyhow::ensure!(
            current == expected,
            "会议名称在编辑期间已变化，请重新打开编辑器"
        );
    }
    value["title"] = title.into();
    if let Some(expected) = expected_meeting {
        let current = opencap_core::meeting_manifest(base)?;
        anyhow::ensure!(
            current
                .as_ref()
                .map(|manifest| manifest.meeting_id.as_str())
                == expected,
            "保存前会议身份已变化"
        );
    }
    save_metadata(&path, &value, snapshot)
}

pub fn document_filename(title: &str, kind: &str) -> String {
    let title: String = title
        .chars()
        .take(80)
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let title = title.trim_matches([' ', '.']);
    format!(
        "{}-{kind}.docx",
        if title.is_empty() { "会议" } else { title }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rename_preserves_sources_and_metadata() {
        let dir = std::env::temp_dir().join(format!("opencap-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("recording");
        std::fs::write(base.with_extension("m4a"), b"original").unwrap();
        std::fs::write(base.with_extension("meeting.json"), br#"{"other":42}"#).unwrap();
        rename_meeting(&base, "  项目评审  ").unwrap();
        assert_eq!(meeting_name(&base).as_deref(), Some("项目评审"));
        assert!(
            std::fs::read_to_string(base.with_extension("meeting.json"))
                .unwrap()
                .contains("42")
        );
        assert_eq!(
            std::fs::read(base.with_extension("m4a")).unwrap(),
            b"original"
        );
        assert!(rename_meeting(&base, " ").is_err());
        assert_eq!(
            document_filename("预算/评审", "纪要"),
            "预算_评审-纪要.docx"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
