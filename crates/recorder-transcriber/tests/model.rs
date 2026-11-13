//! Tests with the real model. They need the model files and test audio, so
//! they are ignored by default:
//!
//! ```sh
//! MR_MODEL_DIR=.../models \
//! MR_TEST_EN_WAV=.../english-source.wav MR_TEST_EN_REF=.../english-reference.txt \
//! MR_TEST_EN_GOLDEN=.../en-rust-ultra-pad.txt \
//! MR_TEST_UA_WAV=.../voice-114619.wav MR_TEST_UA_GOLDEN=.../uk-rust-ultra-pad.txt \
//! cargo test --release -p recorder-transcriber -- --ignored --test-threads 1
//! ```
//!
//! The golden texts are what this build produced; any change in the engine,
//! model, VAD or chunking shows up as a difference.
//!
//! The Best-quality tests (macOS) need the Whisper model in `MR_MODEL_DIR`
//! too; `MR_TEST_REF_DIR` (optional) points at the reference excerpts
//! (`reference-check`) with two passages Whisper alone skipped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use recorder_core::manifest::{Manifest, OsInfo, TranscriptStatus};
use recorder_core::opus_ogg::EncoderConfig;
use recorder_core::session::{SessionConfig, SessionWriter};
use recorder_transcribe::Speaker;
use recorder_transcribe::checkpoint::PARTIAL_FILE;
use recorder_transcribe::dialog::normalized_words;
use recorder_transcribe::model::{ModelPaths, Quality};
use recorder_transcribe::output::{Document, JSON_FILE, txt_path};
use recorder_transcribe::protocol::Message;
use recorder_transcriber::engine::Engine;
use recorder_transcriber::pipeline::{self, Options};

const THREADS: u32 = 2;

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
}

fn model_dir() -> PathBuf {
    env_path("MR_MODEL_DIR")
}

fn engine() -> Engine {
    Engine::load(&ModelPaths::in_dir(&model_dir()), THREADS).unwrap()
}

fn wav(name: &str) -> Vec<f32> {
    let w = sherpa_onnx::Wave::read(&env_path(name).to_string_lossy()).unwrap();
    assert_eq!(w.sample_rate(), 16_000);
    w.samples().to_vec()
}

fn golden(name: &str) -> Option<String> {
    std::env::var_os(name).map(|p| std::fs::read_to_string(p).unwrap().trim().to_string())
}

/// The benchmark normalization (work/bilingual-score.py), reduced to what
/// this reference needs: lower case, words of letters/digits/apostrophes,
/// titles expanded, "to morrow" joined, British spellings made American.
fn wer_tokens(text: &str) -> Vec<String> {
    let words: Vec<String> = text
        .to_lowercase()
        .replace(['’', '‘'], "'")
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .map(|w| {
            match w {
                "mr" => "mister",
                "mrs" => "missus",
                "ms" => "miss",
                "dr" => "doctor",
                "labour" => "labor",
                "labourer" => "laborer",
                "labourers" => "laborers",
                "colour" => "color",
                "favourite" => "favorite",
                "centre" => "center",
                "theatre" => "theater",
                w => w,
            }
            .to_string()
        })
        .collect();
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if words[i] == "to" && words.get(i + 1).is_some_and(|w| w == "morrow") {
            out.push("tomorrow".to_string());
            i += 2;
        } else {
            out.push(words[i].clone());
            i += 1;
        }
    }
    out
}

fn wer(reference: &[String], hyp: &[String]) -> f64 {
    let mut prev: Vec<usize> = (0..=hyp.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, h) in hyp.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(r != h))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[hyp.len()] as f64 / reference.len() as f64
}

/// Share of `a`'s words found in order in `b`.
fn overlap(a: &str, b: &str) -> f64 {
    let (a, b) = (normalized_words(a), normalized_words(b));
    let mut prev = vec![0usize; b.len() + 1];
    for x in &a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        prev = cur;
    }
    prev[b.len()] as f64 / a.len().max(1) as f64
}

