//! TUI state, and how session events change it (§6).

use std::time::Duration;

use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::exec::{Outcome, Run};
use crate::input::{Input, Progress, Scope};
use crate::prompt::thousands;
use crate::session::{EventKind, MAX_REPAIRS, TurnResult};

const VIEW_LINES: usize = 1000;
const VIEW_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Compose,
    Working,
    Review,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Query,
    Input,
    Output,
}

#[derive(Clone, Debug, Default)]
pub struct Pane {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    pub scroll: (u16, u16),
}

#[derive(Clone, Debug, Default)]
pub struct CommandView {
    pub command: Option<String>,
    pub explanation: Option<String>,
    pub problem: Option<String>,
}

impl CommandView {
    pub fn is_empty(&self) -> bool {
        self.command.is_none() && self.explanation.is_none() && self.problem.is_none()
    }
}

/// The parts a discarded turn must not change.
#[derive(Clone, Debug)]
struct Saved {
    command: CommandView,
    output: Pane,
    acceptable: bool,
}

pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub query: tui_input::Input,
    pub last_query: String,
    pub step: String,
    pub spinner: usize,
    pub input: Pane,
    pub output: Pane,
    pub command: CommandView,
    /// A transport error or similar, shown in the status line until the next submit.
    pub error: Option<String>,
    /// A short confirmation (e.g. "copied"), shown until the next key.
    pub notice: Option<String>,
    pub warnings: Vec<String>,
    /// "openrouter · <model>".
    pub label: String,
    pub sandboxed: bool,
    pub acceptable: bool,
    pub turn: u64,
    pub timeout: Duration,
    pub sample_bytes: u64,
    input_loaded: bool,
    saved: Option<Saved>,
}

impl App {
    pub fn new(
        label: String,
        sandboxed: bool,
        warnings: Vec<String>,
        timeout: Duration,
        sample_bytes: u64,
    ) -> App {
        App {
            mode: Mode::Compose,
            focus: Focus::Query,
            query: tui_input::Input::default(),
            last_query: String::new(),
            step: String::new(),
            spinner: 0,
            input: Pane {
                title: String::from("Input"),
                ..Pane::default()
            },
            output: Pane {
                title: String::from("Output"),
                ..Pane::default()
            },
            command: CommandView::default(),
            error: None,
            notice: None,
            warnings,
            label,
            sandboxed,
            acceptable: false,
            turn: 0,
            timeout,
            sample_bytes,
            input_loaded: false,
            saved: None,
        }
    }

    pub fn has_proposal(&self) -> bool {
        !self.command.is_empty()
    }

    pub fn start_turn(&mut self, query: &str, turn: u64) {
        self.saved = Some(Saved {
            command: self.command.clone(),
            output: self.output.clone(),
            acceptable: self.acceptable,
        });
        self.mode = Mode::Working;
        self.last_query = query.to_string();
        self.step = String::from("asking model");
        self.error = None;
        self.turn = turn;
    }

    /// Esc while working: the turn is discarded, the query kept.
    pub fn cancel_turn(&mut self) {
        self.restore();
        self.mode = Mode::Compose;
    }

    fn restore(&mut self) {
        if let Some(s) = self.saved.take() {
            self.command = s.command;
            self.output = s.output;
            self.acceptable = s.acceptable;
        }
    }

    /// Apply a session event of the current turn. `acceptable` is whether the session now has
    /// a pipeline that passed validation (only read on Done).
    pub fn on_event(&mut self, kind: EventKind, acceptable: bool) {
        match kind {
            EventKind::Thinking => self.step = String::from("asking model"),
            EventKind::Repairing { attempt, reason } => {
                self.step = format!("repairing {attempt}/{MAX_REPAIRS}: {}", sanitize(&reason));
            }
            EventKind::Proposed {
                command,
                explanation,
            } => {
                self.command = CommandView {
                    command: Some(command),
                    explanation: Some(explanation),
                    problem: None,
                };
            }
            EventKind::Rejected {
                command,
                explanation,
                reason,
            } => {
                self.command = CommandView {
                    command,
                    explanation,
                    problem: Some(reason),
                };
            }
            EventKind::Declined { explanation } => {
                self.command = CommandView {
                    command: None,
                    explanation: Some(explanation),
                    problem: None,
                };
                self.output = Pane {
                    title: String::from("Output"),
                    ..Pane::default()
                };
            }
            EventKind::WaitingForInput => self.step = String::from("waiting for input"),
            EventKind::Running { .. } => self.step = String::from("running preview"),
            EventKind::Ran { run } => {
                self.output = output_pane(&run, self.timeout, self.sample_bytes)
            }
            EventKind::Done { result } => {
                match result {
                    TurnResult::Ok | TurnResult::Declined => self.mode = Mode::Review,
                    TurnResult::Failed { reason } => {
                        self.mode = Mode::Review;
                        if self.command.problem.is_none() {
                            self.command.problem = Some(reason);
                        }
                    }
                    TurnResult::Error { message } => {
                        self.restore();
                        self.mode = Mode::Compose;
                        self.error = Some(message);
                        return;
                    }
                }
                self.saved = None;
                self.acceptable = acceptable;
            }
        }
    }

