//! The speech models and which of them each [`Quality`] needs:
//!
//! - **Fast** (every system): NVIDIA Parakeet TDT 0.6B v3 (full precision,
//!   sherpa-onnx export, CC-BY-4.0), the silero voice activity detector (MIT)
//!   and the 3D-Speaker CAM++ voice-print model that tells the voices of the
//!   system audio apart (Apache-2.0, 27 MiB), 2.4 GiB. The INT8 export used
//!   before is a quarter of the size but much less accurate (FLEURS
//!   sentences joined into 90 s clips with pauses: Ukrainian WER 13.4% →
//!   10.9%, English 18.1% → 8.2%; INT8 drops whole English sentences) and
//!   on Apple silicon no faster.
//! - **Best** (macOS): the same plus OpenAI Whisper large-v3-turbo (Q8,
//!   ggml for whisper.cpp, MIT), 834 MiB, run on the GPU through Metal. The
//!   transcript takes Whisper's words and fills what Whisper skipped from
//!   Parakeet (see [`crate::hybrid`]).
//!
//! The files are not in the installer; they are downloaded on first use into
//! the models folder, laid out as
//!
//! ```text
//! models/
//!   silero_vad.onnx
//!   3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx
//!   sherpa-onnx-nemo-parakeet-tdt-0.6b-v3/
//!     encoder.onnx  encoder.weights  decoder.onnx  joiner.onnx  tokens.txt
//!   ggml-large-v3-turbo-q8_0.bin           (Best only)
//! ```
//!
//! Download uses the system `curl` (present on macOS and on Windows 10
//! 1803+): resumable, retried, and checked against pinned SHA256 hashes
//! before anything is moved into place. The folder of the old INT8 model is
//! removed once the new one is in place.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ENGINE: &str = "sherpa-onnx 1.13.8";
pub const WHISPER_ENGINE: &str = "whisper.cpp 1.8.3";
pub const ASR_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3";
/// Folders of earlier Parakeet exports: an existing one means "update".
const OLD_ASR_DIRS: [&str; 1] = ["sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"];
pub const MODEL_NAME: &str = ASR_DIR;
pub const WHISPER_MODEL_NAME: &str = "whisper-large-v3-turbo-q8_0";
pub const ATTRIBUTION: &str = "Speech recognition: NVIDIA Parakeet TDT 0.6B v3 (CC-BY-4.0), \
OpenAI Whisper large-v3-turbo (MIT, macOS), silero VAD (MIT), 3D-Speaker CAM++ voices \
(Apache-2.0), sherpa-onnx (Apache-2.0), whisper.cpp (MIT), ONNX Runtime (MIT).";

/// How the transcript is made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Whisper large-v3-turbo on the GPU plus Parakeet for what Whisper
    /// skips (macOS only): the most accurate, about 5x slower.
    Best,
    /// Parakeet alone: fast on any CPU.
    #[default]
    Fast,
}

