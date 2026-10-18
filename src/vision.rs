//! Prepare workspace images for Grok vision (`input_image` on the Responses API).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::metadata::Orientation;
use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageReader, RgbImage, RgbaImage};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::tools::resolve_in_workspace;

const MAX_EDGE: u32 = 1280;
/// xAI `invalid_image` if width×height is below this.
pub const MIN_VISION_PIXELS: u32 = 512;
const JPEG_QUALITY: u8 = 82;
const MAX_BYTES: usize = 8 * 1024 * 1024;

pub fn below_vision_min(width: u32, height: u32) -> bool {
    width.saturating_mul(height) < MIN_VISION_PIXELS
}

pub fn enlarge_yourself_note(width: u32, height: u32) -> String {
    format!(
        "This image is {width}x{height} ({} pixels). xAI vision rejects images below {MIN_VISION_PIXELS} pixels, so pixels were not attached. Enlarge or regenerate the file in the workspace (nearest-neighbor scale is fine), then read_image again. Do not ask the user to upscale it.",
        width.saturating_mul(height)
    )
}

pub fn input_has_image(items: &[Value]) -> bool {
    items.iter().any(value_has_image)
}

fn value_has_image(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) == Some("input_image") {
        return true;
    }
    if let Some(arr) = v.as_array() {
        return arr.iter().any(value_has_image);
    }
    if let Some(obj) = v.as_object() {
        return obj.values().any(value_has_image);
    }
    false
}

/// If the tool JSON asks to attach an image, load it as a JPEG data URI.
pub fn data_uri_for_tool(workspace: &Path, output: &str) -> Result<Option<String>> {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return Ok(None);
    };
    if v.get("attach_image").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let path = v
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool("attach_image missing path".into()))?;
    let resolved = resolve_in_workspace(workspace, path)?;
    Ok(Some(data_uri_from_file(&resolved)?))
}

pub fn function_call_output(call_id: &str, output: &str, image_uri: Option<&str>) -> Value {
    match image_uri {
        Some(uri) => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": [
                {"type": "input_text", "text": output},
                {"type": "input_image", "image_url": uri, "detail": "high"}
            ]
        }),
        None => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }),
    }
}

const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_USER_IMAGES: usize = 4;
/// Newest image-bearing items stay on the wire; older ones drop to text.
/// Token pressure is the 50% context compact, not this cap.
pub const KEEP_RECENT_IMAGES: usize = 10;
/// Completes that include pixels before they expire. First look + five working turns.
pub const IMAGE_KEEP_TURNS: u32 = 6;

pub fn image_item_count(items: &[Value]) -> usize {
    items.iter().filter(|item| value_has_image(item)).count()
}

/// Drop every attached image. Used on provider errors and when the keep window ends.
pub fn strip_attached_images(items: &mut [Value]) {
    retain_recent_images(items, 0);
}

/// Keep the newest `keep` image items; strip the rest to text.
pub fn retain_recent_images(items: &mut [Value], keep: usize) {
    let idxs: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| value_has_image(item))
        .map(|(i, _)| i)
        .collect();
    if idxs.len() <= keep {
        return;
    }
    for i in idxs.iter().take(idxs.len() - keep).copied() {
        strip_one_item_images(&mut items[i]);
    }
}

/// After a complete that included images, expire or cap them.
pub fn prune_attached_images(items: &mut [Value], turns_seen: u32) {
    if turns_seen >= IMAGE_KEEP_TURNS {
        strip_attached_images(items);
    } else {
        retain_recent_images(items, KEEP_RECENT_IMAGES);
    }
}

fn strip_one_item_images(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
        let Some(arr) = item.get("output").and_then(Value::as_array).cloned() else {
            return;
        };
        if !arr
            .iter()
            .any(|p| p.get("type").and_then(Value::as_str) == Some("input_image"))
        {
            return;
        }
        let text = arr
            .iter()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
            .and_then(|p| p.get("text").and_then(Value::as_str))
            .unwrap_or("");
        item["output"] = Value::String(text.to_string());
        return;
    }
    if item.get("role").and_then(Value::as_str) == Some("user") {
        strip_user_content_images(item);
    }
}

