//! End-to-end smoke test (§13): `cat testdata/sales.csv | fov | wc -l` on a real pty,
//! against a fake OpenAI-compatible server on localhost.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// Serves canned Responses API replies with this tool call; returns the base URL and the request log.
fn fake_openai(arguments: serde_json::Value) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = reader.read_exact(&mut body);
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap_or_default());
            let reply = json!({
                "id": "resp_1", "object": "response", "created_at": 0, "model": "test", "status": "completed",
                "output": [{ "type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "propose_pipeline", "arguments": arguments.to_string(), "status": "completed" }],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, log)
}

const REVENUE: &str = r#"{
    "commands": [
        { "program": "awk", "args": ["-F,", "$6==\"completed\"{s[$3]+=$7*$8} END{for(r in s) printf \"%s %.2f\\n\",r,s[r]}"] },
        { "program": "sort", "args": ["-k2", "-rn"] }
    ],
    "explanation": "revenue per region"
}"#;

/// A config dir whose config points at `url`.
fn config_dir(url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("fov/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, format!("provider = \"openai\"\n[openai]\nmodel = \"test\"\napi_key = \"k\"\nbase_url = \"{url}\"\n")).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

/// What to press after the query: `key` every 200 ms until `done` holds for the terminal
/// output or the script exits, then `then`.
struct Drive<'a> {
    key: &'a [u8],
    done: &'a dyn Fn(&[u8]) -> bool,
    then: &'a [u8],
}

/// Press `a` until the script exits: `a` is ignored while working and accepts in Review.
const ACCEPT: Drive = Drive {
    key: b"a",
    done: &|_| false,
    then: b"",
};

/// Run a bash script on a fresh pty (its controlling terminal) with `env` added, type `query`,
/// then follow `drive`. Returns the exit status and everything written to the terminal.
fn interactive(
    script: &str,
    xdg: &std::path::Path,
    env: &[(&str, &str)],
    query: &str,
    drive: Drive,
) -> (std::process::ExitStatus, Vec<u8>) {
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills in two fds.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        },
        0
    );
    // openpty doesn't set close-on-exec. Without it, the parallel tests' children inherit each
    // other's pty masters, and a pty whose master stays open never hangs up.
    for fd in [master, slave] {
        // SAFETY: fd was just opened by openpty.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    // SAFETY: both fds were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c")
        .arg(script)
        .env("XDG_CONFIG_HOME", xdg)
        .env_remove("SSH_TTY")
        .env_remove("SSH_CONNECTION")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .envs(env.iter().copied())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::null());
    // SAFETY: setsid and TIOCSCTTY are async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(slave);

    let screen = Arc::new(Mutex::new(Vec::new()));
    let mut reader = std::fs::File::from(master.try_clone().unwrap());
    let sink = screen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let mut tty = std::fs::File::from(master);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !String::from_utf8_lossy(&screen.lock().unwrap()).contains("Input") {
        assert!(Instant::now() < deadline, "no first frame");
        std::thread::sleep(Duration::from_millis(20));
    }
    tty.write_all(format!("{query}\r").as_bytes()).unwrap();
    let mut pressed_then = false;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            std::thread::sleep(Duration::from_millis(50));
            return (status, screen.lock().unwrap().clone());
        }
        assert!(Instant::now() < deadline, "fov didn't finish");
        if !pressed_then && (drive.done)(&screen.lock().unwrap()) {
            tty.write_all(drive.then).unwrap();
            pressed_then = true;
        } else if !pressed_then {
            tty.write_all(drive.key).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn pipe_in_pipe_out() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let (url, log) = fake_openai(serde_json::from_str(REVENUE).unwrap());
    let dir = config_dir(&url);
    let (out, err) = (dir.path().join("out.txt"), dir.path().join("err.txt"));
    let script = format!(
        "cat {root}/testdata/sales.csv | {bin} 2> {} | wc -l > {}; echo \"status=${{PIPESTATUS[1]:-?}}\" >> {}",
        err.display(),
        out.display(),
        out.display()
    );
    assert!(
        interactive(&script, dir.path(), &[], "revenue per region", ACCEPT)
            .0
            .success()
    );
    let out = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        out.split_whitespace().collect::<Vec<_>>(),
        ["5", "status=0"],
        "{out}"
    );
    let err = std::fs::read_to_string(&err).unwrap();
    assert!(
        err.starts_with("awk -F, '$6==\"completed\""),
        "the command goes to stderr: {err}"
    );
    assert!(err.trim_end().ends_with("| sort -k2 -rn"), "{err}");

    let requests = log.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["tool_choice"]["name"], "propose_pipeline");
    let first = requests[0]["input"][0]["content"].as_str().unwrap();
    assert!(
        first.starts_with("Request: revenue per region") && first.contains("source: stdin"),
        "{first}"
    );
}

