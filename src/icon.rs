//! Render the battery percentage as text into a 32x32 HICON via GDI.
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null_mut, write_bytes};

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GdiFlush, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, HDC, HGDIOBJ, NONANTIALIASED_QUALITY, OUT_DEFAULT_PRECIS,
    TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO,
};

// COLORREF is 0x00BBGGRR
pub const COLOR_NORMAL: u32 = 0x00FFFFFF; // white
pub const COLOR_LOW: u32 = 0x005050FF; // red
pub const COLOR_CHARGING: u32 = 0x0078DC50; // green
pub const COLOR_STALE: u32 = 0x00A0A0A0; // gray

const SIZE: i32 = 32;
const PIXELS: usize = (SIZE * SIZE) as usize;

/// Owned HICON destroyed on drop. Null is a valid "no icon" state.
pub struct Hicon(HICON);

impl Hicon {
    pub fn raw(&self) -> HICON {
        self.0
    }
}

impl Drop for Hicon {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by CreateIconIndirect, is owned
            // solely by this struct, and is destroyed exactly once.
            unsafe { DestroyIcon(self.0) };
        }
    }
}

/// Screen DC + compatible memory DC, released in reverse order on drop.
struct Dcs {
    screen: HDC,
    mem: HDC,
}

impl Dcs {
    fn new() -> Dcs {
        // SAFETY: GetDC(NULL) is always valid; CreateCompatibleDC tolerates a
        // null source. Both are released in Drop.
        unsafe {
            let screen = GetDC(null_mut());
            Dcs {
                screen,
                mem: CreateCompatibleDC(screen),
            }
        }
    }
}

impl Drop for Dcs {
    fn drop(&mut self) {
        // SAFETY: each DC was obtained in `new` and is released exactly once.
        // Both functions are no-ops on null.
        unsafe {
            DeleteDC(self.mem);
            ReleaseDC(null_mut(), self.screen);
        }
    }
}

/// GDI object deleted on drop. `DeleteObject(null)` is a harmless no-op.
struct GdiObj(HGDIOBJ);

impl Drop for GdiObj {
    fn drop(&mut self) {
        // SAFETY: the object is owned by this struct and never selected into a
        // DC at drop time (callers restore the previous object first).
        unsafe { DeleteObject(self.0) };
    }
}

pub fn battery_icon(text: &str, color: u32) -> Hicon {
    let dcs = Dcs::new();
    let hdc = dcs.mem;

    // SAFETY: BITMAPINFO is plain data for which all-zero is valid; `bits` is
    // an out-pointer that CreateDIBSection fills on success.
    let (bmp, bits) = unsafe {
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = SIZE;
        bmi.bmiHeader.biHeight = -SIZE; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let bmp = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        (GdiObj(bmp as HGDIOBJ), bits)
    };
    if bmp.0.is_null() || bits.is_null() {
        return Hicon(null_mut()); // `dcs` released by Drop
    }

    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    // 3 digits ("100") need a smaller face than 1-2 digits.
    let height = if text.chars().count() >= 3 { 18 } else { 26 };
    // SAFETY: `face` is a null-terminated wide string that outlives the call.
    let font = GdiObj(unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            700, // bold
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            NONANTIALIASED_QUALITY as u32,
            0,
            face.as_ptr(),
        ) as HGDIOBJ
    });

    let wtext: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: SIZE,
        bottom: SIZE,
    };

    // SAFETY: `bits` points to a live 32x32x32bpp DIB section (exactly PIXELS
    // u32s) owned by `bmp`, which is selected into `hdc` for the whole block;
    // GdiFlush() orders our direct pixel access after GDI's drawing. The
    // previous bitmap/font are restored before the objects are deleted.
    unsafe {
        let old_bmp = SelectObject(hdc, bmp.0);
        write_bytes(bits as *mut u8, 0, PIXELS * 4);

        let old_font = SelectObject(hdc, font.0);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        DrawTextW(
            hdc,
            wtext.as_ptr(),
            wtext.len() as i32,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        GdiFlush();

        // GDI leaves alpha at 0; make every colored pixel opaque.
        let px = bits as *mut u32;
        for i in 0..PIXELS {
            let p = px.add(i);
            if *p & 0x00FF_FFFF != 0 {
                *p |= 0xFF00_0000;
            }
        }

        SelectObject(hdc, old_font);
        SelectObject(hdc, old_bmp);
    }

    // Windows uses the DIB's alpha channel when any pixel is opaque; give the
    // AND mask defined (all-zero) contents anyway so it never shows garbage.
    let mask_bits = [0u8; PIXELS / 8];
    // SAFETY: `mask_bits` is exactly 32x32 1bpp and CreateBitmap copies it.
    // CreateIconIndirect copies both bitmaps, so they can be deleted (by
    // their GdiObj drops) after it returns.
    let icon = unsafe {
        let mask =
            GdiObj(CreateBitmap(SIZE, SIZE, 1, 1, mask_bits.as_ptr() as *const c_void) as HGDIOBJ);
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask.0 as _,
            hbmColor: bmp.0 as _,
        };
        CreateIconIndirect(&ii)
    };
    Hicon(icon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // Not worth a crate feature for a test; stable Win32 ABI.
    #[link(name = "user32")]
    extern "system" {
        fn GetGuiResources(process: HANDLE, flags: u32) -> u32;
    }
    const GR_GDIOBJECTS: u32 = 0;
    const GR_USEROBJECTS: u32 = 1;

    fn gui_counts() -> (u32, u32) {
        // SAFETY: the pseudo-handle from GetCurrentProcess is always valid.
        unsafe {
            let p = GetCurrentProcess();
            (
                GetGuiResources(p, GR_GDIOBJECTS),
                GetGuiResources(p, GR_USEROBJECTS),
            )
        }
    }

    // Single test touching GDI: object counts are process-wide, so two such
    // tests running on parallel threads would race each other's measurements.
    #[test]
    fn renders_icons_without_leaking_gdi_or_user_objects() {
        for text in ["7", "42", "100", "?", "…"] {
            assert!(!battery_icon(text, COLOR_NORMAL).raw().is_null(), "{text}");
        }
        // Warm up (fonts/DCs may be cached by GDI on first use).
        for _ in 0..5 {
            let _ = battery_icon("50", COLOR_NORMAL);
        }
        let before = gui_counts();
        for i in 0..300u32 {
            let text = (i % 101).to_string();
            let icon = battery_icon(&text, COLOR_LOW);
            assert!(!icon.raw().is_null());
            drop(icon);
        }
        let after = gui_counts();
        assert_eq!(before, after, "(gdi, user) objects grew across 300 renders");
    }
}
