//! The settings editor tab: connection, account, models, toggles, skills.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use ratatui::Frame;

use super::{boxed_edit, line};
use crate::tui::app::{App, Focus, Hit};
use crate::tui::edit::display_cols;
use crate::tui::settings::{CatalogStatus, DropKind, Field, LoginUi};
use crate::tui::theme::*;

const LABEL_W: u16 = 20;
const FORM_W: u16 = 84;

/// Field ↔ hit id.
pub(crate) const FIELDS: [Field; 16] = [
    Field::Kind,
    Field::Account,
    Field::Endpoint,
    Field::ApiKey,
    Field::Model,
    Field::ChildModel,
    Field::TaskModel,
    Field::Context,
    Field::Effort,
    Field::Search,
    Field::Dispatcher,
    Field::Ultracode,
    Field::WorkflowSize,
    Field::ImportClaude,
    Field::ImportCodex,
    Field::Skills,
];

pub(crate) fn field_hit(f: Field) -> Hit {
    Hit::SetField(FIELDS.iter().position(|x| *x == f).unwrap_or(0) as u8)
}

fn focus_style(on: bool) -> Style {
    Style::default().fg(if on { ACCENT } else { DIM })
}

fn toggle(f: &mut Frame, app: &mut App, x: u16, y: u16, on: bool, focused: bool, hit: Hit) {
    let r = Rect::new(x, y, 5, 1);
    let style = if on {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(ACCENT).bg(COMPOSER)
    } else {
        Style::default().fg(DIM).bg(COMPOSER)
    };
    line(f, r, if on { " 開 " } else { " 關 " }, style);
    app.ui.hits.push((r, hit));
}

/// The child and task model boxes when they share a row.
const PAIR_LABEL: &str = "子代理／任務代理";

/// Names a box on its top border, for boxes that share a row.
fn box_title(f: &mut Frame, area: Rect, title: &str, focused: bool) {
    let text = format!(" {title} ");
    let w = display_cols(&text).min(area.width.saturating_sub(4));
    line(f, Rect::new(area.x + 2, area.y, w, 1), text, Style::default().fg(if focused { ACCENT } else { DIM }));
}

fn combo(f: &mut Frame, app: &mut App, area: Rect, label: &str, focused: bool, open: bool, hit: Hit) {
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused || open { ACCENT } else { BORDER }))
            .style(Style::default().bg(COMPOSER)),
        area,
    );
    app.ui.hits.push((area, hit));
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
    line(f, Rect::new(inner.x, inner.y, inner.width.saturating_sub(2), 1), label, Style::default().fg(TEXT));
    line(f, Rect::new(inner.right().saturating_sub(2), inner.y, 2, 1), if open { " ▴" } else { " ▾" }, Style::default().fg(DIM));
}

