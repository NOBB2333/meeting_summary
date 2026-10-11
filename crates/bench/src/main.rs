mod fixtures;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use opencap_core::*;
use opencap_media::{RecordingCommand, RecordingSession};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    version,
    about = "OpenCap native recording and reproducible encoding benchmarks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Enumerate monitors, windows and audio devices (IDs are local to this machine).
    Sources,
    /// Exercise each encoder and decode its output; does not assume GPU capabilities.
    Probe {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Native WGC + WASAPI recording. Ctrl+C stops and finalizes the MP4.
    Record {
        #[command(flatten)]
        options: Options,
        #[arg(long, default_value_t = 0)]
        seconds: u64,
        #[arg(long, default_value_t = 0)]
        monitor: usize,
        #[arg(long)]
        window: Option<isize>,
        /// Physical pixels: x,y,width,height, relative to selected monitor.
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        no_audio: bool,
        #[arg(long)]
        microphone: bool,
        /// Automated pause/resume validation: pause at this wall-clock second.
        #[arg(long)]
        pause_at: Option<u64>,
        #[arg(long, default_value_t = 2)]
        pause_for: u64,
        /// Integration check: start muted, enable/switch/mute both routes over 8 seconds.
        #[arg(long)]
        audio_switch_check: bool,
    },
    /// Procedural, redistributable 1080p scene. Reports synthetic results, not real application quality.
    Fixture {
        #[command(flatten)]
        options: Options,
        #[arg(long,default_value="ppt",value_parser=["ppt","document","ide","scroll","motion"])]
        scene: String,
        #[arg(long, default_value_t = 300)]
        seconds: u64,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        /// Encode without real-time pacing. Can saturate CPU/GPU; use on an idle machine.
        #[arg(long)]
        fast: bool,
    },
    /// Encode a user-supplied reference clip through the same policy and media pipeline.
    Encode {
        #[command(flatten)]
        options: Options,
        #[arg(long)]
        input: PathBuf,
    },
    /// Recover a fragmented recording without changing the original.
    Recover { input: PathBuf, output: PathBuf },
    /// Diagnostic: report what the system-audio loopback endpoint actually yields.
    AudioProbe {
        #[arg(long)]
        seconds: u64,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        microphone: bool,
    },
    /// Write a complete editable recording configuration.
    Config { output: PathBuf },
}
#[derive(Args)]
struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    encoder: Option<String>,
    #[arg(long,value_parser=["auto","h264","hevc","av1"])]
    codec: Option<String>,
    #[arg(long,value_parser=["space","balanced","quality"])]
    preset: Option<String>,
    #[arg(long)]
    fps: Option<u32>,
    #[arg(long)]
    fixed_fps: bool,
    #[arg(long)]
    quality: Option<u32>,
    #[arg(long)]
    max_kbps: Option<u32>,
}
impl Options {
    fn config(self) -> Result<RecordingConfig> {
        let mut c = if let Some(p) = self.config {
            RecordingConfig::load(&p)?
        } else {
            RecordingConfig::default()
        };
        if let Some(p) = self.output {
            c.output_dir = p;
        }
        if let Some(p) = self.preset {
            c.apply_preset(match p.as_str() {
                "balanced" => Preset::Balanced,
                "quality" => Preset::Quality,
                _ => Preset::Space,
            });
        }
        if let Some(codec) = self.codec {
            c.codec = match codec.as_str() {
                "h264" => Codec::H264,
                "hevc" => Codec::Hevc,
                "av1" => Codec::Av1,
                _ => Codec::Auto,
            };
        }
        if self.encoder.is_some() {
            c.encoder = self.encoder;
        }
        if let Some(fps) = self.fps {
            c.max_fps = fps;
        }
        if self.fixed_fps {
            c.adaptive = false;
        }
        if let Some(q) = self.quality {
            c.quality = q;
        }
        if let Some(b) = self.max_kbps {
            c.max_bitrate_kbps = b;
        }
        c.validate()?;
        Ok(c)
    }
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Sources => {
            for source in opencap_platform::targets()? {
                println!(
                    "{:?}\t{}\t{}x{}",
                    source.target, source.name, source.width, source.height
                );
            }
            for d in opencap_platform::audio_devices()? {
                println!("{:?}\t{}\t{}", d.kind, d.name, d.id);
            }
        }
        Command::Probe { output } => {
            let probes = opencap_media::probe_encoders();
            let text = serde_json::to_string_pretty(&probes)?;
            println!("{text}");
            if let Some(p) = output {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(p, text)?;
            }
        }
        Command::Config { output } => RecordingConfig::default().save(&output)?,
        Command::Recover { input, output } => opencap_media::recover_file(&input, &output)?,
        Command::AudioProbe {
            seconds,
            device,
            microphone,
        } => {
            if microphone {
                println!("probe microphone seconds={seconds} device={device:?}");
                println!(
                    "{}",
                    opencap_platform::audio_probe_microphone(device, seconds)?
                );
            } else {
                println!("probe system loopback seconds={seconds} device={device:?}");
                println!(
                    "{}",
                    opencap_platform::audio_probe_loopback(device, seconds)?
                );
            }
        }
        Command::Record {
            options,
            seconds,
            monitor,
            window,
            region,
            no_audio,
            microphone,
            pause_at,
            pause_for,
            audio_switch_check,
        } => {
            let mut c = options.config()?;
            c.target = if let Some(hwnd) = window {
                CaptureTarget::Window(hwnd)
            } else if let Some(region) = region {
                let n: Vec<u32> = region
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?;
                anyhow::ensure!(n.len() == 4, "Region syntax: x,y,width,height");
                CaptureTarget::Region {
                    monitor,
                    rect: Rect {
                        x: n[0],
                        y: n[1],
                        width: n[2],
                        height: n[3],
                    },
                }
            } else {
                CaptureTarget::Monitor(monitor)
            };
            if no_audio {
                c.system_audio = false;
                c.microphone = false;
                c.reserve_audio_track = false;
            }
            if microphone {
                c.microphone = true;
            }
            let devices = if audio_switch_check {
                anyhow::ensure!(seconds >= 8, "Audio switch check needs --seconds 8 or more");
                c.system_audio = false;
                c.microphone = false;
                c.reserve_audio_track = true;
                opencap_platform::audio_devices()?
            } else {
                Vec::new()
            };
            let mut audio_step = 0;
            let mut routing = c.audio_routing();
            let stop = Arc::new(AtomicBool::new(false));
            let signal = stop.clone();
            ctrlc::set_handler(move || signal.store(true, Ordering::Release))?;
            let mut session = RecordingSession::start(c)?;
            let mut started = None;
            let mut paused = false;
            let mut resumed = false;
            while !session.finished() {
                let s = session.stats();
                if s.state == RecordingState::Recording && started.is_none() {
                    started = Some(Instant::now());
                }
                if let Some(t) = started {
                    if audio_switch_check && audio_step < 7 && t.elapsed().as_secs() > audio_step {
                        audio_step += 1;
                        match audio_step {
                            1 => routing.system_audio = true,
                            2 => routing.microphone = true,
                            3 => {
                                routing.system_device = devices
                                    .iter()
                                    .find(|d| d.kind == AudioKind::System)
                                    .map(|d| d.id.clone())
                            }
                            4 => routing.microphone = false,
                            5 => routing.system_audio = false,
                            6 => {
                                routing.microphone_device = devices
                                    .iter()
                                    .find(|d| d.kind == AudioKind::Microphone)
                                    .map(|d| d.id.clone());
                                routing.microphone = true;
                            }
                            _ => {
                                routing.system_audio = false;
                                routing.microphone = false;
                            }
                        }
                        session.set_audio(routing.clone())?;
                    }
                    if let Some(at) = pause_at {
                        if !paused && t.elapsed().as_secs() >= at {
                            session.command(RecordingCommand::Pause);
                            paused = true;
                        }
                        if paused && !resumed && t.elapsed().as_secs() >= at + pause_for {
                            session.command(RecordingCommand::Resume);
                            resumed = true;
                        }
                    }
                    if seconds > 0 && t.elapsed().as_secs() >= seconds {
                        session.command(RecordingCommand::Stop);
                    }
                }
                if stop.load(Ordering::Acquire) {
                    session.command(RecordingCommand::Stop);
                }
                eprintln!(
                    "{:?} {:.1}s {:.1} FPS {:.0} kbps {:.2} MB {}",
                    s.state,
                    s.duration_seconds,
                    s.fps,
                    s.bitrate_kbps,
                    s.bytes as f64 / 1e6,
                    s.encoder
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            session.join();
            eprintln!("audio-diag {}", opencap_platform::audio_diagnostics());
            let stats = session.stats();
            println!("{}", serde_json::to_string_pretty(&stats)?);
            anyhow::ensure!(
                stats.state == RecordingState::Finished,
                "{}",
                stats.error.unwrap_or_default()
            );
        }
        Command::Fixture {
            options,
            scene,
            seconds,
            width,
            height,
            fast,
        } => {
            anyhow::ensure!(
                seconds > 0 && width >= 2 && height >= 2 && width <= 7680 && height <= 4320,
                "Invalid fixture dimensions or duration"
            );
            fixtures::run(options.config()?, &scene, seconds, width, height, fast)?;
        }
        Command::Encode { options, input } => {
            let config = options.config()?;
            std::fs::create_dir_all(&config.output_dir)?;
            let stem = input
                .file_stem()
                .context("Missing input filename")?
                .to_string_lossy();
            let output = config.output_dir.join(format!("{stem}-encoded.mp4"));
            opencap_media::encode_reference(&input, &output, &config)?;
            println!("{}", output.display());
        }
    }
    Ok(())
}