/// §15: the output is byte-identical to running the printed command in bash and zsh.
#[test]
fn output_file_matches_printed_command() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let (url, _log) = fake_openai(serde_json::from_str(REVENUE).unwrap());
    let dir = config_dir(&url);
    let (out, err) = (dir.path().join("out.txt"), dir.path().join("err.txt"));
    std::fs::write(&out, "old content\n").unwrap();
    let script = format!(
        "{bin} {root}/testdata/sales.csv -o {} 2> {}",
        out.display(),
        err.display()
    );
    assert!(
        interactive(&script, dir.path(), &[], "revenue per region", ACCEPT)
            .0
            .success()
    );
    let ours = std::fs::read(&out).unwrap();
    assert!(ours.starts_with(b"North 295430.31\n"));
    let printed = std::fs::read_to_string(&err).unwrap();
    assert!(
        printed.contains(&format!("< {root}/testdata/sales.csv |")),
        "{printed}"
    );
    for shell in ["/bin/bash", "/bin/zsh"] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        let theirs = Command::new(shell)
            .arg("-c")
            .arg(printed.trim_end())
            .output()
            .unwrap();
        assert_eq!(theirs.stdout, ours, "{shell}");
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `y` hands the shown command to the clipboard tool (a fake `pbcopy` here); `q` quits and
/// prints the shown command.
#[test]
fn copy_with_clipboard_tool() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let (url, _log) = fake_openai(serde_json::from_str(REVENUE).unwrap());
    let dir = config_dir(&url);
    let (bin_dir, clip) = (dir.path().join("bin"), dir.path().join("clip.txt"));
    std::fs::create_dir(&bin_dir).unwrap();
    // `pbcopy` is the first tool tried and needs no display, so a fake one first in PATH
    // always wins, and no real clipboard is touched (OrbStack has a pbcopy on Linux too).
    let fake = bin_dir.join("pbcopy");
    std::fs::write(&fake, format!("#!/bin/sh\ncat > {}\n", clip.display())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str())];
    let err = dir.path().join("err.txt");
    let script = format!("{bin} {root}/testdata/sales.csv 2> {}", err.display());
    let done = |_: &[u8]| clip.exists();
    let drive = Drive {
        key: b"y",
        done: &done,
        then: b"q",
    };
    let (status, _) = interactive(&script, dir.path(), &env, "revenue per region", drive);
    assert_eq!(status.code(), Some(130), "q quits");
    let copied = std::fs::read_to_string(&clip).unwrap();
    assert!(copied.starts_with("awk -F, '$6==\"completed\""), "{copied}");
    assert!(
        copied.ends_with(&format!("< {root}/testdata/sales.csv | sort -k2 -rn")),
        "{copied}"
    );
    let printed = std::fs::read_to_string(&err).unwrap();
    assert_eq!(printed, format!("{copied}\n"), "q prints the shown command");
}

