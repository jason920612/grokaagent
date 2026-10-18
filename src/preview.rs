//! Chat image preview: Sixel / Kitty / iTerm2 when the terminal can paint
//! graphics, otherwise half-block `▀` cells.

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use image::{imageops::FilterType, DynamicImage, Rgb, Rgba};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui_image::picker::cap_parser::{Parser, Response};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Image, Resize};

pub const MAX_COLS: u16 = 56;
pub const MAX_ROWS: u16 = 12;
pub const INDENT: u16 = 6;

/// Chat-panel background so transparent pixels do not punch through as black.
const PANEL_BG: Rgba<u8> = Rgba([32, 32, 32, 255]);
const PANEL_RGB: [u8; 3] = [32, 32, 32];
/// Cell size to assume when nothing reports one.
const GUESS_CELL: FontSize = (10, 20);

pub fn from_path(path: &Path, max_cols: u16, max_rows: u16) -> Vec<Line<'static>> {
    match crate::vision::open_image(path) {
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
    let rgb = crate::vision::flatten_alpha(&fit(img, max_cols, max_px_h), PANEL_RGB).to_rgb8();
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
                Rgb(PANEL_RGB)
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

/// `w`×`h` scaled down (never up) to fit `max_w`×`max_h`.
fn fit_dims(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let w = w.max(1);
    let h = h.max(1);
    let scale = (max_w as f32 / w as f32)
        .min(max_h as f32 / h as f32)
        .min(1.0);
    let nw = ((w as f32) * scale).max(1.0).round() as u32;
    let nh = ((h as f32) * scale).max(1.0).round() as u32;
    (nw, nh)
}

fn fit(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (nw, nh) = fit_dims(img.width(), img.height(), max_w, max_h);
    if nw == img.width().max(1) && nh == img.height().max(1) {
        img.clone()
    } else {
        img.resize_exact(nw, nh, FilterType::Triangle)
    }
}

/// Cells `halfblock_lines` fills for an image of `dims`, without decoding it.
pub fn halfblock_size(dims: Option<(u32, u32)>, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let max_cols = max_cols.max(1);
    let Some((w, h)) = dims else {
        // The one-line "[無法預覽 …]" placeholder.
        return (max_cols, 1);
    };
    let (nw, nh) = fit_dims(w, h, max_cols as u32, (max_rows.max(1) as u32).saturating_mul(2));
    (nw.min(max_cols as u32) as u16, nh.div_ceil(2).max(1) as u16)
}

pub fn uses_graphics(picker: &Picker) -> bool {
    !matches!(picker.protocol_type(), ProtocolType::Halfblocks)
}

/// Pick a graphics protocol and the cell size in pixels. Call after raw mode
/// and the alternate screen, before EventStream.
/// Never call this from unit tests — it may write to stdio.
pub fn detect_picker() -> Picker {
    let hints = TermHints::from_env();
    let decided = decided_protocol(&hints);
    let winsize = winsize_cell();
    let answer = match decided {
        Some(ProtocolType::Halfblocks) => None,
        Some(_) if winsize.is_some() => None,
        _ => {
            if hints.tmux {
                // Building a picker turns on tmux passthrough, which the query needs.
                let _ = Picker::from_fontsize(GUESS_CELL);
            }
            query_terminal(hints.tmux)
        }
    };
    let (proto, cell) = choose_protocol(&hints, decided, answer.as_ref(), winsize);
    picker_for(proto, cell)
}

fn picker_for(proto: ProtocolType, cell: FontSize) -> Picker {
    let mut picker = Picker::from_fontsize(cell);
    picker.set_protocol_type(proto);
    picker.set_background_color(PANEL_BG);
    picker
}

/// The same protocol for a new cell size (the font was zoomed).
pub fn with_cell(picker: &Picker, cell: FontSize) -> Picker {
    picker_for(picker.protocol_type(), cell)
}

/// Protocol and cell size from the environment and what the terminal said.
fn choose_protocol(
    h: &TermHints,
    decided: Option<ProtocolType>,
    answer: Option<&Answer>,
    winsize: Option<FontSize>,
) -> (ProtocolType, FontSize) {
    let proto = decided.unwrap_or_else(|| match answer {
        Some(a) if a.kitty => ProtocolType::Kitty,
        Some(a) if a.sixel => ProtocolType::Sixel,
        // It answered without a graphics protocol; inherited variables do not overrule that.
        Some(_) => ProtocolType::Halfblocks,
        None => inherited_guess(h).unwrap_or(ProtocolType::Halfblocks),
    });
    match answer.and_then(|a| a.cell).or(winsize) {
        Some(cell) => (proto, cell),
        // Pixel protocols size images as cells × cell size, so a guessed cell
        // overflows or shrinks them. Only an explicit choice keeps one.
        None if h.forced().is_some() || proto == ProtocolType::Halfblocks => (proto, GUESS_CELL),
        None => (ProtocolType::Halfblocks, GUESS_CELL),
    }
}

/// What the terminal said about itself.
#[derive(Debug, Default, PartialEq)]
struct Answer {
    kitty: bool,
    sixel: bool,
    cell: Option<FontSize>,
}

/// Collects replies to the capability query up to the closing status report.
#[derive(Default)]
#[cfg_attr(not(unix), allow(dead_code))]
struct AnswerReader {
    parser: Parser,
    answer: Answer,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl AnswerReader {
    /// `Some` once the terminal has answered everything.
    fn feed(&mut self, bytes: &[u8]) -> Option<Answer> {
        for &b in bytes {
            for reply in self.parser.push(char::from(b)) {
                match reply {
                    Response::Status => return Some(std::mem::take(&mut self.answer)),
                    Response::Kitty => self.answer.kitty = true,
                    Response::Sixel => self.answer.sixel = true,
                    Response::CellSize(Some(cell)) => self.answer.cell = Some(cell),
                    _ => {}
                }
            }
        }
        None
    }
}

/// Ask the terminal for its graphics protocols and cell size. Polls stdin
/// against a deadline instead of reading on a helper thread: a thread left
/// behind by a late or missing answer would keep eating keystrokes.
#[cfg(unix)]
fn query_terminal(is_tmux: bool) -> Option<Answer> {
    use ratatui_image::picker::cap_parser::QueryStdioOptions;
    use std::time::{Duration, Instant};
    const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return None;
    }
    let query = Parser::query(is_tmux, QueryStdioOptions { text_sizing_protocol: false });
    let mut out = io::stdout();
    out.write_all(query.as_bytes()).and_then(|_| out.flush()).ok()?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut reader = AnswerReader::default();
    let mut buf = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis().clamp(1, 1000) as libc::c_int) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return None;
        }
        if ready <= 0 {
            continue;
        }
        // SAFETY: reads at most buf.len() bytes into buf.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return None;
        }
        if let Some(answer) = reader.feed(&buf[..n as usize]) {
            return Some(answer);
        }
    }
}