impl Quality {
    /// The qualities this build can run.
    pub fn available() -> &'static [Quality] {
        if cfg!(target_os = "macos") {
            &[Quality::Best, Quality::Fast]
        } else {
            &[Quality::Fast]
        }
    }

    /// Best where available.
    pub fn default_for_this_system() -> Quality {
        Quality::available()[0]
    }

    pub fn is_available(self) -> bool {
        Quality::available().contains(&self)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Best => "best",
            Quality::Fast => "fast",
        }
    }

    pub fn parse(s: &str) -> Option<Quality> {
        match s {
            "best" => Some(Quality::Best),
            "fast" => Some(Quality::Fast),
            _ => None,
        }
    }

    pub fn needs_whisper(self) -> bool {
        self == Quality::Best
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

const ENCODER: ModelFile = ModelFile {
    name: "encoder.onnx",
    size: 41_766_257,
    sha256: "3eed7ce424bf8339ad09233533c687e2dbd07e74ccf5027b5e7344019ea373b0",
};
/// The encoder's weights (over 2 GB, so outside the ONNX file); the
/// encoder finds them by this name next to it.
const ENCODER_WEIGHTS: ModelFile = ModelFile {
    name: "encoder.weights",
    size: 2_435_420_160,
    sha256: "3af3f51af5f2d01dbbf5af47d42c7962a2c205f11004254bb4f2b979862f39a8",
};
const DECODER: ModelFile = ModelFile {
    name: "decoder.onnx",
    size: 47_233_743,
    sha256: "d593cdb0e571f5a457ec2219af9968cbf6b0e8198e8f7839b40a8754593bf68c",
};
const JOINER: ModelFile = ModelFile {
    name: "joiner.onnx",
    size: 25_286_330,
    sha256: "b9b0bcf88ac571902e69a6536223ed2d94885e981b85045410f1403d53121a63",
};
const TOKENS: ModelFile = ModelFile {
    name: "tokens.txt",
    size: 93_939,
    sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
};
pub const ASR_FILES: [ModelFile; 5] = [ENCODER, ENCODER_WEIGHTS, DECODER, JOINER, TOKENS];
/// The sherpa-onnx export on Hugging Face, pinned to a revision.
const ASR_URL: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3/resolve/1a468a35cbba69418f126de829e75261dea4a4e4";

const fn asr_size() -> u64 {
    let mut n = 0;
    let mut i = 0;
    while i < ASR_FILES.len() {
        n += ASR_FILES[i].size;
        i += 1;
    }
    n
}

pub const VAD_FILE: ModelFile = ModelFile {
    name: "silero_vad.onnx",
    size: 643_854,
    sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
};
pub const VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// Voice prints for telling the voices of the system audio apart.
pub const VOICE_FILE: ModelFile = ModelFile {
    name: "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
    size: 28_281_164,
    sha256: "aa3cfc16963a10586a9393f5035d6d6b57e98d358b347f80c2a30bf4f00ceba2",
};
pub const VOICE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx";
pub const VOICE_MODEL_NAME: &str = "3dspeaker-campplus-zh-en-advanced";

pub const WHISPER_FILE: ModelFile = ModelFile {
    name: "ggml-large-v3-turbo-q8_0.bin",
    size: 874_188_075,
    sha256: "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
};
pub const WHISPER_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin";

/// Bytes downloaded for `quality` when nothing is there yet.
pub fn download_size(quality: Quality) -> u64 {
    asr_size()
        + VAD_FILE.size
        + VOICE_FILE.size
        + if quality.needs_whisper() {
            WHISPER_FILE.size
        } else {
            0
        }
}

/// Partial downloads and unpacking happen here.
const WORK_DIR: &str = ".download";

/// Paths the engine needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    pub vad: PathBuf,
    pub voice: PathBuf,
    /// Used by [`Quality::Best`] only.
    pub whisper: PathBuf,
}

