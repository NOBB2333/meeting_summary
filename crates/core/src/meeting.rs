//! Storage shared by recordings and imported meeting artifacts.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn folder_name(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .take(80)
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '.'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '_']).to_owned();
    if cleaned.is_empty() {
        return "会议".into();
    }
    let upper = cleaned.to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
    {
        format!("会议_{cleaned}")
    } else {
        cleaned
    }
}
/// Atomically allocate a new directory. Never reuse or overwrite an existing meeting.
/// The returned extensionless prefix keeps all generated artifacts in that directory.
pub fn create_meeting_base(root: &Path, label: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root).context("无法创建会议保存目录")?;
    let name = folder_name(label);
    for n in 0..1000 {
        let name = if n == 0 {
            name.clone()
        } else {
            format!("{name}_{n:02}")
        };
        let dir = root.join(&name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir.join(name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("无法创建本场会议文件夹"),
        }
    }
    anyhow::bail!("同名会议过多，请更换保存目录或文件名")
}
/// Include legacy flat files and new meeting folders; do not follow directory symlinks.
pub fn recoverable_recordings(root: &Path) -> Vec<PathBuf> {
    fn scan(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten().take(2000) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                scan(&entry.path(), depth - 1, files);
            } else if kind.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if (name.ends_with(".partial.mp4") || name.ends_with(".partial.m4a"))
                    && entry.metadata().is_ok_and(|m| m.len() > 0)
                {
                    files.push(entry.path());
                }
            }
        }
    }
    let mut files = Vec::new();
    scan(root, 3, &mut files);
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meeting_folders_are_distinct_portable_and_recovery_finds_nested_and_legacy_files() {
        let root = std::env::temp_dir().join(format!(
            "opencap-folders-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = create_meeting_base(&root, "例会/预算:2.0?").unwrap();
        let second = create_meeting_base(&root, "例会/预算:2.0?").unwrap();
        assert_ne!(base.parent(), second.parent());
        assert!(base.parent().unwrap().starts_with(&root));
        assert_eq!(base.file_name().unwrap(), "例会_预算_2_0");
        let recording = base.with_extension("partial.m4a");
        std::fs::write(&recording, b"media fixture").unwrap();
        let legacy = root.join("legacy.partial.mp4");
        std::fs::write(&legacy, b"legacy fixture").unwrap();
        std::fs::write(root.join("empty.partial.mp4"), []).unwrap();
        std::fs::write(root.join("completed.m4a"), b"complete").unwrap();
        let mut expected = vec![recording, legacy];
        expected.sort();
        assert_eq!(recoverable_recordings(&root), expected);
        assert_eq!(
            std::fs::read(base.with_extension("partial.m4a")).unwrap(),
            b"media fixture"
        );
        for label in ["CON", "LPT1", "..", " / ", "NUL", "COM9"] {
            let safe = create_meeting_base(&root, label).unwrap();
            assert_ne!(
                safe.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_uppercase(),
                label
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