#[cfg(not(unix))]
fn query_terminal(_is_tmux: bool) -> Option<Answer> {
    query_with_ratatui_image()
}

/// ratatui-image's own query. Its reader thread outlives the 1 s timeout
/// when the terminal never answers, and it drops replies that carry no cell
/// size; both read as no answer here.
#[cfg_attr(unix, allow(dead_code))]
fn query_with_ratatui_image() -> Option<Answer> {
    use ratatui_image::picker::Capability;
    let picker = Picker::from_query_stdio().ok()?;
    let caps = picker.capabilities();
    (!caps.is_empty()).then(|| Answer {
        kitty: caps.contains(&Capability::Kitty),
        sixel: caps.contains(&Capability::Sixel),
        cell: Some(picker.font_size()),
    })
}

/// Cell size in pixels from the tty's window size; `None` where the terminal
/// or the OS reports no pixels (Windows).
pub fn winsize_cell() -> Option<FontSize> {
    let ws = crossterm::terminal::window_size().ok()?;
    cell_from_window(ws.columns, ws.rows, ws.width, ws.height)
}

fn cell_from_window(cols: u16, rows: u16, width: u16, height: u16) -> Option<FontSize> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell = (width / cols, height / rows);
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

pub fn protocol_for(picker: &Picker, path: &Path, cols: u16, rows: u16) -> Option<Protocol> {
    let img = crate::vision::open_image(path).ok()?;
    picker
        .new_protocol(
            img,
            Rect::new(0, 0, cols.max(1), rows.max(1)),
            // ratatui-image defaults to nearest-neighbour, which drops thin
            // lines and garbles text when a screenshot shrinks 2-4×.
            Resize::Fit(Some(FilterType::Triangle)),
        )
        .ok()
}

