//! Markdown → ratatui lines for the chat transcript.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const TEXT: Color = Color::Rgb(212, 212, 212);
const DIM: Color = Color::Rgb(110, 110, 110);
const ACCENT: Color = Color::Rgb(88, 166, 255);
const CODE: Color = Color::Rgb(210, 180, 80);
const HEAD: Color = Color::Rgb(156, 196, 255);

pub fn markdown_lines(src: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(src, opts);
    let mut out = MdOut::new();
    for ev in parser {
        out.feed(ev);
    }
    out.finish()
}

struct MdOut {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style: Style,
    stack: Vec<Style>,
    item_prefix: String,
    in_code: bool,
    list_level: usize,
    ol_index: Vec<u64>,
}

impl MdOut {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style: Style::default().fg(TEXT),
            stack: Vec::new(),
            item_prefix: String::new(),
            in_code: false,
            list_level: 0,
            ol_index: Vec::new(),
        }
    }

    fn push_style(&mut self, add: Style) {
        self.stack.push(self.style);
        self.style = self.style.patch(add);
    }

    fn pop_style(&mut self) {
        if let Some(s) = self.stack.pop() {
            self.style = s;
        }
    }

    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        if self.in_code {
            for (i, part) in t.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                    self.item_prefix = "    ".into();
                }
                if !part.is_empty() {
                    self.spans.push(Span::styled(part.to_string(), self.style));
                }
            }
            return;
        }
        self.spans.push(Span::styled(t.to_string(), self.style));
    }

    fn flush_line(&mut self) {
        let empty = self.spans.is_empty() && self.item_prefix.is_empty();
        if empty {
            self.lines.push(Line::from(""));
            return;
        }
        let mut spans = Vec::new();
        if !self.item_prefix.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut self.item_prefix),
                Style::default().fg(DIM),
            ));
        }
        spans.append(&mut self.spans);
        self.lines.push(Line::from(spans));
    }

    fn feed(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph | Tag::HtmlBlock => {}
                Tag::Heading { .. } => {
                    if !self.spans.is_empty() {
                        self.flush_line();
                    }
                    self.push_style(
                        Style::default()
                            .fg(HEAD)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                Tag::BlockQuote(_) => {
                    self.item_prefix = "│ ".into();
                    self.push_style(Style::default().fg(DIM).add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    if !self.spans.is_empty() {
                        self.flush_line();
                    }
                    self.in_code = true;
                    self.push_style(Style::default().fg(CODE));
                    let label = match kind {
                        CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                            format!("```{lang}")
                        }
                        _ => "```".into(),
                    };
                    self.item_prefix = String::new();
                    self.spans
                        .push(Span::styled(label, Style::default().fg(DIM)));
                    self.flush_line();
                    self.item_prefix = "    ".into();
                }
                Tag::List(start) => {
                    self.list_level += 1;
                    self.ol_index.push(start.unwrap_or(0));
                }
                Tag::Item => {
                    if !self.spans.is_empty() {
                        self.flush_line();
                    }
                    let indent = "  ".repeat(self.list_level.saturating_sub(1));
                    if let Some(n) = self.ol_index.last_mut() {
                        if *n > 0 {
                            self.item_prefix = format!("{indent}{n}. ");
                            *n += 1;
                        } else {
                            self.item_prefix = format!("{indent}• ");
                        }
                    } else {
                        self.item_prefix = format!("{indent}• ");
                    }
                }
                Tag::Emphasis => {
                    self.push_style(Style::default().add_modifier(Modifier::ITALIC));
                }
                Tag::Strong => {
                    self.push_style(Style::default().add_modifier(Modifier::BOLD));
                }
                Tag::Strikethrough => {
                    self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { dest_url, .. } => {
                    self.push_style(
                        Style::default()
                            .fg(ACCENT)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                    if !dest_url.is_empty() {
                        self.spans.push(Span::styled(
                            format!("[{dest_url}] "),
                            Style::default().fg(DIM),
                        ));
                    }
                }
                Tag::Image { dest_url, .. } => {
                    let url = dest_url.to_string();
                    self.spans.push(Span::styled(
                        format!("[image {url}]"),
                        Style::default().fg(DIM),
                    ));
                }
                Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::Heading(_) => {
                    self.flush_line();
                    if matches!(tag, TagEnd::Heading(_)) {
                        self.pop_style();
                    }
                }
                TagEnd::BlockQuote(_) => {
                    self.flush_line();
                    self.pop_style();
                }
                TagEnd::CodeBlock => {
                    self.flush_line();
                    self.spans
                        .push(Span::styled("```", Style::default().fg(DIM)));
                    self.flush_line();
                    self.in_code = false;
                    self.pop_style();
                }
                TagEnd::List(_) => {
                    self.list_level = self.list_level.saturating_sub(1);
                    self.ol_index.pop();
                }
                TagEnd::Item => {
                    self.flush_line();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    self.pop_style();
                }
                TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow => {
                    self.flush_line();
                }
                TagEnd::TableCell => {
                    self.spans
                        .push(Span::styled("  ", Style::default().fg(DIM)));
                }
                _ => {}
            },
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t),
            Event::Code(t) => {
                self.spans.push(Span::styled(
                    t.to_string(),
                    Style::default().fg(CODE).add_modifier(Modifier::BOLD),
                ));
            }
            Event::SoftBreak => {
                self.spans.push(Span::raw(" "));
            }
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.spans.push(Span::styled(
                    "────────",
                    Style::default().fg(DIM),
                ));
                self.flush_line();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.spans
                    .push(Span::styled(mark, Style::default().fg(DIM)));
            }
            Event::FootnoteReference(t) => {
                self.spans.push(Span::styled(
                    format!("[{t}]"),
                    Style::default().fg(DIM),
                ));
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => self.push_text(&t),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.spans.is_empty() || !self.item_prefix.is_empty() {
            self.flush_line();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        // drop a single trailing blank
        while self.lines.len() > 1
            && self
                .lines
                .last()
                .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

/// Compact clock for TUI captions: `3.4s`, `18s`, `1m 12s`.
pub fn fmt_duration(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms.div_ceil(1000))
    } else {
        let s = ms.div_ceil(1000);
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// Fixed-width clock so a live timer cannot shift the rest of the line.
pub const DURATION_FIELD: usize = 7;

pub fn fmt_duration_field(ms: u64) -> String {
    format!("{:>width$}", fmt_duration(ms), width = DURATION_FIELD)
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Markdown → HTML using only tags we emit. Raw HTML in the source is escaped.
pub fn markdown_html(src: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let mut out = String::new();
    let mut in_code = false;
    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    out.push_str(&format!("<h{level}>"));
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(_) => {
                    in_code = true;
                    out.push_str("<pre><code>");
                }
                Tag::List(None) => out.push_str("<ul>"),
                Tag::List(Some(_)) => out.push_str("<ol>"),
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<del>"),
                Tag::Link { dest_url, .. } => {
                    let href = dest_url.trim();
                    let safe = if href.starts_with("https://") || href.starts_with("http://") {
                        html_escape(href)
                    } else {
                        String::new()
                    };
                    if safe.is_empty() {
                        out.push_str("<span>");
                    } else {
                        out.push_str("<a href=\"");
                        out.push_str(&safe);
                        out.push_str("\" rel=\"noreferrer\" target=\"_blank\">");
                    }
                }
                Tag::Table(_) => out.push_str("<table>"),
                Tag::TableHead => out.push_str("<thead>"),
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => out.push_str("<td>"),
                Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(level) => out.push_str(&format!("</h{level}>")),
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => {
                    in_code = false;
                    out.push_str("</code></pre>");
                }
                TagEnd::List(false) => out.push_str("</ul>"),
                TagEnd::List(true) => out.push_str("</ol>"),
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</del>"),
                TagEnd::Link => out.push_str("</a>"),
                TagEnd::Table => out.push_str("</table>"),
                TagEnd::TableHead => out.push_str("</thead>"),
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => out.push_str("</td>"),
                _ => {}
            },
            Event::Text(t) => out.push_str(&html_escape(&t)),
            Event::Code(t) => {
                if in_code {
                    out.push_str(&html_escape(&t));
                } else {
                    out.push_str("<code>");
                    out.push_str(&html_escape(&t));
                    out.push_str("</code>");
                }
            }
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push_str("<br>"),
            Event::Rule => out.push_str("<hr>"),
            Event::Html(t) | Event::InlineHtml(t) => out.push_str(&html_escape(&t)),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
    if out.is_empty() {
        html_escape(src)
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn heading_bold_code_and_list() {
        let lines = markdown_lines("# Title\n\nHello **bold** and `code`.\n\n- one\n- two\n");
        let blob: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(blob.contains("Title"), "{blob}");
        assert!(blob.contains("bold"), "{blob}");
        assert!(blob.contains("code"), "{blob}");
        assert!(blob.contains("• one") || blob.contains("one"), "{blob}");
        let title = &lines[0];
        assert!(
            title
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "{title:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains("code") && s.style.fg == Some(CODE)),
            "{blob}"
        );
    }

    #[test]
    fn fenced_code_keeps_newlines() {
        let lines = markdown_lines("```rs\nfn hi() {}\nlet x = 1;\n```\n");
        let blob: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(blob.contains("```rs"), "{blob}");
        assert!(blob.contains("fn hi() {}"), "{blob}");
        assert!(blob.contains("let x = 1;"), "{blob}");
        assert!(
            lines.len() >= 3,
            "code block must not collapse: {blob}"
        );
    }

    #[test]
    fn fmt_duration_buckets() {
        assert_eq!(fmt_duration(3400), "3.4s");
        assert_eq!(fmt_duration(18_400), "19s");
        assert_eq!(fmt_duration(72_000), "1m 12s");
    }

    #[test]
    fn duration_field_keeps_a_stable_width() {
        for ms in [0, 3400, 9_900, 10_000, 18_400, 72_000] {
            let field = fmt_duration_field(ms);
            assert_eq!(field.chars().count(), DURATION_FIELD, "{ms} {field:?}");
            assert!(field.contains(fmt_duration(ms).trim()), "{field}");
        }
    }

    #[test]
    fn markdown_html_escapes_raw_tags() {
        let html = markdown_html("hi <script>alert(1)</script> **b**");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("<strong>b</strong>"), "{html}");
    }
}
