use anyhow::Result;
use clap::{Parser, Subcommand};
use opencap_asr::*;
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "OpenCap local CPU transcription; no GPU or Python required")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Inspect saved status without reading or printing meeting text or secrets.
    Status {
        #[arg(long)]
        meeting: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Create a new JSON report; an existing file is never overwritten.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Decode an authorized sample and write a new, text-free JSON diagnostic report.
    DiagnoseReplay {
        input: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[arg(long, value_enum, default_value = "none")]
        fault: DiagnosticFault,
    },
    /// Replay the existing Zipformer live backend on the same POC PCM clock.
    #[cfg(feature = "nemo-poc")]
    BaselineAsrPoc {
        input: PathBuf,
        #[arg(long)]
        model_root: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Replay Nemotron ASR using a trusted native library and pinned model.
    #[cfg(feature = "nemo-poc")]
    NemoAsrPoc {
        input: PathBuf,
        #[arg(long)]
        library: PathBuf,
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Experimental, local-only streaming diarization replay using a trusted native library.
    #[cfg(feature = "nemo-poc")]
    NemoDiarizePoc {
        input: PathBuf,
        #[arg(long)]
        library: PathBuf,
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Prepare {
        #[arg(value_enum)]
        model: ModelKind,
    },
    PreparePunctuation,
    Transcribe {
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "offline")]
        model: ModelKind,
        #[arg(long)]
        realtime: bool,
        /// Do not run a second, sentence-level model for streaming recognition.
        #[arg(long)]
        no_revision: bool,
        /// Do not add automatic punctuation to recognition results.
        #[arg(long)]
        no_punctuation: bool,
        /// Offline model used for optional sentence-level correction.
        #[arg(long, value_enum, default_value = "offline")]
        revision_model: ModelKind,
        #[arg(long,default_value=DEFAULT_HOTWORDS)]
        hotwords: String,
        /// Language hint: zh (Chinese with English terms), en, or auto (five languages).
        #[arg(long, default_value = "zh", value_parser = ["zh", "en", "auto"])]
        language: String,
    },
    Summary {
        transcript: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        model: Option<String>,
    },
    Speakers {
        transcript: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "0")]
        count: u32,
        #[arg(long, default_value = "0.9")]
        threshold: f32,
        #[arg(long, default_value = "2")]
        threads: usize,
    },
    ExportWord {
        markdown: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    ExportTranscript {
        transcript: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
fn main() -> Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--is-production-build"))
    {
        std::process::exit(if opencap_core::is_dev_profile() {
            43
        } else {
            42
        });
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-identity")) {
        println!("{}", opencap_core::app_name());
        return Ok(());
    }
    match Cli::parse().command {
        Command::Status {
            meeting,
            config,
            output,
        } => {
            let report = diagnostic_status(config.as_deref(), meeting.as_deref())?;
            let text = serde_json::to_string_pretty(&report)?;
            if let Some(path) = output {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
            } else {
                println!("{text}");
            }
        }
        Command::DiagnoseReplay {
            input,
            report,
            fault,
        } => diagnostic_replay(&input, &report, fault)?,
        #[cfg(feature = "nemo-poc")]
        Command::BaselineAsrPoc {
            input,
            model_root,
            output,
        } => {
            let directory = model_root.join(ModelKind::Streaming.directory());
            let mut files = Vec::new();
            let mut model_fingerprints = Vec::new();
            for name in ModelKind::Streaming.files() {
                let (bytes, sha256) = fingerprint_poc_file(&directory.join(name))?;
                files.push(serde_json::json!({"name":name,"bytes":bytes,"sha256":sha256.clone()}));
                model_fingerprints.push((directory.join(name), bytes, sha256));
            }
            let punctuation_path = punctuation_model_path(&model_root);
            let punctuation_present = punctuation_ready(&model_root);
            let punctuation_fingerprint = if punctuation_present {
                let (size_bytes, sha256) = fingerprint_poc_file(&punctuation_path)?;
                Some(opencap_core::MediaFingerprint { size_bytes, sha256 })
            } else {
                None
            };
            let loading = std::time::Instant::now();
            let mut backend = SherpaStreamingAsr::new(model_root.clone())?;
            let metadata = serde_json::json!({"route":"A", "backend":"sherpa Zipformer", "provider":"cpu", "model_files":files,
                "punctuation_fingerprint_before_replay":punctuation_fingerprint.clone(),
                "startup_ms":loading.elapsed().as_millis(), "model_load_ms":"worker asynchronous; not separately measured",
                "punctuation":"requested; effective status not independently measured", "word_timing":"raw token starts when supplied; not mapped or evaluated"});
            replay_streaming_asr(&input, &output, &mut backend, metadata, || {
                for (path, bytes, sha256) in &model_fingerprints {
                    anyhow::ensure!(
                        fingerprint_poc_file(path)? == (*bytes, sha256.clone()),
                        "实验期间基线模型文件发生变化"
                    );
                }
                anyhow::ensure!(
                    punctuation_ready(&model_root) == punctuation_present,
                    "实验期间标点模型可用状态发生变化"
                );
                if let Some(expected) = &punctuation_fingerprint {
                    let (size_bytes, sha256) = fingerprint_poc_file(&punctuation_path)?;
                    anyhow::ensure!(
                        &opencap_core::MediaFingerprint { size_bytes, sha256 } == expected,
                        "实验期间标点模型发生变化"
                    );
                }
                Ok(())
            })?;
        }
        #[cfg(feature = "nemo-poc")]
        Command::NemoAsrPoc {
            input,
            library,
            model,
            output,
        } => {
            let loading = std::time::Instant::now();
            // The explicit path must name a trusted library built from the pinned ABI.
            let mut backend = unsafe { NemoStreamingAsr::open(&library, &model)? };
            let library_sha256 = backend.library_sha256.clone();
            let metadata = serde_json::json!({"route":"B", "backend":"Nemotron 3.5 streaming", "provider":"cpu",
                "reference_revision":NEMO_REFERENCE_REVISION,"binary_revision":"unverified",
                "library_sha256":backend.library_sha256.as_str(),"model_bytes":NEMOTRON_ASR_BYTES,"model_sha256":NEMOTRON_ASR_SHA256,
                "model_load_ms":loading.elapsed().as_millis(), "word_timing":"requested decoder emission times; not forced alignment or evaluated"});
            replay_streaming_asr(&input, &output, &mut backend, metadata, || {
                anyhow::ensure!(
                    fingerprint_poc_file(&model)?
                        == (NEMOTRON_ASR_BYTES, NEMOTRON_ASR_SHA256.to_owned()),
                    "实验期间模型文件发生变化"
                );
                anyhow::ensure!(
                    fingerprint_poc_file(&library)?.1 == library_sha256,
                    "实验期间动态库发生变化"
                );
                Ok(())
            })?;
        }
        #[cfg(feature = "nemo-poc")]
        Command::NemoDiarizePoc {
            input,
            library,
            model,
            output,
        } => {
            replay_nemo_diarization(&input, &library, &model, &output)?;
        }
        Command::ExportTranscript { transcript, output } => {
            let t: Transcript = serde_json::from_slice(&std::fs::read(transcript)?)?;
            t.save(&output)?;
            println!("{}", output.with_extension("transcript.docx").display());
        }
        Command::ExportWord { markdown, output } => {
            export_minutes_docx(&std::fs::read_to_string(markdown)?, &output)?;
            println!("{}", output.display());
        }
        Command::Prepare { model } => {
            println!(
                "{}",
                prepare(model, &model_root(), |bytes, total| eprintln!(
                    "download {bytes}/{}",
                    total.unwrap_or(0)
                ))?
                .display()
            );
        }
        Command::PreparePunctuation => {
            println!(
                "{}",
                prepare_punctuation(&model_root(), |bytes, total| {
                    eprintln!("download {bytes}/{}", total.unwrap_or(0));
                })?
                .display()
            );
        }
        Command::Transcribe {
            input,
            output,
            model,
            realtime,
            no_revision,
            no_punctuation,
            revision_model,
            hotwords,
            language,
        } => {
            let input = input.canonicalize()?;
            let language = match language.as_str() {
                "en" => opencap_core::TranscriptionLanguage::English,
                "auto" => opencap_core::TranscriptionLanguage::Auto,
                _ => opencap_core::TranscriptionLanguage::Chinese,
            };
            let session = AsrSession::file_with_options(
                model_root(),
                model,
                input,
                output,
                realtime,
                hotwords,
                !no_revision,
                revision_model,
                language,
                !no_punctuation,
            )?;
            let mut complete = false;
            while let Ok(event) = session.events.recv() {
                match event {
                    AsrEvent::Segment(s) => println!("{}", serde_json::to_string(&s)?),
                    AsrEvent::Status(s) => eprintln!("{s}"),
                    AsrEvent::Error(s) => anyhow::bail!(s),
                    AsrEvent::Finished(t) => {
                        complete = t.complete;
                    }
                    _ => (),
                }
            }
            anyhow::ensure!(complete, "转录未完整完成");
        }
        Command::Summary {
            transcript,
            output,
            model,
        } => {
            let t: Transcript = serde_json::from_slice(&std::fs::read(transcript)?)?;
            summarize(&t, model.as_deref(), &output)?;
        }
        Command::Speakers {
            transcript,
            output,
            count,
            threshold,
            threads,
        } => {
            let t: Transcript = serde_json::from_slice(&std::fs::read(transcript)?)?;
            let t = distinguish_speakers_with_threads(
                t,
                &model_root(),
                &output,
                count,
                threshold,
                threads,
                &std::sync::atomic::AtomicBool::new(false),
                |s| eprintln!("{s}"),
            )?;
            println!("speaker_count={}", t.speaker_count());
        }
    }
    Ok(())
}
