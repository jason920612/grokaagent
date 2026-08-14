//! Chat image preview: Sixel / Kitty / iTerm2 when the terminal can paint
//! graphics, otherwise half-block `▀` cells.

use std::env;
use std::io::{self, Write};
use std::path::Path;

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::queue;
use image::{imageops::FilterType, DynamicImage, Rgb, Rgba};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

pub const MAX_COLS: u16 = 56;
pub const MAX_ROWS: u16 = 12;
pub const INDENT: u16 = 6;

/// Chat-panel background so transparent pixels do not punch through as black.
const PANEL_BG: Rgba<u8> = Rgba([32, 32, 32, 255]);

pub fn from_path(path: &Path, max_cols: u16, max_rows: u16) -> Vec<Line<'static>> {
    match image::open(path) {
        Ok(img) => halfblock_lines(&img, max_cols, max_rows),
        Err(_) => vec![Line::from(Span::styled(
            format!("[無法預覽 {}]", path.display()),
            Style::default().fg(Color::Rgb(110, 110, 110)),
        ))],
    }
}

pub fn halfblock_lines(img: &DynamicImage, max_cols: u16, max_rows: u16) -> Vec<Line<'static>> {
    let max_cols = max_cols.max(1) as u32;
    let max_px_h = (max_rows.max(1) as u32).saturating_mul(2);
    let rgb = fit(img, max_cols, max_px_h).to_rgb8();
    let w = rgb.width();
    let h = rgb.height();
    let mut lines = Vec::new();
    let mut y = 0;
    while y < h {
        let mut spans = Vec::with_capacity(w as usize);
        for x in 0..w {
            let top = rgb.get_pixel(x, y);
            let bot = if y + 1 < h {
                *rgb.get_pixel(x, y + 1)
            } else {
                Rgb([24, 24, 24])
            };
            spans.push(Span::styled(
                "▀",
                Style::default()
                    .fg(Color::Rgb(top[0], top[1], top[2]))
                    .bg(Color::Rgb(bot[0], bot[1], bot[2])),
            ));
        }
        lines.push(Line::from(spans));
        y += 2;
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn fit(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let w = img.width().max(1);
    let h = img.height().max(1);
    let scale = (max_w as f32 / w as f32)
        .min(max_h as f32 / h as f32)
        .min(1.0);
    let nw = ((w as f32) * scale).max(1.0).round() as u32;
    let nh = ((h as f32) * scale).max(1.0).round() as u32;
    if nw == w && nh == h {
        img.clone()
    } else {
        img.resize_exact(nw, nh, FilterType::Triangle)
    }
}

pub fn uses_graphics(picker: &Picker) -> bool {
    !matches!(picker.protocol_type(), ProtocolType::Halfblocks)
}

/// Query / guess a graphics protocol. Call after alternate screen, before EventStream.
/// Never call this from unit tests — it may write to stdio.
pub fn detect_picker() -> Picker {
    let hints = TermHints::from_env();
    if let Some(forced) = hints
        .override_proto
        .as_deref()
        .and_then(parse_protocol_name)
    {
        return finish_picker(Picker::from_fontsize((10, 20)), forced);
    }
    if hints.is_vscode() {
        return finish_picker(Picker::from_fontsize((10, 20)), ProtocolType::Halfblocks);
    }
    let queried = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((10, 20)));
    let proto = if queried.protocol_type() != ProtocolType::Halfblocks {
        queried.protocol_type()
    } else {
        protocol_from_hints(&hints).unwrap_or(ProtocolType::Halfblocks)
    };
    finish_picker(queried, proto)
}

fn finish_picker(mut picker: Picker, proto: ProtocolType) -> Picker {
    picker.set_protocol_type(proto);
    picker.set_background_color(PANEL_BG);
    picker
}

pub fn protocol_for(picker: &Picker, path: &Path, cols: u16, rows: u16) -> Option<Protocol> {
    let img = image::open(path).ok()?;
    picker
        .new_protocol(
            img,
            Rect::new(0, 0, cols.max(1), rows.max(1)),
            Resize::Fit(None),
        )
        .ok()
}

/// Graphics sequence that ratatui-image stores in a single cell (Sixel / iTerm2).
/// Those payloads have a huge unicode width; leaving them in the buffer makes
/// `Buffer::diff` treat every following cell as invalidated, so the terminal
/// punches holes in the image on every keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicBlit {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub data: String,
}

impl GraphicBlit {
    pub fn area(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

pub fn immediate_payload(proto: &Protocol) -> Option<&str> {
    match proto {
        Protocol::Sixel(s) => Some(s.data.as_str()),
        Protocol::ITerm2(i) => Some(i.data.as_str()),
        _ => None,
    }
}

/// Mark the graphic area skipped and width-1 so ratatui will not rewrite it.
pub fn reserve_graphic_cells(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_skip(true);
            }
        }
    }
}

