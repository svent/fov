//! Modes and keys (§6.2).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_input::InputRequest;

use super::app::{App, Focus, Mode};
use crate::pipeline::has_forbidden_control;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    Cancel,
    Accept,
    /// Copy this command to the clipboard.
    Copy(String),
    /// Quit. `q` carries the shown command, to be printed; Ctrl-C carries nothing.
    Quit(Option<String>),
}

pub fn handle(app: &mut App, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    app.notice = None;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return Action::Quit(None);
    }
    if key.code == KeyCode::Tab {
        app.focus = match app.focus {
            Focus::Query => Focus::Input,
            Focus::Input => Focus::Output,
            Focus::Output => Focus::Query,
        };
        return Action::None;
    }
    if app.focus != Focus::Query && scroll(app, key.code) {
        return Action::None;
    }
    match app.mode {
        Mode::Working => match key.code {
            KeyCode::Esc => Action::Cancel,
            _ => Action::None,
        },
        Mode::Review => match key.code {
            KeyCode::Char('a') if app.acceptable => Action::Accept,
            KeyCode::Char('e') => {
                app.mode = Mode::Compose;
                app.focus = Focus::Query;
                app.query = tui_input::Input::default();
                Action::None
            }
            KeyCode::Char('y') => copy(app),
            KeyCode::Char('q') => Action::Quit(shown_command(app).ok().cloned()),
            _ => Action::None,
        },
        Mode::Compose => compose(app, key, ctrl),
    }
}

/// The shown command, or why it can't leave the TUI: there is none, or a terminal wouldn't
/// show it as it is.
fn shown_command(app: &App) -> Result<&String, &'static str> {
    match &app.command.command {
        None => Err("no command to copy"),
        Some(c) if has_forbidden_control(c) => {
            Err("not copied: the command contains control characters")
        }
        Some(c) => Ok(c),
    }
}

fn copy(app: &mut App) -> Action {
    match shown_command(app) {
        Ok(c) => Action::Copy(c.clone()),
        Err(why) => {
            app.notice = Some(String::from(why));
            Action::None
        }
    }
}

fn compose(app: &mut App, key: KeyEvent, ctrl: bool) -> Action {
    match key.code {
        KeyCode::Enter => {
            let q = app.query.value().trim().to_string();
            if q.is_empty() {
                Action::None
            } else {
                Action::Submit(q)
            }
        }
        KeyCode::Esc => {
            if app.has_proposal() {
                app.mode = Mode::Review;
            }
            Action::None
        }
        _ if app.focus != Focus::Query => Action::None,
        KeyCode::Up => {
            app.query = tui_input::Input::new(app.last_query.clone());
            Action::None
        }
        code => {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let req = match code {
                KeyCode::Char('u') if ctrl => Some(InputRequest::DeleteLine),
                KeyCode::Char('w') if ctrl => Some(InputRequest::DeletePrevWord),
                KeyCode::Char('a') if ctrl => Some(InputRequest::GoToStart),
                KeyCode::Char('e') if ctrl => Some(InputRequest::GoToEnd),
                KeyCode::Char('k') if ctrl => Some(InputRequest::DeleteTillEnd),
                KeyCode::Char('b') if alt => Some(InputRequest::GoToPrevWord),
                KeyCode::Char('f') if alt => Some(InputRequest::GoToNextWord),
                KeyCode::Char(c) if !ctrl && !alt => Some(InputRequest::InsertChar(c)),
                KeyCode::Backspace => Some(InputRequest::DeletePrevChar),
                KeyCode::Delete => Some(InputRequest::DeleteNextChar),
                KeyCode::Left => Some(InputRequest::GoToPrevChar),
                KeyCode::Right => Some(InputRequest::GoToNextChar),
                KeyCode::Home => Some(InputRequest::GoToStart),
                KeyCode::End => Some(InputRequest::GoToEnd),
                _ => None,
            };
            if let Some(req) = req {
                app.query.handle(req);
            }
            Action::None
        }
    }
}

