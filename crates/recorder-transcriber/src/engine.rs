//! sherpa-onnx: the Parakeet recognizer and silero voice activity detection,
//! configured exactly as in the benchmarks (greedy search, CPU; VAD
//! threshold 0.5, 0.25 s minimum silence and speech, 20 s maximum speech,
//! 512-sample windows).

use anyhow::{Context, Result, anyhow};
use recorder_transcribe::SAMPLE_RATE;
use recorder_transcribe::model::{self, ModelPaths};
use recorder_transcribe::output::VadSettings;
use recorder_transcribe::transcript::ChunkResult;
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig, SileroVadModelConfig,
    VadModelConfig, VoiceActivityDetector,
};

pub const VAD_WINDOW: usize = 512;
const VAD_MAX_SPEECH_S: f32 = 20.0;

pub fn vad_settings() -> VadSettings {
    VadSettings {
        model: model::VAD_FILE.name.into(),
        threshold: 0.5,
        min_silence_s: 0.25,
        min_speech_s: 0.25,
        max_speech_s: VAD_MAX_SPEECH_S,
        window_samples: VAD_WINDOW as u32,
    }
}

fn path(p: &std::path::Path) -> Result<String> {
    if !p.is_file() {
        return Err(anyhow!("model file missing: {}", p.display()));
    }
    Ok(p.to_string_lossy().into_owned())
}

pub struct Engine {
    recognizer: OfflineRecognizer,
    vad_config: VadModelConfig,
}

impl Engine {
    pub fn load(paths: &ModelPaths, threads: u32) -> Result<Self> {
        let mut cfg = OfflineRecognizerConfig::default();
        cfg.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(path(&paths.encoder)?),
            decoder: Some(path(&paths.decoder)?),
            joiner: Some(path(&paths.joiner)?),
        };
        cfg.model_config.tokens = Some(path(&paths.tokens)?);
        cfg.model_config.model_type = Some("nemo_transducer".into());
        cfg.model_config.num_threads = threads.max(1) as i32;
        cfg.model_config.provider = Some("cpu".into());
        cfg.decoding_method = Some("greedy_search".into());
        let recognizer = OfflineRecognizer::create(&cfg)
            .context("cannot load the speech model (damaged files or not enough memory)")?;
        let s = vad_settings();
        let vad_config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(path(&paths.vad)?),
                threshold: s.threshold,
                min_silence_duration: s.min_silence_s,
                min_speech_duration: s.min_speech_s,
                window_size: VAD_WINDOW as i32,
                max_speech_duration: s.max_speech_s,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        Ok(Self {
            recognizer,
            vad_config,
        })
    }

    /// A fresh detector for one channel.
    pub fn vad(&self) -> Result<VoiceActivityDetector> {
        VoiceActivityDetector::create(&self.vad_config, VAD_MAX_SPEECH_S + 60.0)
            .context("cannot load the voice activity detector")
    }

    pub fn decode(&self, samples: &[f32]) -> Result<ChunkResult> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        self.recognizer.decode(&stream);
        let r = stream
            .get_result()
            .context("the recognizer returned no result")?;
        Ok(ChunkResult {
            text: r.text,
            tokens: r.tokens,
            timestamps: r.timestamps.unwrap_or_default(),
            durations: r.durations.unwrap_or_default(),
        })
    }
}
