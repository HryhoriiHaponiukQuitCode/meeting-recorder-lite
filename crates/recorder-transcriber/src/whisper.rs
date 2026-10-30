//! Whisper large-v3-turbo through whisper.cpp on the GPU (Metal), macOS only.
//!
//! Settings as measured on the user's calls: Ukrainian, beam search of 5, no
//! previous text as context (it made Whisper skip more), one word per
//! segment (word timestamps for the merge, and fewer skips than sentence
//! segments).

use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use recorder_transcribe::SAMPLE_RATE;
use recorder_transcribe::transcript::Word;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub const LANGUAGE: &str = "uk";
pub const BEAM_SIZE: i32 = 5;

pub struct WhisperEngine {
    _ctx: WhisperContext,
    /// One state (with its GPU buffers) for all regions: creating one per
    /// region doubled the time.
    state: RefCell<WhisperState>,
    threads: i32,
}

impl WhisperEngine {
    pub fn load(model: &Path, threads: u32) -> Result<Self> {
        if !model.is_file() {
            return Err(anyhow!("model file missing: {}", model.display()));
        }
        // whisper.cpp and ggml log to stderr by default; route them to the
        // `log` facade, which nothing listens to here.
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(true).flash_attn(true);
        let ctx = WhisperContext::new_with_params(model, params)
            .context("cannot load the Whisper model (damaged file or not enough memory)")?;
        let state = ctx.create_state().context("cannot start Whisper")?;
        Ok(Self {
            _ctx: ctx,
            state: RefCell::new(state),
            threads: threads.max(1) as i32,
        })
    }

    /// Words of `samples` (16 kHz mono), with times on the recording
    /// timeline (`offset_s` is where the samples start).
    pub fn transcribe(&self, samples: &[f32], offset_s: f64) -> Result<Vec<Word>> {
        let mut state = self.state.borrow_mut();
        let mut p = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: BEAM_SIZE,
            patience: -1.0,
        });
        p.set_language(Some(LANGUAGE));
        p.set_n_threads(self.threads);
        p.set_no_context(true);
        p.set_token_timestamps(true);
        p.set_max_len(1);
        p.set_split_on_word(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        // Whisper needs at least a second; shorter regions get silence.
        let mut padded;
        let audio = if samples.len() < SAMPLE_RATE {
            padded = samples.to_vec();
            padded.resize(SAMPLE_RATE + SAMPLE_RATE / 10, 0.0);
            &padded[..]
        } else {
            samples
        };
        state.full(p, audio).context("Whisper failed")?;
        let mut words = Vec::new();
        for seg in state.as_iter() {
            let text = seg
                .to_str_lossy()
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            // Skip empty segments and non-speech tags like "[Музика]".
            if text.is_empty()
                || (text.starts_with('[') && text.ends_with(']'))
                || (text.starts_with('(') && text.ends_with(')'))
            {
                continue;
            }
            let start = seg.start_timestamp() as f64 / 100.0;
            let end = seg.end_timestamp() as f64 / 100.0;
            let limit = samples.len() as f64 / SAMPLE_RATE as f64;
            if start > limit + 0.5 {
                continue; // in the padding
            }
            words.push(Word::new(text, offset_s + start, offset_s + end.max(start)));
        }
        Ok(words)
    }
}
