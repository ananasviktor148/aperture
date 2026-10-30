//! Encoding pipeline: frame queue -> Media Foundation -> H.264 packets.
//!
//! The encoder thread never queue-builds: it only feeds the MFT when the MFT
//! asks for input and always processes the freshest captured frame, so latency
//! stays bounded under load.

pub mod mf;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::broadcast;

use mf::{MfEncoder, MfEvent};

pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub hardware: bool,
}

pub struct FrameIn {
    pub nv12: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub capture_us: u64,
}

pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_us: u64,
    pub encode_us: u32,
    pub width: u32,
    pub height: u32,
}

pub struct EncoderControl {
    pub stop: AtomicBool,
    pub force_keyframe: AtomicBool,
    /// False when no viewer is connected: frames are tracked but not encoded.
    pub active: AtomicBool,
    pub bitrate: AtomicU32,
    pub last_encode_us: AtomicU32,
    pub encoded_frames: AtomicU64,
}

impl EncoderControl {
    pub fn new(bitrate: u32) -> Self {
        Self {
            stop: AtomicBool::new(false),
            force_keyframe: AtomicBool::new(true),
            active: AtomicBool::new(false),
            bitrate: AtomicU32::new(bitrate),
            last_encode_us: AtomicU32::new(0),
            encoded_frames: AtomicU64::new(0),
        }
    }
}

pub struct EncoderInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Spawns the encoder thread. Frames arrive via `frames` (capacity-1 channel
/// with overwrite semantics), encoded packets leave via `out`.
pub fn spawn(
    settings: EncoderSettings,
    frames: Receiver<FrameIn>,
    recycle: Sender<Vec<u8>>,
    out: broadcast::Sender<Arc<EncodedPacket>>,
    control: Arc<EncoderControl>,
    ready: StdSender<Result<EncoderInfo, String>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("aperture-encoder".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(settings, frames, recycle, out, control, &ready)
            }));
            if let Err(panic) = result {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "encoder panicked".into());
                tracing::error!(%msg, "encoder thread panicked");
                let _ = ready.send(Err(msg));
            }
        })
        .expect("spawn encoder thread")
}

struct MfGuard;