fn strip_user_content_images(item: &mut Value) {
    let Some(arr) = item.get("content").and_then(Value::as_array).cloned() else {
        return;
    };
    let n = arr
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_image"))
        .count();
    if n == 0 {
        return;
    }
    let mut text = arr
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let note = if n == 1 {
        "(1 張圖片)".to_string()
    } else {
        format!("({n} 張圖片)")
    };
    if text.trim().is_empty() {
        text = note;
    } else {
        text = format!("{text}\n{note}");
    }
    item["content"] = Value::String(text);
}

/// User turn for the Responses API: plain string, or text + `input_image` parts.
pub fn user_message(text: &str, images: &[PathBuf], workspace: &Path) -> Value {
    if images.is_empty() {
        return json!({"role": "user", "content": text});
    }
    let mut parts = Vec::new();
    if !text.trim().is_empty() {
        parts.push(json!({"type": "input_text", "text": text}));
    }
    for p in images {
        let rel = p.to_string_lossy();
        match resolve_in_workspace(workspace, &rel).and_then(|abs| data_uri_from_file(&abs)) {
            Ok(uri) => parts.push(json!({
                "type": "input_image",
                "image_url": uri,
                "detail": "high"
            })),
            Err(e) => {
                parts.push(json!({
                    "type": "input_text",
                    "text": format!("({rel}: {e})")
                }));
            }
        }
    }
    if parts.is_empty() {
        json!({"role": "user", "content": text})
    } else {
        json!({"role": "user", "content": parts})
    }
}

/// Extensions of the formats the enabled `image` decoders can read.
pub(crate) fn is_image_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "jfif" | "png" | "gif" | "webp" | "bmp")
    )
}

/// Decode an image by its content, not its extension, and apply its EXIF
/// orientation so phone photos come out upright.
pub fn open_image(path: &Path) -> Result<DynamicImage> {
    let mut dec = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| Error::Tool(format!("open image: {e}")))?
        .into_decoder()
        .map_err(|e| Error::Tool(format!("open image: {e}")))?;
    let orientation = dec.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| Error::Tool(format!("open image: {e}")))?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Width and height as `open_image` would return them, read from the header only.
pub fn image_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut dec = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| Error::Tool(format!("open image: {e}")))?
        .into_decoder()
        .map_err(|e| Error::Tool(format!("open image: {e}")))?;
    let (w, h) = dec.dimensions();
    let swapped = matches!(
        dec.orientation().unwrap_or(Orientation::NoTransforms),
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    );
    Ok(if swapped { (h, w) } else { (w, h) })
}

/// Composite transparent pixels onto `bg`. Dropping alpha instead would turn
/// them into whatever RGB they store, usually black.
pub fn flatten_alpha(img: &DynamicImage, bg: [u8; 3]) -> DynamicImage {
    if !img.color().has_alpha() {
        return img.clone();
    }
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        for c in 0..3 {
            o[c] = ((p[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8;
        }
    }
    DynamicImage::ImageRgb8(out)
}

/// `<workspace>/.groka/<sub>`, created on demand. `.groka/` gets a `*`
/// .gitignore so screenshots and pasted images never land in a commit.
pub fn groka_dir(workspace: &Path, sub: &str) -> Result<PathBuf> {
    let root = workspace.join(".groka");
    let dir = root.join(sub);
    std::fs::create_dir_all(&dir)?;
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    Ok(dir)
}

/// Image files a terminal pasted for a file drop: one path per line, or
/// shell-quoted words, `file://` URIs included. Empty unless every entry is
/// an absolute path to an existing image file, so prose or code that only
/// mentions a path stays text. Formats the decoders cannot read are kept for
/// the caller to report (see `is_unsupported_image_ext`).
pub fn parse_image_drop(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()) {
        match drop_line(line) {
            Some(paths) => out.extend(paths),
            None => return Vec::new(),
        }
    }
    out
}

/// Image formats people drop or copy that the enabled decoders cannot read.
/// SVG is not one: it is source text, and a pasted path to it stays text.
pub(crate) fn is_unsupported_image_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("heic" | "heif" | "avif" | "tif" | "tiff" | "ico" | "jxl" | "psd")
    )
}

/// One pasted line: a single path as is (unquoted spaces and all), or shell
/// words that each name a dropped file.
fn drop_line(line: &str) -> Option<Vec<PathBuf>> {
    if let Some(p) = dropped_file(line) {
        return Some(vec![p]);
    }
    shell_words(line, !cfg!(windows))?
        .iter()
        .map(|w| dropped_file(w))
        .collect()
}

