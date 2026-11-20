//! Dynamic anti-aliased Win32 system tray icon renderer and shell notification manager.

use std::mem::size_of;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetSystemMetrics, ICONINFO, HICON, SM_CXSMICON,
};

pub const WM_APP_TRAY: u32 = 0x8000 + 1;
const TRAY_ICON_ID: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayVisualState {
    Idle,
    Processing(u8),
    Success,
    Error,
}

impl TrayVisualState {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Idle => "待机",
            Self::Processing(_) => "处理中",
            Self::Success => "完成",
            Self::Error => "出错",
        }
    }
}

pub struct TrayManager {
    hwnd: HWND,
    added: bool,
    idle_icon: HICON,
    processing_icons: [HICON; 4],
    success_icon: HICON,
    error_icon: HICON,
    current_state: TrayVisualState,
}

impl TrayManager {
    pub fn new(hwnd: HWND) -> Self {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON).clamp(16, 64) } as usize;
        let idle_icon = render_state_icon(size, TrayVisualState::Idle);
        let processing_icons = [
            render_state_icon(size, TrayVisualState::Processing(0)),
            render_state_icon(size, TrayVisualState::Processing(1)),
            render_state_icon(size, TrayVisualState::Processing(2)),
            render_state_icon(size, TrayVisualState::Processing(3)),
        ];
        let success_icon = render_state_icon(size, TrayVisualState::Success);
        let error_icon = render_state_icon(size, TrayVisualState::Error);

        let mut mgr = Self {
            hwnd,
            added: false,
            idle_icon,
            processing_icons,
            success_icon,
            error_icon,
            current_state: TrayVisualState::Idle,
        };
        mgr.install();
        mgr
    }

    pub fn current_state(&self) -> TrayVisualState {
        self.current_state
    }

    pub fn set_state(&mut self, state: TrayVisualState) {
        self.current_state = state;
        if !self.added {
            self.install();
            return;
        }
        let mut nid = self.base_nid();
        nid.uFlags = NIF_ICON;
        nid.hIcon = self.icon_for(state);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn install(&mut self) {
        let mut nid = self.base_nid();
        // Deliberately omit NIF_TIP and NIF_INFO so hover tips and popup balloons never interrupt focus.
        nid.uFlags = NIF_MESSAGE | NIF_ICON;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = self.icon_for(self.current_state);
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
                self.added = true;
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &nid);
            }
        }
    }

    fn base_nid(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = TRAY_ICON_ID;
        nid
    }

    fn icon_for(&self, state: TrayVisualState) -> HICON {
        match state {
            TrayVisualState::Idle => self.idle_icon,
            TrayVisualState::Processing(frame) => self.processing_icons[(frame as usize) % 4],
            TrayVisualState::Success => self.success_icon,
            TrayVisualState::Error => self.error_icon,
        }
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        if self.added {
            let nid = self.base_nid();
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }
        unsafe {
            if !self.idle_icon.is_null() {
                DestroyIcon(self.idle_icon);
            }
            for icon in self.processing_icons {
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
            }
            if !self.success_icon.is_null() {
                DestroyIcon(self.success_icon);
            }
            if !self.error_icon.is_null() {
                DestroyIcon(self.error_icon);
            }
        }
    }
}

fn render_state_icon(size: usize, state: TrayVisualState) -> HICON {
    let mut pixels = vec![0u32; size * size];
    let s = size as f32;
    let cx = (s - 1.0) * 0.5;
    let cy = (s - 1.0) * 0.5;
    let radius = s * 0.45;

    let (bg_r, bg_g, bg_b) = match state {
        TrayVisualState::Idle => (30u8, 136u8, 229u8),       // Calm vivid blue
        TrayVisualState::Processing(_) => (245u8, 158u8, 11u8), // Bright amber orange
        TrayVisualState::Success => (16u8, 185u8, 129u8),    // Emerald green
        TrayVisualState::Error => (239u8, 68u8, 68u8),       // Alert red
    };

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32;
            let fy = y as f32;
            let dx = fx - cx;
            let dy = fy - cy;
            let dist = (dx * dx + dy * dy).sqrt();

            // Outer anti-aliased badge circle
            let bg_alpha = (radius + 0.6 - dist).clamp(0.0, 1.0);
            if bg_alpha <= 0.0 {
                continue;
            }

            let fg_cov = match state {
                TrayVisualState::Idle => glyph_idle(dx / radius, dy / radius),
                TrayVisualState::Processing(frame) => {
                    glyph_spinner(dx / radius, dy / radius, frame)
                }
                TrayVisualState::Success => glyph_check(dx / radius, dy / radius),
                TrayVisualState::Error => glyph_cross(dx / radius, dy / radius),
            };

            let (r, g, b) = blend_rgb((bg_r, bg_g, bg_b), (255, 255, 255), fg_cov);
            pixels[y * size + x] = pack_premultiplied_bgra(r, g, b, bg_alpha);
        }
    }

    create_hicon_from_bgra(size, &pixels)
}

