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
    // read_image already wrote the exact JPEG to send. Anything at `view`
    // that model_jpeg could not have written is ignored in favor of `path`.
    if let Some(view) = v.get("view").and_then(Value::as_str) {
        if let Some(bytes) = resolve_in_workspace(workspace, view).ok().and_then(|abs| ready_jpeg(&abs)) {
            return Ok(Some(jpeg_data_uri(&bytes)));
        }
    }
    let path = v
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool("attach_image missing path".into()))?;
    let resolved = resolve_in_workspace(workspace, path)?;
    Ok(Some(data_uri_from_region(&resolved, output_region(&v))?))
}

/// A read_image `region` echoed in its result: the crop that was sent.
fn output_region(v: &Value) -> Option<(u32, u32, u32, u32)> {
    let r = v.get("region")?;
    let n = |k: &str| r.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    Some((n("x")?, n("y")?, n("width")?, n("height")?))
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
        strip_one_item_images(&mut items[i], None);
    }
}

/// Drop every attached image for `model`, which cannot view any: tool
/// results say no pixels were attached, user turns keep their paths, and
/// neither invites read_image again. Returns the call ids of the tool
/// results rewritten.
pub fn strip_images_for_blind_model(items: &mut [Value], model: &str) -> Vec<String> {
    let mut rewritten = Vec::new();
    for item in items.iter_mut().filter(|item| value_has_image(item)) {
        strip_one_item_images(item, Some(model));
        if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                rewritten.push(id.to_string());
            }
        }
    }
    rewritten
}

/// [`retain_recent_images`], but the image items in `items[seen..]` are
/// never stripped and count against `keep` first: the model has not been
/// sent them yet, and would read `attach_image: true` beside no pixels.
pub fn retain_seen_images(items: &mut [Value], keep: usize, seen: usize) {
    let seen = seen.min(items.len());
    let unseen = image_item_count(&items[seen..]);
    retain_recent_images(&mut items[..seen], keep.saturating_sub(unseen));
}

/// After a complete that included images, expire or cap them.
pub fn prune_attached_images(items: &mut [Value], turns_seen: u32) {
    if turns_seen >= IMAGE_KEEP_TURNS {
        strip_attached_images(items);
    } else {
        retain_recent_images(items, KEEP_RECENT_IMAGES);
    }
}

/// `blind`: the model the pixels were for cannot view images at all.
fn strip_one_item_images(item: &mut Value, blind: Option<&str>) {
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
        if let (Some(model), Ok(Value::Object(mut o))) = (blind, serde_json::from_str::<Value>(text)) {
            o.insert("attach_image".into(), json!(false));
            o.insert(
                "attach_error".into(),
                json!(format!("{model} cannot view images, so no pixels were attached. Do not guess what the image shows.")),
            );
            item["output"] = Value::String(Value::Object(o).to_string());
            return;
        }
        let note = detached_note(1, blind.is_some());
        item["output"] = Value::String(if text.is_empty() { note } else { format!("{text}\n{note}") });
        return;
    }
    if item.get("role").and_then(Value::as_str) == Some("user") {
        strip_user_content_images(item, blind.is_some());
    }
}

/// Left where stripped pixels were, so the model knows it no longer has
/// them and how to look again (tool results and user turns name the path).
/// A model that cannot view images is not sent back to read_image.
fn detached_note(n: usize, blind: bool) -> String {
    if blind {
        if n == 1 {
            "(image not shown: this model cannot view images; use its path for file operations)".into()
        } else {
            format!("({n} images not shown: this model cannot view images; use their paths for file operations)")
        }
    } else if n == 1 {
        "(image no longer attached; read_image its path to view it again)".into()
    } else {
        format!("({n} images no longer attached; read_image their paths to view them again)")
    }
}

