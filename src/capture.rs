//! Windows Graphics Capture session producing NV12 frames.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::convert::{bgra_to_nv12, fit_even, Nv12};
use crate::encoder::FrameIn;

pub struct CaptureConfig {
    pub fps: u32,
    pub max_width: u32,
    pub max_height: u32,
}

pub struct CaptureStats {
    pub epoch: Instant,
    pub frames: AtomicU64,
    pub convert_us: AtomicU32,
    pub active: AtomicBool,
    source_dims: AtomicU64,
}

impl CaptureStats {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            frames: AtomicU64::new(0),
            convert_us: AtomicU32::new(0),
            active: AtomicBool::new(false),
            source_dims: AtomicU64::new(0),
        }
    }

    pub fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.convert_us.store(0, Ordering::Relaxed);
        self.source_dims.store(0, Ordering::Relaxed);
    }

    pub fn source_dims(&self) -> (u32, u32) {
        let packed = self.source_dims.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }
}

struct Flags {
    frames: Sender<FrameIn>,
    drain: Receiver<FrameIn>,
    recycle: Receiver<Vec<u8>>,
    stats: Arc<CaptureStats>,
    max_width: u32,
    max_height: u32,
}

struct CaptureHandler {
    flags: Flags,
    last_idle_frame: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Flags;
    type Error = Box<dyn Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, last_idle_frame: Instant::now() })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let stats = &self.flags.stats;
        stats.frames.fetch_add(1, Ordering::Relaxed);

        let (src_w, src_h) = (frame.width(), frame.height());
        stats
            .source_dims
            .store(((src_w as u64) << 32) | src_h as u64, Ordering::Relaxed);

        // Nobody is watching: convert occasionally so a fresh still exists for
        // the next client, but don't burn CPU on every frame.
        if !stats.active.load(Ordering::Relaxed) {
            if self.last_idle_frame.elapsed() < Duration::from_millis(500) {
                return Ok(());
            }
            self.last_idle_frame = Instant::now();
        }

        let t0 = Instant::now();
        let capture_us = stats.epoch.elapsed().as_micros() as u64;
        let (dst_w, dst_h) = fit_even(src_w, src_h, self.flags.max_width, self.flags.max_height);

        let mut buffer = match frame.buffer() {
            Ok(buffer) => buffer,
            Err(err) => {
                tracing::debug!(%err, "frame mapping failed");
                return Ok(());
            }
        };
        let pitch = buffer.row_pitch() as usize;
        let src = buffer.as_raw_buffer();

        let pooled = self.flags.recycle.try_recv().unwrap_or_default();
        let mut nv12 = Nv12::with_buffer(pooled, dst_w, dst_h);
        bgra_to_nv12(src, pitch, src_w, src_h, dst_w, dst_h, &mut nv12.data);
        stats
            .convert_us
            .store(t0.elapsed().as_micros() as u32, Ordering::Relaxed);

        let msg = FrameIn {
            nv12: nv12.data,
            width: dst_w,
            height: dst_h,
            capture_us,
        };
        match self.flags.frames.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(msg)) => {
                // Encoder is behind: drop the stale frame, keep the newest.
                let _ = self.flags.drain.try_recv();
                let _ = self.flags.frames.try_send(msg);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(())
    }
}

pub struct CaptureSession {
    control: CaptureControl<CaptureHandler, Box<dyn Error + Send + Sync>>,
}

impl CaptureSession {
    pub fn stop(self) {
        let _ = self.control.stop();
    }

    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }
}

pub fn start(
    monitor: Monitor,
    config: &CaptureConfig,
    frames: Sender<FrameIn>,
    drain: Receiver<FrameIn>,
    recycle: Receiver<Vec<u8>>,
    stats: Arc<CaptureStats>,
) -> Result<CaptureSession, Box<dyn Error + Send + Sync>> {
    let interval = Duration::from_millis((1000 / config.fps.max(1)) as u64);
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(interval),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        Flags {
            frames,
            drain,
            recycle,
            stats: stats.clone(),
            max_width: config.max_width,
            max_height: config.max_height,
        },
    );
    let control = CaptureHandler::start_free_threaded(settings)?;
    Ok(CaptureSession { control })
}
