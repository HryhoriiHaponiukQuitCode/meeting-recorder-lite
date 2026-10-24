//! System audio on macOS 13.0–14.3 through ScreenCaptureKit, audio only.
//!
//! A display filter with a 2×2 px, 1 fps video configuration (SCK requires a
//! video stream; we never attach a screen output, so frames are discarded by
//! the framework) and `capturesAudio` + `excludesCurrentProcessAudio`.
//! Sample buffers carry host-clock presentation timestamps. Unlike
//! OpenCassava's version, non-interleaved buffers (one per channel) are mixed
//! correctly.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};

use super::permissions::{self, Access};
use crate::ring::{SharedProducer, push_shared};
use crate::{CaptureEvent, Source, SourceHandle};

const RATE: u32 = 48_000;

struct Ivars {
    producer: SharedProducer,
    mono: RefCell<Vec<f32>>,
    stopped: Arc<AtomicBool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MeetingRecorderAudioOutput"]
    #[ivars = Ivars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Audio {
                self.handle(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            tracing::warn!(error = %error.localizedDescription(), "ScreenCaptureKit stream stopped");
            self.ivars().stopped.store(true, Ordering::SeqCst);
        }
    }
);

impl AudioOutput {
    fn new(ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }

    fn handle(&self, sample: &CMSampleBuffer) {
        // Room for an AudioBufferList with up to 8 channel buffers.
        let mut storage = [0u64; 32];
        let abl = storage.as_mut_ptr().cast::<AudioBufferList>();
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: the storage is large and aligned enough; we release the block buffer below.
        let status = unsafe {
            sample.audio_buffer_list_with_retained_block_buffer(
                std::ptr::null_mut(),
                abl,
                size_of_val(&storage),
                None,
                None,
                kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
                &mut block,
            )
        };
        if status != 0 {
            return;
        }
        let mut mono = self.ivars().mono.borrow_mut();
        unsafe { super::downmix_buffer_list(abl, &mut mono) };
        if let Some(b) = NonNull::new(block) {
            // SAFETY: the function returned a +1 retained block buffer.
            drop(unsafe { objc2_core_foundation::CFRetained::from_raw(b) });
        }
        let pts = unsafe { sample.presentation_time_stamp() };
        let host_ns = if pts.flags.contains(CMTimeFlags::Valid) && pts.timescale > 0 {
            (pts.value as i128 * 1_000_000_000 / pts.timescale as i128) as u64
        } else {
            crate::host_now_ns().saturating_sub(mono.len() as u64 * 1_000_000_000 / RATE as u64)
        };
        push_shared(&self.ivars().producer, host_ns, RATE, &mono);
    }
}

struct Running {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    stopped: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let block = RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: stopping a stream we started.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        let _ = rx.recv_timeout(Duration::from_secs(3));
    }
}

fn open(producer: &SharedProducer) -> Result<(Running, String), String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: completion arguments are valid for the duration of the call.
        let content = unsafe { Retained::retain(content) };
        let err = unsafe { Retained::retain(err) }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send((content, err));
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let (content, err) = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "shareable content timed out".to_string())?;
    let content = content.ok_or_else(|| err.unwrap_or_else(|| "no shareable content".into()))?;
    let displays = unsafe { content.displays() };
    let display = displays.firstObject().ok_or("no display")?;

    // SAFETY: documented SCK initialisers and setters.
    let (stream, output, stopped) = unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        );
        let config = SCStreamConfiguration::new();
        config.setCapturesAudio(true);
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(RATE as isize);
        config.setChannelCount(2);
        config.setWidth(2);
        config.setHeight(2);
        config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: 1,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        let stopped = Arc::new(AtomicBool::new(false));
        let output = AudioOutput::new(Ivars {
            producer: producer.clone(),
            mono: RefCell::new(Vec::with_capacity(RATE as usize)),
            stopped: stopped.clone(),
        });
        let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(delegate),
        );
        let queue = DispatchQueue::new("meeting-recorder.sck-audio", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Audio,
                Some(&queue),
            )
            .map_err(|e| e.localizedDescription().to_string())?;
        (stream, output, stopped)
    };

    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |err: *mut NSError| {
        let err = unsafe { Retained::retain(err) }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(err);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => {}
        Ok(Some(e)) => return Err(e),
        Err(_) => return Err("start capture timed out".into()),
    }
    let name = format!("display {}", unsafe { display.displayID() });
    Ok((
        Running {
            stream,
            _output: output,
            stopped,
        },
        name,
    ))
}

pub fn start(producer: SharedProducer, events: Sender<CaptureEvent>) -> SourceHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = thread::Builder::new()
        .name("sck-capture".into())
        .spawn(move || supervise(producer, events, stop2))
        .expect("spawn sck thread");
    SourceHandle::new(stop, thread)
}

fn supervise(producer: SharedProducer, events: Sender<CaptureEvent>, stop: Arc<AtomicBool>) {
    if permissions::ensure_screen_capture() == Access::Denied {
        let _ = events.send(CaptureEvent::Failed {
            source: Source::System,
            error: "screen recording permission denied".into(),
            permission_denied: true,
        });
    }
    let mut backoff = Duration::from_millis(500);
    while !stop.load(Ordering::SeqCst) {
        match open(&producer) {
            Ok((running, name)) => {
                backoff = Duration::from_millis(500);
                let _ = events.send(CaptureEvent::Opened {
                    source: Source::System,
                    device: name,
                    rate: RATE,
                    channels: 2,
                });
                while !stop.load(Ordering::SeqCst) && !running.stopped.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(250));
                }
                if !stop.load(Ordering::SeqCst) {
                    let _ = events.send(CaptureEvent::Lost {
                        source: Source::System,
                        reason: "ScreenCaptureKit stream stopped".into(),
                    });
                }
                drop(running);
            }
            Err(error) => {
                let denied = error.to_lowercase().contains("declined")
                    || error.to_lowercase().contains("permission");
                let _ = events.send(CaptureEvent::Failed {
                    source: Source::System,
                    error,
                    permission_denied: denied,
                });
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}