/// An absolute path or `file://` URI naming an existing image file.
fn dropped_file(s: &str) -> Option<PathBuf> {
    let p = match s.strip_prefix("file://") {
        Some(rest) => PathBuf::from(file_uri_path(rest, cfg!(windows))?),
        None => PathBuf::from(s),
    };
    let image = is_image_ext(&p) || is_unsupported_image_ext(&p);
    (image && p.is_absolute() && p.is_file()).then_some(p)
}

/// Split a line the way a POSIX shell would: blanks separate words, quotes
/// group, and a backslash keeps the next char literal (macOS terminals drop
/// `Screenshot\ 1.png`). Without `posix` (Windows) only double quotes group,
/// as Windows terminals quote drops, and `\` and `'` are literal: `\` is the
/// path separator and `Bob's.png` needs no quoting. `None` on an unclosed
/// quote.
fn shell_words(line: &str, posix: bool) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            // Only the shell's blanks: macOS names hold U+202F, which is whitespace too.
            ' ' | '\t' => words.extend(word.take()),
            '\'' if posix => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' if posix => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => w.push(c),
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        c => w.push(c),
                    }
                }
            }
            '\\' if posix => word.get_or_insert_with(String::new).push(chars.next()?),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// The local path of a `file://` URI given without its scheme: no host or
