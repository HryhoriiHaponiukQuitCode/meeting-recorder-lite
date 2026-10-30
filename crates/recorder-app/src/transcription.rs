//! Transcription from the app's side: the model download (a thread here)
//! and transcription jobs (the `meeting-recorder-transcribe` process, at
//! low priority, so the engine never runs inside the recording process).

use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use recorder_core::manifest::{Manifest, TranscriptStatus};
use recorder_transcribe::model::{self, DownloadProgress, ModelStatus};
use recorder_transcribe::output::TXT_FILE;
use recorder_transcribe::protocol::Message;

pub use recorder_transcribe::model::{ATTRIBUTION, Phase, Quality, download_size, missing_bytes};

/// The transcriber program next to the running executable (inside the app
/// bundle on macOS, in the install folder on Windows, in target/ in dev).
pub fn helper_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let p = exe.with_file_name(recorder_transcribe::HELPER_NAME);
    if p.is_file() {
        Ok(p)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is missing next to {}",
                recorder_transcribe::HELPER_NAME,
                exe.display()
            ),
        ))
    }
}

pub fn model_status(models_dir: &Path, quality: Quality) -> ModelStatus {
    model::status(models_dir, quality)
}

pub fn model_disk_size(models_dir: &Path) -> u64 {
    model::disk_size(models_dir)
}

pub fn delete_model(models_dir: &Path) -> io::Result<()> {
    model::delete(models_dir)
}

/// What a session's transcript looks like on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptState {
    None,
    Running,
    Done(PathBuf),
    Failed(String),
}

pub fn transcript_state(session: &Path) -> TranscriptState {
    match Manifest::load(session) {
        Ok(m) => transcript_state_of(&m, session),
        Err(_) => TranscriptState::None,
    }
}

/// The same from a manifest already loaded.
pub fn transcript_state_of(m: &Manifest, session: &Path) -> TranscriptState {
    match &m.transcript {
        Some(t) => match t.status {
            TranscriptStatus::Done if session.join(TXT_FILE).is_file() => {
                TranscriptState::Done(session.join(TXT_FILE))
            }
            TranscriptStatus::Done => TranscriptState::None,
            TranscriptStatus::Running => TranscriptState::Running,
            TranscriptStatus::Failed => {
                TranscriptState::Failed(t.error.clone().unwrap_or_default())
            }
        },
        None => TranscriptState::None,
    }
}

#[derive(Clone, Debug, Default)]
pub struct DownloadState {
    pub progress: Option<DownloadProgress>,
    /// `Some` when finished: `Ok` or the error text (`None` inside when cancelled).
    pub finished: Option<Result<(), Option<String>>>,
}

/// The model download on a background thread.
pub struct ModelDownload {
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<DownloadState>>,
    thread: Option<JoinHandle<()>>,
}

impl ModelDownload {
    pub fn start(models_dir: PathBuf, quality: Quality) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(DownloadState::default()));
        let thread = {
            let (cancel, state) = (cancel.clone(), state.clone());
            std::thread::Builder::new()
                .name("model-download".into())
                .spawn(move || {
                    tracing::info!(dir = %models_dir.display(), quality = quality.as_str(), "model download started");
                    let result = model::download(
                        &models_dir,
                        quality,
                        &mut |p| state.lock().expect("state").progress = Some(p),
                        &cancel,
                    );
                    let finished = match result {
                        Ok(()) => {
                            tracing::info!("model downloaded and verified");
                            Ok(())
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(None),
                        Err(e) => {
                            tracing::error!(error = %e, "model download failed");
                            Err(Some(e.to_string()))
                        }
                    };
                    state.lock().expect("state").finished = Some(finished);
                })
                .ok()
        };
        Self {
            cancel,
            state,
            thread,
        }
    }

    pub fn state(&self) -> DownloadState {
        self.state.lock().expect("state").clone()
    }

    /// Stops the download; the partial file stays and a new start resumes.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl Drop for ModelDownload {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct JobState {
    pub done_s: f64,
    pub total_s: f64,
    /// The model is loaded and audio is being processed.
    pub started: bool,
    pub done: Option<Vec<String>>,
    pub error: Option<String>,
    pub stdout_closed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobOutcome {
    Done(PathBuf),
    Failed(String),
}

/// One transcriber process working on one session.
pub struct TranscriptionJob {
    session: PathBuf,
    child: Child,
    state: Arc<Mutex<JobState>>,
    outcome: Option<JobOutcome>,
}

impl TranscriptionJob {
    pub fn start(
        session: &Path,
        models_dir: &Path,
        threads: u32,
        quality: Quality,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(helper_path()?);
        cmd.arg("session")
            .arg(session)
            .arg("--model-dir")
            .arg(models_dir)
            .arg("--threads")
            .arg(threads.to_string())
            .arg("--quality")
            .arg(quality.as_str())
            .arg("--json")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        tracing::info!(session = %session.display(), threads, quality = quality.as_str(), "transcription started");
        let state = Arc::new(Mutex::new(JobState::default()));
        if let Some(out) = child.stdout.take() {
            let state = state.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let Some(m) = Message::parse(&line) else {
                        continue;
                    };
                    let mut st = state.lock().expect("state");
                    match m {
                        Message::Started { audio_s } => {
                            st.started = true;
                            st.total_s = audio_s;
                        }
                        Message::Progress { done_s, total_s } => {
                            st.done_s = done_s;
                            st.total_s = total_s;
                        }
                        Message::Done { files, .. } => st.done = Some(files),
                        Message::Error { message } => st.error = Some(message),
                    }
                }
                state.lock().expect("state").stdout_closed = true;
            });
        }
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    tracing::info!(target: "transcriber", "{line}");
                }
            });
        }
        Ok(Self {
            session: session.to_path_buf(),
            child,
            state,
            outcome: None,
        })
    }

    pub fn session(&self) -> &Path {
        &self.session
    }

    pub fn state(&self) -> JobState {
        self.state.lock().expect("state").clone()
    }

    /// `Some` once the process has exited and its output was read.
    pub fn poll(&mut self) -> Option<JobOutcome> {
        if self.outcome.is_some() {
            return self.outcome.clone();
        }
        let status = self.child.try_wait().ok().flatten()?;
        let st = self.state();
        if !st.stdout_closed {
            return None;
        }
        let outcome = match (st.done, st.error) {
            (Some(_), _) if status.success() => JobOutcome::Done(self.session.join(TXT_FILE)),
            (_, Some(e)) => JobOutcome::Failed(e),
            _ => JobOutcome::Failed(format!(
                "the transcriber stopped unexpectedly ({status}); not enough memory?"
            )),
        };
        match &outcome {
            JobOutcome::Done(_) => {
                tracing::info!(session = %self.session.display(), "transcription done")
            }
            JobOutcome::Failed(e) => {
                tracing::error!(session = %self.session.display(), error = %e, "transcription failed")
            }
        }
        self.outcome = Some(outcome.clone());
        Some(outcome)
    }

    /// Stops the process. Finished chunks are kept; the next run resumes.
    pub fn cancel(self) {
        tracing::info!(session = %self.session.display(), "transcription stopped");
        // Drop does the work.
    }
}

impl Drop for TranscriptionJob {
    fn drop(&mut self) {
        if self.outcome.is_none() && self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            // The killed process could not record that it stopped.
            if let Ok(mut m) = Manifest::load(&self.session)
                && m.transcript
                    .as_ref()
                    .is_some_and(|t| t.status == TranscriptStatus::Running)
            {
                m.transcript = None;
                let _ = m.save(&self.session);
            }
        }
    }
}