/// Sixel cannot be clipped. If an overlay painted over any cell of a graphic,
/// drop the blit and un-skip the whole area so those cells erase the image
/// instead of leaving a remnant around (or on top of) the overlay.
pub fn reveal_obscured_graphics(buf: &mut Buffer, blits: &mut Vec<GraphicBlit>) {
    blits.retain(|blit| {
        let area = blit.area();
        if !graphic_is_obscured(buf, area) {
            return true;
        }
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_skip(false);
                }
            }
        }
        false
    });
}

fn graphic_is_obscured(buf: &Buffer, area: Rect) -> bool {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if buf.cell((x, y)).is_some_and(|c| !c.skip) {
                return true;
            }
        }
    }
    false
}

pub fn write_blits<W: Write>(out: &mut W, blits: &[GraphicBlit]) -> io::Result<()> {
    if blits.is_empty() {
        return Ok(());
    }
    queue!(out, SavePosition)?;
    for blit in blits {
        queue!(out, MoveTo(blit.x, blit.y))?;
        out.write_all(blit.data.as_bytes())?;
    }
    queue!(out, RestorePosition)?;
    out.flush()
}

pub fn cell_size_for(picker: &Picker, path: &Path, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let max_cols = max_cols.min(MAX_COLS).max(1);
    let max_rows = max_rows.min(MAX_ROWS).max(1);
    let Ok(img) = image::open(path) else {
        return (max_cols.min(24).max(1), 2);
    };
    let (fw, fh) = picker.font_size();
    let fw = fw.max(1) as f32;
    let fh = fh.max(1) as f32;
    let need_w = (img.width() as f32 / fw).ceil().max(1.0);
    let need_h = (img.height() as f32 / fh).ceil().max(1.0);
    let scale = (max_cols as f32 / need_w)
        .min(max_rows as f32 / need_h)
        .min(1.0);
    let w = ((need_w * scale).round() as u16).max(1).min(max_cols);
    let h = ((need_h * scale).round() as u16).max(1).min(max_rows);
    (w, h)
}

#[derive(Debug, Clone)]
pub struct TermHints {
    pub override_proto: Option<String>,
    pub term: String,
    pub term_program: String,
    pub wt_session: bool,
    pub kitty: bool,
    pub ghostty: bool,
}

impl TermHints {
    pub fn from_env() -> Self {
        Self {
            override_proto: env::var("GROKA_IMAGE_PROTOCOL").ok(),
            term: env::var("TERM").unwrap_or_default(),
            term_program: env::var("TERM_PROGRAM").unwrap_or_default(),
            wt_session: env::var("WT_SESSION").is_ok(),
            kitty: env::var("KITTY_WINDOW_ID")
                .ok()
                .is_some_and(|s| !s.is_empty()),
            ghostty: env::var("GHOSTTY_RESOURCES_DIR").is_ok()
                || env::var("GHOSTTY_BIN_DIR").is_ok(),
        }
    }

    pub fn is_vscode(&self) -> bool {
        self.term_program.to_ascii_lowercase().contains("vscode")
    }
}

pub fn parse_protocol_name(s: &str) -> Option<ProtocolType> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "halfblocks" | "halfblock" | "unicode" => Some(ProtocolType::Halfblocks),
        "sixel" => Some(ProtocolType::Sixel),
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