/// Draw the settings tab; returns the caret of the focused text field.
pub(crate) fn draw(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    f.render_widget(Block::default().style(Style::default().bg(PANEL)), area);
    let x = area.x + 2;
    let w = area.width.saturating_sub(4).min(FORM_W);
    let right = x + w;
    let ctl_x = x + LABEL_W;
    let ctl_w = w.saturating_sub(LABEL_W);
    let focused = app.ui.focus == Focus::Settings;
    let field = app.settings.field;
    let openai = app.settings.conn.kind.is_openai();
    let mut y = area.y + 1;
    let mut caret = None;
    let mut drop_anchor = None;
    // The web link keeps the last row however short the terminal: the
    // status bar points here when it has no room for it.
    let link_row = app.web.url.as_ref().map(|_| area.bottom().saturating_sub(1)).filter(|r| *r > area.y);
    let bottom = link_row.unwrap_or(area.bottom());

    line(f, Rect::new(x, y, w, 1), "設定  · Tab 下一欄 · Enter/空白 切換或開啟 · Esc 關閉分頁", Style::default().fg(DIM));
    y += 2;

    // Connection kind.
    line(f, Rect::new(x, y, LABEL_W, 1), "連線", focus_style(focused && field == Field::Kind));
    let mut kx = ctl_x;
    for (i, (label, on)) in [(" Grok ", !openai), (" 自訂 API ", openai)].iter().enumerate() {
        if let Some(r) = super::chip(f, app, kx, y, right, label, *on, Hit::SetKind(i as u8)) {
            kx = r.right() + 1;
        }
    }
    y += 2;

    let row_label = |f: &mut Frame, y: u16, text: &str, on: bool| {
        line(f, Rect::new(x, y + 1, LABEL_W, 1), text, focus_style(on));
    };
    // Down to the last toggle with every box on its own row. A shorter
    // screen puts the child and task model boxes side by side, so the
    // fields, toggles and skills list after them stay in view.
    let boxes: u16 = if openai { 6 } else { 4 };
    let need = y + if openai { 0 } else { 2 } + 3 * boxes + 5;
    // The skills block: a blank row, its header and a list with one row
    // inside its border (plus the spare row it keeps with no link).
    let skills_need = 5 + u16::from(link_row.is_none());
    let pair = need + skills_need > bottom && ctl_w >= 40;
    let pair_w = (ctl_w.saturating_sub(1) / 2).min(40);

    if !openai {
        // Account.
        line(f, Rect::new(x, y, LABEL_W, 1), "帳號", focus_style(focused && field == Field::Account));
        let (btn, note) = match &app.settings.login {
            LoginUi::Starting => (" 連線中… ".to_string(), "正在向 xAI 取得登入代碼".to_string()),
            LoginUi::Waiting { .. } => (" 取消 ".to_string(), String::new()),
            LoginUi::Failed(e) => (
                if app.settings.xai_ready { " 登出 " } else { " 登入 Grok " }.to_string(),
                format!("失敗：{e}"),
            ),
            LoginUi::Idle if app.settings.xai_ready => (" 登出 ".into(), "已登入".into()),
            LoginUi::Idle => (" 登入 Grok ".into(), "未登入".into()),
        };
        let acc_on = focused && field == Field::Account;
        if let Some(r) = super::chip(f, app, ctl_x, y, right, &btn, acc_on, Hit::SetAccount) {
            if let LoginUi::Waiting { user_code, .. } = &app.settings.login {
                let code = format!(" {user_code} ");
                if let Some(c) = super::chip(f, app, r.right() + 2, y, right, &code, true, Hit::SetLoginCode) {
                    line(f, Rect::new(c.right() + 1, y, right.saturating_sub(c.right() + 1), 1), "點此複製 · 已開瀏覽器", Style::default().fg(DIM));
                }
            } else {
                line(f, Rect::new(r.right() + 2, y, right.saturating_sub(r.right() + 2), 1), note, Style::default().fg(DIM));
            }
        }
        y += 2;
        let status = match &app.settings.catalog_status {
            CatalogStatus::Loading => "  （目錄載入中…）".to_string(),
            CatalogStatus::Failed(e) => format!("  （目錄失敗：{e}）"),
            _ => String::new(),
        };
        // Model, child model, task model, effort.
        let model = (Field::Model, format!("模型{status}"), app.model_label(), DropKind::Model, "");
        let child = (Field::ChildModel, "子代理模型".to_string(), app.child_model_label(), DropKind::ChildModel, "子代理");
        let task = (Field::TaskModel, "任務代理模型".to_string(), app.task_model_label(), DropKind::TaskModel, "任務代理");
        let effort = (Field::Effort, "思考強度".to_string(), app.effort_label(), DropKind::Effort, "");
        let rows = if pair {
            vec![vec![model], vec![child, task], vec![effort]]
        } else {
            vec![vec![model], vec![child], vec![task], vec![effort]]
        };
        for row in rows {
            if y + 3 > bottom {
                break;
            }
            let shared = row.len() > 1;
            let on_row = row.iter().any(|c| focused && field == c.0);
            row_label(f, y, if shared { PAIR_LABEL } else { &row[0].1 }, on_row);
            let w = if shared { pair_w } else { ctl_w.min(40) };
            for (i, (fld, _, value, kind, title)) in row.into_iter().enumerate() {
                let on = focused && field == fld;
                let r = Rect::new(ctl_x + i as u16 * (w + 1), y, w, 3);
                let open = app.settings.drop == Some(kind);
                combo(f, app, r, &value, on, open, field_hit(fld));
                if shared {
                    box_title(f, r, title, on);
                }
                if open {
                    drop_anchor = Some(r);
                }
            }
            y += 3;
        }
        y += 1;
    } else {
        let has_list = !app.settings.custom_catalog.models.is_empty();
        let endpoint = app.settings.endpoint.clone();
        let api_key = app.settings.api_key.clone();
        let context = app.settings.context.clone();
        let model_edit = app.settings.model_edit.clone();
        let child_edit = app.settings.child_model_edit.clone();
        let task_edit = app.settings.task_model_edit.clone();
        let model_title = if has_list {
            "模型".to_string()
        } else if app.settings.custom_err.is_some() {
            "模型名（手動）".to_string()
        } else {
            "模型名".to_string()
        };
        let label_of = |fld: Field| -> String {
            match fld {
                Field::Endpoint => "端點".into(),
                Field::ApiKey => "API 金鑰（選填）".into(),
                Field::Model => model_title.clone(),
                Field::ChildModel if has_list => "子代理模型".into(),
                Field::ChildModel => "子代理（空=同主）".into(),
                Field::TaskModel if has_list => "任務代理模型".into(),
                Field::TaskModel => "任務代理（空=同主）".into(),
                _ => "上下文（如 262K）".into(),
            }
        };
        let rows: Vec<Vec<Field>> = if pair {
            vec![vec![Field::Endpoint], vec![Field::ApiKey], vec![Field::Model], vec![Field::ChildModel, Field::TaskModel], vec![Field::Context]]
        } else {
            [Field::Endpoint, Field::ApiKey, Field::Model, Field::ChildModel, Field::TaskModel, Field::Context].into_iter().map(|f| vec![f]).collect()
        };
        for row in rows {
            if y + 3 > bottom {
                break;
            }
            let shared = row.len() > 1;
            let on_row = row.iter().any(|c| focused && field == *c);
            row_label(f, y, &if shared { PAIR_LABEL.to_string() } else { label_of(row[0]) }, on_row);
            let w = if shared { pair_w } else { ctl_w };
            for (i, fld) in row.into_iter().enumerate() {
                let on = focused && field == fld;
                let r = Rect::new(ctl_x + i as u16 * (w + 1), y, w, 3);
                match fld {
                    Field::Model | Field::ChildModel | Field::TaskModel if has_list => {
                        let (kind, value) = match fld {
                            Field::Model => (DropKind::Model, app.model_label()),
                            Field::ChildModel => (DropKind::ChildModel, app.child_model_label()),
                            _ => (DropKind::TaskModel, app.task_model_label()),
                        };
                        let open = app.settings.drop == Some(kind);
                        combo(f, app, Rect::new(r.x, r.y, r.width.min(48), 3), &value, on, open, field_hit(fld));
                        if open {
                            drop_anchor = Some(r);
                        }
                    }
                    _ => {
                        let edit = match fld {
                            Field::Endpoint => &endpoint,
                            Field::ApiKey => &api_key,
                            Field::Model => &model_edit,
                            Field::ChildModel => &child_edit,
                            Field::TaskModel => &task_edit,
                            _ => &context,
                        };
                        let pos = boxed_edit(f, app, r, edit, on, field_hit(fld), fld == Field::ApiKey);
                        if on {
                            caret = Some(pos);
                        }
                    }
                }
                if shared {
                    let title = match (fld, has_list) {
                        (Field::ChildModel, true) => "子代理",
                        (Field::ChildModel, false) => "子代理（空=同主）",
                        (_, true) => "任務代理",
                        (_, false) => "任務代理（空=同主）",
                    };
                    box_title(f, r, title, on);
                }
            }
            y += 3;
        }
        y += 1;
    }

    if y < bottom {
        let foc = focused && field == Field::Search;
        line(f, Rect::new(x, y, LABEL_W, 1), "搜尋（web + X）", focus_style(foc));
        toggle(f, app, ctl_x, y, app.opts.web_search, foc, Hit::SetToggle(0));
        let (hint, style) = search_hint(app);
        line(f, Rect::new(ctl_x + 7, y, ctl_w.saturating_sub(7), 1), hint, style);
        y += 1;
    }
    for (fld, label, on, id) in [
        (Field::Dispatcher, "調度員模式", app.opts.dispatcher, 1u8),
        (Field::ImportClaude, "引入 Claude 技能", app.skills_pref(true), 2),
        (Field::ImportCodex, "引入 Codex 技能", app.skills_pref(false), 3),
    ] {
        if y >= bottom {
            break;
        }
        let foc = focused && field == fld;
        line(f, Rect::new(x, y, LABEL_W, 1), label, focus_style(foc));
        toggle(f, app, ctl_x, y, on, foc, Hit::SetToggle(id));
        if fld == Field::Dispatcher {
            // Workflows share the row: Ultracode (the standing opt-in) and
            // the size guideline it scales.
            let mut cx = ctl_x + 7;
            let room = (ctl_x + ctl_w).saturating_sub(cx);
            let foc_u = focused && field == Field::Ultracode;
            let foc_s = focused && field == Field::WorkflowSize;
            if room >= 32 {
                line(f, Rect::new(cx, y, 10, 1), "Ultracode", focus_style(foc_u));
                cx += 10;
                toggle(f, app, cx, y, app.opts.ultracode, foc_u, Hit::SetToggle(4));
                cx += 7;
                line(f, Rect::new(cx, y, 5, 1), "規模", focus_style(foc_s));
                cx += 5;
                let size = app.opts.workflow_size;
                let text = if (ctl_x + ctl_w).saturating_sub(cx) >= 24 { size.label() } else { size.short() };
                let r = Rect::new(cx, y, (ctl_x + ctl_w).saturating_sub(cx).min(display_cols(text) + 4), 1);
                let style = if foc_s { Style::default().fg(ACCENT).bg(COMPOSER) } else { Style::default().fg(DIM).bg(COMPOSER) };
                line(f, r, &format!(" ‹{text}› "), style);
                app.ui.hits.push((r, field_hit(Field::WorkflowSize)));
            } else {
                line(f, Rect::new(cx, y, room, 1), "主代理只規劃、指揮子代理", Style::default().fg(DIM));
            }
        }
        y += 1;
    }
    y += 1;

    // Skills.
    if y + 2 < bottom {
        let foc = focused && field == Field::Skills;
        line(f, Rect::new(x, y, w, 1), "技能（空白鍵 開/關 · Enter 查看）", focus_style(foc));
        y += 1;
        let list = Rect::new(x, y, w, bottom.saturating_sub(y + u16::from(link_row.is_none())));
        skills(f, app, list, foc);
    }
    if let Some(r) = link_row {
        web_link(f, app, Rect::new(x, r, w, 1));
    }

    if let (Some(kind), Some(anchor)) = (app.settings.drop, drop_anchor) {
        drop_list(f, app, kind, anchor, area);
    }
    if !focused || app.settings.drop.is_some() {
        return None;
    }
    caret
}

