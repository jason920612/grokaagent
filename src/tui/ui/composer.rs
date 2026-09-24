//! The main chat's composer: send-mode chips, queue, attachments, text box.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::{chip, draw_edit, line, truncate};
use crate::tui::app::{App, Focus, Hit, SendMode};
use crate::tui::edit::display_cols;
use crate::tui::theme::*;

const QUEUE_SHOWN: usize = 3;

pub(crate) fn height(app: &App) -> u16 {
    let s = app.cur();
    let queue = s.queue.len().min(QUEUE_SHOWN) as u16;
    let attach = u16::from(!s.pending.is_empty());
    let extra = s.draft.text.lines().count().saturating_sub(1).min(3) as u16;
    6 + queue + attach + extra
}

pub(crate) fn draw(f: &mut Frame, app: &mut App, area: Rect) -> Position {
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG)),
        area,
    );
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(1));
    if inner.height < 4 {
        return Position::new(inner.x, inner.y);
    }
    let right = inner.right();
    let (queue_len, running, editing, pending) = {
        let s = app.cur();
        (s.queue.len(), s.chat.running, s.queue_edit, s.pending.clone())
    };
    let mut y = inner.y;

    // Chips: send mode, paste image, stop, queue-edit state.
    let queue_label = if queue_len == 0 {
        " 接著做 ".to_string()
    } else {
        format!(" {queue_len} 已排隊 ")
    };
    let mode = app.ui.send_mode;
    let mut x = inner.x;
    for (label, on, hit) in [
        (queue_label.as_str(), mode == SendMode::Queue, Hit::QueueChip),
        (" 調整工作 ", mode == SendMode::Insert, Hit::InsertChip),
        (" 貼上圖片 ", false, Hit::PasteImage),
    ] {
        if let Some(r) = chip(f, app, x, y, right, label, on, hit) {
            x = r.right() + 1;
        }
    }
    if running || app.cur().agents.live_count() > 0 {
        let label = if running { " 停止 Esc " } else { " 中斷子代理 " };
        let w = display_cols(label);
        if x + w <= right {
            let r = Rect::new(x, y, w, 1);
            f.render_widget(Paragraph::new(Span::styled(label, Style::default().fg(WARN).bg(COMPOSER))), r);
            app.ui.hits.push((r, Hit::StopChip));
            x += w + 1;
        }
    }
    if let Some(i) = editing {
        let label = format!(" 編輯排隊 #{} ", i + 1);
        line(f, Rect::new(x, y, display_cols(&label).min(right.saturating_sub(x)), 1), label.clone(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
        x += display_cols(&label) + 1;
        let cancel = " 取消 ";
        let cx = if x + 6 <= right { x } else { right.saturating_sub(6) };
        let r = Rect::new(cx, y, 6, 1);
        f.render_widget(Paragraph::new(Span::styled(cancel, Style::default().bg(WARN).fg(ratatui::style::Color::Black))), r);
        app.ui.hits.push((r, Hit::CancelQueueEdit));
    }
    y += 1;

    // Queue.
    for i in 0..queue_len.min(QUEUE_SHOWN) {
        let s = app.cur();
        let (text, style, mark) = if editing == Some(i) {
            (s.draft.text.replace('\n', " "), Style::default().fg(ACCENT), "▸")
        } else {
            (s.queue[i].label(), Style::default().fg(DIM), "•")
        };
        let r = Rect::new(inner.x, y, inner.width, 1);
        line(f, r, format!(" {mark} {text}"), style);
        app.ui.hits.push((r, Hit::QueueItem(i as u16)));
        y += 1;
    }

    // Attachments.
    if !pending.is_empty() {
        let mut x = inner.x;
        for (i, rel) in pending.iter().enumerate() {
            let name = std::path::Path::new(rel).file_name().and_then(|s| s.to_str()).unwrap_or(rel);
            let label = format!(" {name} × ");
            let w = display_cols(&label);
            if x + w > right {
                break;
            }
            let r = Rect::new(x, y, w, 1);
            f.render_widget(Paragraph::new(Span::styled(label, Style::default().bg(COMPOSER).fg(ACCENT))), r);
            app.ui.hits.push((r, Hit::PendingClose(i as u16)));
            x += w + 1;
        }
        y += 1;
    }

    // Text box and hint.
    let box_h = inner.bottom().saturating_sub(y + 1).max(3);
    let boxr = Rect::new(inner.x, y, inner.width, box_h);
    let focused = app.ui.focus == Focus::Chat;
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        boxr,
    );
    app.ui.hits.push((boxr, Hit::Composer));
    let text_area = Rect::new(boxr.x + 1, boxr.y + 1, boxr.width.saturating_sub(2), boxr.height.saturating_sub(2));
    app.ui.composer_inner = text_area;
    let caret = if app.cur().draft.is_empty() {
        let placeholder = if editing.is_some() {
            "編輯排隊訊息…  Enter 完成  ·  空白則移除  ·  Esc 還原"
        } else if running {
            "模型工作中 · Esc 停止 · Enter 依所選模式送出 · Ctrl+Enter 調整工作"
        } else {
            "傳訊息，或點「貼上圖片」…"
        };
        f.render_widget(
            Paragraph::new(Span::styled(placeholder, Style::default().fg(DIM).bg(COMPOSER))),
            text_area,
        );
        app.ui.composer_vscroll = 0;
        Position::new(text_area.x, text_area.y)
    } else {
        let mut vs = app.ui.composer_vscroll;
        let pos = draw_edit(f, text_area, &app.cur().draft, &mut vs, COMPOSER);
        app.ui.composer_vscroll = vs;
        pos
    };
    let hint = if editing.is_some() {
        "Enter 完成編輯 · Esc 或點取消 還原 · 編輯完成前不會送出".to_string()
    } else {
        format!(
            "Enter 送出 · Shift+Enter 換行 · Ctrl+Enter 調整工作 · Esc 停止 · {} · {}",
            app.opts.model,
            app.opts.reasoning_effort.label()
        )
    };
    let hint_y = boxr.bottom();
    if hint_y < area.bottom() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate(&hint, inner.width), Style::default().fg(DIM)))),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
    caret
}
