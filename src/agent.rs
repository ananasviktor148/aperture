//! Owns the capture + encode pipeline and the current session state.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::broadcast;
use windows::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
use windows_capture::monitor::Monitor;

use crate::capture::{self, CaptureConfig, CaptureSession, CaptureStats};
use crate::convert::fit_even;
use crate::display::{self, MonitorInfo, Rect};
use crate::encoder::{self, EncodedPacket, EncoderControl, EncoderInfo, EncoderSettings};

pub struct AgentConfig {
    pub monitor: usize,
    pub fps: u32,
    pub bitrate: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub hardware: bool,
}

pub struct VideoState {
    pub width: u32,
    pub height: u32,
    pub monitor: usize,
    pub monitor_rect: Rect,
    pub encoder_name: String,
    pub fps: u32,
    pub bitrate: u32,
    pub error: Option<String>,
}

pub struct AgentShared {
    pub state: RwLock<VideoState>,
    pub packets: broadcast::Sender<Arc<EncodedPacket>>,
    pub control: Arc<EncoderControl>,
    pub capture_stats: Arc<CaptureStats>,
    pub clients: AtomicUsize,
    pub monitors: Vec<MonitorInfo>,
    pub ready: AtomicBool,
}

pub struct Agent {
    pub shared: Arc<AgentShared>,
    cmd_tx: Sender<AgentCmd>,
    supervisor: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

/// Decrements the client count when dropped; used by every transport.
pub struct ClientGuard(pub Arc<Agent>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.client_disconnected();
    }
}

enum AgentCmd {
    Switch(usize),
    Stop,
}

impl Agent {
    pub fn start(config: AgentConfig, monitors: Vec<MonitorInfo>) -> Arc<Self> {
        let (packets, _) = broadcast::channel(8);
        let shared = Arc::new(AgentShared {
            state: RwLock::new(VideoState {
                width: 0,
                height: 0,
                monitor: config.monitor,
                monitor_rect: Rect { x: 0, y: 0, width: 1920, height: 1080 },
                encoder_name: String::new(),
                fps: config.fps,
                bitrate: config.bitrate,
                error: None,
            }),
            packets,
            control: Arc::new(EncoderControl::new(config.bitrate)),
            capture_stats: Arc::new(CaptureStats::new()),
            clients: AtomicUsize::new(0),
            monitors,
            ready: AtomicBool::new(false),
        });
        let (cmd_tx, cmd_rx) = channel();
        let supervisor_shared = shared.clone();
        let cfg = AgentConfig { ..config };
        let supervisor = thread::Builder::new()
            .name("aperture-supervisor".into())
            .spawn(move || supervise(cfg, supervisor_shared, cmd_rx))
            .expect("spawn supervisor");
        Arc::new(Self {
            shared,
            cmd_tx,
            supervisor: parking_lot::Mutex::new(Some(supervisor)),
        })
    }

    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.shared.ready.load(Ordering::Relaxed) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn switch_monitor(&self, index: usize) {
        if let Some(monitor) = self.shared.monitors.get(index) {
            let mut state = self.shared.state.write();
            state.monitor = index;
            state.monitor_rect = monitor.rect();
        }
        let _ = self.cmd_tx.send(AgentCmd::Switch(index));
    }

    pub fn client_connected(&self) {
        let count = self.shared.clients.fetch_add(1, Ordering::SeqCst) + 1;
        if count == 1 {
            self.shared.capture_stats.active.store(true, Ordering::Relaxed);
            self.shared.control.active.store(true, Ordering::Relaxed);
            // A sleeping display makes DWM render at ~1 fps, which is what WGC
            // captures — and injected input does not wake it. Keep the display
            // and system awake for the whole session.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            }
        }
        self.shared.control.force_keyframe.store(true, Ordering::Relaxed);
    }

