//! Read a bitmap or image file from the OS clipboard.
//!
//! Windows screenshot tools often put PNG or CF_DIB, not the CF_DIBV5-only
//! format arboard looks for. Decode those bytes ourselves.

use std::path::PathBuf;

use image::DynamicImage;

pub fn from_png_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    image::load_from_memory(bytes).ok()
}

/// Turn a packed Windows DIB (BITMAPINFOHEADER + pixels) into an image.
pub fn from_dib_bytes(dib: &[u8]) -> Option<DynamicImage> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(dib[0..4].try_into().ok()?);
    if header_size < 40 || header_size as usize > dib.len() {
        return None;
    }
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().ok()?);
    let colors_used = u32::from_le_bytes(dib[32..36].try_into().ok()?);
    let palette = if bit_count <= 8 {
        if colors_used == 0 {
            1u32 << bit_count
        } else {
            colors_used
        }
    } else {
        0
    };
    let off_bits = 14u32 + header_size + palette.saturating_mul(4);
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(14u32 + dib.len() as u32).to_le_bytes());
    bmp.extend_from_slice(&[0u8; 4]);
    bmp.extend_from_slice(&off_bits.to_le_bytes());
    bmp.extend_from_slice(dib);
    image::load_from_memory(&bmp).ok()
}

pub fn read_image() -> Option<DynamicImage> {
    #[cfg(windows)]
    {
        if let Some(img) = windows_clipboard_image() {
            return Some(img);
        }
    }
    arboard_image()
}

pub fn read_image_files() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let files = windows_clipboard_files();
        if !files.is_empty() {
            return files
                .into_iter()
                .filter(|p| p.is_file() && crate::vision::is_image_ext(p))
                .collect();
        }
    }
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get().file_list().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_file() && crate::vision::is_image_ext(p))
        .collect()
}

fn arboard_image() -> Option<DynamicImage> {
    let data = arboard::Clipboard::new().ok()?.get_image().ok()?;
    crate::vision::from_rgba(
        data.width as u32,
        data.height as u32,
        data.bytes.into_owned(),
    )
    .ok()
}

#[cfg(windows)]
fn windows_clipboard_image() -> Option<DynamicImage> {
    let _clip = open_clipboard()?;
    if let Some(id) = clipboard_format("PNG") {
        if let Some(bytes) = clipboard_bytes(id) {
            if let Some(img) = from_png_bytes(&bytes) {
                return Some(img);
            }
        }
    }
    if let Some(id) = clipboard_format("image/png") {
        if let Some(bytes) = clipboard_bytes(id) {
            if let Some(img) = from_png_bytes(&bytes) {
                return Some(img);
            }
        }
    }
    const CF_DIBV5: u32 = 17;
    const CF_DIB: u32 = 8;
    for fmt in [CF_DIBV5, CF_DIB] {
        if let Some(bytes) = clipboard_bytes(fmt) {
            if let Some(img) = from_dib_bytes(&bytes) {
                return Some(img);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_clipboard_files() -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    const CF_HDROP: u32 = 15;
    let _clip = match open_clipboard() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let Some(h) = clipboard_handle(CF_HDROP) else {
        return Vec::new();
    };
    unsafe {
        let n = DragQueryFileW(h.0, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
        let mut out = Vec::new();
        for i in 0..n {
            let len = DragQueryFileW(h.0, i, std::ptr::null_mut(), 0);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let wrote = DragQueryFileW(h.0, i, buf.as_mut_ptr(), buf.len() as u32);
            if wrote == 0 {
                continue;
            }
            let os = std::ffi::OsString::from_wide(&buf[..wrote as usize]);
            out.push(PathBuf::from(os));
        }
        out
    }
}

#[cfg(windows)]
struct ClipboardGuard;

#[cfg(windows)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

#[cfg(windows)]
fn open_clipboard() -> Option<ClipboardGuard> {
    use windows_sys::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..12 {
        let ok = unsafe { OpenClipboard(std::ptr::null_mut()) };
        if ok != 0 {
            return Some(ClipboardGuard);
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    None
}

#[cfg(windows)]
fn clipboard_format(name: &str) -> Option<u32> {
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    wide.push(0);
    let id = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
    if id == 0 {
        None
    } else {
        Some(id)
    }
}

#[cfg(windows)]
struct Handle(*mut core::ffi::c_void);

#[cfg(windows)]
fn clipboard_handle(fmt: u32) -> Option<Handle> {
    use windows_sys::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};
    unsafe {
        if IsClipboardFormatAvailable(fmt) == 0 {
            return None;
        }
        let h = GetClipboardData(fmt);
        if h.is_null() {
            None
        } else {
            Some(Handle(h))
        }
    }
}

#[cfg(windows)]
fn clipboard_bytes(fmt: u32) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    let h = clipboard_handle(fmt)?;
    unsafe {
        let ptr = GlobalLock(h.0);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(h.0);
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        GlobalUnlock(h.0);
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_dib_2x2() -> Vec<u8> {
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&16u32.to_le_bytes());
        // bottom-up: last row first. BGRA.
        // row y=0 (bottom in DIB = top in image after decoder flip...):
        // BMP bottom-up: first pixel block is bottom row.
        // bottom row: red, green
        dib[40..44].copy_from_slice(&[0, 0, 255, 255]);
        dib[44..48].copy_from_slice(&[0, 255, 0, 255]);
        // top row: blue, white
        dib[48..52].copy_from_slice(&[255, 0, 0, 255]);
        dib[52..56].copy_from_slice(&[255, 255, 255, 255]);
        dib
    }

    #[test]
    fn from_dib_bytes_reads_2x2() {
        let img = from_dib_bytes(&packed_dib_2x2()).expect("decode DIB");
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 2);
        let rgba = img.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [0, 0, 255, 255], "top-left");
        assert_eq!(rgba.get_pixel(1, 0).0, [255, 255, 255, 255], "top-right");
        assert_eq!(rgba.get_pixel(0, 1).0, [255, 0, 0, 255], "bottom-left");
        assert_eq!(rgba.get_pixel(1, 1).0, [0, 255, 0, 255], "bottom-right");
    }

    #[test]
    fn from_png_bytes_roundtrip() {
        let src = crate::vision::from_rgba(2, 2, vec![255, 0, 0, 255].repeat(4)).unwrap();
        let mut png = Vec::new();
        src.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let got = from_png_bytes(&png).expect("png");
        assert_eq!(got.width(), 2);
        assert_eq!(got.height(), 2);
        assert_eq!(got.to_rgba8().get_pixel(0, 0).0[0], 255);
    }

    #[test]
    fn from_dib_rejects_truncated() {
        assert!(from_dib_bytes(&[0u8; 10]).is_none());
        assert!(from_dib_bytes(&[]).is_none());
    }
}