pub fn graphic_for(picker: &Picker, path: &Path, cols: u16, rows: u16) -> Option<Graphic> {
    protocol_for(picker, path, cols, rows).map(Graphic::from)
}

/// An encoded picture. Sixel / iTerm2 keep only their escape payload, which
/// is written after the frame; Kitty keeps the protocol, which sends the
/// pixels on its first render and then places them with unicode placeholders.
#[derive(Clone)]
pub enum Graphic {
    Immediate { area: Rect, data: Rc<str> },
    Placed(Rc<Protocol>),
}

impl Graphic {
    pub fn area(&self) -> Rect {
        match self {
            Self::Immediate { area, .. } => *area,
            Self::Placed(p) => p.area(),
        }
    }

    /// Rough bytes held, for the cache budget. Kitty keeps its base64 RGBA
    /// transmit string as long as the protocol lives.
    pub fn bytes(&self, (fw, fh): FontSize) -> usize {
        match self {
            Self::Immediate { data, .. } => data.len(),
            Self::Placed(p) => {
                let a = p.area();
                (a.width as usize * fw as usize) * (a.height as usize * fh as usize) * 16 / 3
            }
        }
    }
}

impl From<Protocol> for Graphic {
    fn from(proto: Protocol) -> Self {
        match proto {
            Protocol::Sixel(s) => Self::Immediate { area: s.area, data: s.data.into() },
            Protocol::ITerm2(i) => Self::Immediate { area: i.area, data: i.data.into() },
            other => Self::Placed(Rc::new(other)),
        }
    }
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
    pub data: Rc<str>,
}

impl GraphicBlit {
    pub fn area(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

/// A Kitty picture, rendered into the buffer only after the overlays.
pub struct Placement {
    pub area: Rect,
    pub proto: Rc<Protocol>,
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
    let areas: Vec<Rect> = blits.iter().map(GraphicBlit::area).collect();
    let mut keep = settle_graphics(buf, &areas).into_iter();
    blits.retain(|_| keep.next().unwrap_or(false));
}

/// Kitty placeholders cannot be clipped either: each row is written from its
/// first cell and the rest are skipped, so a covered cell keeps the overlay's
/// glyphs on the picture after it closes, and a covered first render loses
/// the one-shot pixel transmit. Render only the pictures nothing covers.
pub fn place_graphics(buf: &mut Buffer, placements: &[Placement]) {
    let areas: Vec<Rect> = placements.iter().map(|p| p.area).collect();
    for (p, keep) in placements.iter().zip(settle_graphics(buf, &areas)) {
        if keep {
            // ratatui-image writes the first column and skips the rest itself.
            set_skip(buf, p.area, false);
            Image::new(&p.proto).render(p.area, buf);
        }
    }
}

/// Which graphics (in drawing order) survive the frame. One is dropped when
/// an overlay painted any of its cells or a later graphic overlaps it; its
/// area is un-skipped so those cells erase what the terminal still shows.
fn settle_graphics(buf: &mut Buffer, areas: &[Rect]) -> Vec<bool> {
    let keep: Vec<bool> = areas
        .iter()
        .enumerate()
        .map(|(i, a)| !graphic_is_obscured(buf, *a) && !areas[i + 1..].iter().any(|b| b.intersects(*a)))
        .collect();
    for (a, _) in areas.iter().zip(&keep).filter(|(_, k)| !**k) {
        set_skip(buf, *a, false);
    }
    // A dropped graphic may lie under a kept one (the viewer over a thumbnail).
    for (a, _) in areas.iter().zip(&keep).filter(|(_, k)| **k) {
        set_skip(buf, *a, true);
    }
    keep
}

fn set_skip(buf: &mut Buffer, area: Rect, skip: bool) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_skip(skip);
            }
        }
    }
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
    for blit in blits {
        queue!(out, MoveTo(blit.x, blit.y))?;
        out.write_all(blit.data.as_bytes())?;
    }
    out.flush()
}

