//! The terminal is reached through /dev/tty, never through fd 0–2, which may all be
//! redirected (§5.2). fd 0 stays the data source.

use std::fs::{File, OpenOptions};
use std::io;

use anyhow::{Result, anyhow};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type Backend = CrosstermBackend<File>;

pub fn open_tty() -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| anyhow!("no terminal: fov needs /dev/tty for its interface ({e})"))
}

/// The full-screen TUI on /dev/tty. Restores the terminal when dropped.
pub struct Screen {
    pub terminal: Terminal<Backend>,
    tty: File,
    active: bool,
}

impl Screen {
    pub fn enter(tty: File) -> io::Result<Screen> {
        terminal::enable_raw_mode()?;
        let mut out = tty.try_clone()?;
        execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        Ok(Screen {
            terminal: Terminal::new(CrosstermBackend::new(tty.try_clone()?))?,
            tty,
            active: true,
        })
    }

    pub fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = execute!(
            self.tty,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

fn restore() {
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        let _ = execute!(
            tty,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            cursor::Show
        );
    }
    let _ = terminal::disable_raw_mode();
}

/// §6.1: side by side when the terminal is physically wider than tall. Uses the pixel size
/// when the terminal reports one, otherwise assumes cells twice as tall as wide.
pub fn side_by_side(columns: u16, rows: u16, pixels: Option<(u16, u16)>) -> bool {
    match pixels {
        Some((w, h)) if w > 0 && h > 0 => w >= h,
        _ => u32::from(columns) >= 2 * u32::from(rows),
    }
}

/// The layout for the current terminal size.
pub fn current_side_by_side() -> bool {
    match terminal::window_size() {
        Ok(s) => side_by_side(s.columns, s.rows, Some((s.width, s.height))),
        Err(_) => terminal::size()
            .map(|(c, r)| side_by_side(c, r, None))
            .unwrap_or(true),
    }
}

#[cfg(test)]
mod tests {
    use super::side_by_side;

    #[test]
    fn layout_rule() {
        // Cell fallback: columns ≥ 2 × rows.
        assert!(side_by_side(200, 50, None));
        assert!(!side_by_side(90, 55, None));
        assert!(side_by_side(100, 50, None));
        assert!(!side_by_side(99, 50, None));
        // Pixel size wins when reported, on both sides of the threshold.
        assert!(!side_by_side(200, 50, Some((800, 900))));
        assert!(side_by_side(90, 55, Some((1000, 990))));
        assert!(side_by_side(90, 55, Some((1000, 1000))));
        // A zero pixel size means "not reported".
        assert!(!side_by_side(90, 55, Some((0, 0))));
    }
}
