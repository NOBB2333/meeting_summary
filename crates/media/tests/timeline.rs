use opencap_core::*;
use opencap_media::{MediaWriter, export_audio, recover_file};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "opencap-test-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn transcription_tap_has_mixed_audio_tail_without_stalling_full_queues() {
    for capacity in [1, 128] {
        let dir = scratch();
        let path = dir.join("observed.mp4");
        let config = RecordingConfig {
            encoder: Some("libx264".into()),
            ..Default::default()
        };
        let mut writer = MediaWriter::new(&path, 320, 180, &config).unwrap();
        writer
            .video(&VideoFrame {
                width: 320,
                height: 180,
                stride: 320 * 4,
                pts_us: 0,
                data: Arc::new(vec![90; 320 * 180 * 4]),
            })
            .unwrap();
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        writer.observe_audio(tx);
        writer.audio(AudioChunk {
            kind: AudioKind::System,
            pts_us: 0,
            samples: vec![0.1; 48000 * 2],
        });
        writer.audio(AudioChunk {
            kind: AudioKind::Microphone,
            pts_us: 0,
            samples: vec![0.2; 48000 * 2],
        });
        writer.finish(1_000_000).unwrap();
        let chunks: Vec<_> = rx.try_iter().collect();
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .flat_map(|c| &c.samples)
                .all(|x| (*x - 0.3).abs() < 1e-6)
        );
        if capacity == 128 {
            assert_eq!(
                chunks.iter().map(|c| c.samples.len()).sum::<usize>(),
                48000,
                "tap lost the non-AAC-aligned tail"
            );
            for c in chunks {
                assert_eq!((c.sample_rate, c.channels), (48000, 1));
            }
        }
        let context = ffmpeg_next::format::input(&path).unwrap();
        assert!((context.duration() - 1_000_000).abs() < 50000);
        drop(context);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[test]
