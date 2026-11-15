//! Backend-neutral streaming speaker events. Audio source is not speaker ID.
use anyhow::Result;
use opencap_core::PcmAudio;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineSpeakerInterval {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Session-local backend slot, not a person's name or a cross-session ID.
    pub slot: u32,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineSpeakerSnapshot {
    pub revision: u64,
    pub input_end_ms: u64,
    pub finished: bool,
    /// The backend may revise recent intervals. A snapshot replaces the prior
    /// backend view; it does not override user-confirmed speaker identity.
    pub intervals: Vec<OnlineSpeakerInterval>,
}

/// An instance owns state across pushes and is used by one fixed worker.
/// Dropped audio must be surfaced, not silently compressed on the timeline.
pub trait OnlineDiarizer {
    fn push(&mut self, audio: &PcmAudio) -> Result<()>;
    fn snapshot(&mut self) -> Result<OnlineSpeakerSnapshot>;
    fn finish(&mut self) -> Result<OnlineSpeakerSnapshot>;
    fn capacity(&self) -> usize;
}