impl ModelPaths {
    pub fn in_dir(dir: &Path) -> Self {
        let asr = dir.join(ASR_DIR);
        Self {
            encoder: asr.join(ENCODER.name),
            decoder: asr.join(DECODER.name),
            joiner: asr.join(JOINER.name),
            tokens: asr.join(TOKENS.name),
            vad: dir.join(VAD_FILE.name),
            voice: dir.join(VOICE_FILE.name),
            whisper: dir.join(WHISPER_FILE.name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    /// Nothing downloaded.
    Missing,
    /// Some files missing or of the wrong size (interrupted or damaged).
    Incomplete,
    /// An earlier Parakeet export is there, the current one is not yet.
    Outdated,
    /// All files present with the expected sizes.
    Ready,
}

fn all_files(dir: &Path, quality: Quality) -> impl Iterator<Item = (PathBuf, ModelFile)> {
    let asr = dir.join(ASR_DIR);
    let whisper = quality
        .needs_whisper()
        .then(|| (dir.join(WHISPER_FILE.name), WHISPER_FILE));
    ASR_FILES
        .into_iter()
        .map(move |f| (asr.join(f.name), f))
        .chain([
            (dir.join(VAD_FILE.name), VAD_FILE),
            (dir.join(VOICE_FILE.name), VOICE_FILE),
        ])
        .chain(whisper)
}

fn size_ok(path: &Path, f: &ModelFile) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() == f.size)
}

/// Quick check by file sizes of what `quality` needs (hashes are checked
/// after download and by [`verify`]).
pub fn status(dir: &Path, quality: Quality) -> ModelStatus {
    let asr = dir.join(ASR_DIR);
    if all_files(dir, quality).all(|(p, f)| size_ok(&p, &f)) {
        ModelStatus::Ready
    } else if OLD_ASR_DIRS.iter().any(|d| dir.join(d).exists())
        && ASR_FILES.iter().all(|f| !asr.join(f.name).exists())
    {
        ModelStatus::Outdated
    } else if all_files(dir, quality).all(|(p, _)| !p.exists()) && !dir.join(WORK_DIR).exists() {
        ModelStatus::Missing
    } else {
        ModelStatus::Incomplete
    }
}

/// Bytes still to download for `quality` (whole files; a partial download
/// resumes, so it can be less).
pub fn missing_bytes(dir: &Path, quality: Quality) -> u64 {
    let asr = dir.join(ASR_DIR);
    let mut n = 0;
    for f in [VAD_FILE, VOICE_FILE] {
        if !size_ok(&dir.join(f.name), &f) {
            n += f.size;
        }
    }
    for f in &ASR_FILES {
        if !size_ok(&asr.join(f.name), f) {
            n += f.size;
        }
    }
    if quality.needs_whisper() && !size_ok(&dir.join(WHISPER_FILE.name), &WHISPER_FILE) {
        n += WHISPER_FILE.size;
    }
    n
}

/// Bytes the model takes on disk, partial downloads included.
pub fn disk_size(dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        match fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => fs::read_dir(p)
                .map(|rd| rd.flatten().map(|e| walk(&e.path())).sum())
                .unwrap_or(0),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    walk(&dir.join(ASR_DIR))
        + OLD_ASR_DIRS.iter().map(|d| walk(&dir.join(d))).sum::<u64>()
        + walk(&dir.join(VAD_FILE.name))
        + walk(&dir.join(VOICE_FILE.name))
        + walk(&dir.join(WHISPER_FILE.name))
        + walk(&dir.join(WORK_DIR))
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn check_hash(path: &Path, expected: &str) -> io::Result<()> {
    let got = sha256_file(path)?;
    if got == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is damaged (SHA256 {got})", path.display()),
        ))
    }
}

/// Checks every file `quality` needs against its SHA256 (about a second
/// per 600 MB).
pub fn verify(dir: &Path, quality: Quality) -> io::Result<()> {
    for (path, f) in all_files(dir, quality) {
        if !size_ok(&path, &f) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is missing or incomplete", path.display()),
            ));
        }
        check_hash(&path, f.sha256)?;
    }
    Ok(())
}