/// `localhost`, percent-decoded, and on Windows `/C:/x` as `C:\x`.
fn file_uri_path(rest: &str, windows: bool) -> Option<String> {
    let path = percent_decode(rest.strip_prefix("localhost").unwrap_or(rest))?;
    if windows {
        let drive = path.trim_start_matches('/');
        let b = drive.as_bytes();
        return (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| drive.replace('/', "\\"));
    }
    path.starts_with('/').then_some(path)
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    fn png(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0])).save(path).unwrap();
        path.to_path_buf()
    }

    // On Windows `\` separates path components and is never an escape.
    #[cfg(not(windows))]
    #[test]
    fn backslash_escaped_macos_drops_attach() {
        let dir = tempfile::tempdir().unwrap();
        let shot = png(&dir.path().join("drop dir/Screenshot 2026-10-17 at 10.00.00.png"));
        let pasted = shot.to_string_lossy().replace(' ', "\\ ");
        assert_eq!(parse_image_drop(&pasted), vec![shot.clone()]);
        let unescaped = shot.to_string_lossy().into_owned();
        assert_eq!(parse_image_drop(&unescaped), vec![shot], "a lone path with plain spaces");
    }

    #[test]
    fn multi_file_drops_in_every_quoting_attach_all() {
        let dir = tempfile::tempdir().unwrap();
        let a = png(&dir.path().join("My Pics/a.png"));
        let b = png(&dir.path().join("b.png"));
        let (sa, sb) = (a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned());
        let both = vec![a.clone(), b.clone()];
        #[cfg(not(windows))]
        assert_eq!(parse_image_drop(&format!("'{sa}' '{sb}'")), both, "GNOME single quotes");
        assert_eq!(parse_image_drop(&format!("\"{sa}\" {sb}")), both, "mixed quoting");
        assert_eq!(parse_image_drop(&format!("{sb} {sb}")), vec![b.clone(), b.clone()], "plain words");
        assert_eq!(parse_image_drop(&format!("{sa}\r\n{sb}\n")), both, "one per line");
    }

    #[test]
    fn file_uris_keep_the_root_and_decode() {
        let dir = tempfile::tempdir().unwrap();
        let shot = png(&dir.path().join("My Shot.png"));
        let uri = format!("file://{}", shot.to_string_lossy().replace(' ', "%20"));
        assert_eq!(parse_image_drop(&uri), vec![shot.clone()]);
        let local = format!("file://localhost{}", shot.to_string_lossy().replace(' ', "%20"));
        assert_eq!(parse_image_drop(&local), vec![shot]);
        assert_eq!(file_uri_path("/C:/Users/me/a%20b.png", true).as_deref(), Some("C:\\Users\\me\\a b.png"));
        assert_eq!(file_uri_path("C:/x.png", true).as_deref(), Some("C:\\x.png"));
        assert_eq!(file_uri_path("/home/u/x.png", false).as_deref(), Some("/home/u/x.png"));
        assert_eq!(file_uri_path("server/share/x.png", false), None, "remote hosts are not local files");
        assert_eq!(file_uri_path("/bad%2", false), None);
        assert_eq!(file_uri_path("/bad%+1", false), None);
    }

    #[test]
    fn text_that_only_mentions_a_path_stays_text() {
        let dir = tempfile::tempdir().unwrap();
        let logo = png(&dir.path().join("logo.png"));
        let s = logo.to_string_lossy();
        assert!(parse_image_drop(&format!("img = Image.open(\"{s}\")")).is_empty());
        assert!(parse_image_drop(&format!("fix the layout in \"{s}\" please")).is_empty());
        assert!(parse_image_drop(&format!("{s}\nsee above")).is_empty());
        assert!(parse_image_drop("logo.png").is_empty(), "relative paths are typed, not dropped");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "x").unwrap();
        assert!(parse_image_drop(&format!("{s} {}", notes.display())).is_empty());
        assert!(parse_image_drop(&format!("'{s}")).is_empty(), "unclosed quote");
    }

    #[test]
    fn undecodable_image_formats_are_reported_not_pasted() {
        let dir = tempfile::tempdir().unwrap();
        let heic = dir.path().join("IMG_0001.HEIC");
        std::fs::write(&heic, "x").unwrap();
        assert_eq!(parse_image_drop(&heic.to_string_lossy()), vec![heic.clone()]);
        assert!(is_unsupported_image_ext(&heic));
        assert!(!is_image_ext(&heic));
        let svg = dir.path().join("logo.svg");
        std::fs::write(&svg, "<svg/>").unwrap();
        assert!(parse_image_drop(&svg.to_string_lossy()).is_empty(), "an SVG path is source text");
    }

    #[test]
    fn shell_words_follow_the_platform_escape_rule() {
        assert_eq!(shell_words(r"a\ b 'c d' e", true).unwrap(), ["a b", "c d", "e"]);
        assert_eq!(shell_words(r#""x\"y" "\q""#, true).unwrap(), ["x\"y", "\\q"]);
        assert_eq!(
            shell_words(r#""C:\My Pics\a.png" C:\b.png"#, false).unwrap(),
            [r"C:\My Pics\a.png", r"C:\b.png"]
        );
        assert_eq!(shell_words(r"C:\x\Bob's.png C:\y.png", false).unwrap(), [r"C:\x\Bob's.png", r"C:\y.png"]);
        assert!(shell_words(r"/x/Bob's.png /y.png", true).is_none(), "POSIX shells quote such names");
        assert_eq!(shell_words("at 10.00\u{202f}AM.png", true).unwrap(), ["at", "10.00\u{202f}AM.png"]);
        assert_eq!(shell_words("'' x", true).unwrap(), ["", "x"]);
        assert!(shell_words("\"open", true).is_none());
        assert!(shell_words("trailing\\", true).is_none());
    }
}

pub fn rel_inbox_path() -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let id = uuid::Uuid::new_v4();
    let short = &id.to_string()[..8];
    PathBuf::from(".groka")
        .join("inbox")
        .join(format!("{ts}-{short}.jpg"))
}

pub fn save_user_image(workspace: &Path, img: &DynamicImage) -> Result<String> {
    let rel = rel_inbox_path();
    let abs = workspace.join(&rel);
    save_jpeg(&abs, img)?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

pub fn ingest_image_file(workspace: &Path, src: &Path) -> Result<String> {
    let meta = std::fs::metadata(src).map_err(|e| Error::Tool(format!("read image: {e}")))?;
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(Error::Tool("image file larger than 32MiB".into()));
    }
    let img = image::open(src).map_err(|e| Error::Tool(format!("open image: {e}")))?;
    save_user_image(workspace, &img)
}

pub fn save_jpeg(path: &Path, img: &DynamicImage) -> Result<(u32, u32, usize)> {
    let prepared = prepare(img);
    let (w, h) = (prepared.width(), prepared.height());
    let bytes = encode_jpeg(&prepared)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &bytes)?;
    Ok((w, h, bytes.len()))
}

