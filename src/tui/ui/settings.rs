//! The settings editor tab: connection, account, models, toggles, skills.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use ratatui::Frame;

use super::{boxed_edit, line};
use crate::tui::app::{App, Focus, Hit};
use crate::tui::settings::{CatalogStatus, DropKind, Field, LoginUi};
use crate::tui::theme::*;

const LABEL_W: u16 = 18;
const FORM_W: u16 = 84;

/// Field ↔ hit id.
pub(crate) const FIELDS: [Field; 13] = [
    Field::Kind,
    Field::Account,
    Field::Endpoint,
    Field::ApiKey,
    Field::Model,
    Field::ChildModel,
    Field::Context,
    Field::Effort,
    Field::Search,
    Field::Dispatcher,
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
    let bottom = area.bottom();

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
        // Model, child model, effort.
        for (fld, label, value, kind) in [
            (Field::Model, format!("模型{status}"), app.model_label(), DropKind::Model),
            (Field::ChildModel, "子代理模型".to_string(), app.child_model_label(), DropKind::ChildModel),
            (Field::Effort, "思考強度".to_string(), app.effort_label(), DropKind::Effort),
        ] {
            if y + 3 > bottom {
                break;
            }
            row_label(f, y, &label, focused && field == fld);
            let r = Rect::new(ctl_x, y, ctl_w.min(40), 3);
            let open = app.settings.drop == Some(kind);
            combo(f, app, r, &value, focused && field == fld, open, field_hit(fld));
            if open {
                drop_anchor = Some(r);
            }
            y += 3;
        }
        if y < bottom {
            line(f, Rect::new(x, y, LABEL_W, 1), "搜尋（web + X）", focus_style(focused && field == Field::Search));
            toggle(f, app, ctl_x, y, app.opts.web_search, focused && field == Field::Search, Hit::SetToggle(0));
            y += 2;
        }
    } else {
        let has_list = !app.settings.custom_catalog.models.is_empty();
        let endpoint = app.settings.endpoint.clone();
        let api_key = app.settings.api_key.clone();
        let context = app.settings.context.clone();
        let model_edit = app.settings.model_edit.clone();
        let child_edit = app.settings.child_model_edit.clone();
        let model_title = if has_list {
            "模型".to_string()
        } else if app.settings.custom_err.is_some() {
            "模型名（手動）".to_string()
        } else {
            "模型名".to_string()
        };
        let rows: Vec<(Field, String)> = vec![
            (Field::Endpoint, "端點".into()),
            (Field::ApiKey, "API 金鑰（選填）".into()),
            (Field::Model, model_title),
            (Field::ChildModel, if has_list { "子代理模型".into() } else { "子代理（空=同主）".into() }),
            (Field::Context, "上下文（如 262K）".into()),
        ];
        for (fld, label) in rows {
            if y + 3 > bottom {
                break;
            }
            let on = focused && field == fld;
            row_label(f, y, &label, on);
            let r = Rect::new(ctl_x, y, ctl_w, 3);
            match fld {
                Field::Model | Field::ChildModel if has_list => {
                    let kind = if fld == Field::Model { DropKind::Model } else { DropKind::ChildModel };
                    let value = if fld == Field::Model { app.model_label() } else { app.child_model_label() };
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
                        _ => &context,
                    };
                    let pos = boxed_edit(f, app, r, edit, on, field_hit(fld), fld == Field::ApiKey);
                    if on {
                        caret = Some(pos);
                    }
                }
            }
            y += 3;
        }
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
            line(f, Rect::new(ctl_x + 7, y, ctl_w.saturating_sub(7), 1), "主代理只規劃、指揮子代理", Style::default().fg(DIM));
        }
        y += 1;
    }
    y += 1;

    // Skills.
    if y + 2 < bottom {
        let foc = focused && field == Field::Skills;
        line(f, Rect::new(x, y, w, 1), "技能（空白鍵 開/關 · Enter 查看）", focus_style(foc));
        y += 1;
        let list = Rect::new(x, y, w, bottom.saturating_sub(y + 1));
        skills(f, app, list, foc);
    }

    if let (Some(kind), Some(anchor)) = (app.settings.drop, drop_anchor) {
        drop_list(f, app, kind, anchor, area);
    }
    if !focused || app.settings.drop.is_some() {
        return None;
    }
    caret
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
