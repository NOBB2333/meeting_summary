//! Opt-in real AI provider check. The transcript is synthetic and contains no
//! user data; the API key is read from the environment, never from arguments.
use opencap_asr::{
    AiConfig, AiProvider, MinutesAction, MinutesHistory, Segment, Transcript, edit_minutes,
};
use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "requires OPENCAP_REAL_AI_KEY and sends only the synthetic transcript below"]
fn deepseek_responses_generates_minutes_for_synthetic_transcript() {
    let key = std::env::var("OPENCAP_REAL_AI_KEY").expect("set OPENCAP_REAL_AI_KEY");
    println!("key present: {} chars", key.chars().count());
    let config = AiConfig {
        provider: AiProvider::Responses,
        base_url: std::env::var("OPENCAP_REAL_AI_BASE")
            .unwrap_or_else(|_| "https://api.deepseek.com".into()),
        model: std::env::var("OPENCAP_REAL_AI_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
    };
    let transcript = if let Some(path) = std::env::var_os("OPENCAP_REAL_AI_TRANSCRIPT") {
        let mut source: Transcript = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let limit: usize = std::env::var("OPENCAP_REAL_AI_MAX_SEGMENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(12);
        source.segments.truncate(limit);
        source.complete = true;
        source.cancelled = false;
        source
    } else {
        Transcript {
            complete: true,
            segments: vec![
                Segment {
                    extra: Default::default(),
                    id: 0,
                    revision: 1,
                    start_ms: 0,
                    end_ms: 4_000,
                    text: "今天确认内部测试版下周发布，李四负责整理发布清单。".into(),
                    finalized: true,
                    correcting: false,
                    review: None,
                },
                Segment {
                    extra: Default::default(),
                    id: 1,
                    revision: 1,
                    start_ms: 4_000,
                    end_ms: 8_000,
                    text: "预算还没有批准，需要财务在本周五前给出意见。".into(),
                    finalized: true,
                    correcting: false,
                    review: None,
                },
            ],
            ..Default::default()
        }
    };
    let dir = match std::env::var_os("OPENCAP_REAL_AI_OUTPUT") {
        Some(path) => {
            std::fs::create_dir_all(&path).unwrap();
            std::path::PathBuf::from(path)
        }
        None => std::env::temp_dir().join(format!(
            "opencap-real-minutes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )),
    };
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("meeting");
    let history = match edit_minutes(
        &transcript,
        &config,
        &key,
        MinutesHistory::default(),
        MinutesAction::Generate,
        "",
        &base,
        &AtomicBool::new(false),
        |_| {},
    ) {
        Ok(history) => history,
        Err(error) => panic!("real provider failed: {error:#}"),
    };
    let body = history.latest();
    assert!(!body.trim().is_empty(), "minutes body is empty");
    assert!(base.with_extension("summary.md").is_file());
    println!(
        "real provider returned {} chars; saved summary.md and history",
        body.chars().count()
    );
    if std::env::var_os("OPENCAP_REAL_AI_OUTPUT").is_none() {
        std::fs::remove_dir_all(dir).unwrap();
    }
}