    pub fn client_disconnected(&self) {
        let count = self.shared.clients.fetch_sub(1, Ordering::SeqCst) - 1;
        if count == 0 {
            self.shared.capture_stats.active.store(false, Ordering::Relaxed);
            self.shared.control.active.store(false, Ordering::Relaxed);
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(AgentCmd::Stop);
        if let Some(handle) = self.supervisor.lock().take() {
            let _ = handle.join();
        }
    }
}

struct Pipeline {
    session: CaptureSession,
    encoder: JoinHandle<()>,
    source_dims: (u32, u32),
}

fn supervise(config: AgentConfig, shared: Arc<AgentShared>, cmd_rx: Receiver<AgentCmd>) {
    let mut current = config.monitor;
    let mut pipeline: Option<Pipeline> = None;

    loop {
        if pipeline.is_none() {
            let monitors = display::enumerate();
            let selected = monitors
                .get(current)
                .or_else(|| monitors.iter().find(|m| m.primary))
                .or_else(|| monitors.first())
                .cloned();
            match selected {
                Some(monitor) => match start_pipeline(&config, &shared, &monitor) {
                    Ok((session, encoder, info)) => {
                        {
                            let mut state = shared.state.write();
                            state.width = info.width;
                            state.height = info.height;
                            state.monitor = monitor.index;
                            state.monitor_rect = monitor.rect();
                            state.encoder_name = info.name.clone();
                            state.error = None;
                        }
                        current = monitor.index;
                        shared.control.force_keyframe.store(true, Ordering::Relaxed);
                        shared.ready.store(true, Ordering::Relaxed);
                        tracing::info!(
                            encoder = %info.name,
                            width = info.width,
                            height = info.height,
                            monitor = monitor.index,
                            "pipeline running"
                        );
                        pipeline = Some(Pipeline {
                            session,
                            encoder,
                            source_dims: (monitor.width as u32, monitor.height as u32),
                        });
                    }
                    Err(err) => {
                        tracing::error!(%err, "pipeline failed");
                        shared.state.write().error = Some(err);
                        shared.ready.store(true, Ordering::Relaxed);
                        thread::sleep(Duration::from_secs(3));
                    }
                },
                None => {
                    shared.state.write().error = Some("no displays found".into());
                    shared.ready.store(true, Ordering::Relaxed);
                    thread::sleep(Duration::from_secs(3));
                }
            }
        }

        match cmd_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(AgentCmd::Switch(index)) => {
                stop_pipeline(pipeline.take());
                current = index;
            }
            Ok(AgentCmd::Stop) => {
                stop_pipeline(pipeline.take());
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let restart = match &pipeline {
                    Some(p) => {
                        let dims = shared.capture_stats.source_dims();
                        let mode_changed = dims.0 > 0 && dims.1 > 0 && dims != p.source_dims;
                        p.session.is_finished() || mode_changed
                    }
                    None => false,
                };
                if restart {
                    tracing::info!("restarting capture pipeline (closed or display changed)");
                    stop_pipeline(pipeline.take());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                stop_pipeline(pipeline.take());
                return;
            }
        }
    }
}

fn stop_pipeline(pipeline: Option<Pipeline>) {
    let Some(pipeline) = pipeline else { return };
    pipeline.session.stop();
    pipeline
        .encoder
        .join()
        .ok();
}

fn start_pipeline(
    config: &AgentConfig,
    shared: &Arc<AgentShared>,
    monitor: &MonitorInfo,
) -> Result<(CaptureSession, JoinHandle<()>, EncoderInfo), String> {
    let (width, height) = fit_even(
        monitor.width as u32,
        monitor.height as u32,
        config.max_width,
        config.max_height,
    );

    let (frames_tx, frames_rx) = crossbeam_channel::bounded(1);
    let capture_frames_rx = frames_rx.clone();
    let (recycle_tx, recycle_rx) = crossbeam_channel::unbounded();
    shared.control.stop.store(false, Ordering::Relaxed);
    shared.control.force_keyframe.store(true, Ordering::Relaxed);
    shared.control.bitrate.store(config.bitrate, Ordering::Relaxed);
    shared.capture_stats.reset();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let encoder = encoder::spawn(
        EncoderSettings {
            width,
            height,
            fps: config.fps,
            bitrate: config.bitrate,
            hardware: config.hardware,
        },
        frames_rx,
        recycle_tx,
        shared.packets.clone(),
        shared.control.clone(),
        ready_tx,
    );

    let info = match ready_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(info)) => info,
        Ok(Err(err)) => {
            shared.control.stop.store(true, Ordering::Relaxed);
            let _ = encoder.join();
            return Err(err);
        }
        Err(_) => {
            shared.control.stop.store(true, Ordering::Relaxed);
            return Err("encoder did not initialize in time".into());
        }
    };

    let handle = Monitor::from_raw_hmonitor(monitor.hmonitor as *mut c_void);
    let session = capture::start(
        handle,
        &CaptureConfig {
            fps: config.fps,
            max_width: config.max_width,
            max_height: config.max_height,
        },
        frames_tx,
        capture_frames_rx,
        recycle_rx,
        shared.capture_stats.clone(),
    )
    .map_err(|err| {
        shared.control.stop.store(true, Ordering::Relaxed);
        format!("capture failed: {err}")
    })?;

    Ok((session, encoder, info))
}