/// Scroll the focused pane. Returns whether the key was a scroll key.
fn scroll(app: &mut App, code: KeyCode) -> bool {
    let pane = if app.focus == Focus::Input {
        &mut app.input
    } else {
        &mut app.output
    };
    let max_y = pane.lines.len().saturating_sub(1).min(u16::MAX as usize) as u16;
    let (y, x) = &mut pane.scroll;
    match code {
        KeyCode::Up => *y = y.saturating_sub(1),
        KeyCode::Down => *y = (*y + 1).min(max_y),
        KeyCode::PageUp => *y = y.saturating_sub(20),
        KeyCode::PageDown => *y = y.saturating_add(20).min(max_y),
        KeyCode::Home => (*y, *x) = (0, 0),
        KeyCode::End => *y = max_y,
        KeyCode::Left => *x = x.saturating_sub(8),
        KeyCode::Right => *x = x.saturating_add(8),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn app() -> App {
        App::new(
            String::from("test · m"),
            true,
            Vec::new(),
            Duration::from_secs(3),
            1 << 20,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn only_q_and_ctrl_c_quit() {
        let mut a = app();
        assert_eq!(
            handle(&mut a, key(KeyCode::Esc)),
            Action::None,
            "Esc in Compose doesn't quit"
        );
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('q'))),
            Action::None,
            "q is typed in Compose"
        );
        assert_eq!(a.query.value(), "q");
        a.mode = Mode::Review;
        assert_eq!(
            handle(&mut a, key(KeyCode::Esc)),
            Action::None,
            "Esc in Review doesn't quit"
        );
        assert_eq!(handle(&mut a, key(KeyCode::Char('q'))), Action::Quit(None));
        a.mode = Mode::Working;
        assert_eq!(handle(&mut a, key(KeyCode::Char('q'))), Action::None);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for mode in [Mode::Compose, Mode::Working, Mode::Review] {
            a.mode = mode;
            assert_eq!(handle(&mut a, ctrl_c), Action::Quit(None), "{mode:?}");
        }
    }

    #[test]
    fn q_quits_with_the_shown_command() {
        let mut a = app();
        a.mode = Mode::Review;
        a.command.command = Some(String::from("sort < f"));
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('q'))),
            Action::Quit(Some(String::from("sort < f")))
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            handle(&mut a, ctrl_c),
            Action::Quit(None),
            "Ctrl-C prints nothing"
        );
        a.command.command = Some(String::from("awk '\x1b[31m'"));
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('q'))),
            Action::Quit(None),
            "a command with control characters isn't printed"
        );
    }

    #[test]
    fn compose_esc_goes_back_to_review() {
        let mut a = app();
        a.command.command = Some(String::from("sort"));
        assert_eq!(handle(&mut a, key(KeyCode::Esc)), Action::None);
        assert_eq!(a.mode, Mode::Review);
    }

    #[test]
    fn copy_the_shown_command() {
        let mut a = app();
        a.mode = Mode::Review;
        assert_eq!(handle(&mut a, key(KeyCode::Char('y'))), Action::None);
        assert_eq!(a.notice.as_deref(), Some("no command to copy"));
        a.command.command = Some(String::from("awk '{\n  print $1\n}' < f | sort"));
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('y'))),
            Action::Copy(String::from("awk '{\n  print $1\n}' < f | sort"))
        );
        a.command.command = Some(String::from("awk '\x1b[31m'"));
        assert_eq!(handle(&mut a, key(KeyCode::Char('y'))), Action::None);
        assert!(a.notice.as_deref().unwrap().contains("control characters"));
        handle(&mut a, key(KeyCode::Tab));
        assert_eq!(a.notice, None, "the next key clears the notice");
        a.mode = Mode::Compose;
        a.focus = Focus::Query;
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('y'))),
            Action::None,
            "y is typed in Compose"
        );
        assert_eq!(a.query.value(), "y");
    }

    #[test]
    fn typing_submit_refine_recall() {
        let mut a = app();
        for c in "  top ips ".chars() {
            handle(&mut a, key(KeyCode::Char(c)));
        }
        assert_eq!(
            handle(&mut a, key(KeyCode::Enter)),
            Action::Submit(String::from("top ips"))
        );
        a.start_turn("top ips", 1);
        assert_eq!(handle(&mut a, key(KeyCode::Char('x'))), Action::None);
        assert_eq!(handle(&mut a, key(KeyCode::Esc)), Action::Cancel);
        a.mode = Mode::Review;
        assert_eq!(
            handle(&mut a, key(KeyCode::Char('a'))),
            Action::None,
            "nothing to accept"
        );
        a.acceptable = true;
        assert_eq!(handle(&mut a, key(KeyCode::Char('a'))), Action::Accept);
        handle(&mut a, key(KeyCode::Char('e')));
        assert_eq!(
            (a.mode, a.query.value()),
            (Mode::Compose, ""),
            "refine opens an empty line"
        );
        handle(&mut a, key(KeyCode::Up));
        assert_eq!(a.query.value(), "top ips");
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(handle(&mut a, ctrl_u), Action::None);
        assert_eq!(a.query.value(), "");
        assert_eq!(
            handle(&mut a, key(KeyCode::Enter)),
            Action::None,
            "empty query is ignored"
        );
    }
}