#[test]
#[ignore = "needs the model"]
fn english_matches_the_reference_and_the_golden_text() {
    let e = engine();
    let ch = pipeline::transcribe_mono(&e, Speaker::Me, &wav("MR_TEST_EN_WAV")).unwrap();
    let text = ch.text();
    let reference = std::fs::read_to_string(env_path("MR_TEST_EN_REF")).unwrap();
    let w = wer(&wer_tokens(&reference), &wer_tokens(&text));
    println!("EN WER {:.2}% ({} chunks)", w * 100.0, ch.chunks());
    // 3.58% in the benchmarks (10 errors in 279 words); no lost phrases.
    assert!(w <= 0.04, "EN WER {:.2}%", w * 100.0);
    if let Some(g) = golden("MR_TEST_EN_GOLDEN") {
        assert_eq!(text, g);
    }
}

#[test]
#[ignore = "needs the model"]
fn ukrainian_matches_the_golden_text() {
    let e = engine();
    let ch = pipeline::transcribe_mono(&e, Speaker::Me, &wav("MR_TEST_UA_WAV")).unwrap();
    let text = ch.text();
    let latin = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    assert_eq!(latin, 0, "Latin letters in the Ukrainian text: {text}");
    // 0.2 s around each segment: one chunk more than without (12).
    assert_eq!(ch.chunks(), 13);
    assert_eq!(
        text,
        golden("MR_TEST_UA_GOLDEN").expect("set MR_TEST_UA_GOLDEN")
    );
}

fn temp_root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mr-tx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes a session as the recorder would, from two mono signals.
fn make_session(root: &Path, part_seconds: u32, left: &[f32], right: &[f32]) -> PathBuf {
    let mut s = SessionWriter::create(SessionConfig {
        root: root.to_path_buf(),
        part_seconds,
        encoder: EncoderConfig::default(),
        os: OsInfo {
            family: "test".into(),
            version: "0".into(),
            arch: "x".into(),
        },
    })
    .unwrap();
    let dir = s.dir().to_path_buf();
    let n = left.len().max(right.len()).div_ceil(320) * 320;
    let mut frame = vec![0.0f32; 640];
    for start in (0..n).step_by(320) {
        for i in 0..320 {
            frame[2 * i] = left.get(start + i).copied().unwrap_or(0.0);
            frame[2 * i + 1] = right.get(start + i).copied().unwrap_or(0.0);
        }
        s.push_frame(&frame).unwrap();
    }
    s.finish().unwrap();
    dir
}

fn transcribe(
    dir: &Path,
    stop: &AtomicBool,
    on: &mut dyn FnMut(&Message),
) -> anyhow::Result<Vec<String>> {
    transcribe_with(dir, Quality::Fast, stop, on)
}

fn transcribe_with(
    dir: &Path,
    quality: Quality,
    stop: &AtomicBool,
    on: &mut dyn FnMut(&Message),
) -> anyhow::Result<Vec<String>> {
    transcribe_opts(dir, quality, false, stop, on)
}

fn transcribe_opts(
    dir: &Path,
    quality: Quality,
    voices: bool,
    stop: &AtomicBool,
    on: &mut dyn FnMut(&Message),
) -> anyhow::Result<Vec<String>> {
    let opts = Options {
        model_dir: model_dir(),
        threads: THREADS,
        quality,
        voices,
        heat_report: None,
        me_name: None,
    };
    pipeline::transcribe_session(dir, &opts, &mut |m| on(&m), stop)
}

fn document(dir: &Path) -> Document {
    serde_json::from_slice(&std::fs::read(dir.join(JSON_FILE)).unwrap()).unwrap()
}

