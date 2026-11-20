//! Drawing (§6.1). Both layouts have the same panes; only the arrangement differs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::app::{App, Focus, Mode, Pane, sanitize};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn draw(f: &mut Frame, app: &App, side_by_side: bool) {
    let area = f.area();
    let status_height = if side_by_side { 1 } else { 2 };
    let command = command_text(app);
    let inner_width = area.width.saturating_sub(2).max(1);
    let wanted = Paragraph::new(command.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner_width) as u16
        + 2;
    let command_height = wanted.clamp(3, (area.height / 3).max(3));
    let [panes, command_area, query_area, status_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(command_height),
        Constraint::Length(1),
        Constraint::Length(status_height),
    ])
    .areas(area);
    let [input_area, output_area] = if side_by_side {
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(panes)
    } else {
        Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(panes)
    };

    pane(
        f,
        &app.input,
        input_area,
        app.focus == Focus::Input,
        "reading…",
    );
    pane(
        f,
        &app.output,
        output_area,
        app.focus == Focus::Output,
        "no preview yet",
    );
    f.render_widget(
        Paragraph::new(command)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Command ")),
        command_area,
    );
    query_line(f, app, query_area);
    status(f, app, status_area, side_by_side);
}

fn pane(f: &mut Frame, p: &Pane, area: Rect, focused: bool, empty: &str) {
    let mut block = Block::bordered().title(format!(" {} ", p.title));
    if focused {
        block = block.border_style(Style::new().cyan());
    }
    let text = if p.lines.is_empty() {
        Text::from(Line::from(empty.to_string()).dark_gray())
    } else {
        Text::from(p.lines.clone())
    };
    f.render_widget(Paragraph::new(text).block(block).scroll(p.scroll), area);
}

fn command_text(app: &App) -> Text<'static> {
    let c = &app.command;
    let mut lines = Vec::new();
    if let Some(cmd) = &c.command {
        // An argument may hold newlines (an awk program); show them as line breaks.
        for l in cmd.split('\n') {
            lines.push(Line::from(sanitize(&l.replace('\t', "    "))).bold());
        }
    }
    if let Some(e) = &c.explanation {
        lines.push(Line::from(format!("↳ {}", sanitize(e))).dark_gray());
    }
    if let Some(p) = &c.problem {
        lines.push(Line::from(format!("✗ {}", sanitize(p))).red());
    }
    if lines.is_empty() {
        lines.push(Line::from("Describe the output you want and press Enter.").dark_gray());
    }
    Text::from(lines)
}

fn query_line(f: &mut Frame, app: &App, area: Rect) {
    let prompt = "> ";
    let width = area.width.saturating_sub(prompt.len() as u16 + 1) as usize;
    let scroll = app.query.visual_scroll(width);
    let value: String = app.query.value().chars().skip(scroll).collect();
    let mut line = Line::from(vec![
        Span::from(prompt).cyan(),
        Span::from(sanitize(&value)),
    ]);
    if app.mode != Mode::Compose {
        line = line.dark_gray();
    }
    f.render_widget(Paragraph::new(line), area);
    if app.mode == Mode::Compose && app.focus == Focus::Query {
        let x = area.x
            + prompt.len() as u16
            + (app.query.visual_cursor().saturating_sub(scroll)) as u16;
        f.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
    }
}

fn status(f: &mut Frame, app: &App, area: Rect, side_by_side: bool) {
    let mut info = vec![Span::from(format!(" {} · ", sanitize(&app.label)))];
    if app.sandboxed {
        info.push(Span::from("sandbox on"));
    } else {
        info.push(Span::from(" UNSANDBOXED ").white().on_red().bold());
    }
    if let Some(e) = &app.error {
        info.push(Span::from(format!(" · {}", sanitize(e))).red());
    }
    for w in &app.warnings {
        info.push(Span::from(format!(" · {}", sanitize(w))).yellow());
    }
    if let Some(n) = &app.notice {
        info.push(Span::from(format!(" · {}", sanitize(n))).green());
    }
    let hints = hints(app);
    if side_by_side {
        info.push(Span::from("   "));
        info.extend(hints.spans);
        f.render_widget(Paragraph::new(Line::from(info)), area);
    } else {
        let [top, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        f.render_widget(Paragraph::new(Line::from(info)), top);
        let mut hints = hints;
        hints.spans.insert(0, Span::from(" "));
        f.render_widget(Paragraph::new(hints), bottom);
    }
}

fn hints(app: &App) -> Line<'static> {
    let key = |k: &str, what: &str| {
        vec![
            Span::from(k.to_string()).bold(),
            Span::from(format!(" {what}")),
        ]
    };
    let sep = || Span::from(" · ").dark_gray();
    let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
    match app.mode {
        Mode::Compose => {
            parts.push(key("Enter", "send"));
            parts.push(key("Tab", "focus"));
            if app.has_proposal() {
                parts.push(key("Esc", "back"));
            }
            parts.push(key("Ctrl-C", "quit"));
        }
        Mode::Working => {
            let spin = SPINNER[app.spinner % SPINNER.len()];
            parts.push(vec![Span::from(format!("{spin} {}", app.step)).cyan()]);
            parts.push(key("Esc", "cancel"));
        }
        Mode::Review => {
            let mut accept = key("a", "accept");
            if !app.acceptable {
                accept = accept.into_iter().map(|s| s.dark_gray()).collect();
            }
            parts.push(accept);
            parts.push(key("e", "refine"));
            parts.push(key("y", "copy"));
            parts.push(key("Tab", "focus"));
            parts.push(key("q", "quit"));
        }
    }
    let mut spans = Vec::new();
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(sep());
        }
        spans.extend(p);
    }
    Line::from(spans)
}