/// On accept the output comes first and the command last.
#[test]
fn accept_prints_the_output_then_the_command() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let (url, _log) = fake_openai(serde_json::from_str(REVENUE).unwrap());
    let dir = config_dir(&url);
    let both = dir.path().join("both.txt");
    let script = format!("{bin} {root}/testdata/sales.csv > {} 2>&1", both.display());
    assert!(
        interactive(&script, dir.path(), &[], "revenue per region", ACCEPT)
            .0
            .success()
    );
    let both = std::fs::read_to_string(&both).unwrap();
    let lines: Vec<&str> = both.lines().collect();
    assert_eq!(lines.len(), 6, "{both}");
    assert_eq!(lines[0], "North 295430.31");
    assert!(lines[5].starts_with("awk -F, ") && lines[5].ends_with("| sort -k2 -rn"));
}

/// Output that goes to the terminal can't send it escape sequences (here: OSC 52, which
/// would write the clipboard).
#[test]
fn terminal_output_is_sanitized() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let proposal = json!({
        "commands": [{ "program": "awk", "args": ["BEGIN{printf \"a\\033]52;c;YXVkaXQ=\\007b\\n\"}"] }],
        "explanation": "prints an escape sequence"
    });
    let (url, _log) = fake_openai(proposal);
    let dir = config_dir(&url);
    let script = format!("{bin} {root}/testdata/sales.csv");
    let (status, screen) = interactive(&script, dir.path(), &[], "anything", ACCEPT);
    assert!(status.success());
    assert!(
        !contains(&screen, b"\x1b]52;c;YXVkaXQ="),
        "the escape sequence reached the terminal"
    );
    let left_tui = screen
        .windows(8)
        .rposition(|w| w == b"\x1b[?1049l")
        .expect("the TUI left the alternate screen");
    assert!(
        contains(
            &screen[left_tui..],
            "a\u{fffd}]52;c;YXVkaXQ=\u{fffd}b".as_bytes()
        ),
        "the final output is shown sanitized: {}",
        String::from_utf8_lossy(&screen[left_tui..])
    );
}

/// Over SSH, `y` sends the command to the terminal as OSC 52.
#[test]
fn copy_over_ssh_uses_osc52() {
    let (root, bin) = (env!("CARGO_MANIFEST_DIR"), env!("CARGO_BIN_EXE_fov"));
    let (url, _log) = fake_openai(serde_json::from_str(REVENUE).unwrap());
    let dir = config_dir(&url);
    let script = format!("{bin} {root}/testdata/sales.csv");
    let done = |screen: &[u8]| contains(screen, b"\x1b]52;c;");
    let drive = Drive {
        key: b"y",
        done: &done,
        then: b"q",
    };
    let env = [("SSH_TTY", "/dev/pts/9")];
    let (status, screen) = interactive(&script, dir.path(), &env, "revenue per region", drive);
    assert_eq!(status.code(), Some(130));
    let command = format!(
        "awk -F, '$6==\"completed\"{{s[$3]+=$7*$8}} END{{for(r in s) printf \"%s %.2f\\n\",r,s[r]}}' < {root}/testdata/sales.csv | sort -k2 -rn"
    );
    let expected = fov::clipboard::osc52(&command);
    assert!(
        contains(&screen, &expected),
        "the OSC 52 sequence carries the shown command"
    );
}

#[test]
fn usage_errors_exit_125() {
    let bin = env!("CARGO_BIN_EXE_fov");
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("XDG_CONFIG_HOME", dir.path())
            .env("OPENAI_API_KEY", "k")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = run(&["-o", "x", "-n", "f"]);
    assert_eq!(out.status.code(), Some(125), "-o and -n conflict");
    let out = run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(125));
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let f = dir.path().join("in.txt");
    std::fs::write(&f, "x\n").unwrap();
    let p = f.to_str().unwrap();
    let out = run(&["--provider", "openai", "--model", "m", p, "-o", p]);
    assert_eq!(out.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is the input file"));
    let out = run(&[
        "--provider",
        "openai",
        "--model",
        "m",
        "--context-head-kib",
        "0",
        p,
    ]);
    assert_eq!(out.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("--context-head-kib must be a positive integer")
    );
    let out = run(&[p]);
    assert_eq!(out.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no config file at"));
}