/// Cells a chat thumbnail of an image sized `dims` takes.
pub fn cell_size_for(picker: &Picker, dims: Option<(u32, u32)>, max_cols: u16, max_rows: u16) -> (u16, u16) {
    fit_cells(
        picker,
        dims,
        max_cols.min(MAX_COLS).max(1),
        max_rows.min(MAX_ROWS).max(1),
    )
}

/// Fit into `max_cols`×`max_rows` without the in-chat thumbnail cap.
pub fn cell_size_fit(picker: &Picker, dims: Option<(u32, u32)>, max_cols: u16, max_rows: u16) -> (u16, u16) {
    fit_cells(picker, dims, max_cols.max(1), max_rows.max(1))
}

fn fit_cells(picker: &Picker, dims: Option<(u32, u32)>, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let Some((iw, ih)) = dims else {
        // The one-line "[無法預覽]" placeholder.
        return (max_cols.min(24).max(1), 1);
    };
    let (fw, fh) = picker.font_size();
    let fw = fw.max(1) as f32;
    let fh = fh.max(1) as f32;
    let need_w = (iw.max(1) as f32 / fw).ceil().max(1.0);
    let need_h = (ih.max(1) as f32 / fh).ceil().max(1.0);
    let scale = (max_cols as f32 / need_w)
        .min(max_rows as f32 / need_h)
        .min(1.0);
    let w = ((need_w * scale).round() as u16).max(1).min(max_cols);
    let h = ((need_h * scale).round() as u16).max(1).min(max_rows);
    (w, h)
}

#[derive(Debug, Clone, Default)]
pub struct TermHints {
    pub override_proto: Option<String>,
    pub term: String,
    pub term_program: String,
    pub lc_terminal: String,
    pub wt_session: bool,
    pub kitty: bool,
    pub ghostty: bool,
    pub tmux: bool,
    pub iterm_session: bool,
    pub wezterm: bool,
}

impl TermHints {
    pub fn from_env() -> Self {
        let set = |name: &str| env::var(name).ok().is_some_and(|s| !s.is_empty());
        let term = env::var("TERM").unwrap_or_default();
        let term_program = env::var("TERM_PROGRAM").unwrap_or_default();
        Self {
            override_proto: env::var("GROKA_IMAGE_PROTOCOL").ok(),
            // ratatui-image's rule, so the query is wrapped the same way.
            tmux: term.starts_with("tmux") || term_program == "tmux",
            term,
            term_program,
            lc_terminal: env::var("LC_TERMINAL").unwrap_or_default(),
            wt_session: env::var("WT_SESSION").is_ok(),
            kitty: set("KITTY_WINDOW_ID"),
            ghostty: env::var("GHOSTTY_RESOURCES_DIR").is_ok()
                || env::var("GHOSTTY_BIN_DIR").is_ok(),
            iterm_session: set("ITERM_SESSION_ID"),
            wezterm: set("WEZTERM_EXECUTABLE"),
        }
    }

    pub fn is_vscode(&self) -> bool {
        self.term_program.to_ascii_lowercase().contains("vscode")
    }

