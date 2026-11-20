//! The only module that spawns processes (§8). Every process is wrapped by the sandbox.
//!
//! Each stage is its own duct expression; neighbours are connected with anonymous pipes,
//! so every stage's exit status is known (§8.2, §8.4).

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, IsTerminal, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::input::{Scope, Snapshot};
use crate::pipeline::{ResolvedPipeline, is_forbidden_control};
use crate::sandbox::Sandbox;
use crate::tools::Tool;

pub const DISPLAY_LINES: usize = 1000;
pub const DISPLAY_BYTES: usize = 1 << 20;
pub const STDERR_CAP: usize = 64 << 10;
const POLL: Duration = Duration::from_millis(2);
/// What is still read from a pipe after the stages have exited: more than a pipe can hold.
const PIPE_REST: usize = 1 << 20;

#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// How a run ended, by the rule in §8.4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The rightmost failed stage (0-based) and its status: exit code, or 128 + signal.
    Failed {
        stage: usize,
        code: i32,
    },
    TimedOut,
    Cancelled,
    /// A stage couldn't be started.
    SpawnError(String),
}

/// One preview run.
#[derive(Clone, Debug)]
pub struct Run {
    pub scope: Scope,
    pub outcome: Outcome,
    /// The first 1,000 lines or 1 MiB of stdout.
    pub stdout: Vec<u8>,
    /// All stdout lines, counted until EOF.
    pub stdout_lines: u64,
    /// The first 64 KiB of stderr, from all stages.
    pub stderr: Vec<u8>,
    pub duration: Duration,
}

/// Where the final run's stdout goes.
pub enum Output {
    Inherit,
    File(File),
}

enum Stdin {
    Path(PathBuf),
    Pipe(PipeReader),
}

enum Stdout {
    Inherit,
    File(File),
    Pipe(PipeWriter),
}

pub struct Executor {
    sandbox: Sandbox,
    work: PathBuf,
    passthrough: Vec<(OsString, OsString)>,
}

impl Executor {
    /// `work` is a private directory for scratch dirs and the probe file.
    pub fn new(sandbox: Sandbox, work: &Path) -> io::Result<Executor> {
        let passthrough = std::env::vars_os()
            .filter(|(k, _)| k == "LANG" || k == "TZ" || k.to_string_lossy().starts_with("LC_"))
            .collect();
        Ok(Executor {
            sandbox,
            work: fs::canonicalize(work)?,
            passthrough,
        })
    }

    pub fn sandbox(&self) -> Sandbox {
        self.sandbox
    }