    /// Every 100 ms: spinner and input progress.
    pub fn tick(&mut self, input: &Input) {
        self.spinner = self.spinner.wrapping_add(1);
        let p = input.progress();
        self.input.title = input_title(input.name(), &p);
        if self.input_loaded {
            return;
        }
        if let Ok(bytes) = input.view(VIEW_LINES, VIEW_BYTES) {
            let full = bytes.len() >= VIEW_BYTES
                || memchr::memchr_iter(b'\n', &bytes).count() >= VIEW_LINES;
            self.input.lines = text_lines(&bytes, None);
            self.input_loaded = p.complete || full;
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.mode != Mode::Compose || self.focus != Focus::Query {
            return;
        }
        for c in text.chars().filter(|c| !c.is_control()) {
            self.query.handle(tui_input::InputRequest::InsertChar(c));
        }
    }
}

fn input_title(name: &str, p: &Progress) -> String {
    let name = sanitize(name);
    if !p.complete {
        return format!("Input · {name} · reading… {}", human(p.bytes));
    }
    if !p.counted {
        return format!("Input · {name} · {}", human(p.bytes));
    }
    format!(
        "Input · {name} · {} · {} lines",
        human(p.bytes),
        thousands(p.lines)
    )
}

fn output_pane(run: &Run, timeout: Duration, sample_bytes: u64) -> Pane {
    let status = match &run.outcome {
        Outcome::Ok => String::from("exit 0"),
        Outcome::Failed { code, .. } => format!("exit {code}"),
        Outcome::TimedOut => String::from("timeout"),
        Outcome::Cancelled => String::from("cancelled"),
        Outcome::SpawnError(_) => String::from("error"),
    };
    let scope = match run.scope {
        Scope::Full => String::from("full"),
        Scope::Sample => format!(
            "SAMPLE {} (full run > {})",
            human_binary(sample_bytes),
            seconds(timeout)
        ),
        Scope::Partial => String::from("PARTIAL (stdin still open)"),
    };
    let title = format!(
        "Output · {status} · {} lines · {:.2}s · {scope}",
        thousands(run.stdout_lines),
        run.duration.as_secs_f64()
    );
    let mut lines = text_lines(&run.stdout, None);
    if let Outcome::SpawnError(e) = &run.outcome {
        lines.push(Line::from(sanitize(e)).red());
    }
    lines.extend(text_lines(&run.stderr, Some(())));
    if lines.is_empty() {
        lines.push(Line::from("(no output)").dark_gray());
    }
    Pane {
        title,
        lines,
        scroll: (0, 0),
    }
}

/// Display lines: untrusted bytes with control characters replaced and tabs expanded.
fn text_lines(bytes: &[u8], red: Option<()>) -> Vec<Line<'static>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| {
            let line = Line::from(sanitize(&l.replace('\t', "    ")));
            if red.is_some() { line.red() } else { line }
        })
        .collect()
}

/// Replace control characters, so untrusted text can't send escape sequences to the terminal.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || ('\u{80}'..='\u{9f}').contains(&c) {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// 133525 → "133 KB", 12_400_000 → "12.4 MB".
pub fn human(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} KB", bytes / 1_000),
        1_000_000..1_000_000_000 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

fn human_binary(bytes: u64) -> String {
    if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
    }
}

fn seconds(d: Duration) -> String {
    if d.as_millis().is_multiple_of(1000) {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human(133_525), "133 KB");
        assert_eq!(human(12_400_000), "12.4 MB");
        assert_eq!(human_binary(1 << 20), "1 MiB");
        assert_eq!(seconds(Duration::from_millis(3000)), "3s");
        assert_eq!(seconds(Duration::from_millis(500)), "0.5s");
    }

    #[test]
    fn sanitizing() {
        assert_eq!(
            sanitize("a\x1b[31mb\rc\u{9b}d"),
            "a\u{fffd}[31mb\u{fffd}c\u{fffd}d"
        );
    }
}
