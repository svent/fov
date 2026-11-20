//! Executor integration tests (§13), with the sandbox on as in production.

mod common;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use fov::exec::{CancelToken, Outcome, Output};
use fov::input::{Input, Scope, Snapshot};
use fov::pipeline::{Cmd, ResolvedPipeline, Stage};
use fov::sandbox::Sandbox;
use fov::tools::Tool;

#[test]
fn fixed_pipelines_on_fixtures() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[
        (
            "awk",
            &[
                "-F,",
                r#"$6=="completed"{s[$3]+=$7*$8} END{for(r in s) printf "%s %.2f\n",r,s[r]}"#,
            ],
        ),
        ("sort", &["-k2", "-rn"]),
    ]);
    let run = preview(&exec, &p, &fixture("sales.csv"));
    assert_eq!(
        run.outcome,
        Outcome::Ok,
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.scope, Scope::Full);
    assert_eq!(
        stdout(&run),
        "North 295430.31\nSouth 240817.31\nCentral 174306.15\nEast 172422.70\nWest 158424.69\n"
    );
    assert_eq!(run.stdout_lines, 5);

    let p = pipeline(&[
        ("jq", &["-r", ".type"]),
        ("sort", &[]),
        ("uniq", &["-c"]),
        ("sort", &["-rn"]),
        ("head", &["-3"]),
    ]);
    let run = preview(&exec, &p, &fixture("github-events.ndjson"));
    let got: Vec<Vec<String>> = stdout(&run)
        .lines()
        .map(|l| l.split_whitespace().map(String::from).collect())
        .collect();
    assert_eq!(
        got,
        [
            ["165", "PushEvent"],
            ["44", "CreateEvent"],
            ["20", "PullRequestEvent"]
        ]
    );
}

#[test]
fn display_is_capped_but_lines_are_counted() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[("cat", &[])]);
    let run = preview(&exec, &p, &fixture("access.log"));
    assert_eq!(run.stdout_lines, 5000);
    assert_eq!(run.stdout.iter().filter(|&&b| b == b'\n').count(), 1000);
}

#[test]
fn timeout_falls_back_to_sample() {
    let (exec, dir) = executor(Sandbox::On);
    let big = dir.path().join("big.txt");
    let mut f = std::fs::File::create(&big).unwrap();
    for i in 0..200_000 {
        writeln!(f, "line {i}").unwrap();
    }
    let p = pipeline(&[("awk", &["{for(i=0;i<2000;i++)s+=i} END{print NR}"])]);
    let sample = Snapshot {
        path: big.clone(),
        len: Some(70),
        scope: Scope::Sample,
    }; // 10 lines
    let cancel = CancelToken::default();
    let run = exec.run_preview(
        &p,
        &full(&big),
        Some(&sample),
        Duration::from_millis(50),
        &cancel,
    );
    assert_eq!(run.scope, Scope::Sample);
    assert_eq!(run.outcome, Outcome::Ok);
    assert_eq!(stdout(&run), "10\n");
    // Without a sample, the timeout is the result.
    let run = exec.run_preview(&p, &full(&big), None, Duration::from_millis(50), &cancel);
    assert_eq!(run.outcome, Outcome::TimedOut);
}

#[test]
fn cancel_kills_the_preview() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[("awk", &["BEGIN{for(;;)x++}"])]);
    let cancel = CancelToken::default();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        c.cancel();
    });
    let run = exec.run_preview(
        &p,
        &full(&fixture("sales.csv")),
        None,
        Duration::from_secs(20),
        &cancel,
    );
    assert_eq!(run.outcome, Outcome::Cancelled);
    assert!(run.duration < Duration::from_secs(5));
}

/// A one-stage pipeline `sh -c script`, for stages that fork. `sh` is never on the allowlist
/// and can't fork in the macOS sandbox, so these run unsandboxed.
fn sh(script: &str) -> ResolvedPipeline {
    let cmd = Cmd {
        program: String::from("sh"),
        args: vec![String::from("-c"), script.to_string()],
    };
    ResolvedPipeline {
        stages: vec![Stage {
            tool: Tool::at(Path::new("/bin/sh")).unwrap(),
            args: cmd.args.clone(),
        }],
        commands: vec![cmd],
    }
}