fn glyph_idle(nx: f32, ny: f32) -> f32 {
    // Crisp clipboard rectangle outline + top clip bar in normalized [-1, 1] space
    let in_outer = nx.abs() <= 0.46 && ny >= -0.44 && ny <= 0.56;
    let in_inner = nx.abs() <= 0.28 && ny >= -0.24 && ny <= 0.38;
    let clip_bar = nx.abs() <= 0.24 && ny >= -0.58 && ny <= -0.34;
    let center_dot = (nx * nx + (ny - 0.07) * (ny - 0.07)).sqrt() <= 0.14;
    if (in_outer && !in_inner) || clip_bar || center_dot {
        0.95
    } else {
        0.0
    }
}

fn glyph_spinner(nx: f32, ny: f32, frame: u8) -> f32 {
    let r = (nx * nx + ny * ny).sqrt();
    let ring = (1.0 - ((r - 0.56).abs() / 0.17)).clamp(0.0, 1.0);
    if ring <= 0.0 {
        return 0.0;
    }
    let angle = ny.atan2(nx); // [-PI, PI]
    let head = (frame as f32) * std::f32::consts::FRAC_PI_2 - std::f32::consts::FRAC_PI_2;
    let mut diff = angle - head;
    while diff < 0.0 {
        diff += std::f32::consts::TAU;
    }
    while diff >= std::f32::consts::TAU {
        diff -= std::f32::consts::TAU;
    }
    // Draw a ~220-degree rotating arc
    if diff <= 3.85 {
        ring
    } else {
        ring * 0.18
    }
}

fn glyph_check(nx: f32, ny: f32) -> f32 {
    let d1 = dist_to_segment(nx, ny, -0.46, 0.02, -0.10, 0.38);
    let d2 = dist_to_segment(nx, ny, -0.10, 0.38, 0.48, -0.34);
    let d = d1.min(d2);
    (1.0 - (d - 0.12) / 0.10).clamp(0.0, 1.0)
}

fn glyph_cross(nx: f32, ny: f32) -> f32 {
    let d1 = dist_to_segment(nx, ny, -0.38, -0.38, 0.38, 0.38);
    let d2 = dist_to_segment(nx, ny, -0.38, 0.38, 0.38, -0.38);
    let d = d1.min(d2);
    (1.0 - (d - 0.12) / 0.10).clamp(0.0, 1.0)
}

fn dist_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let len_sq = abx * abx + aby * aby;
    let t = if len_sq > 0.0 {
        (((px - ax) * abx + (py - ay) * aby) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cx = ax + t * abx;
    let cy = ay + t * aby;
    ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt()
}

fn blend_rgb(bg: (u8, u8, u8), fg: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let r = (bg.0 as f32 * (1.0 - t) + fg.0 as f32 * t).round() as u8;
    let g = (bg.1 as f32 * (1.0 - t) + fg.1 as f32 * t).round() as u8;
    let b = (bg.2 as f32 * (1.0 - t) + fg.2 as f32 * t).round() as u8;
    (r, g, b)
}

fn pack_premultiplied_bgra(r: u8, g: u8, b: u8, alpha: f32) -> u32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    let pr = ((r as f32) * alpha).round() as u8;
    let pg = ((g as f32) * alpha).round() as u8;
    let pb = ((b as f32) * alpha).round() as u8;
    ((a as u32) << 24) | ((pr as u32) << 16) | ((pg as u32) << 8) | (pb as u32)
}

fn create_hicon_from_bgra(size: usize, pixels: &[u32]) -> HICON {
    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = size as i32;
        bi.bmiHeader.biHeight = -(size as i32); // Top-down DIB
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;

        let hdc = CreateCompatibleDC(std::ptr::null_mut());
        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbm_color = CreateDIBSection(
            hdc,
            &bi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            std::ptr::null_mut(),
            0,
        );
        if !bits_ptr.is_null() && !hbm_color.is_null() {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits_ptr as *mut u32, pixels.len());
        }
        let hbm_mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());

        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&ii);

        if !hbm_color.is_null() {
            DeleteObject(hbm_color);
        }
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask);
        }
        if !hdc.is_null() {
            DeleteDC(hdc);
        }
        hicon
    }
}