fn strip_user_content_images(item: &mut Value, blind: bool) {
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
    let note = detached_note(n, blind);
    if text.trim().is_empty() {
        text = note;
    } else {
        text = format!("{text}\n{note}");
    }
    item["content"] = Value::String(text);
}

/// User turn for the Responses API: plain string, or text + `input_image`
/// parts. Each image is followed by its workspace path, so the model can
/// use the file and read it again once the pixels expire.
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
            Ok(uri) => {
                parts.push(json!({
                    "type": "input_image",
                    "image_url": uri,
                    "detail": "high"
                }));
                parts.push(json!({
                    "type": "input_text",
                    "text": format!("(image file: {})", rel.replace('\\', "/"))
                }));
            }
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

#[cfg(test)]
mod agent_tests {
    use super::*;

    fn shot(id: &str) -> Value {
        function_call_output(id, &format!("{{\"path\":\"{id}.jpg\"}}"), Some("data:image/jpeg;base64,abc"))
    }

    #[test]
    fn user_images_carry_their_path_through_the_strip() {
        let dir = tempfile::tempdir().unwrap();
        let img = from_rgba(32, 32, vec![0, 0, 255, 255].repeat(32 * 32)).unwrap();
        save_jpeg(&dir.path().join(".groka/inbox/u.jpg"), &img).unwrap();
        let v = user_message("fix this", &[PathBuf::from(".groka/inbox/u.jpg")], dir.path());
        let parts = v["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[2]["text"], "(image file: .groka/inbox/u.jpg)");
        let mut items = vec![v];
        strip_attached_images(&mut items);
        let text = items[0]["content"].as_str().unwrap();
        assert!(text.starts_with("fix this\n(image file: .groka/inbox/u.jpg)\n"), "{text}");
        assert!(text.contains("no longer attached") && text.contains("read_image"), "{text}");
    }

    #[test]
    fn a_stripped_tool_image_says_so_once() {
        let mut items = vec![shot("c1")];
        strip_attached_images(&mut items);
        let once = items[0]["output"].as_str().unwrap().to_string();
        assert_eq!(once, format!("{{\"path\":\"c1.jpg\"}}\n{}", detached_note(1, false)));
        strip_attached_images(&mut items);
        assert_eq!(items[0]["output"], once.as_str());
    }

    #[test]
    fn a_blind_model_is_not_sent_back_to_read_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = from_rgba(32, 32, vec![0, 0, 255, 255].repeat(32 * 32)).unwrap();
        save_jpeg(&dir.path().join(".groka/inbox/u.jpg"), &img).unwrap();
        let tool = function_call_output("c1", r#"{"path":"c1.jpg","attach_image":true}"#, Some("data:image/jpeg;base64,AAAA"));
        let mut items = vec![user_message("fix this", &[PathBuf::from(".groka/inbox/u.jpg")], dir.path()), tool];
        let rewritten = strip_images_for_blind_model(&mut items, "text-model");
        assert_eq!(rewritten, ["c1"]);
        assert_eq!(image_item_count(&items), 0);
        let user = items[0]["content"].as_str().unwrap();
        assert!(user.contains("(image file: .groka/inbox/u.jpg)") && user.contains("cannot view images"), "{user}");
        assert!(!user.contains("read_image"), "{user}");
        let out: Value = serde_json::from_str(items[1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(out["attach_image"], false);
        assert!(out["attach_error"].as_str().unwrap().starts_with("text-model cannot view images"), "{out}");
        assert!(!out.to_string().contains("read_image"), "{out}");
    }

    #[test]
    fn images_not_sent_yet_are_never_capped_away() {
        // Two seen, then a batch of three the model has not been sent.
        let mut items: Vec<Value> = ["s1", "s2", "n1", "n2", "n3"].into_iter().map(shot).collect();
        retain_seen_images(&mut items, 3, 2);
        let kept: Vec<bool> = items.iter().map(value_has_image).collect();
        assert_eq!(kept, [false, false, true, true, true]);
        // More unseen than the cap: all of them still go out once.
        let mut items: Vec<Value> = ["s1", "n1", "n2"].into_iter().map(shot).collect();
        retain_seen_images(&mut items, 1, 1);
        assert_eq!(image_item_count(&items), 2);
        assert!(!value_has_image(&items[0]));
        // `seen` past the end is clamped.
        let mut items = vec![shot("s1"), shot("s2")];
        retain_seen_images(&mut items, 1, 9);
        assert!(!value_has_image(&items[0]) && value_has_image(&items[1]));
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
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| Error::Tool(format!("open image: {e}")))?;
    decode_capped(reader).map_err(|e| Error::Tool(format!("open image: {e}")))
}

/// Decode with EXIF orientation applied, refusing images whose pixels would
/// take more than the allocation cap `image::open` applies (512 MiB):
/// `from_decoder` skips it, and a small file can declare a huge image.
pub fn decode_capped<R: std::io::BufRead + std::io::Seek>(
    reader: ImageReader<R>,
) -> std::result::Result<DynamicImage, image::ImageError> {
    let mut dec = reader.into_decoder()?;
    image::Limits::default().reserve(dec.total_bytes())?;
    let orientation = dec.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec)?;
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
    let owned;
    let rgba = match img {
        DynamicImage::ImageRgba8(b) => b,
        _ => {
            owned = img.to_rgba8();
            &owned
        }
    };
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
    // A `.groka` (or a folder in it) committed as a symlink would carry
    // screenshots, pastes and logs out of the workspace.
    let mut at = workspace.to_path_buf();
    for c in Path::new(".groka").join(sub).components() {
        at.push(c);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Tool(format!("{} is a symlink; not writing through it", at.display())));
        }
    }
    std::fs::create_dir_all(&dir)?;
    let ignore = root.join(".gitignore");
    if std::fs::symlink_metadata(&ignore).is_err() {
        let _ = std::fs::write(&ignore, "*\n");
    }
    Ok(dir)
}