    /// Run the pipeline on `primary` with a timeout. On timeout, run it again on `sample`.
    pub fn run_preview(
        &self,
        p: &ResolvedPipeline,
        primary: &Snapshot,
        sample: Option<&Snapshot>,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Run {
        let run = self.run_once(p, primary, timeout, cancel);
        match (&run.outcome, sample) {
            (Outcome::TimedOut, Some(sample)) => self.run_once(p, sample, timeout, cancel),
            _ => run,
        }
    }

    /// The final run: the whole input, no timeout. A redirected stdout or stderr is inherited
    /// and gets the pipeline's bytes as they are. One that is a terminal is sanitized like the
    /// preview, and the stages get a pipe instead of the terminal.
    pub fn run_final(
        &self,
        p: &ResolvedPipeline,
        input: &Path,
        out: Output,
    ) -> io::Result<Outcome> {
        let scratch = self.scratch()?;
        let exited = Arc::new(AtomicBool::new(false));
        let mut copiers = Vec::new();
        let stdout = match out {
            Output::File(f) => Stdout::File(f),
            Output::Inherit if io::stdout().is_terminal() => {
                let (w, copier) = sanitized(io::stdout(), &exited)?;
                copiers.push(copier);
                Stdout::Pipe(w)
            }
            Output::Inherit => Stdout::Inherit,
        };
        let mut stderr = None;
        if io::stderr().is_terminal() {
            let (w, copier) = sanitized(io::stderr(), &exited)?;
            copiers.push(copier);
            stderr = Some(w);
        }
        // The stages stay in our process group, so Ctrl-C reaches them (§8.5).
        let handles = self.spawn(
            p,
            Stdin::Path(input.to_path_buf()),
            stdout,
            stderr,
            scratch.path(),
            false,
        )?;
        let mut statuses = Vec::with_capacity(handles.len());
        for h in &handles {
            statuses.push(h.wait()?.status);
        }
        exited.store(true, Ordering::SeqCst);
        for copier in copiers {
            let _ = copier.join();
        }
        Ok(classify(p, &statuses))
    }

    /// The first line of `tool args…` (stdin /dev/null) if it exits 0 before `deadline`.
    pub fn version_line(&self, tool: &Tool, args: &[&str], deadline: Instant) -> Option<String> {
        let scratch = self.scratch().ok()?;
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let h = self
            .command(tool, &args, scratch.path())
            .ok()?
            .stdin_null()
            .stdout_capture()
            .stderr_null()
            .start()
            .ok()?;
        let Some(out) = h.wait_deadline(deadline).ok()? else {
            let _ = h.kill();
            let _ = h.wait();
            return None;
        };
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .to_string();
        (!line.is_empty()).then_some(line)
    }

    /// §9.6: a sandboxed `cat` must run, and must not be able to read a file outside its scratch dir.
    pub fn probe_sandbox(&self) -> Result<(), String> {
        if self.sandbox == Sandbox::Off {
            return Ok(());
        }
        let cat = Tool::at(Path::new("/bin/cat")).map_err(|e| format!("/bin/cat: {e}"))?;
        let scratch = self
            .scratch()
            .map_err(|e| format!("creating a scratch dir: {e}"))?;
        let run = |args: &[String]| -> Result<std::process::Output, String> {
            self.command(&cat, args, scratch.path())
                .and_then(|e| e.stdin_null().stdout_capture().stderr_capture().run())
                .map_err(|e| e.to_string())
        };
        let out = run(&[])?;
        if !out.status.success() {
            return Err(format!(
                "a sandboxed `cat` failed to run: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let probe = self.work.join("sandbox-probe");
        fs::write(&probe, "probe").map_err(|e| format!("writing the probe file: {e}"))?;
        let out = run(&[probe.to_string_lossy().into_owned()])?;
        let _ = fs::remove_file(&probe);
        if out.status.success() || !out.stdout.is_empty() {
            return Err(String::from(
                "the sandbox started a process but didn't stop it from reading a file outside its scratch dir",
            ));
        }
        Ok(())
    }

    fn run_once(
        &self,
        p: &ResolvedPipeline,
        input: &Snapshot,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Run {
        let start = Instant::now();
        let done = |outcome, stdout, stdout_lines, stderr| Run {
            scope: input.scope,
            outcome,
            stdout,
            stdout_lines,
            stderr,
            duration: start.elapsed(),
        };
        let prepared = (|| -> io::Result<_> {
            let scratch = self.scratch()?;
            let (out_r, out_w) = io::pipe()?;
            let (err_r, err_w) = io::pipe()?;
            let stdin = match input.len {
                None => Stdin::Path(input.path.clone()),
                Some(len) => {
                    let (r, w) = io::pipe()?;
                    feed(input.path.clone(), len, w);
                    Stdin::Pipe(r)
                }
            };
            let handles = self.spawn(
                p,
                stdin,
                Stdout::Pipe(out_w),
                Some(err_w),
                scratch.path(),
                true,
            )?;
            Ok((scratch, handles, out_r, err_r))
        })();
        let (_scratch, handles, out_r, err_r) = match prepared {
            Ok(v) => v,
            Err(e) => {
                return done(
                    Outcome::SpawnError(e.to_string()),
                    Vec::new(),
                    0,
                    Vec::new(),
                );
            }
        };
        let exited = Arc::new(AtomicBool::new(false));
        let (out_r, err_r) = (
            StagePipe::new(out_r, &exited),
            StagePipe::new(err_r, &exited),
        );
        let stdout = thread::spawn(move || read_stdout(out_r));
        let stderr = thread::spawn(move || read_capped(err_r, STDERR_CAP));

        let deadline = start + timeout;
        let mut stopped = None;
        while !handles
            .iter()
            .all(|h| matches!(h.try_wait(), Ok(Some(_)) | Err(_)))
        {
            if cancel.is_cancelled() {
                stopped = Some(Outcome::Cancelled);
            } else if Instant::now() >= deadline {
                stopped = Some(Outcome::TimedOut);
            }
            if stopped.is_some() {
                handles.iter().for_each(kill);
                break;
            }
            thread::sleep(POLL);
        }
        let statuses: Vec<ExitStatus> = handles
            .iter()
            .map(|h| {
                h.wait()
                    .map(|o| o.status)
                    .unwrap_or(ExitStatus::from_raw(1 << 8))
            })
            .collect();
        exited.store(true, Ordering::SeqCst);
        let (out, lines) = stdout.join().unwrap_or_default();
        let err = stderr.join().unwrap_or_default();
        done(
            stopped.unwrap_or_else(|| classify(p, &statuses)),
            out,
            lines,
            err,
        )
    }

    /// Start every stage, connected by pipes. On error, already started stages are killed.
    /// With `own_groups`, each stage leads a process group of its own, so that `kill` takes
    /// what the stage forked with it.
    fn spawn(
        &self,
        p: &ResolvedPipeline,
        stdin: Stdin,
        stdout: Stdout,
        stderr: Option<PipeWriter>,
        scratch: &Path,
        own_groups: bool,
    ) -> io::Result<Vec<duct::Handle>> {
        let mut handles = Vec::with_capacity(p.stages.len());
        let result = (|| {
            let mut next_in = Some(stdin);
            let mut last_out = Some(stdout);
            for (i, stage) in p.stages.iter().enumerate() {
                let mut e = self.command(&stage.tool, &stage.args, scratch)?;
                e = match next_in.take() {
                    Some(Stdin::Path(path)) => e.stdin_path(path),
                    Some(Stdin::Pipe(r)) => e.stdin_file(r),
                    None => unreachable!("every stage gets an input"),
                };
                if i + 1 < p.stages.len() {
                    let (r, w) = io::pipe()?;
                    e = e.stdout_file(w);
                    next_in = Some(Stdin::Pipe(r));
                } else {
                    e = match last_out.take() {
                        Some(Stdout::File(f)) => e.stdout_file(f),
                        Some(Stdout::Pipe(w)) => e.stdout_file(w),
                        Some(Stdout::Inherit) | None => e,
                    };
                }
                if let Some(w) = &stderr {
                    e = e.stderr_file(w.try_clone()?);
                }
                if own_groups {
                    e = e.before_spawn(|cmd| {
                        cmd.process_group(0);
                        Ok(())
                    });
                }
                // `e` holds the parent's copies of the pipe ends and is dropped right after start.
                handles.push(e.start()?);
            }
            Ok(())
        })();
        if let Err(e) = result {
            for h in &handles {
                kill(h);
                let _ = h.wait();
            }
            return Err(e);
        }
        Ok(handles)
    }

    fn command(
        &self,
        tool: &Tool,
        args: &[String],
        scratch: &Path,
    ) -> io::Result<duct::Expression> {
        let mut env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TMPDIR".into(), scratch.into()),
        ];
        env.extend(self.passthrough.iter().cloned());
        Ok(self
            .sandbox
            .command(tool, args, scratch)?
            .full_env(env)
            .unchecked())
    }

    /// A fresh per-run scratch dir (`TMPDIR`), removed when dropped.
    fn scratch(&self) -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&self.work)
    }
}

/// §8.4: a stage failed unless it exited 0, died of SIGPIPE, or is `grep` with exit 1.
/// A run's status is that of its rightmost failed stage.
fn classify(p: &ResolvedPipeline, statuses: &[ExitStatus]) -> Outcome {
    for (i, status) in statuses.iter().enumerate().rev() {
        let failed = !status.success()
            && status.signal() != Some(libc::SIGPIPE)
            && !(p.stages[i].tool.name == "grep" && status.code() == Some(1));
        if failed {
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            return Outcome::Failed { stage: i, code };
        }
    }
    Outcome::Ok
}

/// Kill a stage. While it's running, also kill the process group it leads, if it does:
/// that's everything it forked.
fn kill(h: &duct::Handle) {
    if matches!(h.try_wait(), Ok(None)) {
        for pid in h.pids() {
            // SAFETY: a plain syscall. The stage isn't reaped, so the pid is still its own.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
    }
    let _ = h.kill();
}

/// Copy the first `len` bytes of `path` to `w` on a thread. Nobody waits for it: it ends
/// when the first stage's stdin closes, and a process the stage left behind can delay that.
fn feed(path: PathBuf, len: u64, mut w: PipeWriter) {
    thread::spawn(move || {
        if let Ok(f) = File::open(path) {
            let _ = io::copy(&mut f.take(len), &mut w);
        }
    });
}

/// The reading end of a pipe the stages write to. It ends at EOF, or when every stage has
/// exited and what they wrote is read. A process a stage left behind can keep the pipe open,
/// and must not keep us waiting.
struct StagePipe {
    pipe: PipeReader,
    exited: Arc<AtomicBool>,
    rest: usize,
}

impl StagePipe {
    /// `exited` is set once every stage has exited.
    fn new(pipe: PipeReader, exited: &Arc<AtomicBool>) -> StagePipe {
        StagePipe {
            pipe,
            exited: exited.clone(),
            rest: PIPE_REST,
        }
    }
}

impl Read for StagePipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let exited = self.exited.load(Ordering::SeqCst);
            let mut fd = libc::pollfd {
                fd: self.pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `fd` is one valid pollfd.
            let ready = unsafe { libc::poll(&mut fd, 1, if exited { 0 } else { 50 }) };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready > 0 && !exited {
                return self.pipe.read(buf);
            }
            if ready > 0 {
                // What the stages wrote before they exited fits in the pipe.
                let n = buf.len().min(self.rest);
                let n = self.pipe.read(&mut buf[..n])?;
                self.rest -= n;
                return Ok(n);
            }
            if exited {
                return Ok(0);
            }
        }
    }
}

/// A pipe for the stages to write to. A thread copies it to `to`, a terminal, sanitized.
fn sanitized(
    to: impl Write + Send + 'static,
    exited: &Arc<AtomicBool>,
) -> io::Result<(PipeWriter, thread::JoinHandle<()>)> {
    let (r, w) = io::pipe()?;
    let r = StagePipe::new(r, exited);
    Ok((w, thread::spawn(move || copy_sanitized(r, to))))
}

/// Copy `from` to `to` the way the preview shows output: invalid UTF-8 and control
/// characters become U+FFFD, except newline, tab and the CR of a CRLF. So what a pipeline
/// prints can't send escape sequences to the terminal.
fn copy_sanitized(mut from: impl Read, mut to: impl Write) {
    let mut buf = vec![0u8; 64 << 10];
    let mut held = 0;
    loop {
        let n = match from.read(&mut buf[held..]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        let len = held + n;
        held = if n == 0 { 0 } else { unfinished(&buf[..len]) };
        let text = String::from_utf8_lossy(&buf[..len - held]);
        let mut shown = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let crlf = c == '\r' && chars.peek() == Some(&'\n');
            shown.push(if is_forbidden_control(c) && !crlf {
                '\u{fffd}'
            } else {
                c
            });
        }
        if to.write_all(shown.as_bytes()).is_err() || to.flush().is_err() || n == 0 {
            return;
        }
        buf.copy_within(len - held..len, 0);
    }
}

/// How many bytes at the end of `bytes` can't be judged without what follows: a CR, or the
/// start of a UTF-8 sequence.
fn unfinished(bytes: &[u8]) -> usize {
    if bytes.last() == Some(&b'\r') {
        return 1;
    }
    for (i, &b) in bytes.iter().rev().take(3).enumerate() {
        if b < 0x80 {
            return 0;
        }
        if b >= 0xC0 {
            let sequence = match b {
                0xF0.. => 4,
                0xE0.. => 3,
                _ => 2,
            };
            return if sequence > i + 1 { i + 1 } else { 0 };
        }
    }
    0
}

/// Keep the first 1,000 lines or 1 MiB; count lines until EOF.
fn read_stdout(mut r: impl Read) -> (Vec<u8>, u64) {
    let mut kept = Vec::new();
    let mut full = false;
    let (mut lines, mut last) = (0u64, b'\n');
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let chunk = &buf[..n];
        lines += memchr::memchr_iter(b'\n', chunk).count() as u64;
        last = chunk[n - 1];
        if full {
            continue;
        }
        kept.extend_from_slice(chunk);
        if let Some(i) = memchr::memchr_iter(b'\n', &kept).nth(DISPLAY_LINES - 1) {
            kept.truncate(i + 1);
            full = true;
        }
        if kept.len() >= DISPLAY_BYTES {
            kept.truncate(DISPLAY_BYTES);
            full = true;
        }
    }
    (kept, lines + u64::from(last != b'\n'))
}