    fn forced(&self) -> Option<ProtocolType> {
        self.override_proto.as_deref().and_then(parse_protocol_name)
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

/// A protocol settled without asking the terminal: the override, VS Code, a
/// terminal that names itself in TERM / TERM_PROGRAM / LC_TERMINAL (which
/// other terminals overwrite), or inside tmux, which may not pass the query
/// through, ratatui-image's guess at the terminal outside.
fn decided_protocol(h: &TermHints) -> Option<ProtocolType> {
    if let Some(forced) = h.forced() {
        return Some(forced);
    }
    if h.is_vscode() {
        return Some(ProtocolType::Halfblocks);
    }
    let prog = h.term_program.to_ascii_lowercase();
    let term = h.term.to_ascii_lowercase();
    if ["kitty", "ghostty"].iter().any(|t| term.contains(t) || prog.contains(t)) {
        return Some(ProtocolType::Kitty);
    }
    const ITERM2: [&str; 8] = ["iterm", "wezterm", "mintty", "rio", "bobcat", "tabby", "hyper", "warpterminal"];
    if ITERM2.iter().any(|p| prog.contains(p)) || h.lc_terminal.contains("iTerm") {
        return Some(ProtocolType::Iterm2);
    }
    if h.tmux && h.kitty {
        return Some(ProtocolType::Kitty);
    }
    if h.tmux && (h.iterm_session || h.wezterm) {
        return Some(ProtocolType::Iterm2);
    }
    None
}

/// Variables a terminal leaves to everything started inside it, other
/// terminals included. Trusted only when the terminal did not answer.
fn inherited_guess(h: &TermHints) -> Option<ProtocolType> {
    if h.kitty || h.ghostty {
        return Some(ProtocolType::Kitty);
    }
    if h.wt_session || h.term.to_ascii_lowercase().contains("sixel") {
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
            term_program: prog.into(),
            wt_session: wt,
            ..TermHints::default()
        }
    }

    /// The protocol picked when the terminal does not answer.
    fn guess(h: &TermHints) -> Option<ProtocolType> {
        decided_protocol(h).or_else(|| inherited_guess(h))
    }

    #[test]
    fn vscode_stays_on_halfblocks_even_inside_windows_terminal() {
        assert_eq!(
            guess(&hints("vscode", true)),
            Some(ProtocolType::Halfblocks)
        );
    }

    #[test]
    fn windows_terminal_guesses_sixel() {
        assert_eq!(
            guess(&hints("", true)),
            Some(ProtocolType::Sixel)
        );
    }

    #[test]
    fn override_wins_over_vscode() {
        let mut h = hints("vscode", true);
        h.override_proto = Some("sixel".into());
        assert_eq!(guess(&h), Some(ProtocolType::Sixel));
    }

    #[test]
    fn kitty_and_ghostty_use_kitty_protocol() {
        let mut h = hints("", false);
        h.kitty = true;
        assert_eq!(guess(&h), Some(ProtocolType::Kitty));
        h.kitty = false;
        h.ghostty = true;
        assert_eq!(guess(&h), Some(ProtocolType::Kitty));
        h.ghostty = false;
        h.term_program = "WezTerm".into();
        assert_eq!(guess(&h), Some(ProtocolType::Iterm2));
    }

    #[test]
    fn unknown_env_does_not_invent_a_protocol() {
        assert_eq!(guess(&hints("", false)), None);
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
        let (w, h) = cell_size_for(&picker, crate::vision::image_dimensions(&path).ok(), 56, 12);
        assert!(w >= 1 && w <= 56, "{w}");
        assert!(h >= 1 && h <= 12, "{h}");
        let Graphic::Immediate { data: payload, .. } = Graphic::from(proto) else {
            panic!("sixel is an immediate protocol");
        };
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
    fn cell_size_fit_can_exceed_chat_thumbnail_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        image::RgbImage::from_pixel(400, 400, image::Rgb([255, 0, 0]))
            .save(&path)
            .unwrap();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ProtocolType::Sixel);
        let dims = crate::vision::image_dimensions(&path).ok();
        let (cw, ch) = cell_size_for(&picker, dims, 80, 40);
        assert!(ch <= MAX_ROWS, "{ch}");
        let (vw, vh) = cell_size_fit(&picker, dims, 80, 40);
        assert!(
            vh > MAX_ROWS,
            "viewer height {vh} should exceed chat cap {MAX_ROWS}"
        );
        assert!(vw >= cw && vh > ch, "chat {cw}x{ch} viewer {vw}x{vh}");
    }

