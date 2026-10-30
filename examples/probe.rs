use std::error::Error;
use std::ptr;
use std::time::Instant;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::ImageFormat;
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

fn list_encoders(flags: MFT_ENUM_FLAG, label: &str) -> windows::core::Result<usize> {
    let in_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&in_info),
            Some(&out_info),
            &mut activates,
            &mut count,
        )?;
        println!("[{label}] count={count}");
        for entry in std::slice::from_raw_parts(activates, count as usize) {
            let Some(activate) = entry else { continue };
            let mut name = windows::core::PWSTR::null();
            let mut len = 0u32;
            let mut name_str = String::new();
            if activate
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                .is_ok()
            {
                name_str = name.to_string().unwrap_or_default();
            }
            let clsid = activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).unwrap_or_default();
            println!(
                "  - {name_str} ({clsid:?}) hardware={} async={}",
                activate.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute).unwrap_or(0),
                activate.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0)
            );
            if !name.is_null() {
                CoTaskMemFree(Some(name.0 as _));
            }
        }
        CoTaskMemFree(Some(activates as _));
    }
    Ok(count as usize)
}

struct Grab;

impl GraphicsCaptureApiHandler for Grab {
    type Flags = ();
    type Error = Box<dyn Error + Send + Sync>;

    fn new(_ctx: Context<()>) -> Result<Self, Self::Error> {
        Ok(Grab)
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let t0 = Instant::now();
        println!(
            "frame: {}x{} cursor_in_frame={} format={:?}",
            frame.width(),
            frame.height(),
            true,
            frame.color_format()
        );
        frame.save_as_image("target/probe.png", ImageFormat::Png)?;
        println!("saved target/probe.png in {:?}", t0.elapsed());
        control.stop();
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
        list_encoders(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, "hardware")?;
        list_encoders(MFT_ENUM_FLAG_ALL, "all")?;
        let _ = MFShutdown();
    }

    let settings = Settings::new(
        Monitor::primary()?,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (),
    );

    let control = Grab::start_free_threaded(settings)?;
    control.wait()?;
    println!("capture ok");
    Ok(())
}