/// The web workbench's current link, for terminals too narrow for the status
/// bar to show it. Never clipped: a cut one-time code opens nothing.
fn web_link(f: &mut Frame, app: &App, r: Rect) {
    let Some(url) = &app.web.url else {
        return;
    };
    let full = format!("網頁界面  {url}  · 每個網址只能用一次");
    let text = [full.as_str(), url.as_str()]
        .into_iter()
        .find(|t| display_cols(t) <= r.width)
        .unwrap_or("網頁界面  視窗太窄，放寬後顯示網址");
    line(f, r, text, Style::default().fg(DIM));
}

/// How search reaches the current model: Grok's own tools, or `grok_search`.
fn search_hint(app: &App) -> (&'static str, Style) {
    let dim = Style::default().fg(DIM);
    if !app.settings.conn.route_for(&app.opts.model).is_openai() {
        ("Grok 內建搜尋", dim)
    } else if app.settings.xai_ready {
        ("grok_search：借用已登入的 Grok 代查", dim)
    } else {
        ("需登入 Grok 才能借用搜尋", Style::default().fg(WARN))
    }
}

fn skills(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
            .style(Style::default().bg(COMPOSER)),
        area,
    );
    app.ui.hits.push((area, field_hit(Field::Skills)));
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    if inner.area() == 0 {
        return;
    }
    if app.settings.skill_list.is_empty() {
        line(f, inner, "尚無技能 — 請模型寫一個，或開啟上方引入", Style::default().fg(DIM));
        return;
    }
    let vis = inner.height as usize;
    let st = &mut app.settings;
    if st.skill_cursor < st.skill_scroll {
        st.skill_scroll = st.skill_cursor;
    } else if st.skill_cursor >= st.skill_scroll + vis {
        st.skill_scroll = st.skill_cursor + 1 - vis;
    }
    let rows: Vec<(usize, String, bool)> = st
        .skill_list
        .iter()
        .enumerate()
        .skip(st.skill_scroll)
        .take(vis)
        .map(|(i, s)| (i, format!("{}  · {}", s.name, s.origin.label()), s.enabled))
        .collect();
    let cursor = st.skill_cursor;
    for (row, (i, label, enabled)) in rows.into_iter().enumerate() {
        let y = inner.y + row as u16;
        toggle(f, app, inner.x, y, enabled, false, Hit::SkillToggle(i as u16));
        let rest = Rect::new(inner.x + 6, y, inner.width.saturating_sub(6), 1);
        let style = if focused && i == cursor {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        line(f, rest, label, style);
        app.ui.hits.push((rest, Hit::SkillRow(i as u16)));
    }
}

