//! Channel check: opens the microphone and system audio the same way a
//! recording does, but only measures levels. Nothing is written to disk.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use recorder_capture::{BlockConsumer, CaptureEvent, Source, block_channel, mic};
use recorder_core::{MIC, SYSTEM};

use crate::recorder::{SystemOption, start_system};

#[derive(Clone, Debug, Default)]
pub struct MonitorStatus {
    /// Smoothed peak level per channel, 0.0..=1.0.
    pub levels: [f32; 2],
    /// Highest peak per channel since the check started.
    pub max: [f32; 2],
    /// Why a channel could not be opened, if it could not.
    pub failed: [Option<String>; 2],
    /// Device delivering each channel.
    pub devices: [Option<String>; 2],
}

pub struct Monitor {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<MonitorStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(mic_device: Option<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(MonitorStatus::default()));
        let thread = {
            let (stop, status) = (stop.clone(), status.clone());
            std::thread::Builder::new()
                .name("channel-check".into())
                .spawn(move || run(mic_device, stop, status))
                .ok()
        };
        if thread.is_none() {
            tracing::error!("cannot start the channel check thread");
        }
        Self {
            stop,
            status,
            thread,
        }
    }

    pub fn status(&self) -> MonitorStatus {
        self.status.lock().expect("status").clone()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn channel(source: Source) -> usize {
    match source {
        Source::Mic => MIC,
        Source::System => SYSTEM,
    }
}

fn peak(consumer: &mut BlockConsumer, buf: &mut Vec<f32>) -> f32 {
    let mut peak = 0f32;
    while consumer.pop(buf).is_some() {
        peak = buf.iter().fold(peak, |p, s| p.max(s.abs()));
    }
    peak
}

fn run(mic_device: Option<String>, stop: Arc<AtomicBool>, status: Arc<Mutex<MonitorStatus>>) {
    tracing::info!("channel check started");
    let (ev_tx, ev_rx) = mpsc::channel();
    let (mic_p, mut mic_c) = block_channel(2);
    let (sys_p, mut sys_c) = block_channel(2);
    let mic_handle = mic::start(
        mic_p.into_shared(),
        ev_tx.clone(),
        mic::MicOptions { device: mic_device },
    );
    let system = start_system(&SystemOption::Auto, sys_p.into_shared(), ev_tx);
    let mut buf = Vec::with_capacity(48_000);
    while !stop.load(Ordering::SeqCst) {
        let mut st = status.lock().expect("status");
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                CaptureEvent::Opened { source, device, .. } => {
                    st.devices[channel(source)] = Some(device);
                    st.failed[channel(source)] = None;
                }
                CaptureEvent::Failed {
                    source,
                    error,
                    permission_denied,
                } => {
                    st.failed[channel(source)] = Some(if permission_denied {
                        "no permission".into()
                    } else {
                        error
                    });
                }
                CaptureEvent::Lost { .. } => {}
            }
        }
        let peaks = [peak(&mut mic_c, &mut buf), peak(&mut sys_c, &mut buf)];
        let MonitorStatus { levels, max, .. } = &mut *st;
        for ((level, max), p) in levels.iter_mut().zip(max).zip(peaks) {
            *level = p.max(*level * 0.9);
            *max = max.max(p);
        }
        drop(st);
        std::thread::sleep(Duration::from_millis(20));
    }
    mic_handle.stop();
    if let Some((h, _)) = system {
        h.stop();
    }
    let st = status.lock().expect("status");
    tracing::info!(
        mic_max = st.max[MIC],
        system_max = st.max[SYSTEM],
        failed = ?st.failed,
        "channel check finished"
    );
}