#[cfg(all(test, unix))]
mod groka_dir_tests {
    use super::*;

    #[test]
    fn a_committed_groka_symlink_is_not_written_through() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join(".groka")).unwrap();
        assert!(groka_dir(ws.path(), "inbox").is_err());
        assert!(!outside.path().join("inbox").exists());
        assert!(!outside.path().join(".gitignore").exists());
    }

    #[test]
    fn a_symlinked_subfolder_is_refused_too() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join(".groka")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join(".groka/shots")).unwrap();
        assert!(groka_dir(ws.path(), "shots").is_err());
        assert!(groka_dir(ws.path(), "inbox").is_ok());
    }
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

/// `<timestamp>-<short uuid>.jpg`, unique even for saves in the same instant
/// (several calls in one response, or child agents sharing the workspace).
fn stamped_jpg_name() -> String {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f");
    let id = uuid::Uuid::new_v4();
    format!("{ts}-{}.jpg", &id.to_string()[..8])
}

pub fn rel_inbox_path() -> PathBuf {
    PathBuf::from(".groka").join("inbox").join(stamped_jpg_name())
}

pub fn save_user_image(workspace: &Path, img: &DynamicImage) -> Result<String> {
    let rel = rel_inbox_path();
    groka_dir(workspace, "inbox")?;
    save_jpeg(&workspace.join(&rel), img)?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

pub fn ingest_image_file(workspace: &Path, src: &Path) -> Result<String> {
    check_source_size(src)?;
    let img = open_image(src)?;
    save_user_image(workspace, &img)
}

/// Refuse source files too large to decode in reasonable time and memory.
pub fn check_source_size(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| Error::Tool(format!("read image: {e}")))?;
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(Error::Tool("image file larger than 32MiB".into()));
    }
    Ok(())
}