/// Whether the process whose pid is the first line of `run`'s stdout is gone within 5 s.
fn gone(run: &fov::exec::Run) -> bool {
    let pid: libc::pid_t = stdout(run).lines().next().unwrap().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks whether the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn timeout_kills_what_a_stage_forked() {
    let (exec, _dir) = executor(Sandbox::Off);
    // The background `sleep` inherits the stage's stdout and stderr.
    let run = exec.run_preview(
        &sh("sleep 60 & echo $!; wait"),
        &full(&fixture("sales.csv")),
        None,
        Duration::from_millis(300),
        &CancelToken::default(),
    );
    assert_eq!(run.outcome, Outcome::TimedOut);
    assert!(run.duration < Duration::from_secs(10), "{:?}", run.duration);
    assert!(gone(&run), "the forked process survived the timeout");
}

#[test]
fn a_process_left_behind_cannot_hold_up_the_run() {
    let (exec, _dir) = executor(Sandbox::Off);
    // The stage exits at once; its background `sleep` keeps stdout and stderr open.
    let run = preview(
        &exec,
        &sh("sleep 30 & echo $!; echo done"),
        &fixture("sales.csv"),
    );
    assert_eq!(run.outcome, Outcome::Ok);
    assert_eq!(stdout(&run).lines().nth(1), Some("done"));
    assert!(run.duration < Duration::from_secs(10), "{:?}", run.duration);
    let pid: libc::pid_t = stdout(&run).lines().next().unwrap().parse().unwrap();
    // SAFETY: cleaning up the `sleep` this test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

#[test]
fn sigpipe_upstream_is_success() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[("awk", &["BEGIN{for(;;)print}"]), ("head", &["-1"])]);
    let run = preview(&exec, &p, &fixture("sales.csv"));
    assert_eq!(run.outcome, Outcome::Ok);
    assert_eq!(stdout(&run), "\n");
}

#[test]
fn grep_without_match_is_success() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[("grep", &["nomatch-xyz"]), ("wc", &["-l"])]);
    let run = preview(&exec, &p, &fixture("access.log"));
    assert_eq!(run.outcome, Outcome::Ok);
    assert_eq!(stdout(&run).trim(), "0");

    let p = pipeline(&[("grep", &["-E", "[a"]), ("wc", &["-l"])]);
    let run = preview(&exec, &p, &fixture("access.log"));
    assert!(
        matches!(run.outcome, Outcome::Failed { stage: 0, code } if code >= 2),
        "{:?}",
        run.outcome
    );
}

#[test]
fn failed_stage_left_of_a_successful_one_is_reported() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[("awk", &["BEGIN{exit 3}"]), ("wc", &["-l"])]);
    let run = preview(&exec, &p, &fixture("sales.csv"));
    assert_eq!(run.outcome, Outcome::Failed { stage: 0, code: 3 });
    let p = pipeline(&[
        ("awk", &["BEGIN{exit 3}"]),
        ("awk", &["END{exit 4}"]),
        ("wc", &["-l"]),
    ]);
    assert_eq!(
        preview(&exec, &p, &fixture("sales.csv")).outcome,
        Outcome::Failed { stage: 1, code: 4 }
    );
}

#[test]
fn stderr_is_captured_from_all_stages() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[
        ("awk", &[r#"BEGIN{print "first" > "/dev/stderr"}"#]),
        ("sed", &["-n", "1p"]),
    ]);
    let run = preview(&exec, &p, &fixture("sales.csv"));
    let err = String::from_utf8_lossy(&run.stderr);
    assert!(err.contains("first"), "{err}");
}

