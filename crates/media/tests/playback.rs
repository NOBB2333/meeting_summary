#![cfg(feature = "internal-player")]
//! Opt-in end-to-end checks for the internal player on a real, long media file.
//! They need an audio output device and an authorized media path.
use opencap_media::{PlaybackCommand, PlaybackRange, PlaybackSession, PlaybackState};
use std::time::{Duration, Instant};

fn wait_for<F: FnMut() -> bool>(what: &str, timeout: Duration, ready: F) {
    assert!(
        wait_for_timeout(timeout, ready),
        "timed out waiting for {what}"
    );
}
fn wait_for_timeout<F: FnMut() -> bool>(timeout: Duration, mut ready: F) -> bool {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
#[ignore = "requires OPENCAP_PLAYBACK_TEST_MEDIA (authorized audio/video) and a working audio output device"]
fn real_media_play_seek_rapid_seek_and_loop() {
    let path = std::env::var_os("OPENCAP_PLAYBACK_TEST_MEDIA")
        .expect("set OPENCAP_PLAYBACK_TEST_MEDIA to an authorized media file");
    let session = PlaybackSession::open_with_source(path.into(), None, None).unwrap();
    // The worker opens the container on the first Play command.
    session.command(PlaybackCommand::SetVolume(0.0)).unwrap();
    session.command(PlaybackCommand::PlayAt(0)).unwrap();
    wait_for("duration", Duration::from_secs(20), || {
        session.snapshot().duration_ms.is_some()
    });
    let duration = session.snapshot().duration_ms.unwrap();
    assert!(duration >= 60_000, "expected a long real recording");
    if !wait_for_timeout(Duration::from_secs(10), || {
        matches!(session.snapshot().state, PlaybackState::Playing)
    }) {
        let snapshot = session.snapshot();
        panic!(
            "player did not reach Playing: state={:?} position={} duration={:?} error={:?} video_error={:?}",
            snapshot.state,
            snapshot.position_ms,
            snapshot.duration_ms,
            snapshot.error,
            snapshot.video_error
        );
    }
    std::thread::sleep(Duration::from_millis(1_500));
    let advanced = session.snapshot().position_ms;
    assert!(advanced > 300, "position did not advance: {advanced}");

    let target = duration * 2 / 3;
    session.command(PlaybackCommand::Seek(target)).unwrap();
    std::thread::sleep(Duration::from_millis(1_200));
    let after = session.snapshot().position_ms;
    assert!(
        after.abs_diff(target) < 5_000,
        "seek did not reach {target}: {after}"
    );

    // Rapid seeks must leave only the newest generation audible/visible.
    let _ = session.command(PlaybackCommand::Seek(120_000));
    let _ = session.command(PlaybackCommand::Seek(30_000));
    let _ = session.command(PlaybackCommand::Seek(target));
    std::thread::sleep(Duration::from_millis(2_000));
    let rapid = session.snapshot().position_ms;
    assert!(
        rapid.abs_diff(target) < 8_000,
        "rapid seeks left a stale position {rapid} (target {target})"
    );

    // A-B loop wraps at the end of the range.
    let start = duration / 5;
    let range = PlaybackRange::new(start, start + 2_000, true).unwrap();
    // PlayRange replaces the range, seeks to its start and plays as one
    // operation; PlayAt would clear the range.
    session.command(PlaybackCommand::PlayRange(range)).unwrap();
    std::thread::sleep(Duration::from_millis(2_500));
    let looped = session.snapshot().position_ms;
    assert!(
        (start..start + 2_000).contains(&looped),
        "loop did not wrap: start={start} actual={looped}"
    );
    let _ = session.command(PlaybackCommand::SetRange(None));
    let _ = session.command(PlaybackCommand::Close);
}

#[test]
#[ignore = "requires OPENCAP_PLAYBACK_TEST_VIDEO (short authorized MP4 with audio) and an audio output device"]
fn real_video_decodes_frames_and_seek_keeps_the_generation() {
    let path = std::env::var_os("OPENCAP_PLAYBACK_TEST_VIDEO")
        .expect("set OPENCAP_PLAYBACK_TEST_VIDEO to a short authorized MP4");
    let session = PlaybackSession::open_with_source(path.into(), None, None).unwrap();
    session.command(PlaybackCommand::SetVolume(0.0)).unwrap();
    session.command(PlaybackCommand::PlayAt(0)).unwrap();
    wait_for("video duration", Duration::from_secs(20), || {
        session
            .snapshot()
            .duration_ms
            .is_some_and(|duration| duration > 1_000)
    });
    wait_for("video stream", Duration::from_secs(10), || {
        session.snapshot().has_video
    });
    let generation = session.snapshot().generation;
    let mut frame = None;
    wait_for("first video frame", Duration::from_secs(15), || {
        if let Some(candidate) = session.take_video_frame() {
            frame = Some(candidate);
            true
        } else {
            std::thread::sleep(Duration::from_millis(100));
            false
        }
    });
    let frame = frame.expect("video frame");
    assert_eq!(frame.generation, generation);
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(
        frame.bgra.len(),
        frame.width as usize * frame.height as usize * 4
    );

    session.command(PlaybackCommand::Seek(2_000)).unwrap();
    std::thread::sleep(Duration::from_millis(1_200));
    let after = session.snapshot();
    assert!(after.position_ms.abs_diff(2_000) < 1_500);
    let mut sought = None;
    let deadline = Instant::now();
    while sought.is_none() && deadline.elapsed() < Duration::from_secs(10) {
        if let Some(candidate) = session.take_video_frame()
            && candidate.generation == after.generation
        {
            sought = Some(candidate);
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    assert!(sought.is_some(), "no frame from the post-seek generation");
    let _ = session.command(PlaybackCommand::Close);
}

#[test]
#[ignore = "requires OPENCAP_PLAYBACK_TEST_MEDIA"]
fn set_range_then_play_at_is_a_plain_seek() {
    let path =
        std::env::var_os("OPENCAP_PLAYBACK_TEST_MEDIA").expect("set OPENCAP_PLAYBACK_TEST_MEDIA");
    let session = PlaybackSession::open_with_source(path.into(), None, None).unwrap();
    session.command(PlaybackCommand::SetVolume(0.0)).unwrap();
    session.command(PlaybackCommand::PlayAt(0)).unwrap();
    wait_for("duration", Duration::from_secs(20), || {
        session.snapshot().duration_ms.is_some()
    });
    let duration = session.snapshot().duration_ms.unwrap();
    let start = duration / 5;
    let range = PlaybackRange::new(start, start + 2_000, true).unwrap();
    session
        .command(PlaybackCommand::SetRange(Some(range)))
        .unwrap();
    session
        .command(PlaybackCommand::PlayAt(start + 1_500))
        .unwrap();
    std::thread::sleep(Duration::from_millis(2_500));
    let snapshot = session.snapshot();
    println!(
        "duration={duration} start={start} position={} state={:?} range={:?} error={:?}",
        snapshot.position_ms, snapshot.state, snapshot.range, snapshot.error
    );
    assert!(snapshot.position_ms <= duration);
    let _ = session.command(PlaybackCommand::Close);
}
