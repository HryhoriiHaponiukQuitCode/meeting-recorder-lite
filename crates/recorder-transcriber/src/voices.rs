//! Voice prints (3D-Speaker CAM++ through sherpa-onnx) for telling the
//! voices of the system audio apart; the clustering is in
//! `recorder_transcribe::voices`.

use std::path::Path;

use anyhow::{Result, anyhow};
use recorder_transcribe::SAMPLE_RATE;
use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};

pub struct Embedder {
    inner: SpeakerEmbeddingExtractor,
}

impl Embedder {
    pub fn load(model: &Path, threads: u32) -> Result<Self> {
        if !model.is_file() {
            return Err(anyhow!("model file missing: {}", model.display()));
        }
        let inner = SpeakerEmbeddingExtractor::create(&SpeakerEmbeddingExtractorConfig {
            model: Some(model.to_string_lossy().into_owned()),
            num_threads: threads.max(1) as i32,
            provider: Some("cpu".into()),
            ..Default::default()
        })
        .ok_or_else(|| anyhow!("cannot load the voice model (damaged file?)"))?;
        Ok(Self { inner })
    }

    pub fn dim(&self) -> usize {
        self.inner.dim() as usize
    }

    /// The voice print of a short stretch of audio; `None` when it is too
    /// short for the model.
    pub fn embed(&self, samples: &[f32]) -> Option<Vec<f32>> {
        let stream = self.inner.create_stream()?;
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        stream.input_finished();
        if !self.inner.is_ready(&stream) {
            return None;
        }
        self.inner.compute(&stream)
    }
}
