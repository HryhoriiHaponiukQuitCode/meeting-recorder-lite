//! System-audio capture on Windows: WASAPI loopback on the default output
//! device, or process loopback (Windows 10 2004+) for one application tree.
//!
//! The capture skeleton (default render endpoint → shared-mode loopback
//! client → capture client) follows OpenCassava (MIT, © 2026 OpenCassava contributors,
//! `opencassava/src-tauri/src/audio_windows.rs`), with these differences:
//! event-driven instead of 10 ms polling, every pending packet is drained per
//! wake-up, `AUDCLNT_BUFFERFLAGS_SILENT` buffers are read as zeros, buffers
//! carry their QPC timestamp so the engine can place them on the timeline
//! (loopback delivers *nothing* while the system is silent; the engine fills
//! that time with silence), and the default device is followed.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use wasapi::{
    AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat,
};

use crate::ring::{SharedProducer, push_shared};
use crate::{CaptureEvent, Source, SourceHandle, host_now_ns};

type Res<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Debug, Default)]
pub struct LoopbackOptions {
    /// Capture only this executable (e.g. `Zoom.exe`) and its children.
    pub process: Option<String>,
}

struct Opened {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    rate: u32,
    channels: usize,
    device_id: Option<String>,
}

enum Exit {
    Stop,
    DeviceChanged,
}

pub fn start(
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
    opts: LoopbackOptions,
) -> SourceHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = thread::Builder::new()
        .name("loopback-capture".into())
        .spawn(move || supervise(producer, events, opts, stop2))
        .expect("spawn loopback thread");
    SourceHandle::new(stop, thread)
}

fn supervise(
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
    opts: LoopbackOptions,
    stop: Arc<AtomicBool>,
) {
    if wasapi::initialize_mta().ok().is_err() {
        tracing::error!("CoInitializeEx failed");
    }
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::SeqCst) {
        match open(&opts, &events) {
            Ok(opened) => {
                backoff = Duration::from_millis(200);
                let result = capture_loop(&opened, &producer, &stop, opts.process.is_none());
                let _ = opened.client.stop_stream();
                match result {
                    Ok(Exit::Stop) => break,
                    Ok(Exit::DeviceChanged) => {
                        let _ = events.send(CaptureEvent::Lost {
                            source: Source::System,
                            reason: "default output device changed".into(),
                        });
                    }
                    Err(e) => {
                        let _ = events.send(CaptureEvent::Lost {
                            source: Source::System,
                            reason: e.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                let _ = events.send(CaptureEvent::Failed {
                    source: Source::System,
                    error: e.to_string(),
                    permission_denied: false,
                });
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
    wasapi::deinitialize();
}

fn open(opts: &LoopbackOptions, events: &Sender<CaptureEvent>) -> Res<Opened> {
    let (client, format, name, device_id) = match &opts.process {
        Some(exe) => {
            let pid = find_process_tree_root(exe)
                .ok_or_else(|| format!("process {exe} is not running"))?;
            let client = AudioClient::new_application_loopback_client(pid, true)?;
            // Process loopback cannot report a mix format; ask for 48 kHz stereo float.
            let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
            (client, format, format!("process {exe} (pid {pid})"), None)
        }
        None => {
            let enumerator = DeviceEnumerator::new()?;
            let device = enumerator.get_default_device(&Direction::Render)?;
            let client = device.get_iaudioclient()?;
            let mix = client.get_mixformat()?;
            let format = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                mix.get_samplespersec() as usize,
                mix.get_nchannels() as usize,
                None,
            );
            (
                client,
                format,
                device.get_friendlyname()?,
                Some(device.get_id()?),
            )
        }
    };
    let mut client = client;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 2_000_000,
    };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    let rate = format.get_samplespersec();
    let channels = format.get_nchannels() as usize;
    let _ = events.send(CaptureEvent::Opened {
        source: Source::System,
        device: name,
        rate,
        channels: channels as u16,
    });
    Ok(Opened {
        client,
        capture,
        event,
        rate,
        channels,
        device_id,
    })
}

fn capture_loop(
    o: &Opened,
    producer: &SharedProducer,
    stop: &AtomicBool,
    follow_default: bool,
) -> Res<Exit> {
    let frame_bytes = o.channels * 4;
    let mut raw = vec![0u8; o.rate as usize * frame_bytes];
    let mut mono: Vec<f32> = Vec::with_capacity(o.rate as usize);
    let mut last_check = Instant::now();
    let mut discontinuities = 0u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(Exit::Stop);
        }
        // Times out every 100 ms while the system is silent: loopback then
        // delivers no packets at all.
        let _ = o.event.wait_for_event(100);
        while o.capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let (frames, info) = o.capture.read_from_device(&mut raw)?;
            if frames == 0 {
                break;
            }
            let frames = frames as usize;
            mono.clear();
            if info.flags.silent {
                mono.resize(frames, 0.0);
            } else {
                let scale = 1.0 / o.channels as f32;
                for frame in raw[..frames * frame_bytes].chunks_exact(frame_bytes) {
                    let sum: f32 = frame
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .sum();
                    mono.push(sum * scale);
                }
            }
            if info.flags.data_discontinuity {
                discontinuities += 1;
                if discontinuities.is_power_of_two() {
                    tracing::debug!(discontinuities, "loopback data discontinuity");
                }
            }
            let host_ns = if info.timestamp != 0 && !info.flags.timestamp_error {
                info.timestamp * 100
            } else {
                host_now_ns().saturating_sub(frames as u64 * 1_000_000_000 / o.rate as u64)
            };
            push_shared(producer, host_ns, o.rate, &mono);
        }
        if follow_default && last_check.elapsed() >= Duration::from_secs(1) {
            last_check = Instant::now();
            let current = DeviceEnumerator::new()
                .and_then(|e| e.get_default_device(&Direction::Render))
                .and_then(|d| d.get_id())
                .ok();
            if current.is_some() && current != o.device_id {
                return Ok(Exit::DeviceChanged);
            }
        }
    }
}

/// PID of the topmost process named `exe` (case-insensitive), so that
/// `include_tree` covers helper processes of multi-process apps.
fn find_process_tree_root(exe: &str) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let want = exe.to_lowercase();
    let mut procs = Vec::new();
    // SAFETY: standard Toolhelp snapshot iteration; the entry struct is sized before use.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                procs.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    let matching: Vec<_> = procs.iter().filter(|p| p.2 == want).collect();
    matching
        .iter()
        .find(|p| !matching.iter().any(|q| q.0 == p.1))
        .or(matching.first())
        .map(|p| p.0)
}

/// Names of the available render devices (for `recorder devices`).
pub fn list_render_devices() -> Vec<String> {
    let _ = wasapi::initialize_mta();
    let Ok(enumerator) = DeviceEnumerator::new() else {
        return Vec::new();
    };
    let Ok(collection) = enumerator.get_device_collection(&Direction::Render) else {
        return Vec::new();
    };
    let n = collection.get_nbr_devices().unwrap_or(0);
    (0..n)
        .filter_map(|i| collection.get_device_at_index(i).ok())
        .filter_map(|d| d.get_friendlyname().ok())
        .collect()
}
