//! Read a bitmap or image file from the OS clipboard.
//!
//! Windows screenshot tools often put PNG or CF_DIB, not the CF_DIBV5-only
//! format arboard looks for. Decode those bytes ourselves.

use std::path::PathBuf;
use std::sync::Mutex;

use image::DynamicImage;

/// One clipboard handle for the whole session. On X11 a copy lives only as
/// long as some handle does: one dropped right after `set` takes the data
/// with it unless a clipboard manager grabbed it first.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

pub fn with_clipboard<T>(f: impl FnOnce(&mut arboard::Clipboard) -> Option<T>) -> Option<T> {
    let mut slot = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = arboard::Clipboard::new().ok();
    }
    f(slot.as_mut()?)
}

/// Drop the session handle on exit, so X11 hands the last copy to a
/// clipboard manager if one runs.
pub fn release_clipboard() {
    CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner()).take();
}

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
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    let colors_used = u32::from_le_bytes(dib[32..36].try_into().ok()?);
    let palette = if bit_count <= 8 && colors_used == 0 {
        1u32 << bit_count
    } else {
        colors_used
    };
    // BI_BITFIELDS after a plain BITMAPINFOHEADER: three color masks follow it.
    let masks = if header_size == 40 && compression == 3 { 12 } else { 0 };
    let off_bits = (14 + header_size)
        .saturating_add(masks)
        .saturating_add(palette.saturating_mul(4));
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

/// Copied image files, including formats the decoders cannot read (the
/// caller reports those).
pub fn read_image_files() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let files = image_files(windows_clipboard_files());
        if !files.is_empty() {
            return files;
        }
    }
    image_files(with_clipboard(|c| c.get().file_list().ok()).unwrap_or_default())
}

/// Existing image files of a copied file list. GTK and Qt write text/uri-list
/// with CRLF line ends, and arboard splits on LF only, so each path may end
/// in `\r`.
fn image_files(list: Vec<PathBuf>) -> Vec<PathBuf> {
    list.into_iter()
        .map(|p| match p.to_str() {
            Some(s) => PathBuf::from(s.trim_end_matches(['\r', '\n', '\0'])),
            None => p,
        })
        .filter(|p| {
            (crate::vision::is_image_ext(p) || crate::vision::is_unsupported_image_ext(p)) && p.is_file()
        })
        .collect()
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn copied_file_lists_lose_their_crlf_and_keep_every_image_format() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        let photo = dir.path().join("photo.webp");
        let heic = dir.path().join("phone.heic");
        let notes = dir.path().join("notes.txt");
        for p in [&shot, &photo, &heic, &notes] {
            std::fs::write(p, "x").unwrap();
        }
        let crlf = |p: &PathBuf| PathBuf::from(format!("{}\r", p.display()));
        let got = image_files(vec![crlf(&shot), crlf(&photo), heic.clone(), notes, dir.path().join("gone.png")]);
        assert_eq!(got, vec![shot, photo, heic]);
    }

    /// 4x1, 32 bpp, BI_BITFIELDS after a 40-byte header: masks, then pixels.
    fn bitfields_dib() -> Vec<u8> {
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&4i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        for mask in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF] {
            dib.extend_from_slice(&mask.to_le_bytes());
        }
        for bgra in [[0, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0], [255, 255, 255, 0]] {
            dib.extend_from_slice(&bgra);
        }
        dib
    }

    #[test]
    fn bitfields_dib_pixels_start_after_the_masks() {
        let img = from_dib_bytes(&bitfields_dib()).expect("decode DIB").to_rgb8();
        let px: Vec<[u8; 3]> = img.pixels().map(|p| p.0).collect();
        assert_eq!(px, [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]);
    }

    #[test]
    fn a_color_table_on_a_true_color_dib_is_skipped() {
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib[32..36].copy_from_slice(&2u32.to_le_bytes());
        dib.extend_from_slice(&[9; 8]);
        dib.extend_from_slice(&[0, 255, 0, 0]);
        let img = from_dib_bytes(&dib).expect("decode DIB").to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 255, 0]);
    }
}

fn arboard_image() -> Option<DynamicImage> {
    let data = with_clipboard(|c| c.get_image().ok())?;
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
