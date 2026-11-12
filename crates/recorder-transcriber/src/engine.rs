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

/// ONNX Runtime settings for every model: its worker threads sleep when
/// they run out of work instead of spinning (busy-waiting) for the next
/// piece, which by default burns processor time between the many small
/// calls (the decoder runs once per token). Measured with 2 threads: the
/// same text for about 6% less energy at the same speed.
const ORT_SETTINGS: &str = "\
SessionConfig.session.intra_op.allow_spinning=0
SessionConfig.session.inter_op.allow_spinning=0
";

/// The sherpa-onnx provider: `cpu:<file>`, the file holding
/// [`ORT_SETTINGS`] (sherpa-onnx passes `SessionConfig.*` keys on to ONNX
/// Runtime and reads the file whenever a model loads). One file in the
/// temporary folder, written once per process (a copy is renamed into
/// place, so another transcriber never reads half of it); plain `cpu` if
/// it cannot be written.
pub fn provider() -> String {
    static PROVIDER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            let dir = std::env::temp_dir();
            let file = dir.join("meeting-recorder-onnxruntime.conf");
            let part = dir.join(format!(
                "meeting-recorder-onnxruntime.conf.{}",
                std::process::id()
            ));
            match std::fs::write(&part, ORT_SETTINGS).and_then(|()| std::fs::rename(&part, &file)) {
                Ok(()) => format!("cpu:{}", file.display()),
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    eprintln!("ONNX Runtime settings not written ({e}): spinning threads");
                    "cpu".into()
                }
            }
        })
        .clone()
}

fn path(p: &std::path::Path) -> Result<String> {
    if !p.is_file() {
        return Err(anyhow!("model file missing: {}", p.display()));
    }
    Ok(p.to_string_lossy().into_owned())
}

fn vad_config(vad: &std::path::Path) -> Result<VadModelConfig> {
    let s = vad_settings();
    Ok(VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: Some(path(vad)?),
            threshold: s.threshold,
            min_silence_duration: s.min_silence_s,
            min_speech_duration: s.min_speech_s,
            window_size: VAD_WINDOW as i32,
            max_speech_duration: s.max_speech_s,
        },
        sample_rate: SAMPLE_RATE as i32,
        num_threads: 1,
        provider: Some(provider()),
        ..Default::default()
    })
}

/// Speech segments `(start, length)` in samples of one signal, found the
/// same way as for recognition (checks).
pub fn speech_segments(vad: &std::path::Path, samples: &[f32]) -> Result<Vec<(u64, usize)>> {
    let det = VoiceActivityDetector::create(&vad_config(vad)?, VAD_MAX_SPEECH_S + 60.0)
        .context("cannot load the voice activity detector")?;
    let mut out = Vec::new();
    let mut take = |det: &VoiceActivityDetector| {
        while let Some(seg) = det.front() {
            out.push((seg.start() as u64, seg.samples().len()));
            drop(seg);
            det.pop();
        }
    };
    for window in samples.as_chunks::<VAD_WINDOW>().0 {
        det.accept_waveform(window);
        take(&det);
    }
    det.flush();
    take(&det);
    Ok(out)
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
        cfg.model_config.provider = Some(provider());
        cfg.decoding_method = Some("greedy_search".into());
        let recognizer = OfflineRecognizer::create(&cfg)
            .context("cannot load the speech model (damaged files or not enough memory)")?;
        Ok(Self {
            recognizer,
            vad_config: vad_config(&paths.vad)?,
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
            // The full-precision model now and then emits its "unknown"
            // token where a quote or dash was said; the words skip it
            // (`transcript::chunk_words`), so the text does too.
            text: if r.text.contains("<unk>") {
                r.text
                    .replace("<unk>", " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                r.text
            },
            tokens: r.tokens,
            timestamps: r.timestamps.unwrap_or_default(),
            durations: r.durations.unwrap_or_default(),
        })
    }
}
