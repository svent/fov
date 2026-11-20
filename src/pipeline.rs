//! Pipelines as proposed by the model: validation (§7.4) and shell rendering (§4).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::tools::{Tool, Tools};

pub const MAX_COMMANDS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

/// The arguments of a `propose_pipeline` call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub commands: Vec<Cmd>,
    pub explanation: String,
}

/// A validated pipeline: every command bound to an allowlisted binary.
#[derive(Clone, Debug)]
pub struct ResolvedPipeline {
    pub commands: Vec<Cmd>,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug)]
pub struct Stage {
    pub tool: Tool,
    pub args: Vec<String>,
}

pub fn is_bare_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// Check a non-empty proposal. The error is the reason reported back to the model.
pub fn validate(commands: &[Cmd], tools: &Tools) -> Result<ResolvedPipeline, String> {
    if commands.is_empty() || commands.len() > MAX_COMMANDS {
        return Err(format!(
            "a pipeline has 1 to {MAX_COMMANDS} commands, this one has {}",
            commands.len()
        ));
    }
    let mut stages = Vec::with_capacity(commands.len());
    for cmd in commands {
        if !is_bare_name(&cmd.program) {
            return Err(format!(
                "`{}` is not a bare tool name; use only the listed tools",
                cmd.program
            ));
        }
        let Some(tool) = tools.get(&cmd.program) else {
            return Err(format!("`{}` is not allowed", cmd.program));
        };
        for (i, arg) in cmd.args.iter().enumerate() {
            if let Some(c) = arg.chars().find(|&c| is_forbidden_control(c)) {
                return Err(format!(
                    "argument {} of `{}` contains the control character U+{:04X}; write it with the tool's own escape syntax instead, e.g. \\033 in awk",
                    i + 1,
                    cmd.program,
                    c as u32
                ));
            }
        }
        stages.push(Stage {
            tool: tool.clone(),
            args: cmd.args.clone(),
        });
    }
    Ok(ResolvedPipeline {
        commands: commands.to_vec(),
        stages,
    })
}

/// Whether `s` holds a control character other than newline and tab (§7.4).
pub fn has_forbidden_control(s: &str) -> bool {
    s.chars().any(is_forbidden_control)
}

/// A control character other than newline and tab: a terminal would act on it.
pub fn is_forbidden_control(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c)
}

/// The command as a POSIX shell line. `input` is the INPUT argument, attached as `< FILE`.
pub fn render(commands: &[Cmd], input: Option<&str>) -> String {
    let mut stages = Vec::with_capacity(commands.len());
    for (i, cmd) in commands.iter().enumerate() {
        let mut words = vec![cmd.program.clone()];
        words.extend(cmd.args.iter().map(|a| quote(a).into_owned()));
        if i == 0
            && let Some(file) = input
        {
            words.push(format!("< {}", quote(file)));
        }
        stages.push(words.join(" "));
    }
    stages.join(" | ")
}

/// Bare if it can't be misread by sh, bash or zsh; otherwise single-quoted.
pub fn quote(arg: &str) -> Cow<'_, str> {
    let bare = !arg.is_empty()
        && !arg.starts_with('=')
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if bare {
        return Cow::Borrowed(arg);
    }
    Cow::Owned(format!("'{}'", arg.replace('\'', r"'\''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(program: &str, args: &[&str]) -> Cmd {
        Cmd {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("-k2"), "-k2");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("=ls"), "'=ls'");
        assert_eq!(quote("x=1"), "x=1");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote("*.txt"), "'*.txt'");
    }

    #[test]
    fn render_attaches_input_to_first_command() {
        let cmds = [
            cmd("jq", &["-r", ".[].type"]),
            cmd("sort", &[]),
            cmd("uniq", &["-c"]),
        ];
        assert_eq!(
            render(&cmds, Some("events.json")),
            "jq -r '.[].type' < events.json | sort | uniq -c"
        );
        assert_eq!(render(&cmds, None), "jq -r '.[].type' | sort | uniq -c");
        assert_eq!(render(&cmds[1..2], Some("my file")), "sort < 'my file'");
    }

    /// Render, run through the shell, compare with the original argv (§13).
    #[test]
    fn quoting_round_trips_through_shells() {
        let args = [
            "plain",
            "a b",
            "it's",
            "''",
            "=ls",
            "==",
            "=",
            "x=1",
            "$HOME",
            "*",
            "a\nb",
            "tab\there",
            "\\n",
            "\"q\"",
            "!",
            "#c",
            "~",
            "{a,b}",
            "-",
            "é",
            "",
        ];
        let line = format!(
            "printf '%s\\0' {}",
            args.iter()
                .map(|a| quote(a).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let out = std::process::Command::new(shell)
                .arg("-c")
                .arg(&line)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let got: Vec<&str> = std::str::from_utf8(&out.stdout)
                .unwrap()
                .split_terminator('\0')
                .collect();
            assert_eq!(got, args, "{shell}");
        }
    }

    #[test]
    fn validation() {
        let tools = Tools::resolve(&["sort".to_string(), "awk".to_string()]);
        assert!(validate(&[cmd("sort", &["-rn"])], &tools).is_ok());
        assert_eq!(
            validate(&[cmd("xargs", &[])], &tools).unwrap_err(),
            "`xargs` is not allowed"
        );
        assert!(
            validate(&[cmd("/usr/bin/sort", &[])], &tools)
                .unwrap_err()
                .contains("bare tool name")
        );
        assert!(validate(&[], &tools).is_err());
        assert!(
            validate(&vec![cmd("sort", &[]); 9], &tools)
                .unwrap_err()
                .contains("has 9")
        );
        let esc = validate(&[cmd("awk", &["{print \"\x1b[31m\" $0}"])], &tools).unwrap_err();
        assert!(esc.contains("U+001B"), "{esc}");
        assert!(validate(&[cmd("awk", &["x\ry"])], &tools).is_err());
        assert!(validate(&[cmd("awk", &["x\u{85}y"])], &tools).is_err());
        assert!(validate(&[cmd("awk", &["BEGIN{\n\tprint 1\n}"])], &tools).is_ok());
        assert!(validate(&[cmd("awk", &["\0"])], &tools).is_err());
    }
}
