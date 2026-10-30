//! The transcriber process: decodes a session's parts in a stream, finds
//! speech in each channel with silero VAD, joins it into chunks of at most
//! 30 s of speech (pauses removed) and recognizes each chunk with Parakeet.
//! The two channels become one dialogue (Me = microphone, Others = system
//! audio, its voices told apart as Others 1, 2, …) written as
//! `transcript.txt/json/srt`.
//!
//! It runs as its own low-priority process, started by the window or the
//! CLI, so a crash or a memory spike in the engine cannot touch a recording,
//! and all its memory is returned when it exits.

pub mod engine;
pub mod pipeline;
pub mod voices;
#[cfg(target_os = "macos")]
pub mod whisper;