    #[test]
    fn write_blits_is_a_no_op_when_empty() {
        let mut out: Vec<u8> = Vec::new();
        write_blits(&mut out, &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn write_blits_moves_without_saving_cursor() {
        let mut out: Vec<u8> = Vec::new();
        write_blits(&mut out, &[sample_blit(3, 4, 2, 1)]).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(
            !s.contains("\x1b7") && !s.contains("\x1b[s"),
            "DECSC would let sixel steal the composer cursor: {s:?}"
        );
        assert!(s.contains("\x1b[5;4H"), "{s:?}");
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

#[cfg(test)]
mod tui_tests {
    use super::*;
    use base64::Engine;
    use image::{ImageFormat, RgbImage, RgbaImage};

    fn kitty_picker() -> Picker {
        picker_for(ProtocolType::Kitty, (10, 20))
    }

    fn kitty_graphic(dir: &Path, w: u32, h: u32, cols: u16, rows: u16) -> Rc<Protocol> {
        let path = dir.join("k.png");
        RgbImage::from_pixel(w, h, Rgb([200, 30, 30])).save(&path).unwrap();
        match graphic_for(&kitty_picker(), &path, cols, rows).expect("kitty encode") {
            Graphic::Placed(p) => p,
            Graphic::Immediate { .. } => panic!("kitty is placed, not blitted"),
        }
    }

    /// Pixels of the image a Kitty transmit sequence carries (f=32, RGBA).
    fn transmitted_rgba(symbol: &str) -> RgbaImage {
        let mut size = (0, 0);
        let mut raw = String::new();
        for chunk in symbol.split("\x1b_G").skip(1) {
            let (keys, rest) = chunk.split_once(';').unwrap();
            for kv in keys.split(',') {
                match kv.split_once('=') {
                    Some(("s", v)) => size.0 = v.parse().unwrap(),
                    Some(("v", v)) => size.1 = v.parse().unwrap(),
                    _ => {}
                }
            }
            raw.push_str(rest.split("\x1b\\").next().unwrap());
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(raw).unwrap();
        RgbaImage::from_raw(size.0, size.1, bytes).unwrap()
    }

    #[test]
    fn halfblocks_composite_transparency_onto_the_panel() {
        // Transparent, with an opaque black "axis" as the last (odd) row.
        let mut img = RgbaImage::from_pixel(4, 3, Rgba([0, 0, 0, 0]));
        for x in 0..4 {
            img.put_pixel(x, 2, Rgba([0, 0, 0, 255]));
        }
        let lines = halfblock_lines(&DynamicImage::ImageRgba8(img), 4, 4);
        assert_eq!(lines.len(), 2);
        let top = &lines[0].spans[0].style;
        assert_eq!(top.fg, Some(Color::Rgb(32, 32, 32)), "transparent shows the panel");
        let axis = &lines[1].spans[0].style;
        assert_eq!(axis.fg, Some(Color::Rgb(0, 0, 0)), "black content stays visible");
        assert_eq!(axis.bg, Some(Color::Rgb(32, 32, 32)), "odd-height padding is the panel colour");
    }

    #[test]
    fn halfblock_size_predicts_the_decoded_lines() {
        for (w, h, cols, rows) in [(1280, 720, 56, 12), (800, 600, 40, 7), (300, 1000, 56, 12), (3, 5, 56, 12), (999, 333, 13, 3)] {
            let img = DynamicImage::ImageRgb8(RgbImage::new(w, h));
            let lines = halfblock_lines(&img, cols, rows);
            let got = (lines[0].spans.len() as u16, lines.len() as u16);
            assert_eq!(halfblock_size(Some((w, h)), cols, rows), got, "{w}x{h} in {cols}x{rows}");
            assert!(got.0 <= cols && got.1 <= rows);
        }
        assert_eq!(halfblock_size(None, 30, 12), (30, 1), "the unreadable placeholder is one line");
    }

    #[test]
    fn previews_decode_by_content_not_extension() {
        // `screenshot {path: "home.png"}` writes JPEG bytes under a .png name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.png");
        RgbImage::from_pixel(32, 16, Rgb([0, 200, 0]))
            .save_with_format(&path, ImageFormat::Jpeg)
            .unwrap();
        let lines = from_path(&path, 16, 8);
        assert!(!lines[0].spans[0].content.contains("無法預覽"), "half-blocks decode it");
        let mut picker = picker_for(ProtocolType::Sixel, (8, 16));
        assert!(protocol_for(&picker, &path, 8, 2).is_some(), "sixel decodes it");
        picker.set_protocol_type(ProtocolType::Kitty);
        assert!(graphic_for(&picker, &path, 8, 2).is_some(), "kitty decodes it");
    }

    #[test]
    fn graphics_downscale_with_a_smoothing_filter() {
        // 1-px white lines every 3 px: nearest-neighbour turns a 3× shrink
        // into solid white or black; a smoothing filter gives grey.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.png");
        RgbImage::from_fn(300, 60, |x, _| if x % 3 == 0 { Rgb([255; 3]) } else { Rgb([0; 3]) })
            .save(&path)
            .unwrap();
        let proto = protocol_for(&kitty_picker(), &path, 10, 1).unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 2));
        Image::new(&proto).render(proto.area(), &mut buf);
        let rgba = transmitted_rgba(buf[(0, 0)].symbol());
        for x in 10..90 {
            let v = rgba.get_pixel(x, 5)[0];
            assert!((40..=130).contains(&v), "pixel {x} = {v}: thin lines must average out");
        }
    }

    #[test]
    fn terminals_that_name_themselves_skip_the_query() {
        let term = |t: &str| TermHints { term: t.into(), ..TermHints::default() };
        let prog = |p: &str| TermHints { term_program: p.into(), ..TermHints::default() };
        assert_eq!(decided_protocol(&term("xterm-kitty")), Some(ProtocolType::Kitty));
        assert_eq!(decided_protocol(&prog("ghostty")), Some(ProtocolType::Kitty));
        assert_eq!(decided_protocol(&prog("iTerm.app")), Some(ProtocolType::Iterm2));
        let lc = TermHints { lc_terminal: "iTerm2".into(), ..TermHints::default() };
        assert_eq!(decided_protocol(&lc), Some(ProtocolType::Iterm2));
        let inherited = TermHints { kitty: true, wt_session: true, ..term("alacritty") };
        assert_eq!(decided_protocol(&inherited), None, "inherited ids leave it to the terminal");
        let tmux = TermHints { kitty: true, tmux: true, ..term("tmux-256color") };
        assert_eq!(decided_protocol(&tmux), Some(ProtocolType::Kitty), "tmux may hide the outer terminal");
        let halfblocks = TermHints { override_proto: Some("halfblocks".into()), ..term("xterm-kitty") };
        assert_eq!(decided_protocol(&halfblocks), Some(ProtocolType::Halfblocks));
    }

    #[test]
    fn a_terminal_answer_beats_inherited_variables() {
        // Alacritty started from kitty inside Windows Terminal: both ids leak in.
        let h = TermHints { kitty: true, wt_session: true, ..TermHints::default() };
        let none = Answer { cell: Some((9, 18)), ..Answer::default() };
        assert_eq!(choose_protocol(&h, None, Some(&none), None), (ProtocolType::Halfblocks, (9, 18)));
        let sixel = Answer { sixel: true, cell: Some((9, 18)), ..Answer::default() };
        assert_eq!(choose_protocol(&h, None, Some(&sixel), Some((8, 16))), (ProtocolType::Sixel, (9, 18)));
        assert_eq!(
            choose_protocol(&h, None, None, Some((8, 16))),
            (ProtocolType::Kitty, (8, 16)),
            "no answer at all: the guess stands"
        );
    }

    #[test]
    fn pixel_protocols_need_a_real_cell_size() {
        let wez = TermHints { term_program: "WezTerm".into(), ..TermHints::default() };
        let decided = decided_protocol(&wez);
        assert_eq!(choose_protocol(&wez, decided, None, Some((7, 15))), (ProtocolType::Iterm2, (7, 15)));
        assert_eq!(
            choose_protocol(&wez, decided, None, None),
            (ProtocolType::Halfblocks, GUESS_CELL),
            "a guessed 10x20 cell would overflow or shrink every picture"
        );
        let forced = TermHints { override_proto: Some("sixel".into()), ..TermHints::default() };
        let decided = decided_protocol(&forced);
        assert_eq!(choose_protocol(&forced, decided, None, Some((8, 16))), (ProtocolType::Sixel, (8, 16)));
        let answer = Answer { cell: Some((9, 18)), ..Answer::default() };
        assert_eq!(choose_protocol(&forced, decided, Some(&answer), None), (ProtocolType::Sixel, (9, 18)));
        assert_eq!(choose_protocol(&forced, decided, None, None), (ProtocolType::Sixel, GUESS_CELL), "an explicit choice is kept");
    }

    #[test]
    fn answer_reader_waits_for_the_status_report() {
        let reply = b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c\x1b[6;16;8t\x1b[0n";
        let mut r = AnswerReader::default();
        let (a, b) = reply.split_at(17);
        assert_eq!(r.feed(a), None, "split mid-reply");
        assert_eq!(r.feed(b), Some(Answer { kitty: true, sixel: true, cell: Some((8, 16)) }));
        let mut r = AnswerReader::default();
        assert_eq!(r.feed(b"\x1b[?62;22c\x1b[0n"), Some(Answer::default()), "answered: no graphics");
    }

    #[test]
    fn window_pixels_give_the_cell_size() {
        assert_eq!(cell_from_window(100, 40, 800, 640), Some((8, 16)));
        assert_eq!(cell_from_window(100, 40, 0, 0), None, "no pixel size reported");
        assert_eq!(cell_from_window(0, 0, 800, 640), None);
    }

    fn sample(x: u16, y: u16, w: u16, h: u16) -> GraphicBlit {
        GraphicBlit { x, y, width: w, height: h, data: "\x1bPq$".into() }
    }

    #[test]
    fn a_graphic_under_a_later_one_drops_and_the_top_one_stays() {
        // A thumbnail, then the viewer panel (cleared cells) and its picture.
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 12));
        reserve_graphic_cells(&mut buf, Rect::new(4, 2, 10, 4));
        let panel = Rect::new(2, 1, 26, 10);
        set_skip(&mut buf, panel, false);
        reserve_graphic_cells(&mut buf, Rect::new(3, 2, 24, 8));
        let mut blits = vec![sample(4, 2, 10, 4), sample(3, 2, 24, 8)];
        reveal_obscured_graphics(&mut buf, &mut blits);
        assert_eq!(blits, vec![sample(3, 2, 24, 8)], "the viewer picture survives the thumbnail under it");
        assert!(buf[(5, 3)].skip && buf[(26, 9)].skip, "its cells stay reserved");
        assert!(!buf[(2, 1)].skip);
    }

    #[test]
    fn kitty_placements_render_only_when_uncovered() {
        let dir = tempfile::tempdir().unwrap();
        let proto = kitty_graphic(dir.path(), 40, 40, 4, 2);
        let area = Rect::new(2, 1, proto.area().width, proto.area().height);
        let placement = || vec![Placement { area, proto: proto.clone() }];

        // Covered on its first render: nothing is placed and the transmit is kept.
        let mut covered = Buffer::empty(Rect::new(0, 0, 12, 5));
        reserve_graphic_cells(&mut covered, area);
        covered[(3, 1)].set_skip(false).set_symbol("│");
        place_graphics(&mut covered, &placement());
        assert_eq!(covered[(2, 1)].symbol(), " ");
        assert!(!covered[(4, 2)].skip, "the hidden picture's cells are erased");

        let mut shown = Buffer::empty(Rect::new(0, 0, 12, 5));
        reserve_graphic_cells(&mut shown, area);
        place_graphics(&mut shown, &placement());
        let first = shown[(2, 1)].symbol();
        assert!(first.contains("\x1b_G") && first.contains('\u{10EEEE}'), "pixels go out with the first visible render");
        assert!(!shown[(2, 1)].skip && shown[(3, 1)].skip);

        // After an overlay closes, every row's first cell changes, so ratatui
        // writes the placeholders again over what the overlay left behind.
        let mut again = Buffer::empty(Rect::new(0, 0, 12, 5));
        reserve_graphic_cells(&mut again, area);
        place_graphics(&mut again, &placement());
        let updates: Vec<(u16, u16)> = covered.diff(&again).iter().map(|(x, y, _)| (*x, *y)).collect();
        for y in area.y..area.bottom() {
            assert!(updates.contains(&(area.x, y)), "row {y} is placed again: {updates:?}");
        }
        assert!(!again[(2, 1)].symbol().contains("\x1b_G"), "transmitted once");
    }
}
