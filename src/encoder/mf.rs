//! Minimal Media Foundation H.264 encoder wrapper.
//!
//! Uses hardware encoder MFTs (NVENC / Quick Sync / AMF) when present and falls
//! back to the Microsoft software H.264 encoder. Configured for latency: CBR,
//! no B-frames, no lookahead, 1-second GOP.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::slice;

use windows::core::{Interface, Result as WinResult};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VT_BOOL, VT_UI4};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfEvent {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Other,
}

pub struct MfEncoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    async_mft: bool,
    provides_samples: bool,
    out_sample: Option<IMFSample>,
    frame_bytes: u32,
    frame_duration: i64,
    name: String,
}

fn pack_ratio(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn variant_u32(value: u32) -> VARIANT {
    let mut inner = VARIANT_0_0::default();
    inner.vt = VT_UI4;
    inner.Anonymous.ulVal = value;
    let mut var = VARIANT::default();
    var.Anonymous = VARIANT_0 { Anonymous: std::mem::ManuallyDrop::new(inner) };
    var
}

fn variant_bool(value: bool) -> VARIANT {
    let mut inner = VARIANT_0_0::default();
    inner.vt = VT_BOOL;
    inner.Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    let mut var = VARIANT::default();
    var.Anonymous = VARIANT_0 { Anonymous: std::mem::ManuallyDrop::new(inner) };
    var
}

impl MfEncoder {
    /// Creates and fully negotiates an encoder MFT.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, hardware: bool) -> WinResult<Self> {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut attempts: Vec<MFT_ENUM_FLAG> = Vec::new();
        if hardware {
            attempts.push(MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0));
        }
        attempts.push(MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_ASYNCMFT.0 | MFT_ENUM_FLAG_HARDWARE.0,
        ));

        let mut last_error = None;
        for flags in attempts {
            for activate in enumerate(&input_info, &output_info, flags)? {
                let name = activate_name(&activate);
                match Self::from_activate(&activate, width, height, fps, bitrate) {
                    Ok(encoder) => {
                        tracing::info!(encoder = %name, "Media Foundation encoder ready");
                        return Ok(encoder);
                    }
                    Err(err) => {
                        tracing::warn!(encoder = %name, %err, "encoder candidate rejected");
                        last_error = Some(err);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            "no H.264 encoder MFT available",
        )))
    }

    fn from_activate(
        activate: &IMFActivate,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> WinResult<Self> {
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };
        let attrs = unsafe { transform.GetAttributes()? };
        let async_mft = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 };

        if async_mft {
            // Must be set before any type negotiation happens.
            unsafe {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
        }
        unsafe {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        let output_type = unsafe { MFCreateMediaType()? };
        unsafe {
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
            output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))?;
            output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
            output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let _ = output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
        }
        unsafe { transform.SetOutputType(0, &output_type, 0)? };

        let codec: Option<ICodecAPI> = transform.cast().ok();
        if let Some(codec) = &codec {
            set_u32(codec, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
            set_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate);
            set_u32(codec, &CODECAPI_AVEncCommonMaxBitRate, bitrate);
            set_u32(codec, &CODECAPI_AVEncCommonBufferSize, bitrate / 4);
            set_u32(codec, &CODECAPI_AVEncMPVGOPSize, fps.max(1));
            set_u32(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            set_bool(codec, &CODECAPI_AVLowLatencyMode, true);
        }

        let input_type = unsafe { MFCreateMediaType()? };
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
            input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))?;
            input_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
            input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        unsafe { transform.SetInputType(0, &input_type, 0)? };

        let stream_info = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides_samples = stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let out_sample = if provides_samples {
            None
        } else {
            Some(new_sample(stream_info.cbSize.max(width * height * 3 / 2))?)
        };

        let events: Option<IMFMediaEventGenerator> = transform.cast().ok();

        let mut encoder = Self {
            transform,
            codec,
            events,
            async_mft,
            provides_samples,
            out_sample,
            frame_bytes: width * height * 3 / 2,
            frame_duration: 10_000_000 / fps.max(1) as i64,
            name: activate_name(activate),
        };
        encoder.start()?;
        Ok(encoder)
    }

    fn start(&mut self) -> WinResult<()> {
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_async(&self) -> bool {
        self.async_mft
    }

    pub fn feed(&mut self, nv12: &[u8], timestamp: i64) -> WinResult<()> {
        debug_assert_eq!(nv12.len(), self.frame_bytes as usize);
        let sample = new_sample(self.frame_bytes)?;
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let mut dst: *mut u8 = ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            ptr::copy_nonoverlapping(nv12.as_ptr(), dst, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.frame_bytes)?;
            sample.SetSampleTime(timestamp)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        Ok(())
    }

    /// Polls for the next MFT event without blocking. Returns `None` if the
    /// queue is empty or the transform is synchronous.
    pub fn poll_event(&self) -> WinResult<Option<MfEvent>> {
        let Some(events) = &self.events else {
            return Ok(None);
        };
        match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                let kind = unsafe { event.GetType()? };
                Ok(Some(if kind == METransformNeedInput.0 as u32 {
                    MfEvent::NeedInput
                } else if kind == METransformHaveOutput.0 as u32 {
                    MfEvent::HaveOutput
                } else if kind == METransformDrainComplete.0 as u32 {
                    MfEvent::DrainComplete
                } else {
                    MfEvent::Other
                }))
            }
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Pulls one encoded sample. `Ok(None)` means "need more input" (or a
    /// benign stream change was handled).
    pub fn pull_output(&mut self) -> WinResult<Option<Vec<u8>>> {
        let mut data = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(self.out_sample.take()),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut data), &mut status)
        };

        let sample = unsafe { ManuallyDrop::take(&mut data.pSample) };
        let events = unsafe { ManuallyDrop::take(&mut data.pEvents) };
        drop(events);

        match result {
            Ok(()) => {
                let Some(sample) = sample else {
                    return Ok(None);
                };
                let bytes = unsafe { sample_bytes(&sample)? };
                if !self.provides_samples {
                    self.out_sample = Some(new_sample(self.frame_bytes)?);
                }
                Ok(Some(bytes))
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                if !self.provides_samples {
                    self.out_sample = sample;
                }
                Ok(None)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if !self.provides_samples {
                    self.out_sample = sample;
                }
                self.renegotiate_output()?;
                Ok(None)
            }
            Err(err) if err.code() == windows::Win32::Foundation::E_UNEXPECTED => {
                // The NVIDIA MFT occasionally raises a spurious HaveOutput
                // event before a sample is ready; retry on the next event.
                if !self.provides_samples {
                    self.out_sample = sample;
                }
                tracing::debug!("encoder output not ready yet");
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    fn renegotiate_output(&mut self) -> WinResult<()> {
        unsafe {
            let available = self.transform.GetOutputAvailableType(0, 0)?;
            self.transform.SetOutputType(0, &available, 0)?;
        }
        Ok(())
    }

    pub fn request_keyframe(&mut self) -> WinResult<()> {
        if let Some(codec) = &self.codec {
            set_u32(codec, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }
        Ok(())
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> WinResult<()> {
        if let Some(codec) = &self.codec {
            set_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate);
            set_u32(codec, &CODECAPI_AVEncCommonMaxBitRate, bitrate);
        }
        Ok(())
    }

    pub fn flush(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}

fn new_sample(len: u32) -> WinResult<IMFSample> {
    unsafe {
        let sample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(len)?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

unsafe fn sample_bytes(sample: &IMFSample) -> WinResult<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let len = buffer.GetCurrentLength()?;
        let mut src: *mut u8 = ptr::null_mut();
        buffer.Lock(&mut src, None, None)?;
        let bytes = slice::from_raw_parts(src, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(bytes)
    }
}

fn enumerate(
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
    flags: MFT_ENUM_FLAG,
) -> WinResult<Vec<IMFActivate>> {
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(input),
            Some(output),
            &mut activates,
            &mut count,
        )?;
    }
    if activates.is_null() {
        return Ok(Vec::new());
    }
    let list = unsafe { slice::from_raw_parts(activates, count as usize).to_vec() };
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const c_void));
    }
    Ok(list.into_iter().flatten().collect())
}

fn activate_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut name = windows::core::PWSTR::null();
        let mut len = 0u32;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
            .is_ok()
        {
            let out = name.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const c_void));
            return out;
        }
    }
    "unknown encoder".into()
}

fn set_u32(codec: &ICodecAPI, api: &windows::core::GUID, value: u32) {
    let var = variant_u32(value);
    unsafe {
        if let Err(err) = codec.SetValue(api, &var) {
            tracing::debug!(%err, "codec setting rejected");
        }
    }
}

fn set_bool(codec: &ICodecAPI, api: &windows::core::GUID, value: bool) {
    let var = variant_bool(value);
    unsafe {
        if let Err(err) = codec.SetValue(api, &var) {
            tracing::debug!(%err, "codec setting rejected");
        }
    }
}
