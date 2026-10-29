//! `meeting-recorder-transcribe`: the transcriber process. Started by the
//! window and by `recorder transcribe`; can be run by hand as well.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use recorder_transcribe::model::{self, ModelPaths};
use recorder_transcribe::protocol::Message;
use recorder_transcribe::{DEFAULT_THREADS, Speaker};
use recorder_transcriber::engine::Engine;
use recorder_transcriber::pipeline::{self, Options};

#[derive(Parser)]
#[command(
    name = "meeting-recorder-transcribe",
    version,
    about = "Transcribes Meeting Recorder sessions locally (Parakeet + silero VAD)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Common {
    /// Folder with silero_vad.onnx and the Parakeet model folder.
    #[arg(long)]
    model_dir: PathBuf,
    /// Recognizer threads.
    #[arg(long, default_value_t = DEFAULT_THREADS)]
    threads: u32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Transcribe a session folder into transcript.txt/json/srt.
    Session {
        dir: PathBuf,
        #[command(flatten)]
        common: Common,
        /// Print progress as JSON lines (for the window).
        #[arg(long)]
        json: bool,
    },
    /// Print the text of a 16 kHz mono WAV file (checks and benchmarks).
    Wav {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Check the model files against their SHA256 hashes.
    VerifyModel {
        #[arg(long)]
        model_dir: PathBuf,
    },
}

/// Transcription is background work: let recordings, calls and the user's
/// other programs go first, and keep the fans quiet.
fn lower_priority() {
    #[cfg(unix)]
    // SAFETY: plain syscall on the current process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
    #[cfg(windows)]
    // SAFETY: plain Win32 call on the current process.
    unsafe {
        use windows::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

fn emit(json: bool, m: &Message) {
    if json {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", m.to_line());
        let _ = out.flush();
        return;
    }
    match m {
        Message::Started { audio_s } => eprintln!("Model loaded; {audio_s:.0} s of audio."),
        Message::Progress { done_s, total_s } => {
            eprint!(
                "\r{:>5.1}%  {done_s:.0} / {total_s:.0} s ",
                done_s / total_s.max(1e-9) * 100.0
            )
        }
        Message::Done {
            files,
            processing_s,
        } => eprintln!("\nDone in {processing_s:.1} s: {}", files.join(", ")),
        Message::Error { message } => eprintln!("\nerror: {message}"),
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Session { dir, common, json } => {
            let opts = Options {
                model_dir: common.model_dir,
                threads: common.threads,
            };
            let result = pipeline::transcribe_session(
                &dir,
                &opts,
                &mut |m| emit(json, &m),
                &AtomicBool::new(false),
            );
            if let Err(e) = &result {
                emit(
                    json,
                    &Message::Error {
                        message: format!("{e:#}"),
                    },
                );
            }
            result.map(|_| ())
        }
        Cmd::Wav { file, common } => {
            let wave = sherpa_onnx::Wave::read(&file.to_string_lossy())
                .with_context(|| format!("cannot read {}", file.display()))?;
            anyhow::ensure!(wave.sample_rate() == 16_000, "the WAV must be 16 kHz mono");
            let engine = Engine::load(&ModelPaths::in_dir(&common.model_dir), common.threads)?;
            let t = std::time::Instant::now();
            let ch = pipeline::transcribe_mono(&engine, Speaker::Me, wave.samples())?;
            println!("{}", ch.text());
            eprintln!(
                "{:.1} s of audio in {:.2} s, {} chunks",
                wave.num_samples() as f64 / 16_000.0,
                t.elapsed().as_secs_f64(),
                ch.chunks()
            );
            Ok(())
        }
        Cmd::VerifyModel { model_dir } => {
            model::verify(&model_dir)?;
            println!("OK: all model files match their SHA256.");
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    lower_priority();
    let cli = Cli::parse();
    let json = matches!(&cli.cmd, Cmd::Session { json: true, .. });
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !json {
                eprintln!("error: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}