fn drop_list(f: &mut Frame, app: &mut App, kind: DropKind, anchor: Rect, clip: Rect) {
    let labels: Vec<String> = match kind {
        DropKind::Model => app.model_choices().into_iter().map(|(id, n)| if n.is_empty() { id } else { n }).collect(),
        DropKind::ChildModel => app.child_model_choices().into_iter().map(|(id, n)| if n.is_empty() { id } else { n }).collect(),
        DropKind::TaskModel => app.task_model_choices().into_iter().map(|(id, n)| if n.is_empty() { id } else { n }).collect(),
        DropKind::Effort => app.effort_choices().into_iter().map(|e| e.label).collect(),
    };
    if labels.is_empty() {
        return;
    }
    let vis = DROP_VISIBLE.min(labels.len()) as u16;
    let h = vis + 2;
    let mut y = anchor.bottom();
    if y + h > clip.bottom() {
        y = anchor.y.saturating_sub(h).max(clip.y);
    }
    let r = Rect::new(anchor.x, y, anchor.width.min(48), h.min(clip.bottom().saturating_sub(y)));
    if r.height < 3 {
        return;
    }
    f.render_widget(Clear, r);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        r,
    );
    let inner = Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), r.height - 2);
    let start = app.settings.drop_scroll;
    for (row, i) in (start..(start + inner.height as usize).min(labels.len())).enumerate() {
        let cell = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let style = if i == app.settings.drop_cursor {
            Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(COMPOSER)
        };
        line(f, cell, format!(" {}", labels[i]), style);
        app.ui.hits.push((cell, Hit::DropPick(i as u16)));
    }
}
