//! Copying text to the clipboard (§6.2, `y`).
//!
//! The system clipboard tool is used where there is one. Otherwise, and over SSH, where the
//! clipboard that matters is the local terminal's, the text goes to the terminal as an OSC 52
//! escape sequence. Not every terminal honours that (macOS Terminal.app doesn't).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine;

#[derive(Debug, PartialEq, Eq)]
pub enum Method {
    Tool(PathBuf, &'static [&'static str]),
    Osc52,
}

/// Each tool with its arguments and the display it needs, if any.
const TOOLS: &[(&str, &[&str], Option<&str>)] = &[
    ("pbcopy", &[], None),
    ("wl-copy", &[], Some("WAYLAND_DISPLAY")),
    ("xclip", &["-selection", "clipboard"], Some("DISPLAY")),
    ("xsel", &["--clipboard", "--input"], Some("DISPLAY")),
];

pub fn method(
    env: &dyn Fn(&str) -> Option<String>,
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Method {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return Method::Osc52;
    }
    for (tool, args, display) in TOOLS {
        if display.is_some_and(|d| !set(d)) {
            continue;
        }
        if let Some(path) = find(tool) {
            return Method::Tool(path, args);
        }
    }
    Method::Osc52
}

/// Copy `text`; `terminal` receives the OSC 52 sequence when no tool is used.
/// Returns a short confirmation for the status line.
pub fn copy(text: &str, terminal: &mut impl Write) -> io::Result<&'static str> {
    match method(&|k| std::env::var(k).ok(), &|t| which::which(t).ok()) {
        Method::Tool(path, args) => {
            let mut child = Command::new(&path)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .expect("stdin is piped")
                .write_all(text.as_bytes())?;
            if !child.wait()?.success() {
                return Err(io::Error::other(format!("{} failed", path.display())));
            }
            Ok("copied to the clipboard")
        }
        Method::Osc52 => {
            terminal.write_all(&osc52(text))?;
            terminal.flush()?;
            Ok("sent to the terminal's clipboard (OSC 52)")
        }
    }
}

pub fn osc52(text: &str) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pick(vars: &[(&str, &str)], tools: &[&str]) -> Method {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        method(&|k| vars.get(k).cloned(), &|t| {
            tools
                .contains(&t)
                .then(|| PathBuf::from(format!("/usr/bin/{t}")))
        })
    }

    #[test]
    fn method_choice() {
        assert_eq!(
            pick(&[], &["pbcopy"]),
            Method::Tool("/usr/bin/pbcopy".into(), &[])
        );
        assert_eq!(
            pick(&[("SSH_TTY", "/dev/ttys001")], &["pbcopy"]),
            Method::Osc52,
            "over SSH"
        );
        assert_eq!(
            pick(&[], &["xclip"]),
            Method::Osc52,
            "xclip needs a display"
        );
        assert_eq!(
            pick(&[("DISPLAY", ":0")], &["xclip", "xsel"]),
            Method::Tool("/usr/bin/xclip".into(), &["-selection", "clipboard"])
        );
        assert_eq!(
            pick(
                &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
                &["wl-copy", "xclip"]
            ),
            Method::Tool("/usr/bin/wl-copy".into(), &[])
        );
        assert_eq!(pick(&[], &[]), Method::Osc52);
    }

    #[test]
    fn osc52_sequence() {
        assert_eq!(osc52("sort -rn"), b"\x1b]52;c;c29ydCAtcm4=\x07");
    }
}