fn texts(doc: &Document, speaker: Speaker) -> String {
    doc.utterances
        .iter()
        .filter(|u| u.speaker == speaker && !u.echo)
        .map(|u| u.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A two-person call: I speak English first, then the other side speaks
/// Ukrainian. One file, then the same call split into one-minute parts,
/// then an interrupted run that resumes.
#[test]
#[ignore = "needs the model"]
fn sessions_become_a_dialogue_with_speakers_and_times() {
    let en = wav("MR_TEST_EN_WAV");
    let ua_full = wav("MR_TEST_UA_WAV");
    // The other side starts after me, on a VAD window boundary.
    let offset = en.len().div_ceil(512) * 512 + 512 * 20;
    let ua = &ua_full[..16_000 * 100];
    let mut right = vec![0.0; offset];
    right.extend_from_slice(ua);
    let root = temp_root("dialog");

    // One file.
    let one = make_session(&root, 0, &en, &right);
    let mut events = Vec::new();
    let files = transcribe(&one, &AtomicBool::new(false), &mut |m| {
        events.push(m.clone())
    })
    .unwrap();
    assert_eq!(files.len(), 3);
    assert!(matches!(events.first(), Some(Message::Started { .. })));
    assert!(matches!(events.last(), Some(Message::Done { .. })));
    assert!(events.iter().any(|m| matches!(m, Message::Progress { .. })));
    let doc = document(&one);
    let me = texts(&doc, Speaker::Me);
    let others = texts(&doc, Speaker::Others);
    let en_golden = golden("MR_TEST_EN_GOLDEN").unwrap_or_default();
    println!("me/en overlap {:.3}", overlap(&en_golden, &me));
    assert!(overlap(&en_golden, &me) > 0.97, "{me}");
    assert!(others.split_whitespace().count() > 150, "{others}");
    assert_eq!(
        others.chars().filter(|c| c.is_ascii_alphabetic()).count(),
        0
    );
    let switch = offset as f64 / 16_000.0;
    for u in &doc.utterances {
        match u.speaker {
            Speaker::Me => assert!(u.end <= switch + 0.5, "{u:?}"),
            Speaker::Others => assert!(u.start >= switch - 0.5, "{u:?}"),
        }
        assert!(u.start <= u.end);
    }
    assert!(doc.utterances.windows(2).all(|w| w[0].start <= w[1].start));
    let txt = std::fs::read_to_string(txt_path(&one).unwrap()).unwrap();
    // Labels from the participants: the microphone speaks first.
    let me_label = doc.label(Speaker::Me, None);
    let others_label = doc.label(Speaker::Others, None);
    assert_ne!(me_label, others_label);
    let me_line = txt.find(&format!("] {me_label}: ")).unwrap();
    let others_line = txt.find(&format!("] {others_label}: ")).unwrap();
    assert!(me_line < others_line);
    let m = Manifest::load(&one).unwrap();
    assert_eq!(
        m.transcript.as_ref().unwrap().status,
        TranscriptStatus::Done
    );
    assert!(!one.join(PARTIAL_FILE).exists());

    // The same call in one-minute parts. Each part starts a new Opus
    // encoder, so the audio after the first minute differs slightly and the
    // recognizer may pick a word differently; nothing may go missing.
    let parts = make_session(&root, 60, &en, &right);
    assert!(Manifest::load(&parts).unwrap().parts.len() >= 4);
    transcribe(&parts, &AtomicBool::new(false), &mut |_| {}).unwrap();
    let doc_parts = document(&parts);
    let (me_p, others_p) = (
        texts(&doc_parts, Speaker::Me),
        texts(&doc_parts, Speaker::Others),
    );
    println!(
        "parts vs one file: me {:.3}, others {:.3}",
        overlap(&me, &me_p),
        overlap(&others, &others_p)
    );
    assert!(overlap(&me, &me_p) > 0.97, "{me_p}");
    // The Ukrainian sample is a noisy, spontaneous voice note: about a
    // fifth of its words are uncertain for the model and flip with any
    // change to the audio (measured: 0.85 between these two sessions, 0.79
    // between either and the original WAV).
    assert!(overlap(&others, &others_p) > 0.75, "{others_p}");
    let last_end = |d: &Document| d.utterances.iter().map(|u| u.end).fold(0.0, f64::max);
    assert!((last_end(&doc_parts) - last_end(&doc)).abs() < 1.0);

    // Stopped after two chunks, then resumed: same result.
    let stop = AtomicBool::new(false);
    let mut seen = 0;
    let err = transcribe(&one, &stop, &mut |m| {
        if matches!(m, Message::Progress { .. }) {
            seen += 1;
            if seen == 3 {
                stop.store(true, Ordering::SeqCst);
            }
        }
    })
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err:#}");
    assert!(one.join(PARTIAL_FILE).exists());
    assert_eq!(
        Manifest::load(&one).unwrap().transcript.unwrap().status,
        TranscriptStatus::Done,
        "a cancelled run restores the previous state"
    );
    transcribe(&one, &AtomicBool::new(false), &mut |_| {}).unwrap();
    let resumed = document(&one);
    assert_eq!(texts(&resumed, Speaker::Me), me);
    assert_eq!(texts(&resumed, Speaker::Others), others);
    std::fs::remove_dir_all(root).ok();
}

/// Loudspeakers without echo cancellation: the microphone hears the other
/// side 30 ms later at -10 dB. Those words must not appear as mine.
#[test]
#[ignore = "needs the model"]
fn echo_of_the_other_side_is_hidden() {
    let en = wav("MR_TEST_EN_WAV");
    let delay = 480;
    let mut left = vec![0.0; delay];
    left.extend(en.iter().map(|s| s * 0.3));
    let root = temp_root("echo");
    let dir = make_session(&root, 0, &left, &en);
    transcribe(&dir, &AtomicBool::new(false), &mut |_| {}).unwrap();
    let doc = document(&dir);
    let me: Vec<_> = doc
        .utterances
        .iter()
        .filter(|u| u.speaker == Speaker::Me)
        .collect();
    let hidden = me.iter().filter(|u| u.echo).count();
    println!(
        "{hidden} of {} microphone utterances marked as echo",
        me.len()
    );
    assert!(!me.is_empty());
    assert_eq!(hidden, me.len(), "{me:#?}");
    // The microphone said nothing of its own: no participant for it.
    let d = document(&dir);
    assert!(
        d.participants.iter().all(|p| p.speaker != Speaker::Me),
        "{:?}",
        d.participants
    );
    std::fs::remove_dir_all(root).ok();
}

/// Best quality: the same two-person call. Whisper's text, nothing lost
/// against Parakeet, English (mine) kept, times ordered.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs the models"]
fn best_quality_is_complete_and_keeps_both_speakers() {
    let en = wav("MR_TEST_EN_WAV");
    let ua_full = wav("MR_TEST_UA_WAV");
    let offset = en.len().div_ceil(512) * 512 + 512 * 20;
    let mut right = vec![0.0; offset];
    right.extend_from_slice(&ua_full[..16_000 * 100]);
    let root = temp_root("best");
    let fast = make_session(&root, 0, &en, &right);
    let best = make_session(&root, 0, &en, &right);
    transcribe_with(&fast, Quality::Fast, &AtomicBool::new(false), &mut |_| {}).unwrap();
    let mut progress = Vec::new();
    transcribe_with(&best, Quality::Best, &AtomicBool::new(false), &mut |m| {
        if let Message::Progress { done_s, .. } = m {
            progress.push(*done_s)
        }
    })
    .unwrap();
    assert!(
        progress.windows(2).all(|w| w[0] <= w[1] + 1e-9),
        "progress goes forward"
    );
    let (f, b) = (document(&fast), document(&best));
    assert_eq!(b.settings.quality, Quality::Best);
    assert!(b.model.contains("whisper") && b.engine.contains("whisper.cpp"));
    let sources: Vec<&str> = b
        .utterances
        .iter()
        .flat_map(|u| u.words.iter().filter_map(|w| w.source.as_deref()))
        .collect();
    assert!(
        sources.contains(&"whisper"),
        "Whisper wrote the Ukrainian side"
    );
    assert!(
        sources.contains(&"parakeet-en"),
        "English came from Parakeet"
    );
    // Nothing Parakeet heard is missing (spans of 6+ words).
    for sp in [Speaker::Me, Speaker::Others] {
        let fw = normalized_words(&texts(&f, sp));
        let bw = normalized_words(&texts(&b, sp));
        let missing = missing_spans(&fw, &bw);
        println!(
            "{sp:?}: fast {} words, best {} words, missing spans {missing:?}",
            fw.len(),
            bw.len()
        );
        assert!(missing.is_empty(), "{sp:?} lost {missing:?}");
    }
    let switch = offset as f64 / 16_000.0;
    for u in &b.utterances {
        match u.speaker {
            Speaker::Me => assert!(u.end <= switch + 1.0, "{u:?}"),
            Speaker::Others => assert!(u.start >= switch - 1.0, "{u:?}"),
        }
    }
    std::fs::remove_dir_all(root).ok();
}

/// Runs of 6+ words of `a` that `b` lacks.
#[cfg(target_os = "macos")]
fn missing_spans(a: &[String], b: &[String]) -> Vec<String> {
    // Longest common subsequence back-trace, collecting unmatched runs.
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            d[i][j] = if a[i] == b[j] {
                d[i + 1][j + 1] + 1
            } else {
                d[i + 1][j].max(d[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut run, mut out) = (0, 0, Vec::new(), Vec::new());
    while i < n {
        if j < m && a[i] == b[j] {
            if run.len() >= 6 {
                out.push(run.join(" "));
            }
            run.clear();
            i += 1;
            j += 1;
        } else if j < m && d[i][j + 1] >= d[i + 1][j] {
            j += 1;
        } else {
            run.push(a[i].clone());
            i += 1;
        }
    }
    if run.len() >= 6 {
        out.push(run.join(" "));
    }
    out
}

/// The two passages from the user's calls that Whisper alone skipped come
/// back from Parakeet in the Best quality.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs the models and the reference excerpts"]
fn best_quality_restores_what_whisper_skipped() {
    let Some(refdir) = std::env::var_os("MR_TEST_REF_DIR").map(PathBuf::from) else {
        println!("MR_TEST_REF_DIR not set, skipped");
        return;
    };
    let read = |name: &str| {
        let w = sherpa_onnx::Wave::read(&refdir.join(name).to_string_lossy()).unwrap();
        w.samples().to_vec()
    };
    let root = temp_root("restore");
    // Key words of the skipped passages (Parakeet's wording from the Opus
    // recording differs from the WAV, so not whole phrases). "за домен":
    // the INT8 model heard "поза домен".
    for (file, must) in [
        ("01-meeting-others-a.wav", ["смішно", "за вік", "окей"]),
        (
            "08-project-others.wav",
            ["писала", "за домен", "їм написала"],
        ),
    ] {
        let dir = make_session(&root, 0, &[], &read(file));
        transcribe_with(&dir, Quality::Best, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let t = texts(&document(&dir), Speaker::Others).to_lowercase();
        println!("{file}: {t}");
        for m in must {
            assert!(t.contains(m), "{file}: «{m}» missing in {t}");
        }
    }
    std::fs::remove_dir_all(root).ok();
}

/// Four people one after the other on the system audio: the three
/// LibriSpeech readers of the English test file (two clips each; speakers
/// and durations from `english-source.json` next to it), then the
/// Ukrainian voice message.
///
/// No voice may hold two people, and every clip stays on one voice. One
/// reader's two clips come from different recording sessions and are 0.65
/// apart for the voice model (different people: 0.75 and more), so they
/// may get two numbers; within one call that does not happen.
#[test]
#[ignore = "needs the models"]
fn voices_tell_people_apart() {
    people_apart(Speaker::Others);
}

/// The same four people in one room, on the microphone (nothing on the
/// system audio): told apart the same way.
#[test]
#[ignore = "needs the models"]
fn voices_on_the_microphone_tell_people_apart() {
    people_apart(Speaker::Me);
}

fn people_apart(channel: Speaker) {
    let en_path = PathBuf::from(std::env::var_os("MR_TEST_EN_WAV").expect("MR_TEST_EN_WAV"));
    let meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(en_path.with_file_name("english-source.json")).expect("english-source.json"),
    )
    .unwrap();
    // (start, end, clip, person) on the timeline; 0.5 s between clips.
    let mut truth: Vec<(f64, f64, String, String)> = Vec::new();
    let mut t = 0.0;
    for clip in meta["utterances"].as_array().unwrap() {
        let d = clip["duration_seconds"].as_f64().unwrap();
        let id = clip["id"].as_str().unwrap().to_string();
        let person = clip["speaker_id"].as_str().unwrap().to_string();
        truth.push((t, t + d, id, person));
        t += d + 0.5;
    }
    let en = wav("MR_TEST_EN_WAV");
    let ua = wav("MR_TEST_UA_WAV");
    let mut speech = en.clone();
    speech.extend(vec![0.0; 16_000 * 3]);
    let switch = speech.len() as f64 / 16_000.0;
    truth.push((switch, switch + 120.0, "ua".into(), "ua".into()));
    speech.extend_from_slice(&ua[..16_000 * 120]);
    let silence = vec![0.0; speech.len()];
    let (left, right) = if channel == Speaker::Me {
        (speech, silence)
    } else {
        (silence, speech)
    };
    let root = temp_root(if channel == Speaker::Me {
        "mic-voices"
    } else {
        "voices"
    });
    let dir = make_session(&root, 60, &left, &right);
    transcribe_opts(
        &dir,
        Quality::Fast,
        true,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let d = document(&dir);
    let mut by_clip: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
    let mut by_voice: std::collections::BTreeMap<u32, Vec<String>> = Default::default();
    for u in d.utterances.iter().filter(|u| u.speaker == channel) {
        for w in &u.words {
            let mid = (w.start + w.end) / 2.0;
            let Some((_, _, clip, person)) = truth
                .iter()
                .find(|(a, b, _, _)| *a - 0.3 <= mid && mid <= *b + 0.3)
            else {
                continue;
            };
            let v = w.voice.expect("every word has a voice");
            by_clip.entry(clip.clone()).or_default().push(v);
            by_voice.entry(v).or_default().push(person.clone());
        }
    }
    let top = |xs: &[u32]| {
        let mut c: std::collections::BTreeMap<u32, usize> = Default::default();
        for x in xs {
            *c.entry(*x).or_default() += 1;
        }
        c.into_iter().max_by_key(|x| x.1).unwrap()
    };
    let voices = if channel == Speaker::Me {
        d.me_voices
    } else {
        d.others_voices
    };
    println!("{voices} voices");
    for (clip, vs) in &by_clip {
        let (v, n) = top(vs);
        println!("clip {clip}: voice {v}, {n} of {} words", vs.len());
        assert!(
            n as f64 >= 0.9 * vs.len() as f64,
            "clip {clip} split: {vs:?}"
        );
    }
    for (v, people) in &by_voice {
        let main = people.iter().filter(|p| **p == people[0]).count();
        let mut distinct = people.clone();
        distinct.sort();
        distinct.dedup();
        println!("voice {v}: {distinct:?}");
        assert!(
            distinct.len() == 1 || main as f64 >= 0.97 * people.len() as f64,
            "voice {v} holds {distinct:?}"
        );
    }
    assert!((4..=5).contains(&voices), "{voices}");
    // In order of appearance: the first reader is voice 1, the second 2.
    assert_eq!(top(&by_clip["1272-128104-0004"]).0, 1);
    assert_eq!(top(&by_clip["1462-170138-0024"]).0, 2);
    let txt = std::fs::read_to_string(txt_path(&dir).unwrap()).unwrap();
    for v in 1..=4 {
        let label = d.label(channel, Some(v));
        assert!(txt.contains(&format!("] {label}: ")), "{label}");
    }
    assert!(!txt.contains("] Others") && !txt.contains("] Me"));
    std::fs::remove_dir_all(root).ok();
}