fn vfr_static_tail_audio_and_recovery_preserve_duration() {
    let dir = scratch();
    let partial = dir.join("test.partial.mp4");
    let output = dir.join("test.mp4");
    let config = RecordingConfig {
        encoder: Some("libx264".into()),
        ..Default::default()
    };
    let mut writer = MediaWriter::new(&partial, 320, 180, &config).unwrap();
    let pixels = Arc::new(vec![100; 320 * 180 * 4]);
    for pts in [0, 500_000, 1_500_000] {
        writer
            .video(&VideoFrame {
                width: 320,
                height: 180,
                stride: 320 * 4,
                pts_us: pts,
                data: pixels.clone(),
            })
            .unwrap();
        writer.audio_until(pts, false).unwrap();
    }
    writer.finish(2_000_000).unwrap();
    recover_file(&partial, &output).unwrap();
    let context = ffmpeg_next::format::input(&output).unwrap();
    assert!(
        (context.duration() - 2_000_000).abs() < 50_000,
        "duration={}",
        context.duration()
    );
    assert_eq!(context.nb_streams(), 2);
    assert!(
        recover_file(&partial, &output).is_err(),
        "Existing files must never be overwritten"
    );
    drop(context);
    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(partial).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn exports_a_same_timeline_m4a_without_video() {
    let dir = scratch();
    let video = dir.join("recording.mp4");
    let audio = dir.join("recording.m4a");
    let config = RecordingConfig {
        encoder: Some("libx264".into()),
        reserve_audio_track: true,
        ..Default::default()
    };
    let mut writer = MediaWriter::new(&video, 320, 180, &config).unwrap();
    let pixels = Arc::new(vec![80; 320 * 180 * 4]);
    for step in 0..20 {
        let pts = step * 100_000;
        writer
            .video(&VideoFrame {
                width: 320,
                height: 180,
                stride: 320 * 4,
                pts_us: pts,
                data: pixels.clone(),
            })
            .unwrap();
        writer.audio(AudioChunk {
            kind: AudioKind::Microphone,
            pts_us: pts,
            samples: vec![0.15; 4_800 * 2],
        });
        writer.audio_until(pts, false).unwrap();
    }
    writer.finish(2_000_000).unwrap();
    export_audio(&video, &audio).unwrap();
    let context = ffmpeg_next::format::input(&audio).unwrap();
    assert_eq!(context.nb_streams(), 1);
    assert!(
        context
            .streams()
            .best(ffmpeg_next::media::Type::Audio)
            .is_some()
    );
    assert!(
        context
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .is_none()
    );
    assert!((context.duration() - 2_000_000).abs() < 100_000);
    drop(context);
    std::fs::remove_file(audio).unwrap();
    std::fs::remove_file(video).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn invalid_private_encoder_option_is_rejected() {
    let mut config = RecordingConfig {
        encoder: Some("libx264".into()),
        ..Default::default()
    };
    config
        .encoder_options
        .insert("typo_not_a_real_option".into(), "1".into());
    assert!(opencap_media::FfmpegVideoEncoder::new("libx264", 320, 180, &config).is_err());
}

#[test]
fn resizing_source_keeps_output_geometry() {
    let dir = scratch();
    let path = dir.join("resize.mp4");
    let config = RecordingConfig {
        encoder: Some("libx264".into()),
        system_audio: false,
        ..Default::default()
    };
    let mut writer = MediaWriter::new(&path, 320, 180, &config).unwrap();
    for (width, height, pts) in [(320, 180, 0), (180, 320, 500_000), (640, 360, 1_000_000)] {
        writer
            .video(&VideoFrame {
                width,
                height,
                stride: width as usize * 4,
                pts_us: pts,
                data: Arc::new(vec![80; width as usize * height as usize * 4]),
            })
            .unwrap();
    }
    writer.finish(1_500_000).unwrap();
    let context = ffmpeg_next::format::input(&path).unwrap();
    assert_eq!(
        context.nb_streams(),
        1,
        "Explicit video-only recordings must remain video-only"
    );
    let s = context
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .unwrap();
    let decoder = ffmpeg_next::codec::context::Context::from_parameters(s.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    assert_eq!((decoder.width(), decoder.height()), (320, 180));
    drop(decoder);
    drop(context);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn initially_muted_recording_can_add_audio_without_restarting_mp4() {
    let dir = scratch();
    let path = dir.join("live-audio.mp4");
    let config = RecordingConfig {
        encoder: Some("libx264".into()),
        system_audio: false,
        microphone: false,
        reserve_audio_track: true,
        ..Default::default()
    };
    let mut writer = MediaWriter::new(&path, 320, 180, &config).unwrap();
    let pixels = Arc::new(vec![90; 320 * 180 * 4]);
    for step in 0..30 {
        let pts = step * 100_000;
        writer
            .video(&VideoFrame {
                width: 320,
                height: 180,
                stride: 320 * 4,
                pts_us: pts,
                data: pixels.clone(),
            })
            .unwrap();
        if (10..20).contains(&step) {
            let samples = (0..4800)
                .flat_map(|i| {
                    let value = (i as f32 * std::f32::consts::TAU * 440. / 48000.).sin() * 0.4;
                    [value, value]
                })
                .collect();
            writer.audio(AudioChunk {
                kind: AudioKind::Microphone,
                pts_us: pts,
                samples,
            });
        }
        writer.audio_until(pts, false).unwrap();
    }
    writer.finish(3_000_000).unwrap();
    let mut context = ffmpeg_next::format::input(&path).unwrap();
    assert_eq!(context.nb_streams(), 2);
    assert!((context.duration() - 3_000_000).abs() < 50_000);
    let stream = context
        .streams()
        .best(ffmpeg_next::media::Type::Audio)
        .unwrap();
    let index = stream.index();
    let tb = stream.time_base();
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .audio()
        .unwrap();
    let mut levels = [0.0f32; 3];
    let mut frames = [0; 3];
    let mut receive = |decoder: &mut ffmpeg_next::decoder::Audio| {
        let mut frame = ffmpeg_next::frame::Audio::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            let seconds = frame.pts().unwrap_or(0) as f64 * f64::from(tb);
            let band = if (0.2..0.8).contains(&seconds) {
                Some(0)
            } else if (1.2..1.8).contains(&seconds) {
                Some(1)
            } else if (2.2..2.8).contains(&seconds) {
                Some(2)
            } else {
                None
            };
            if let Some(band) = band {
                frames[band] += 1;
                levels[band] = frame
                    .plane::<f32>(0)
                    .iter()
                    .fold(levels[band], |max, x| max.max(x.abs()));
            }
        }
    };
    for (stream, packet) in context.packets() {
        if stream.index() == index {
            decoder.send_packet(&packet).unwrap();
            receive(&mut decoder);
        }
    }
    decoder.send_eof().unwrap();
    receive(&mut decoder);
    assert!(frames.iter().all(|&n| n > 0));
    assert!(
        levels[0] < 0.001 && levels[1] > 0.2 && levels[2] < 0.001,
        "decoded levels: {levels:?}"
    );
    drop(decoder);
    drop(context);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
