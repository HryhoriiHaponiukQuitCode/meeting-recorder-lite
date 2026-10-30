//! Everything about local transcription that does not need the speech
//! engine, so the window, the CLI and the tests can use it without linking
//! ONNX Runtime:
//!
//! - [`model`]: the Parakeet + silero VAD files, their hashes, download,
//!   verification and removal;
//! - [`chunk`]: joining voice-activity segments into chunks of at most 30 s
//!   of speech, pauses removed, with a map back to recording time;
//! - [`transcript`]: turning recognizer tokens into timed utterances;
//! - [`dialog`]: merging both channels into one dialogue and hiding the
//!   echo of the other side picked up by the microphone;
//! - [`output`]: `transcript.txt`, `transcript.json`, `transcript.srt`;
//! - [`checkpoint`]: finished chunks, so an interrupted run resumes;
//! - [`protocol`]: progress lines the transcriber prints for the window.
//!
//! The engine itself runs in a separate process (`recorder-transcriber`),
//! so a crash or memory spike there can never affect a recording.

pub mod checkpoint;
pub mod chunk;
pub mod dialog;
pub mod hybrid;
pub mod model;
pub mod output;
pub mod protocol;
pub mod transcript;

/// Sample rate of the recordings and of the recognizer input.
pub const SAMPLE_RATE: usize = 16_000;

/// Recognizer threads by default. Two threads are the most efficient (a
/// third less CPU time per minute of audio than four on an M4) and keep a
/// laptop cool; four are about 25% faster.
pub const DEFAULT_THREADS: u32 = 2;

/// Name of the transcriber program, next to the main executable.
pub const HELPER_NAME: &str = if cfg!(windows) {
    "meeting-recorder-transcribe.exe"
} else {
    "meeting-recorder-transcribe"
};

/// Who is speaking: the channel the words came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The microphone (left channel).
    Me,
    /// System audio (right channel): everyone else on the call.
    Others,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Me => "Me",
            Speaker::Others => "Others",
        }
    }

    /// Channel index in the recording: 0 = left, 1 = right.
    pub fn channel(self) -> usize {
        match self {
            Speaker::Me => 0,
            Speaker::Others => 1,
        }
    }
}
