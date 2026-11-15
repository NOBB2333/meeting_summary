//! Opt-in real-provider check for the incremental notes JSON contract, using
//! only synthetic text. The key comes from the environment, never arguments.
use opencap_asr::{
    AiConfig, AiProvider, LiveNotes, Segment, Transcript, TranscriptionRun, request_live_notes,
};
use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "requires OPENCAP_REAL_AI_KEY and sends only the synthetic transcript below"]
fn deepseek_live_notes_returns_valid_cited_json() {
    let key = std::env::var("OPENCAP_REAL_AI_KEY").expect("set OPENCAP_REAL_AI_KEY");
    let config = AiConfig {
        provider: AiProvider::Responses,
        base_url: std::env::var("OPENCAP_REAL_AI_BASE")
            .unwrap_or_else(|_| "https://api.deepseek.com".into()),
        model: std::env::var("OPENCAP_REAL_AI_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        background: false,
        deep_reasoning: false,
    };
    let run = TranscriptionRun {
        run_id: "run-real-test".into(),
        started_unix_ms: 1,
        finished_unix_ms: None,
        application_version: "test".into(),
        backend: "synthetic".into(),
        runtime_version: "test".into(),
        model_id: "synthetic".into(),
        model_sha256: None,
        provider: "cpu".into(),
        threads: 1,
        requested_language: "zh".into(),
        effective_language: None,
        requested_hotwords: String::new(),
        accepted_hotwords: None,
        punctuation_requested: false,
        punctuation_effective: "none".into(),
        revision_scheduled: false,
        revision_model: None,
        input_kind: "file".into(),
        state: "running".into(),
        meeting_id: Some("meeting-real-test".into()),
        input_asset_id: None,
        extra: Default::default(),
    };
    let segment = |id: u64, start_ms: u64, text: &str| Segment {
        extra: Default::default(),
        id,
        revision: 1,
        start_ms,
        end_ms: start_ms + 3_000,
        text: text.into(),
        finalized: true,
        correcting: false,
        review: None,
    };
    let transcript = Transcript {
        run: Some(run),
        segments: vec![
            segment(0, 0, "我们决定下周发布内部测试版。"),
            segment(1, 3_000, "李四负责整理发布清单。"),
            segment(2, 6_000, "预算还没有批准，需要财务周五前回复。"),
        ],
        ..Default::default()
    };
    let mut notes = LiveNotes::new("meeting-real-test".into(), "run-real-test".into()).unwrap();
    let request = notes.begin(&transcript, false).unwrap().unwrap();
    let proposal = request_live_notes(&config, &key, &request, &AtomicBool::new(false))
        .expect("real provider should return valid live-notes JSON");
    assert!(!proposal.items.is_empty(), "no live note items returned");
    notes.apply(&request, &transcript, proposal).unwrap();
    assert!(!notes.items.is_empty());
    for item in &notes.items {
        for evidence in &item.evidence {
            assert!(evidence.segment_id < 3);
        }
    }
    println!(
        "real provider returned {} live-note items after {} revision(s)",
        notes.items.len(),
        notes.revision
    );
}