/// Guess a protocol from env hints. `None` means "no reason to override Halfblocks".
pub fn protocol_from_hints(h: &TermHints) -> Option<ProtocolType> {
    if let Some(forced) = h
        .override_proto
        .as_deref()
        .and_then(parse_protocol_name)
    {
        return Some(forced);
    }
    if h.is_vscode() {
        return Some(ProtocolType::Halfblocks);
    }
    let prog = h.term_program.to_ascii_lowercase();
    let term = h.term.to_ascii_lowercase();
    if h.kitty || prog.contains("kitty") || term.contains("kitty") {
        return Some(ProtocolType::Kitty);
    }
    if h.ghostty || prog.contains("ghostty") || term.contains("ghostty") {
        return Some(ProtocolType::Kitty);
    }
    if prog.contains("iterm")
        || prog.contains("wezterm")
        || prog.contains("mintty")
        || prog.contains("rio")
        || prog.contains("bobcat")
    {
        return Some(ProtocolType::Iterm2);
    }
    if h.wt_session || term.contains("sixel") {
        return Some(ProtocolType::Sixel);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::from_rgba;
    use ratatui_image::picker::ProtocolType;

    #[test]
    fn red_square_preview_uses_red_cells() {
        let img = from_rgba(8, 8, vec![255, 0, 0, 255].repeat(64)).unwrap();
        let lines = halfblock_lines(&img, 8, 8);
        assert!(!lines.is_empty());
        let span = &lines[0].spans[0];
        assert_eq!(span.content.as_ref(), "▀");
        assert_eq!(span.style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(span.style.bg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(lines[0].spans.len(), 8);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn missing_file_is_a_placeholder_not_empty() {
        let lines = from_path(Path::new("no-such-image.jpg"), 20, 4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans[0].content.contains("無法預覽"));
    }

    fn hints(prog: &str, wt: bool) -> TermHints {
        TermHints {
            override_proto: None,
            term: String::new(),
            term_program: prog.into(),
            wt_session: wt,
            kitty: false,
            ghostty: false,
        }
    }

    #[test]
    fn vscode_stays_on_halfblocks_even_inside_windows_terminal() {
        assert_eq!(
            protocol_from_hints(&hints("vscode", true)),
            Some(ProtocolType::Halfblocks)
        );
    }

    #[test]
    fn windows_terminal_guesses_sixel() {
        assert_eq!(
            protocol_from_hints(&hints("", true)),
            Some(ProtocolType::Sixel)
        );
    }

    #[test]
    fn override_wins_over_vscode() {
        let mut h = hints("vscode", true);
        h.override_proto = Some("sixel".into());
        assert_eq!(protocol_from_hints(&h), Some(ProtocolType::Sixel));
    }

    #[test]
    fn kitty_and_ghostty_use_kitty_protocol() {
        let mut h = hints("", false);
        h.kitty = true;
        assert_eq!(protocol_from_hints(&h), Some(ProtocolType::Kitty));
        h.kitty = false;
        h.ghostty = true;
        assert_eq!(protocol_from_hints(&h), Some(ProtocolType::Kitty));
        h.ghostty = false;
        h.term_program = "WezTerm".into();
        assert_eq!(protocol_from_hints(&h), Some(ProtocolType::Iterm2));
    }

    #[test]
    fn unknown_env_does_not_invent_a_protocol() {
        assert_eq!(protocol_from_hints(&hints("", false)), None);
    }

    #[test]
    fn sixel_protocol_encodes_without_a_tty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("red.png");
        image::RgbImage::from_pixel(20, 20, image::Rgb([255, 0, 0]))
            .save(&path)
            .unwrap();
        let mut picker = Picker::from_fontsize((10, 20));
        picker.set_protocol_type(ProtocolType::Sixel);
        picker.set_background_color(PANEL_BG);
        let proto = protocol_for(&picker, &path, 12, 8).expect("sixel encode");
        assert!(matches!(proto, Protocol::Sixel(_)));
        assert!(proto.area().width >= 1);
        assert!(proto.area().height >= 1);
        let (w, h) = cell_size_for(&picker, &path, 56, 12);
        assert!(w >= 1 && w <= 56, "{w}");
        assert!(h >= 1 && h <= 12, "{h}");
        let payload = immediate_payload(&proto).expect("sixel is an immediate protocol");
        assert!(payload.starts_with('\x1b'), "{payload:?}");
        let printable = payload.chars().filter(|c| !c.is_control()).count();
        assert!(
            printable > 1,
            "sixel printable width {printable} (len {}) would invalidate following cells in Buffer::diff",
            payload.len()
        );
    }

    #[test]
    fn reserve_graphic_cells_are_skip_and_width_one() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 4));
        buf[(2, 1)].set_symbol("X");
        reserve_graphic_cells(&mut buf, Rect::new(2, 1, 3, 2));
        for y in 1..3 {
            for x in 2..5 {
                let cell = &buf[(x, y)];
                assert!(cell.skip, "{x},{y}");
                assert_eq!(cell.symbol(), " ");
            }
        }
        assert!(!buf[(1, 1)].skip);
        assert_eq!(buf[(1, 1)].symbol(), " ");
    }

    #[test]
    fn write_blits_is_a_no_op_when_empty() {
        let mut out: Vec<u8> = Vec::new();
        write_blits(&mut out, &[]).unwrap();
        assert!(out.is_empty());
    }

    fn sample_blit(x: u16, y: u16, w: u16, h: u16) -> GraphicBlit {
        GraphicBlit {
            x,
            y,
            width: w,
            height: h,
            data: "\x1bPq#0;2;0;0;0$".into(),
        }
    }

    #[test]
    fn unobscured_graphic_stays_skipped_and_queued() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        reserve_graphic_cells(&mut buf, Rect::new(1, 1, 4, 3));
        let mut blits = vec![sample_blit(1, 1, 4, 3)];
        reveal_obscured_graphics(&mut buf, &mut blits);
        assert_eq!(blits.len(), 1);
        assert!(buf[(1, 1)].skip);
        assert!(buf[(4, 3)].skip);
    }

    #[test]
    fn overlay_on_part_of_graphic_drops_blit_and_unskips_the_rest() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        reserve_graphic_cells(&mut buf, Rect::new(1, 1, 4, 3));
        buf[(2, 2)].set_skip(false);
        buf[(2, 2)].set_symbol("W");
        let mut blits = vec![sample_blit(1, 1, 4, 3)];
        reveal_obscured_graphics(&mut buf, &mut blits);
        assert!(blits.is_empty(), "sixel cannot clip; the whole blit must drop");
        assert!(!buf[(1, 1)].skip, "uncovered remnant cells must be written to erase sixel");
        assert_eq!(buf[(1, 1)].symbol(), " ");
        assert!(!buf[(2, 2)].skip);
        assert_eq!(buf[(2, 2)].symbol(), "W");
    }
}