pub fn save_jpeg(path: &Path, img: &DynamicImage) -> Result<(u32, u32, usize)> {
    let (bytes, w, h) = model_jpeg(img)?;
    write_replacing(path, &bytes)?;
    Ok((w, h, bytes.len()))
}

/// Write through a sibling temp file and a rename: a reader never sees a
/// half-written image, and a symlink at `path` is replaced, not followed.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let id = uuid::Uuid::new_v4();
    let tmp = path.with_extension(format!("{}.part", &id.to_string()[..8]));
    // A failed write (disk full) must not leave the partial file behind either.
    if let Err(e) = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Seen copies kept: the newest, up to a count and a total size.
const SEEN_KEEP: usize = 200;
const SEEN_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Keep the exact JPEG the model was sent as `.groka/seen/<hash>.jpg`. Picture
/// rows point here, so the history still shows what the model saw after the
/// source file is overwritten.
pub fn save_seen(workspace: &Path, jpeg: &[u8]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(jpeg));
    let name = format!("{}.jpg", &hash[..16]);
    groka_dir(workspace, "seen")?;
    // A committed `.groka` symlink must not carry the copy out of the
    // workspace, where it could not be read back either.
    let dir = resolve_in_workspace(workspace, ".groka/seen")?;
    let file = dir.join(&name);
    // Not `is_file`: a planted link of that name is replaced, not touched.
    if std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
        // Seen again: count it as new so the prune keeps it.
        let _ = std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    } else {
        write_replacing(&file, jpeg)?;
        prune_seen(&dir, SEEN_KEEP, SEEN_MAX_BYTES);
    }
    Ok(format!(".groka/seen/{name}"))
}

