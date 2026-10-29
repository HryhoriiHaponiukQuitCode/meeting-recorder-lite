//! The speech model: NVIDIA Parakeet TDT 0.6B v3 (INT8, sherpa-onnx export,
//! CC-BY-4.0) and the silero voice activity detector (MIT), 640 MiB in all.
//!
//! The files are not in the installer; they are downloaded on first use into
//! the models folder, laid out as
//!
//! ```text
//! models/
//!   silero_vad.onnx
//!   sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/
//!     encoder.int8.onnx  decoder.int8.onnx  joiner.int8.onnx  tokens.txt
//! ```
//!
//! Download uses the system `curl` and `tar` (present on macOS and on
//! Windows 10 1803+): resumable, retried, and checked against pinned
//! SHA256 hashes before anything is moved into place.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const ENGINE: &str = "sherpa-onnx 1.13.8";
pub const ASR_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
pub const MODEL_NAME: &str = ASR_DIR;
pub const ATTRIBUTION: &str = "Speech recognition: NVIDIA Parakeet TDT 0.6B v3 (CC-BY-4.0), \
silero VAD (MIT), sherpa-onnx (Apache-2.0), ONNX Runtime (MIT).";

#[derive(Clone, Copy, Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const ASR_FILES: [ModelFile; 4] = [
    ModelFile {
        name: "encoder.int8.onnx",
        size: 652_184_281,
        sha256: "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    },
    ModelFile {
        name: "decoder.int8.onnx",
        size: 11_845_275,
        sha256: "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    },
    ModelFile {
        name: "joiner.int8.onnx",
        size: 6_355_277,
        sha256: "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    },
    ModelFile {
        name: "tokens.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];

pub const VAD_FILE: ModelFile = ModelFile {
    name: "silero_vad.onnx",
    size: 643_854,
    sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
};
pub const VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

const ARCHIVE_NAME: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";
pub const ARCHIVE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";
const ARCHIVE_SIZE: u64 = 487_170_055;
const ARCHIVE_SHA256: &str = "5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf";

/// Bytes downloaded when nothing is there yet.
pub const DOWNLOAD_SIZE: u64 = ARCHIVE_SIZE + VAD_FILE.size;

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
}

impl ModelPaths {
    pub fn in_dir(dir: &Path) -> Self {
        let asr = dir.join(ASR_DIR);
        Self {
            encoder: asr.join(ASR_FILES[0].name),
            decoder: asr.join(ASR_FILES[1].name),
            joiner: asr.join(ASR_FILES[2].name),
            tokens: asr.join(ASR_FILES[3].name),
            vad: dir.join(VAD_FILE.name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    /// Nothing downloaded.
    Missing,
    /// Some files missing or of the wrong size (interrupted or damaged).
    Incomplete,
    /// All files present with the expected sizes.
    Ready,
}

fn all_files(dir: &Path) -> impl Iterator<Item = (PathBuf, ModelFile)> {
    let asr = dir.join(ASR_DIR);
    ASR_FILES
        .into_iter()
        .map(move |f| (asr.join(f.name), f))
        .chain(std::iter::once((dir.join(VAD_FILE.name), VAD_FILE)))
}

fn size_ok(path: &Path, f: &ModelFile) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() == f.size)
}

/// Quick check by file sizes (hashes are checked after download and by
/// [`verify`]).
pub fn status(dir: &Path) -> ModelStatus {
    if all_files(dir).all(|(p, f)| size_ok(&p, &f)) {
        ModelStatus::Ready
    } else if all_files(dir).all(|(p, _)| !p.exists()) && !dir.join(WORK_DIR).exists() {
        ModelStatus::Missing
    } else {
        ModelStatus::Incomplete
    }
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
    walk(&dir.join(ASR_DIR)) + walk(&dir.join(VAD_FILE.name)) + walk(&dir.join(WORK_DIR))
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

/// Checks every file against its SHA256 (about a second per 600 MB).
pub fn verify(dir: &Path) -> io::Result<()> {
    for (path, f) in all_files(dir) {
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
    for p in [dir.join(ASR_DIR), dir.join(WORK_DIR)] {
        if p.exists() {
            fs::remove_dir_all(&p)?;
        }
    }
    let vad = dir.join(VAD_FILE.name);
    if vad.exists() {
        fs::remove_file(vad)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Verifying,
    Unpacking,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownloadProgress {
    pub phase: Phase,
    /// Bytes downloaded of [`DOWNLOAD_SIZE`].
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
    before: u64,
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
                total: DOWNLOAD_SIZE,
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
        total: DOWNLOAD_SIZE,
    });
    Ok(())
}

/// Downloads whatever is missing, checks the hashes and moves the files
/// into place. Safe to call again after an interruption: partial downloads
/// resume. `cancel` stops it (the error kind is `Interrupted`).
pub fn download(
    dir: &Path,
    progress: &mut dyn FnMut(DownloadProgress),
    cancel: &AtomicBool,
) -> io::Result<()> {
    let work = dir.join(WORK_DIR);
    fs::create_dir_all(&work)?;

    let vad = dir.join(VAD_FILE.name);
    if !size_ok(&vad, &VAD_FILE) || check_hash(&vad, VAD_FILE.sha256).is_err() {
        let part = work.join(VAD_FILE.name);
        fetch(VAD_URL, &part, VAD_FILE.size, 0, progress, cancel)?;
        if let Err(e) = check_hash(&part, VAD_FILE.sha256) {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
        fs::rename(&part, &vad)?;
    }

    let asr = dir.join(ASR_DIR);
    let asr_ok = ASR_FILES
        .iter()
        .all(|f| size_ok(&asr.join(f.name), f) && check_hash(&asr.join(f.name), f.sha256).is_ok());
    if !asr_ok {
        let archive = work.join(ARCHIVE_NAME);
        fetch(
            ARCHIVE_URL,
            &archive,
            ARCHIVE_SIZE,
            VAD_FILE.size,
            progress,
            cancel,
        )?;
        if let Err(e) = check_hash(&archive, ARCHIVE_SHA256) {
            let _ = fs::remove_file(&archive);
            return Err(io::Error::new(
                e.kind(),
                "the downloaded model is damaged; it was deleted, please try again",
            ));
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        progress(DownloadProgress {
            phase: Phase::Unpacking,
            done: DOWNLOAD_SIZE,
            total: DOWNLOAD_SIZE,
        });
        let unpack = work.join("unpack");
        if unpack.exists() {
            fs::remove_dir_all(&unpack)?;
        }
        fs::create_dir_all(&unpack)?;
        let out = system_tool("tar")
            .arg("-xjf")
            .arg(&archive)
            .arg("-C")
            .arg(&unpack)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| io::Error::new(e.kind(), format!("cannot run tar: {e}")))?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "cannot unpack the model: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // Keep only the four files, checked, in a fresh folder.
        let staged = work.join(ASR_DIR);
        if staged.exists() {
            fs::remove_dir_all(&staged)?;
        }
        fs::create_dir_all(&staged)?;
        for f in &ASR_FILES {
            let src = unpack.join(ASR_DIR).join(f.name);
            check_hash(&src, f.sha256)?;
            fs::rename(&src, staged.join(f.name))?;
        }
        if asr.exists() {
            fs::remove_dir_all(&asr)?;
        }
        fs::rename(&staged, &asr)?;
        let _ = fs::remove_dir_all(&unpack);
        let _ = fs::remove_file(&archive);
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
        assert_eq!(status(&d), ModelStatus::Missing);
        fs::write(d.join(VAD_FILE.name), b"short").unwrap();
        assert_eq!(status(&d), ModelStatus::Incomplete);
        assert!(verify(&d).is_err());
        assert!(disk_size(&d) >= 5);
        delete(&d).unwrap();
        assert_eq!(status(&d), ModelStatus::Missing);
        assert_eq!(disk_size(&d), 0);
        let p = ModelPaths::in_dir(&d);
        assert!(
            p.encoder
                .ends_with("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx")
        );
        fs::remove_dir_all(d).ok();
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
            0,
            &mut |_| seen += 1,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(seen > 0);
        check_hash(&work.join(VAD_FILE.name), VAD_FILE.sha256).unwrap();
        fs::remove_dir_all(d).ok();
    }
}
