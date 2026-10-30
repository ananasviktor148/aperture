//! Monitor enumeration and geometry helpers.

use std::mem::size_of;

use serde::Serialize;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorInfo {
    #[serde(skip)]
    pub hmonitor: isize,
    pub index: usize,
    pub name: String,
    pub device: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn rect(&self) -> Rect {
        Rect { x: self.x, y: self.y, width: self.width, height: self.height }
    }
}

unsafe extern "system" fn enum_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    unsafe {
        let list = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, (&raw mut info).cast::<windows::Win32::Graphics::Gdi::MONITORINFO>())
            .as_bool()
        {
            return TRUE;
        }
        let device = String::from_utf16_lossy(
            &info.szDevice[..info.szDevice.iter().position(|&c| c == 0).unwrap_or(0)],
        );
        let r = info.monitorInfo.rcMonitor;
        list.push(MonitorInfo {
            hmonitor: monitor.0 as isize,
            index: list.len(),
            name: format!("Display {}", list.len() + 1),
            device,
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
            primary: info.monitorInfo.dwFlags & 1 != 0,
        });
        TRUE
    }
}

pub fn enumerate() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_monitor), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| m.index);
    list
}

pub fn virtual_screen() -> Rect {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}