/// Removes the model files and partial downloads (other files in a
/// user-chosen folder are left alone).
pub fn delete(dir: &Path) -> io::Result<()> {
    let dirs = [ASR_DIR, WORK_DIR].into_iter().chain(OLD_ASR_DIRS);
    for p in dirs.map(|d| dir.join(d)) {
        if p.exists() {
            fs::remove_dir_all(&p)?;
        }
    }
    for f in [VAD_FILE, VOICE_FILE, WHISPER_FILE] {
        let p = dir.join(f.name);
        if p.exists() {
            fs::remove_file(p)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Verifying,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownloadProgress {
    pub phase: Phase,
    /// Bytes downloaded of [`download_size`].
    pub done: u64,
    pub total: u64,
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "download cancelled")
}

/// A system tool; on Windows the one in System32, not a copy from Git or
/// elsewhere on PATH, and without a console window popping up.
fn system_tool(name: &str) -> Command {
    let mut path = PathBuf::from(name);
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let p = PathBuf::from(root)
            .join("System32")
            .join(format!("{name}.exe"));
        if p.exists() {
            path = p;
        }
    }
    #[allow(unused_mut)]
    let mut cmd = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Downloads `url` into `dest` with curl, resuming a partial file, while
/// reporting the file size; `before` bytes were done in earlier steps.
fn fetch(
    url: &str,
    dest: &Path,
    size: u64,
    (before, total): (u64, u64),
    progress: &mut dyn FnMut(DownloadProgress),
    cancel: &AtomicBool,
) -> io::Result<()> {
    if fs::metadata(dest).is_ok_and(|m| m.len() > size) {
        fs::remove_file(dest)?;
    }
    if !fs::metadata(dest).is_ok_and(|m| m.len() == size) {
        let mut child = system_tool("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--retry",
                "5",
                "--retry-delay",
                "3",
                "--connect-timeout",
                "30",
                // Give up if the transfer stalls for a minute.
                "--speed-limit",
                "1",
                "--speed-time",
                "60",
                "--continue-at",
                "-",
                "--output",
            ])
            .arg(dest)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("cannot run curl: {e}")))?;
        let status = loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(cancelled());
            }
            if let Some(s) = child.try_wait()? {
                break s;
            }
            let got = fs::metadata(dest).map_or(0, |m| m.len());
            progress(DownloadProgress {
                phase: Phase::Downloading,
                done: before + got.min(size),
                total,
            });
            std::thread::sleep(Duration::from_millis(250));
        };
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            return Err(io::Error::other(format!(
                "download failed: {}",
                err.trim().lines().last().unwrap_or("curl error")
            )));
        }
    }
    progress(DownloadProgress {
        phase: Phase::Verifying,
        done: before + size,
        total,
    });
    Ok(())
}