pub fn data_uri_from_file(path: &Path) -> Result<String> {
    let img = image::open(path).map_err(|e| Error::Tool(format!("open image: {e}")))?;
    if below_vision_min(img.width(), img.height()) {
        return Err(Error::Tool(enlarge_yourself_note(img.width(), img.height())));
    }
    let prepared = prepare(&img);
    let bytes = encode_jpeg(&prepared)?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        STANDARD.encode(bytes)
    ))
}

pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<DynamicImage> {
    let img = RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| Error::Tool("invalid rgba buffer".into()))?;
    Ok(DynamicImage::ImageRgba8(img))
}

pub fn rel_shot_path() -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    PathBuf::from(".groka")
        .join("shots")
        .join(format!("{ts}.jpg"))
}

fn prepare(img: &DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let edge = w.max(h);
    if edge <= MAX_EDGE {
        return img.clone();
    }
    let scale = MAX_EDGE as f32 / edge as f32;
    let nw = ((w as f32) * scale).max(1.0).round() as u32;
    let nh = ((h as f32) * scale).max(1.0).round() as u32;
    img.resize(nw, nh, FilterType::Triangle)
}

fn encode_jpeg(img: &DynamicImage) -> Result<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut buf = Cursor::new(Vec::new());
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    enc.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| Error::Tool(format!("jpeg encode: {e}")))?;
    let bytes = buf.into_inner();
    if bytes.len() > MAX_BYTES {
        return Err(Error::Tool(format!(
            "image is {} bytes after resize; max {MAX_BYTES}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_square() -> DynamicImage {
        from_rgba(8, 8, vec![255, 0, 0, 255].repeat(64)).unwrap()
    }

    fn vision_ok_square() -> DynamicImage {
        from_rgba(32, 32, vec![255, 0, 0, 255].repeat(32 * 32)).unwrap()
    }

    #[test]
    fn jpeg_roundtrip_is_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        let (w, h, n) = save_jpeg(&path, &red_square()).unwrap();
        assert_eq!((w, h), (8, 8));
        assert!(n > 0 && n < MAX_BYTES);
    }

    #[test]
    fn tiny_image_is_not_attached_and_tells_the_model_to_enlarge() {
        assert!(below_vision_min(16, 16));
        assert!(!below_vision_min(32, 32));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.jpg");
        save_jpeg(&path, &red_square()).unwrap();
        let err = data_uri_from_file(&path).unwrap_err().to_string();
        assert!(err.contains("512"), "{err}");
        assert!(err.contains("8x8"), "{err}");
        assert!(err.contains("Enlarge") || err.contains("enlarge"), "{err}");
        let v = user_message("look", &[PathBuf::from("tiny.jpg")], dir.path());
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "input_text", "{v}");
        let note = parts[1]["text"].as_str().unwrap();
        assert!(note.contains("512"), "{note}");
        assert!(!input_has_image(&[v]));
    }

    #[test]
    fn large_enough_image_attaches_as_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.jpg");
        save_jpeg(&path, &vision_ok_square()).unwrap();
        let uri = data_uri_from_file(&path).unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,"));
        assert!(uri.len() > 32);
    }

    #[test]
    fn resizes_huge_edge() {
        let img = DynamicImage::new_rgb8(4000, 1000);
        let out = prepare(&img);
        assert!(out.width() <= MAX_EDGE);
        assert!(out.height() <= MAX_EDGE);
        assert_eq!(out.width(), MAX_EDGE);
    }

    #[test]
    fn function_output_embeds_image_parts() {
        let v = function_call_output("c1", "{\"path\":\"a.jpg\"}", Some("data:image/jpeg;base64,abc"));
        assert_eq!(v["type"], "function_call_output");
        let parts = v["output"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["detail"], "high");
        assert!(input_has_image(&[v]));
    }

    #[test]
    fn attach_flag_loads_workspace_image() {
        let dir = tempfile::tempdir().unwrap();
        save_jpeg(&dir.path().join("shot.jpg"), &vision_ok_square()).unwrap();
        let out = json!({"attach_image": true, "path": "shot.jpg"}).to_string();
        let uri = data_uri_for_tool(dir.path(), &out).unwrap().unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,"));
        assert!(data_uri_for_tool(dir.path(), "{}").unwrap().is_none());
    }

    #[test]
    fn strip_removes_pixels_keeps_text() {
        let mut items = vec![function_call_output(
            "c1",
            "{\"path\":\"a.jpg\"}",
            Some("data:image/jpeg;base64,abc"),
        )];
        assert!(input_has_image(&items));
        strip_attached_images(&mut items);
        assert!(!input_has_image(&items));
        assert_eq!(items[0]["output"], "{\"path\":\"a.jpg\"}");
    }

    #[test]
    fn retain_recent_keeps_newest_image_items() {
        let mut items: Vec<_> = (1..=KEEP_RECENT_IMAGES + 1)
            .map(|i| {
                function_call_output(
                    &format!("c{i}"),
                    &format!("img-{i}"),
                    Some("data:image/jpeg;base64,abc"),
                )
            })
            .collect();
        retain_recent_images(&mut items, KEEP_RECENT_IMAGES);
        assert_eq!(image_item_count(&items), KEEP_RECENT_IMAGES);
        assert!(!value_has_image(&items[0]));
        assert_eq!(items[0]["output"], "img-1");
        for item in &items[1..] {
            assert!(value_has_image(item));
        }
    }

    #[test]
    fn prune_expires_after_keep_turns() {
        let mut items = vec![function_call_output(
            "c1",
            "shot",
            Some("data:image/jpeg;base64,abc"),
        )];
        prune_attached_images(&mut items, 1);
        assert!(input_has_image(&items));
        prune_attached_images(&mut items, IMAGE_KEEP_TURNS);
        assert!(!input_has_image(&items));
        assert_eq!(items[0]["output"], "shot");
    }

    #[test]
    fn attach_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let out = json!({"attach_image": true, "path": "../secret.png"}).to_string();
        assert!(data_uri_for_tool(dir.path(), &out).is_err());
    }

    #[test]
    fn user_message_embeds_workspace_image() {
        let dir = tempfile::tempdir().unwrap();
        save_jpeg(&dir.path().join("a.jpg"), &vision_ok_square()).unwrap();
        let v = user_message("look", &[PathBuf::from("a.jpg")], dir.path());
        assert_eq!(v["role"], "user");
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["type"], "input_image");
        let uri = parts[1]["image_url"].as_str().unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,"), "{uri}");
        assert!(input_has_image(&[v.clone()]));
        let mut items = vec![v];
        strip_attached_images(&mut items);
        assert!(!input_has_image(&items));
        assert!(items[0]["content"].as_str().unwrap().contains("look"));
        assert!(items[0]["content"].as_str().unwrap().contains("圖片"));
    }

    #[test]
    fn parse_image_drop_only_when_every_token_is_an_image_file() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.jpg");
        save_jpeg(&img, &red_square()).unwrap();
        let found = parse_image_drop(&format!("\"{}\"", img.display()));
        assert_eq!(found, vec![img.clone()]);
        assert!(parse_image_drop("please look at shot.jpg").is_empty());
        assert!(parse_image_drop("").is_empty());
        let missing = dir.path().join("nope.jpg");
        assert!(parse_image_drop(&missing.to_string_lossy()).is_empty());
    }

    #[test]
    fn ingest_copies_into_workspace_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        save_jpeg(&src, &red_square()).unwrap();
        let ws = tempfile::tempdir().unwrap();
        let rel = ingest_image_file(ws.path(), &src).unwrap();
        assert!(rel.starts_with(".groka/inbox/"), "{rel}");
        assert!(ws.path().join(&rel).is_file());
    }

    #[test]
    fn open_image_decodes_by_content_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let jpg = dir.path().join("a.jpg");
        save_jpeg(&jpg, &vision_ok_square()).unwrap();
        let misnamed = dir.path().join("a.png");
        std::fs::copy(&jpg, &misnamed).unwrap();
        assert_eq!(open_image(&misnamed).unwrap().width(), 32);
        assert_eq!(image_dimensions(&misnamed).unwrap(), (32, 32));
    }

    #[test]
    fn flatten_alpha_composites_onto_the_background() {
        let clear = from_rgba(2, 1, vec![0, 0, 0, 0, 0, 0, 0, 255]).unwrap();
        let flat = flatten_alpha(&clear, [255, 255, 255]).to_rgb8();
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flat.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn groka_dir_writes_a_gitignore() {
        let ws = tempfile::tempdir().unwrap();
        let dir = groka_dir(ws.path(), "shots").unwrap();
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_to_string(ws.path().join(".groka/.gitignore")).unwrap().trim(), "*");
    }
}