fn read_capped(mut r: impl Read, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = vec![0u8; 16 << 10];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out its bytes a few at a time, like a pipe.
    struct Slow<'a>(&'a [u8], usize);

    impl Read for Slow<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.1.min(self.0.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn terminal_output_is_sanitized() {
        let shown = |bytes: &[u8], step| {
            let mut out = Vec::new();
            copy_sanitized(Slow(bytes, step), &mut out);
            String::from_utf8(out).unwrap()
        };
        for step in [1, 2, 3, 1 << 16] {
            assert_eq!(
                shown(b"a\x1b]52;c;YQ==\x07b\x1b[2J\n", step),
                "a\u{fffd}]52;c;YQ==\u{fffd}b\u{fffd}[2J\n",
                "escape sequences, {step} bytes at a time"
            );
            assert_eq!(
                shown(b"x\ty\r\nz\rw\r", step),
                "x\ty\r\nz\u{fffd}w\u{fffd}",
                "tab and CRLF stay, a lone CR doesn't"
            );
            assert_eq!(
                shown("é€😀 \u{9b}31m".as_bytes(), step),
                "é€😀 \u{fffd}31m",
                "text stays, a C1 control doesn't"
            );
            assert_eq!(
                shown(b"ok \xff\xe2\x82 end\xe2", step),
                "ok \u{fffd}\u{fffd} end\u{fffd}",
                "invalid UTF-8"
            );
        }
    }
}