/// Downloads whatever `quality` needs and is missing, checks the hashes
/// and moves the files into place. Safe to call again after an
/// interruption: partial downloads resume. `cancel` stops it (the error kind
/// is `Interrupted`).
pub fn download(
    dir: &Path,
    quality: Quality,
    progress: &mut dyn FnMut(DownloadProgress),
    cancel: &AtomicBool,
) -> io::Result<()> {
    let work = dir.join(WORK_DIR);
    fs::create_dir_all(&work)?;
    let total = download_size(quality);

    // The two small single files first.
    let mut before = 0;
    for (f, url) in [(VAD_FILE, VAD_URL), (VOICE_FILE, VOICE_URL)] {
        let dest = dir.join(f.name);
        if !size_ok(&dest, &f) || check_hash(&dest, f.sha256).is_err() {
            let part = work.join(f.name);
            fetch(url, &part, f.size, (before, total), progress, cancel)?;
            if let Err(e) = check_hash(&part, f.sha256) {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
            fs::rename(&part, &dest)?;
        }
        before += f.size;
    }

    // Parakeet, file by file into its folder.
    let asr = dir.join(ASR_DIR);
    fs::create_dir_all(&asr)?;
    for f in &ASR_FILES {
        let dest = asr.join(f.name);
        if !size_ok(&dest, f) || check_hash(&dest, f.sha256).is_err() {
            let part = work.join(f.name);
            let url = format!("{ASR_URL}/{}", f.name);
            fetch(&url, &part, f.size, (before, total), progress, cancel)?;
            if let Err(e) = check_hash(&part, f.sha256) {
                let _ = fs::remove_file(&part);
                return Err(io::Error::new(
                    e.kind(),
                    "the downloaded model is damaged; it was deleted, please try again",
                ));
            }
            fs::rename(&part, &dest)?;
        }
        before += f.size;
    }
    // The old export is not needed any more.
    for d in OLD_ASR_DIRS {
        let p = dir.join(d);
        if p.exists() {
            let _ = fs::remove_dir_all(p);
        }
    }

    let whisper = dir.join(WHISPER_FILE.name);
    if quality.needs_whisper()
        && (!size_ok(&whisper, &WHISPER_FILE) || check_hash(&whisper, WHISPER_FILE.sha256).is_err())
    {
        let part = work.join(WHISPER_FILE.name);
        fetch(
            WHISPER_URL,
            &part,
            WHISPER_FILE.size,
            (before, total),
            progress,
            cancel,
        )?;
        if let Err(e) = check_hash(&part, WHISPER_FILE.sha256) {
            let _ = fs::remove_file(&part);
            return Err(io::Error::new(
                e.kind(),
                "the downloaded Whisper model is damaged; it was deleted, please try again",
            ));
        }
        fs::rename(&part, &whisper)?;
    }
    let _ = fs::remove_dir_all(&work);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_files() {
        let d = std::env::temp_dir().join(format!("mr-model-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        for q in [Quality::Fast, Quality::Best] {
            assert_eq!(status(&d, q), ModelStatus::Missing);
        }
        fs::write(d.join(VAD_FILE.name), b"short").unwrap();
        fs::write(d.join(WHISPER_FILE.name), b"partial").unwrap();
        assert_eq!(status(&d, Quality::Fast), ModelStatus::Incomplete);
        assert!(verify(&d, Quality::Fast).is_err());
        assert!(disk_size(&d) >= 12, "the Whisper file counts too");
        delete(&d).unwrap();
        assert_eq!(status(&d, Quality::Best), ModelStatus::Missing);
        assert_eq!(disk_size(&d), 0);
        assert_eq!(
            missing_bytes(&d, Quality::Best),
            download_size(Quality::Best)
        );
        assert_eq!(
            missing_bytes(&d, Quality::Fast),
            download_size(Quality::Fast)
        );
        let p = ModelPaths::in_dir(&d);
        assert!(
            p.encoder
                .ends_with("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3/encoder.onnx")
        );
        fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_old_int8_model_means_an_update() {
        let d = std::env::temp_dir().join(format!("mr-model-old-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let old = d.join(OLD_ASR_DIRS[0]);
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("encoder.int8.onnx"), b"old").unwrap();
        fs::write(d.join(VAD_FILE.name), b"x").unwrap();
        for q in [Quality::Fast, Quality::Best] {
            assert_eq!(status(&d, q), ModelStatus::Outdated);
        }
        assert!(disk_size(&d) >= 4, "the old model counts");
        // Once part of the new model is there it is an interrupted download.
        fs::create_dir_all(d.join(ASR_DIR)).unwrap();
        fs::write(d.join(ASR_DIR).join(TOKENS.name), b"x").unwrap();
        assert_eq!(status(&d, Quality::Fast), ModelStatus::Incomplete);
        assert!(missing_bytes(&d, Quality::Fast) > ENCODER_WEIGHTS.size);
        delete(&d).unwrap();
        assert!(!old.exists());
        assert_eq!(status(&d, Quality::Fast), ModelStatus::Missing);
        fs::remove_dir_all(d).ok();
    }

    #[test]
    fn qualities_and_sizes_per_system() {
        assert!(Quality::default_for_this_system().is_available());
        if cfg!(target_os = "macos") {
            assert_eq!(Quality::default_for_this_system(), Quality::Best);
        } else {
            assert_eq!(Quality::available(), &[Quality::Fast]);
        }
        assert_eq!(
            download_size(Quality::Best) - download_size(Quality::Fast),
            WHISPER_FILE.size
        );
        for q in [Quality::Best, Quality::Fast] {
            assert_eq!(Quality::parse(q.as_str()), Some(q));
        }
        assert_eq!(Quality::parse("x"), None);
    }

    #[test]
    fn hashes_are_hex_sha256() {
        let d = std::env::temp_dir().join(format!("mr-sha-{}.txt", std::process::id()));
        fs::write(&d, b"abc").unwrap();
        assert_eq!(
            sha256_file(&d).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::remove_file(d).ok();
    }

    /// Real download of the 0.6 MB VAD file (network): `cargo test -p
    /// recorder-transcribe -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn vad_file_downloads_and_verifies() {
        let d = std::env::temp_dir().join(format!("mr-dl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let work = d.join(WORK_DIR);
        fs::create_dir_all(&work).unwrap();
        let mut seen = 0;
        fetch(
            VAD_URL,
            &work.join(VAD_FILE.name),
            VAD_FILE.size,
            (0, VAD_FILE.size),
            &mut |_| seen += 1,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(seen > 0);
        check_hash(&work.join(VAD_FILE.name), VAD_FILE.sha256).unwrap();
        fs::remove_dir_all(d).ok();
    }
}