/// Drop the oldest `.jpg` files in `dir` beyond `keep` files or `max_bytes`
/// in all. The newest always stays; older picture rows lose their preview.
fn prune_seen(dir: &Path, keep: usize, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let path = e.path();
            if !meta.is_file() || path.extension().map_or(true, |x| x != "jpg") {
                return None;
            }
            Some((meta.modified().ok()?, meta.len(), path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut total = 0u64;
    for (i, (_, len, path)) in files.iter().enumerate() {
        total += len;
        if i > 0 && (i >= keep || total > max_bytes) {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn data_uri_from_file(path: &Path) -> Result<String> {
    data_uri_from_region(path, None)
}

/// `data_uri_from_file`, cropped first to `(x, y, width, height)` when given.
fn data_uri_from_region(path: &Path, region: Option<(u32, u32, u32, u32)>) -> Result<String> {
    check_source_size(path)?;
    if region.is_none() {
        if let Some(bytes) = ready_jpeg(path) {
            return Ok(jpeg_data_uri(&bytes));
        }
    }
    let mut img = open_image(path)?;
    if let Some((x, y, w, h)) = region {
        img = img.crop_imm(x, y, w, h);
    }
    if below_vision_min(img.width(), img.height()) {
        return Err(Error::Tool(enlarge_yourself_note(img.width(), img.height())));
    }
    let (bytes, _, _) = model_jpeg(&img)?;
    Ok(jpeg_data_uri(&bytes))
}

fn jpeg_data_uri(bytes: &[u8]) -> String {
    format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes))
}

/// The file's own bytes when it already is what `model_jpeg` makes (a
/// screenshot or an inbox copy), so it is not decoded and re-encoded again.
fn ready_jpeg(path: &Path) -> Option<Vec<u8>> {
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if !plain_jpeg(&bytes) {
        return None;
    }
    let mut dec = image::codecs::jpeg::JpegDecoder::new(Cursor::new(&bytes)).ok()?;
    let (w, h) = dec.dimensions();
    let upright = matches!(dec.orientation(), Ok(Orientation::NoTransforms));
    (upright && w.max(h) <= MAX_EDGE && !below_vision_min(w, h)).then_some(bytes)
}

/// An 8-bit baseline or progressive JPEG in gray or YCbCr, like the ones
/// `encode_jpeg` writes. CMYK and the rarer coding modes are re-encoded.
fn plain_jpeg(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return false;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return false;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        match marker {
            // SOF0-2: length(2) precision(1) height(2) width(2) components(1)
            0xC0..=0xC2 => {
                return bytes.get(i + 4) == Some(&8) && matches!(bytes.get(i + 9), Some(1 | 3));
            }
            // Other frame types, or a scan before any frame.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDA => return false,
            _ => {}
        }
        i += 2 + u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
    }
    false
}

pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<DynamicImage> {
    let img = RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| Error::Tool("invalid rgba buffer".into()))?;
    Ok(DynamicImage::ImageRgba8(img))
}

pub fn rel_shot_path() -> PathBuf {
    PathBuf::from(".groka").join("shots").join(stamped_jpg_name())
}

/// Transparent pixels are shown on white, as image viewers do.
const ALPHA_BG: [u8; 3] = [255, 255, 255];

/// What the model is sent: alpha flattened, long edge at most `MAX_EDGE`,
/// JPEG. Returns the bytes and the width and height actually sent.
pub fn model_jpeg(img: &DynamicImage) -> Result<(Vec<u8>, u32, u32)> {
    let prepared = prepare(img);
    let bytes = encode_jpeg(&prepared)?;
    Ok((bytes, prepared.width(), prepared.height()))
}

fn prepare(img: &DynamicImage) -> DynamicImage {
    // Flatten before resizing: the resize filter does not premultiply alpha,
    // so transparent pixels (usually stored black) would darken the edges.
    if img.color().has_alpha() {
        return fit_edge(&flatten_alpha(img, ALPHA_BG));
    }
    fit_edge(img)
}

fn fit_edge(img: &DynamicImage) -> DynamicImage {
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
pub(crate) mod vision_tests {
    use super::*;
    use image::ImageEncoder;

    /// A 64x32 JPEG (left half red, right half blue) tagged EXIF Orientation=6,
    /// so it shows as 32x64 with the red half on top.
    pub(crate) fn rotated_jpeg(path: &Path) {
        let mut rgb = RgbImage::new(64, 32);
        for (x, _, p) in rgb.enumerate_pixels_mut() {
            p.0 = if x < 32 { [255, 0, 0] } else { [0, 0, 255] };
        }
        // Little-endian TIFF, one IFD entry: 0x0112 Orientation, SHORT, 6.
        let exif = vec![
            0x49, 0x49, 0x2A, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut buf = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut buf);
        enc.set_exif_metadata(exif).unwrap();
        enc.write_image(rgb.as_raw(), 64, 32, image::ExtendedColorType::Rgb8).unwrap();
        std::fs::write(path, buf).unwrap();
    }

    pub(crate) fn decode_uri(uri: &str) -> DynamicImage {
        let b64 = uri.strip_prefix("data:image/jpeg;base64,").expect(uri);
        image::load_from_memory(&STANDARD.decode(b64).unwrap()).unwrap()
    }

    /// Opaque black square in the middle of a fully transparent (0,0,0,0) canvas.
    fn black_on_clear(w: u32, h: u32) -> DynamicImage {
        let mut rgba = RgbaImage::new(w, h);
        for (x, y, p) in rgba.enumerate_pixels_mut() {
            if x >= w / 4 && x < w * 3 / 4 && y >= h / 4 && y < h * 3 / 4 {
                p.0 = [0, 0, 0, 255];
            }
        }
        DynamicImage::ImageRgba8(rgba)
    }

    fn is_white(p: [u8; 3]) -> bool {
        p.iter().all(|&c| c > 240)
    }

    #[test]
    fn transparent_pixels_reach_the_model_white_not_black() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("logo.png");
        black_on_clear(64, 64).save(&png).unwrap();
        let seen = decode_uri(&data_uri_from_file(&png).unwrap()).to_rgb8();
        assert!(is_white(seen.get_pixel(2, 2).0), "{:?}", seen.get_pixel(2, 2));
        assert!(seen.get_pixel(32, 32).0.iter().all(|&c| c < 20), "{:?}", seen.get_pixel(32, 32));
        let rel = ingest_image_file(dir.path(), &png).unwrap();
        let copy = open_image(&dir.path().join(rel)).unwrap().to_rgb8();
        assert!(is_white(copy.get_pixel(2, 2).0), "{:?}", copy.get_pixel(2, 2));
        let big = prepare(&black_on_clear(2600, 400)).to_rgb8();
        assert_eq!(big.dimensions(), (MAX_EDGE, 197));
        assert!(is_white(big.get_pixel(1, 1).0), "{:?}", big.get_pixel(1, 1));
    }

    #[test]
    fn alpha_is_flattened_before_the_resize() {
        // Opaque white stripe on transparent black: resizing first would blend
        // the black into the stripe's edge before the flatten.
        let mut rgba = RgbaImage::new(2560, 64);
        for (x, _, p) in rgba.enumerate_pixels_mut() {
            if x >= 1280 {
                p.0 = [255, 255, 255, 255];
            }
        }
        let out = prepare(&DynamicImage::ImageRgba8(rgba)).to_rgb8();
        assert!(out.pixels().all(|p| is_white(p.0)), "dark fringe");
    }

    #[test]
    fn exif_orientation_is_applied_on_ingest_and_attach() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("phone.jpg");
        rotated_jpeg(&src);
        let seen = decode_uri(&data_uri_from_file(&src).unwrap()).to_rgb8();
        assert_eq!(seen.dimensions(), (32, 64));
        assert!(seen.get_pixel(16, 4)[0] > 200, "red half on top: {:?}", seen.get_pixel(16, 4));
        let rel = ingest_image_file(dir.path(), &src).unwrap();
        let copy = dir.path().join(rel);
        assert_eq!(image_dimensions(&copy).unwrap(), (32, 64));
        assert_eq!(image::image_dimensions(&copy).unwrap(), (32, 64), "copy is stored upright");
    }

    #[test]
    fn misnamed_and_newer_formats_decode_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let img = black_on_clear(40, 40);
        let png_as_jpg = dir.path().join("logo.jpg");
        img.write_to(&mut std::fs::File::create(&png_as_jpg).unwrap(), image::ImageFormat::Png).unwrap();
        for name in ["a.gif", "a.webp", "a.bmp"] {
            img.save(dir.path().join(name)).unwrap();
        }
        for name in ["logo.jpg", "a.gif", "a.webp", "a.bmp"] {
            let path = dir.path().join(name);
            let seen = decode_uri(&data_uri_from_file(&path).unwrap());
            assert_eq!((seen.width(), seen.height()), (40, 40), "{name}");
            assert!(ingest_image_file(dir.path(), &path).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_ready_jpeg_is_sent_as_is_and_others_are_re_encoded() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.jpg");
        save_jpeg(&shot, &black_on_clear(64, 48)).unwrap();
        let uri = data_uri_from_file(&shot).unwrap();
        assert_eq!(uri, jpeg_data_uri(&std::fs::read(&shot).unwrap()));
        // Too large for the model as is: decoded and downscaled.
        let big = dir.path().join("big.jpg");
        DynamicImage::new_rgb8(2000, 100).save(&big).unwrap();
        assert_eq!(decode_uri(&data_uri_from_file(&big).unwrap()).width(), MAX_EDGE);
        // Upright only after the EXIF rotation: re-encoded.
        let phone = dir.path().join("phone.jpg");
        rotated_jpeg(&phone);
        assert_ne!(data_uri_from_file(&phone).unwrap(), jpeg_data_uri(&std::fs::read(&phone).unwrap()));
    }

    #[test]
    fn plain_jpeg_accepts_only_what_the_encoder_writes() {
        let (bytes, _, _) = model_jpeg(&black_on_clear(32, 32)).unwrap();
        assert!(plain_jpeg(&bytes));
        let sof = bytes.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let mut cmyk = bytes.clone();
        cmyk[sof + 9] = 4;
        assert!(!plain_jpeg(&cmyk));
        let mut lossless = bytes.clone();
        lossless[sof + 1] = 0xC3;
        assert!(!plain_jpeg(&lossless));
        assert!(!plain_jpeg(b"\x89PNG\r\n\x1a\n"));
        assert!(!plain_jpeg(&bytes[..sof]));
    }

    #[test]
    fn open_image_keeps_the_allocation_cap() {
        // A 30000x30000 24-bit BMP header (2.7 GB decoded) and no pixel data.
        let mut bmp = b"BM".to_vec();
        for v in [54u32, 0, 54, 40, 30000, 30000] {
            bmp.extend_from_slice(&v.to_le_bytes());
        }
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.bmp");
        std::fs::write(&path, bmp).unwrap();
        assert_eq!(image_dimensions(&path).unwrap(), (30000, 30000));
        let err = open_image(&path).unwrap_err().to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn default_names_are_unique_and_pastes_are_git_ignored() {
        let a = rel_shot_path();
        assert!(a.starts_with(".groka/shots"), "{}", a.display());
        assert_ne!(a, rel_shot_path());
        assert_ne!(rel_inbox_path(), rel_inbox_path());
        let ws = tempfile::tempdir().unwrap();
        let rel = save_user_image(ws.path(), &black_on_clear(32, 32)).unwrap();
        assert!(rel.starts_with(".groka/inbox/"), "{rel}");
        assert_eq!(std::fs::read_to_string(ws.path().join(".groka/.gitignore")).unwrap().trim(), "*");
    }

    #[test]
    fn seen_copies_are_content_addressed_and_preferred_for_attach() {
        let ws = tempfile::tempdir().unwrap();
        let (jpeg, _, _) = model_jpeg(&black_on_clear(32, 32)).unwrap();
        let view = save_seen(ws.path(), &jpeg).unwrap();
        assert!(view.starts_with(".groka/seen/") && view.ends_with(".jpg"), "{view}");
        assert_eq!(save_seen(ws.path(), &jpeg).unwrap(), view);
        assert_eq!(std::fs::read(ws.path().join(&view)).unwrap(), jpeg);
        assert!(ws.path().join(".groka/.gitignore").is_file());
        // The source changed (or is gone) since: the model still gets the view bytes.
        let out = json!({"attach_image": true, "path": "gone.png", "view": view}).to_string();
        assert_eq!(data_uri_for_tool(ws.path(), &out).unwrap().unwrap(), jpeg_data_uri(&jpeg));
    }

    #[test]
    fn a_view_that_is_not_a_prepared_jpeg_falls_back_to_the_path() {
        let ws = tempfile::tempdir().unwrap();
        black_on_clear(40, 40).save(ws.path().join("a.png")).unwrap();
        std::fs::write(ws.path().join("big.bin"), vec![0xFFu8; 4096]).unwrap();
        DynamicImage::new_rgb8(2000, 100).save(ws.path().join("wide.jpg")).unwrap();
        let want = data_uri_from_file(&ws.path().join("a.png")).unwrap();
        for view in ["big.bin", "wide.jpg", "missing.jpg", "../out.jpg"] {
            let out = json!({"attach_image": true, "path": "a.png", "view": view}).to_string();
            assert_eq!(data_uri_for_tool(ws.path(), &out).unwrap().unwrap(), want, "{view}");
        }
        // Without a view, a read_image region is cropped from the path again.
        let out = json!({"attach_image": true, "path": "a.png", "region": {"x": 10, "y": 10, "width": 20, "height": 30}});
        let seen = decode_uri(&data_uri_for_tool(ws.path(), &out.to_string()).unwrap().unwrap()).to_rgb8();
        assert_eq!(seen.dimensions(), (20, 30));
        assert!(seen.get_pixel(15, 5)[0] < 20, "{:?}", seen.get_pixel(15, 5));
        assert!(is_white(seen.get_pixel(15, 28).0), "{:?}", seen.get_pixel(15, 28));
    }

    #[test]
    fn seen_copies_are_pruned_oldest_first_and_a_reread_counts_as_new() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(1000);
        let age = |f: &Path, t: std::time::SystemTime| {
            std::fs::File::options().write(true).open(f).unwrap().set_modified(t).unwrap();
        };
        for i in 0..5u64 {
            let f = dir.path().join(format!("{i}.jpg"));
            std::fs::write(&f, vec![0u8; 100]).unwrap();
            age(&f, old + std::time::Duration::from_secs(i));
        }
        std::fs::write(dir.path().join("notes.txt"), "not ours").unwrap();
        let left = || {
            let mut v: Vec<String> =
                std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        prune_seen(dir.path(), 3, u64::MAX);
        assert_eq!(left(), ["2.jpg", "3.jpg", "4.jpg", "notes.txt"]);
        prune_seen(dir.path(), 50, 250);
        assert_eq!(left(), ["3.jpg", "4.jpg", "notes.txt"]);

        let ws = tempfile::tempdir().unwrap();
        let (jpeg, _, _) = model_jpeg(&black_on_clear(32, 32)).unwrap();
        let file = ws.path().join(save_seen(ws.path(), &jpeg).unwrap());
        age(&file, old);
        save_seen(ws.path(), &jpeg).unwrap();
        let touched = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(touched > old + std::time::Duration::from_secs(500));
    }

    #[cfg(unix)]
    #[test]
    fn seen_copies_never_land_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, outside) = (dir.path().join("ws"), dir.path().join("outside"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join(".groka")).unwrap();
        let (jpeg, _, _) = model_jpeg(&black_on_clear(32, 32)).unwrap();
        assert!(save_seen(&ws, &jpeg).is_err());
        let copies = std::fs::read_dir(outside.join("seen")).map_or(0, |d| d.count());
        assert_eq!(copies, 0);
        // A link planted under the copy's name is replaced; its target is untouched.
        let ws2 = dir.path().join("ws2");
        std::fs::create_dir_all(&ws2).unwrap();
        let rel = save_seen(&ws2, &jpeg).unwrap();
        std::fs::remove_file(ws2.join(&rel)).unwrap();
        std::fs::write(outside.join("victim"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.join("victim"), ws2.join(&rel)).unwrap();
        assert_eq!(save_seen(&ws2, &jpeg).unwrap(), rel);
        assert!(!ws2.join(&rel).is_symlink());
        assert_eq!(std::fs::read(ws2.join(&rel)).unwrap(), jpeg);
        assert_eq!(std::fs::read_to_string(outside.join("victim")).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn save_jpeg_replaces_a_symlink_instead_of_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("bashrc");
        std::fs::write(&victim, "keep me").unwrap();
        let link = dir.path().join("shot.jpg");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        save_jpeg(&link, &black_on_clear(32, 32)).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(!link.is_symlink());
        assert_eq!(image_dimensions(&link).unwrap(), (32, 32));
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 2, "no temp file left: {left:?}");
    }
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
        let text = items[0]["output"].as_str().unwrap();
        assert!(text.starts_with("{\"path\":\"a.jpg\"}\n"), "{text}");
        assert!(text.contains("read_image"), "{text}");
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
        assert!(items[0]["output"].as_str().unwrap().starts_with("img-1\n"));
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
        assert!(items[0]["output"].as_str().unwrap().starts_with("shot\n"));
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
        assert!(items[0]["content"].as_str().unwrap().contains("read_image"));
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