#[test]
fn environment_is_scrubbed() {
    let (exec, _dir) = executor(Sandbox::On);
    let p = pipeline(&[
        (
            "awk",
            &[r#"BEGIN{for(k in ENVIRON) print k "=" ENVIRON[k]}"#],
        ),
        ("sort", &[]),
    ]);
    let run = preview(&exec, &p, &fixture("sales.csv"));
    let env = stdout(&run);
    let keys: Vec<&str> = env.lines().map(|l| l.split('=').next().unwrap()).collect();
    for k in &keys {
        assert!(
            ["PATH", "TMPDIR", "LANG", "TZ"].contains(k)
                || k.starts_with("LC_")
                || *k == "AWKPATH"
                || *k == "AWKLIBPATH",
            "unexpected variable {k} in {env}"
        );
    }
    assert!(env.contains("PATH=/usr/bin:/bin\n"));
    assert!(!keys.contains(&"HOME"));
}

#[test]
fn final_run_writes_to_file() {
    let (exec, dir) = executor(Sandbox::On);
    let out = dir.path().join("out.txt");
    let p = pipeline(&[("cut", &["-d,", "-f3"]), ("sort", &[]), ("uniq", &["-c"])]);
    let outcome = exec
        .run_final(
            &p,
            &fixture("sales.csv"),
            Output::File(std::fs::File::create(&out).unwrap()),
        )
        .unwrap();
    assert_eq!(outcome, Outcome::Ok);
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(text.lines().count(), 6, "{text}"); // 5 regions + the header
}

#[test]
fn json_document_is_never_cut() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let input = Input::file(&fixture("github-events.json"), "github-events.json").unwrap();
    let head = rt.block_on(input.head(16 << 10, 64 << 10)).unwrap();
    assert!(!input.cuttable(&head, 1 << 20));
    let (primary, sample) = input.preview_plan(false, 1 << 20).unwrap().unwrap();
    assert_eq!(primary.scope, Scope::Full);
    assert!(sample.is_none());

    // Line-oriented input gets a sample cut at a line boundary.
    let input = Input::file(&fixture("github-events.ndjson"), "x").unwrap();
    let head = rt.block_on(input.head(16 << 10, 64 << 10)).unwrap();
    assert!(input.cuttable(&head, 1 << 20));
    let (_, sample) = input.preview_plan(true, 1 << 20).unwrap().unwrap();
    let sample = sample.unwrap();
    let data = std::fs::read(fixture("github-events.ndjson")).unwrap();
    let len = sample.len.unwrap() as usize;
    assert!(len <= 1 << 20 && data[len - 1] == b'\n');
}

#[test]
fn partial_stdin_snapshots() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (r, mut w) = std::io::pipe().unwrap();
    let input = Input::spool(
        std::fs::File::from(std::os::fd::OwnedFd::from(r)),
        "stdin",
        None,
        dir.path(),
        1 << 30,
    )
    .unwrap();
    w.write_all(b"a\nb\nc").unwrap();
    let mut rx = input.subscribe();
    rt.block_on(rx.wait_for(|p| p.bytes == 5)).unwrap();
    let (primary, _) = input.preview_plan(true, 1 << 20).unwrap().unwrap();
    assert_eq!((primary.scope, primary.len), (Scope::Partial, Some(4)));
    assert!(
        input.preview_plan(false, 1 << 20).unwrap().is_none(),
        "uncuttable input waits for EOF"
    );
    drop(w);
    let p = rt.block_on(input.wait_complete());
    assert_eq!((p.bytes, p.lines, p.complete), (5, 3, true));
    let (primary, _) = input.preview_plan(true, 1 << 20).unwrap().unwrap();
    assert_eq!((primary.scope, primary.len), (Scope::Full, None));
}

#[test]
fn spool_limit() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (r, mut w) = std::io::pipe().unwrap();
    let input = Input::spool(
        std::fs::File::from(std::os::fd::OwnedFd::from(r)),
        "stdin",
        None,
        dir.path(),
        1000,
    )
    .unwrap();
    std::thread::spawn(move || {
        let _ = w.write_all(&[b'x'; 5000]);
    });
    let p = rt.block_on(input.wait_complete());
    assert!(
        p.error.unwrap().contains("larger than"),
        "spool limit error"
    );
}
