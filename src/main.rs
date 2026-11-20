//! Wiring: args → config → input → TUI → final run.

use std::fs::File;
use std::io::{self, IsTerminal};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use fov::cli::Cli;
use fov::config::{self, Overrides};
use fov::exec::{Executor, Outcome, Output};
use fov::input::{Input, SPOOL_LIMIT};
use fov::llm::Provider;
use fov::pipeline::ResolvedPipeline;
use fov::sandbox::Sandbox;
use fov::session::Session;
use fov::terminal::{Screen, open_tty};
use fov::tools::Tools;
use fov::tui::{self, app::App};

/// The user quit (q / Ctrl-C), or interrupted the final run.
const ABORTED: u8 = 130;
/// fov itself failed and the pipeline never ran (§4).
const FAILED: u8 = 125;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { FAILED } else { 0 };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("fov: {e:#}");
            ExitCode::from(FAILED)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    // §11 startup order: everything the user has to fix fails before the TUI opens.
    let env = |k: &str| std::env::var(k).ok();
    let overrides = Overrides {
        provider: cli.provider,
        model: cli.model.as_deref(),
        reasoning_effort: cli.reasoning_effort.as_deref(),
        settings: Some(&cli.settings),
    };
    let cfg = config::load(overrides, &env)?;
    if let (Some(out), Some(input)) = (&cli.output, &cli.input)
        && same_file(out, input)
    {
        bail!("the output file {} is the input file", out.display());
    }
    if cli.input.is_none() && io::stdin().is_terminal() {
        bail!("no input: pass a file, or pipe data into fov");
    }
    let work = tempfile::Builder::new()
        .prefix("fov-")
        .tempdir()
        .context("creating a temp dir")?;
    let sandbox = if cfg.sandbox {
        Sandbox::On
    } else {
        Sandbox::Off
    };
    let exec = Executor::new(sandbox, work.path())?;
    let mut tools = Tools::resolve(&cfg.allow);
    if tools.is_empty() {
        let why = if tools.warnings.is_empty() {
            String::from("none is installed")
        } else {
            tools.warnings.join("; ")
        };
        bail!(
            "no allowed tool is usable: {why}. Install standalone tools (e.g. gawk, GNU sed) or change [commands].allow"
        );
    }
    exec.probe_sandbox().map_err(|e| {
        anyhow!(
            "the sandbox is unavailable: {e}\nModel-generated pipelines don't run without it. To run them with your full permissions anyway, set `sandbox = false` in {}",
            config::path(&env).display()
        )
    })?;
    tools.detect_flavors(&exec);
    let tty = open_tty()?;
    let input = open_input(&cli, work.path())?;
    let mut warnings = cfg.warnings.clone();
    warnings.extend(tools.warnings.iter().cloned());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let provider = Provider::from_config(&cfg.provider);
        let label = format!("{} · {}", provider.name(), provider.model());
        let app = App::new(
            label,
            sandbox == Sandbox::On,
            warnings,
            cfg.settings.timeout,
            cfg.settings.sample_bytes,
        );
        let (mut session, mut rx) =
            Session::new(provider, exec, tools, input, cfg.settings.clone());
        let mut screen = Screen::enter(tty)?;
        let outcome = tui::run(&mut screen, &mut session, &mut rx, app).await;
        drop(screen);
        match outcome? {
            tui::Outcome::Quit(command) => {
                if let Some(command) = command {
                    print_command(&cli, &command);
                }
                Ok(ABORTED)
            }
            tui::Outcome::Fatal(e) => Err(anyhow!(e)),
            tui::Outcome::Accept { pipeline, command } => {
                final_run(&cli, &session, &pipeline, &command).await
            }
        }
    })
}

/// §4: the command goes to stdout with `-n`, otherwise to stderr.
fn print_command(cli: &Cli, command: &str) {
    if cli.no_output {
        println!("{command}");
    } else {
        eprintln!("{command}");
    }
}

/// §8.5: run on the full input (unless `-n`), then print the command, also after a failure.
async fn final_run(
    cli: &Cli,
    session: &Session,
    pipeline: &ResolvedPipeline,
    command: &str,
) -> Result<u8> {
    let status = if cli.no_output {
        Ok(0)
    } else {
        full_run(cli, session, pipeline).await
    };
    print_command(cli, command);
    status
}

/// Run on the full input with no timeout.
async fn full_run(cli: &Cli, session: &Session, pipeline: &ResolvedPipeline) -> Result<u8> {
    let input = session.input();
    if !input.progress().complete {
        eprintln!("waiting for end of input…");
    }
    if let Some(e) = input.wait_complete().await.error {
        bail!(e);
    }
    let out = match &cli.output {
        Some(path) => Output::File(
            File::create(path).with_context(|| format!("creating {}", path.display()))?,
        ),
        None => Output::Inherit,
    };
    keep_running_on_sigint();
    match session.executor().run_final(pipeline, input.path(), out)? {
        Outcome::Ok => Ok(0),
        Outcome::Failed { code, .. } => Ok(code.clamp(1, 255) as u8),
        Outcome::SpawnError(e) => Err(anyhow!("couldn't start the pipeline: {e}")),
        Outcome::TimedOut | Outcome::Cancelled => Ok(ABORTED),
    }
}

/// Ctrl-C reaches the pipeline through the foreground process group; fov waits and
/// reports the pipeline's status. A handler (unlike SIG_IGN) is reset to default on exec.
fn keep_running_on_sigint() {
    extern "C" fn noop(_: libc::c_int) {}
    // SAFETY: installing an async-signal-safe no-op handler.
    unsafe { libc::signal(libc::SIGINT, noop as *const () as libc::sighandler_t) };
}

fn open_input(cli: &Cli, dir: &Path) -> Result<Input> {
    let Some(path) = &cli.input else {
        let stdin = io::stdin().as_fd().try_clone_to_owned()?;
        return Ok(Input::spool(
            File::from(stdin),
            "stdin",
            None,
            dir,
            SPOOL_LIMIT,
        )?);
    };
    let given = path.to_string_lossy();
    let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
    if meta.is_dir() {
        bail!("{} is a directory", path.display());
    }
    if meta.is_file() {
        return Ok(Input::file(path, &given)?);
    }
    let name = path
        .file_name()
        .map_or_else(|| given.to_string(), |n| n.to_string_lossy().into_owned());
    let file = File::open(path).with_context(|| path.display().to_string())?;
    Ok(Input::spool(
        file,
        &name,
        Some(given.into_owned()),
        dir,
        SPOOL_LIMIT,
    )?)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}