impl MfGuard {
    fn new() -> Result<Self, String> {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
            windows::Win32::Media::MediaFoundation::MFStartup(
                windows::Win32::Media::MediaFoundation::MF_VERSION,
                windows::Win32::Media::MediaFoundation::MFSTARTUP_NOSOCKET,
            )
            .map_err(|e| format!("MFStartup failed: {e}"))?;
        }
        Ok(MfGuard)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Media::MediaFoundation::MFShutdown();
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

fn run(
    settings: EncoderSettings,
    frames: Receiver<FrameIn>,
    recycle: Sender<Vec<u8>>,
    out: broadcast::Sender<Arc<EncodedPacket>>,
    control: Arc<EncoderControl>,
    ready: &StdSender<Result<EncoderInfo, String>>,
) {
    let _mf = match MfGuard::new() {
        Ok(guard) => guard,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let mut encoder = match MfEncoder::new(
        settings.width,
        settings.height,
        settings.fps,
        settings.bitrate,
        settings.hardware,
    ) {
        Ok(encoder) => encoder,
        Err(err) => {
            let _ = ready.send(Err(format!("encoder init failed: {err}")));
            return;
        }
    };

    let _ = ready.send(Ok(EncoderInfo {
        name: encoder.name().to_string(),
        width: settings.width,
        height: settings.height,
    }));

    let (width, height) = (settings.width, settings.height);
    let async_mft = encoder.is_async();
    // Async MFTs must only be fed after they raise NeedInput.
    let mut need_input = !async_mft;
    let mut has_fed = false;
    let mut last_frame: Option<FrameIn> = None;
    let mut last_ts: i64 = 0;
    let mut feed_started: Option<Instant> = None;
    let mut pending_capture_us: u64 = 0;
    let mut cached_sps: Vec<u8> = Vec::new();
    let mut cached_pps: Vec<u8> = Vec::new();
    let mut applied_bitrate = settings.bitrate;
    let started = Instant::now();

    loop {
        if control.stop.load(Ordering::Relaxed) {
            break;
        }

        let target_bitrate = control.bitrate.load(Ordering::Relaxed);
        if target_bitrate != applied_bitrate {
            applied_bitrate = target_bitrate;
            let _ = encoder.set_bitrate(target_bitrate);
        }

        let mut fed = false;
        if !async_mft || need_input {
            match frames.recv_timeout(Duration::from_micros(500)) {
                Ok(frame) => {
                    if control.active.load(Ordering::Relaxed) {
                        if control.force_keyframe.swap(false, Ordering::Relaxed) {
                            let _ = encoder.request_keyframe();
                        }
                        let mut ts = started.elapsed().as_micros() as i64 * 10;
                        if ts <= last_ts {
                            ts = last_ts + 1;
                        }
                        match encoder.feed(&frame.nv12, ts) {
                            Ok(()) => {
                                last_ts = ts;
                                fed = true;
                                has_fed = true;
                                feed_started = Some(Instant::now());
                                pending_capture_us = frame.capture_us;
                            }
                            Err(err) => tracing::debug!(%err, "ProcessInput rejected frame"),
                        }
                        if async_mft {
                            need_input = false;
                        }
                    }
                    if let Some(old) = last_frame.replace(frame) {
                        let _ = recycle.try_send(old.nv12);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Static screen: re-encode the last frame on demand so a
                    // newly connected client still receives a keyframe.
                    if control.force_keyframe.load(Ordering::Relaxed) {
                        if let Some(previous) = &last_frame {
                            let mut still = FrameIn {
                                nv12: previous.nv12.clone(),
                                width: previous.width,
                                height: previous.height,
                                capture_us: started.elapsed().as_micros() as u64,
                            };
                            if control.force_keyframe.swap(false, Ordering::Relaxed) {
                                let _ = encoder.request_keyframe();
                            }
                            let mut ts = started.elapsed().as_micros() as i64 * 10;
                            if ts <= last_ts {
                                ts = last_ts + 1;
                            }
                            if encoder.feed(&still.nv12, ts).is_ok() {
                                last_ts = ts;
                                fed = true;
                                has_fed = true;
                                feed_started = Some(Instant::now());
                                pending_capture_us = still.capture_us;
                            }
                            still.nv12.clear();
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        if async_mft {
            let mut events = 0;
            while let Some(event) = encoder.poll_event().ok().flatten() {
                events += 1;
                match event {
                    MfEvent::NeedInput => need_input = true,
                    MfEvent::HaveOutput => {
                        if has_fed {
                            drain(
                                &mut encoder,
                                &mut feed_started,
                                &mut pending_capture_us,
                                width,
                                height,
                                &mut cached_sps,
                                &mut cached_pps,
                                &out,
                                &control,
                            );
                        }
                    }
                    _ => {}
                }
            }
            if !fed && events == 0 {
                thread::sleep(Duration::from_micros(300));
            }
        } else if fed {
            drain(
                &mut encoder,
                &mut feed_started,
                &mut pending_capture_us,
                width,
                height,
                &mut cached_sps,
                &mut cached_pps,
                &out,
                &control,
            );
        }
    }

    encoder.flush();
    if let Some(frame) = last_frame.take() {
        let _ = recycle.try_send(frame.nv12);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain(
    encoder: &mut MfEncoder,
    feed_started: &mut Option<Instant>,
    capture_us: &mut u64,
    width: u32,
    height: u32,
    sps: &mut Vec<u8>,
    pps: &mut Vec<u8>,
    out: &broadcast::Sender<Arc<EncodedPacket>>,
    control: &EncoderControl,
) {
    loop {
        match encoder.pull_output() {
            Ok(Some(bytes)) => {
                let encode_us = feed_started
                    .take()
                    .map(|t| t.elapsed().as_micros() as u32)
                    .unwrap_or(0);
                control.last_encode_us.store(encode_us, Ordering::Relaxed);
                control.encoded_frames.fetch_add(1, Ordering::Relaxed);
                emit(bytes, *capture_us, encode_us, width, height, sps, pps, out);
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(%err, "ProcessOutput failed");
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit(
    bytes: Vec<u8>,
    capture_us: u64,
    encode_us: u32,
    width: u32,
    height: u32,
    sps: &mut Vec<u8>,
    pps: &mut Vec<u8>,
    out: &broadcast::Sender<Arc<EncodedPacket>>,
) {
    let annexb = normalize_to_annex_b(bytes);
    let mut keyframe = false;
    let mut has_sps = false;
    for nal in nal_units(&annexb) {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1f {
            5 => keyframe = true,
            7 => {
                has_sps = true;
                *sps = with_start_code(nal);
            }
            8 => *pps = with_start_code(nal),
            _ => {}
        }
    }

    let mut data = Vec::with_capacity(annexb.len() + sps.len() + pps.len());
    if keyframe && !has_sps && !sps.is_empty() {
        data.extend_from_slice(sps);
        data.extend_from_slice(pps);
    }
    data.extend_from_slice(&annexb);

    let _ = out.send(Arc::new(EncodedPacket {
        data,
        keyframe,
        capture_us,
        encode_us,
        width,
        height,
    }));
}

fn with_start_code(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len() + 4);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
    out
}

pub fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// Some MFTs emit AVCC (length-prefixed) samples; WebCodecs wants Annex B.
fn normalize_to_annex_b(data: Vec<u8>) -> Vec<u8> {
    if is_annex_b(&data) || data.len() < 4 {
        return data;
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        if len == 0 || offset + len > data.len() {
            return data;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[offset..offset + len]);
        offset += len;
    }
    out
}

/// Iterates NAL units (start codes stripped) from an Annex B buffer.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = None;
    let mut pos = 0usize;
    let mut spans = Vec::new();
    while pos + 3 <= data.len() {
        let sc3 = data[pos] == 0 && data[pos + 1] == 0 && data[pos + 2] == 1;
        let sc4 = pos + 4 <= data.len()
            && data[pos] == 0
            && data[pos + 1] == 0
            && data[pos + 2] == 0
            && data[pos + 3] == 1;
        let sc_len = if sc4 { 4 } else if sc3 { 3 } else { 0 };
        if sc_len > 0 {
            if let Some(s) = start {
                spans.push((s, pos));
            }
            start = Some(pos + sc_len);
            pos += sc_len;
            continue;
        }
        pos += 1;
    }
    if let Some(s) = start {
        if s < data.len() {
            spans.push((s, data.len()));
        }
    }
    spans.into_iter().map(move |(a, b)| &data[a..b])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let data = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 1, 2];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[2], &[0x65, 1, 2]);
    }

    #[test]
    fn converts_avcc() {
        let data = [0, 0, 0, 2, 0x67, 0x42, 0, 0, 0, 1, 0x65];
        let out = normalize_to_annex_b(data.to_vec());
        assert_eq!(out, vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x65]);
    }
}
