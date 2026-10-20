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
    Prepare {
        #[arg(value_enum)]
        model: ModelKind,
    },
    Transcribe {
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "offline")]
        model: ModelKind,
        #[arg(long)]
        realtime: bool,
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
    match Cli::parse().command {
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
        Command::Transcribe {
            input,
            output,
            model,
            realtime,
            hotwords,
            language,
        } => {
            let input = input.canonicalize()?;
            let language = match language.as_str() {
                "en" => opencap_core::TranscriptionLanguage::English,
                "auto" => opencap_core::TranscriptionLanguage::Auto,
                _ => opencap_core::TranscriptionLanguage::Chinese,
            };
            let session = AsrSession::file_with_language(
                model_root(),
                model,
                input,
                output,
                realtime,
                hotwords,
                true,
                language,
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
