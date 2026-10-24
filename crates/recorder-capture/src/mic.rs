//! Microphone capture through cpal (WASAPI shared mode on Windows, Core
//! Audio on macOS). Follows the system default input device unless a device
//! name is pinned, and reopens it after errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, InputCallbackInfo, SampleFormat, SizedSample, StreamInstant};

use crate::ring::{SharedProducer, push_shared};
use crate::{CaptureEvent, Source, SourceHandle, downmix, host_now_ns};

#[derive(Clone, Debug, Default)]
pub struct MicOptions {
    /// Case-insensitive substring of the device name; `None` = system default.
    pub device: Option<String>,
}

struct Opened {
    _stream: cpal::Stream,
    id: Option<cpal::DeviceId>,
}

pub fn start(
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
    opts: MicOptions,
) -> SourceHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = thread::Builder::new()
        .name("mic-capture".into())
        .spawn(move || supervise(producer, events, opts, stop2))
        .expect("spawn mic thread");
    SourceHandle::new(stop, thread)
}

fn supervise(
    producer: SharedProducer,
    events: Sender<CaptureEvent>,
    opts: MicOptions,
    stop: Arc<AtomicBool>,
) {
    let host = cpal::default_host();
    let failed = Arc::new(AtomicBool::new(false));
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::SeqCst) {
        failed.store(false, Ordering::SeqCst);
        match open(&host, &opts, &producer, &failed, &events) {
            Ok(opened) => {
                backoff = Duration::from_millis(200);
                let mut ticks = 0u32;
                loop {
                    thread::sleep(Duration::from_millis(250));
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if failed.load(Ordering::SeqCst) {
                        let _ = events.send(CaptureEvent::Lost {
                            source: Source::Mic,
                            reason: "stream error".into(),
                        });
                        break;
                    }
                    ticks += 1;
                    if opts.device.is_none() && ticks.is_multiple_of(4) {
                        let current = host.default_input_device().and_then(|d| d.id().ok());
                        if current.is_some() && current != opened.id {
                            let _ = events.send(CaptureEvent::Lost {
                                source: Source::Mic,
                                reason: "default input device changed".into(),
                            });
                            break;
                        }
                    }
                }
            }
            Err((error, permission_denied)) => {
                let _ = events.send(CaptureEvent::Failed {
                    source: Source::Mic,
                    error,
                    permission_denied,
                });
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

fn find_device(host: &cpal::Host, opts: &MicOptions) -> Option<cpal::Device> {
    match &opts.device {
        None => host.default_input_device(),
        Some(want) => {
            let want = want.to_lowercase();
            host.input_devices()
                .ok()?
                .find(|d| device_name(d).to_lowercase().contains(&want))
        }
    }
}

pub fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Names of the available input devices (for `recorder devices`).
pub fn list_inputs() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

fn open(
    host: &cpal::Host,
    opts: &MicOptions,
    producer: &SharedProducer,
    failed: &Arc<AtomicBool>,
    events: &Sender<CaptureEvent>,
) -> Result<Opened, (String, bool)> {
    let device = find_device(host, opts).ok_or(("no input device".to_string(), false))?;
    let name = device_name(&device);
    let supported = device.default_input_config().map_err(classify)?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, producer, failed),
        SampleFormat::I16 => build::<i16>(&device, &config, producer, failed),
        SampleFormat::I32 => build::<i32>(&device, &config, producer, failed),
        SampleFormat::U16 => build::<u16>(&device, &config, producer, failed),
        SampleFormat::F64 => build::<f64>(&device, &config, producer, failed),
        other => return Err((format!("unsupported sample format {other:?}"), false)),
    }
    .map_err(classify)?;
    stream.play().map_err(classify)?;
    let _ = events.send(CaptureEvent::Opened {
        source: Source::Mic,
        device: name,
        rate: config.sample_rate,
        channels: config.channels,
    });
    Ok(Opened {
        _stream: stream,
        id: device.id().ok(),
    })
}

fn classify(e: cpal::Error) -> (String, bool) {
    let denied = e.kind() == cpal::ErrorKind::PermissionDenied;
    (e.to_string(), denied)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    producer: &SharedProducer,
    failed: &Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let producer = producer.clone();
    let failed = failed.clone();
    let mut mono = Vec::with_capacity(192_000);
    device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], info: &InputCallbackInfo| {
            downmix(
                data,
                channels,
                &mut mono,
                <f32 as FromSample<T>>::from_sample_,
            );
            let capture = info.timestamp().capture;
            let host_ns = if capture == StreamInstant::ZERO {
                host_now_ns().saturating_sub(mono.len() as u64 * 1_000_000_000 / rate as u64)
            } else {
                capture.duration_since(StreamInstant::ZERO).as_nanos() as u64
            };
            push_shared(&producer, host_ns, rate, &mono);
        },
        move |e| match e.kind() {
            // The stream keeps running after these; reopening would lose audio.
            cpal::ErrorKind::Xrun
            | cpal::ErrorKind::RealtimeDenied
            | cpal::ErrorKind::DeviceChanged => {
                tracing::info!(error = %e, "microphone stream warning");
            }
            _ => {
                tracing::warn!(error = %e, "microphone stream error");
                failed.store(true, Ordering::SeqCst);
            }
        },
        None,
    )
}
