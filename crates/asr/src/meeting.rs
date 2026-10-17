//! Small local metadata; renaming a meeting never moves its source media.
use anyhow::{Context, Result};
use std::path::Path;

pub fn meeting_name(base: &Path) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.with_extension("meeting.json")).ok()?).ok()?;
    value["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

pub fn rename_meeting(base: &Path, title: &str) -> Result<()> {
    let title = title.trim();
    anyhow::ensure!(!title.is_empty(), "会议名称不能为空");
    anyhow::ensure!(title.chars().count() <= 100, "会议名称最多 100 个字");
    anyhow::ensure!(
        !title.chars().any(char::is_control),
        "会议名称不能包含换行或控制字符"
    );
    let path = base.with_extension("meeting.json");
    let mut value = if path.exists() {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path)?)
            .context("会议名称文件无法读取，未覆盖原文件")?
    } else {
        serde_json::json!({})
    };
    anyhow::ensure!(value.is_object(), "会议名称文件格式错误");
    value["title"] = title.into();
    crate::atomic_write(&path, &serde_json::to_vec_pretty(&value)?)
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
