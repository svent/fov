#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use fov::exec::{CancelToken, Executor, Run};
use fov::input::{Scope, Snapshot};
use fov::pipeline::{self, Cmd, ResolvedPipeline};
use fov::sandbox::Sandbox;
use fov::tools::Tools;

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

pub fn executor(sandbox: Sandbox) -> (Executor, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Executor::new(sandbox, dir.path()).unwrap(), dir)
}

/// Resolve `stages` against the user's PATH; panics if a tool is missing.
pub fn pipeline(stages: &[(&str, &[&str])]) -> ResolvedPipeline {
    let cmds: Vec<Cmd> = stages
        .iter()
        .map(|(p, a)| Cmd {
            program: p.to_string(),
            args: a.iter().map(|s| s.to_string()).collect(),
        })
        .collect();
    let names: Vec<String> = cmds.iter().map(|c| c.program.clone()).collect();
    pipeline::validate(&cmds, &Tools::resolve(&names)).unwrap()
}

pub fn full(path: &Path) -> Snapshot {
    Snapshot {
        path: path.to_path_buf(),
        len: None,
        scope: Scope::Full,
    }
}

pub fn preview(exec: &Executor, p: &ResolvedPipeline, input: &Path) -> Run {
    exec.run_preview(
        p,
        &full(input),
        None,
        Duration::from_secs(20),
        &CancelToken::default(),
    )
}

pub fn stdout(run: &Run) -> String {
    String::from_utf8_lossy(&run.stdout).into_owned()
}

pub fn installed(tool: &str) -> bool {
    which::which(tool).is_ok()
}
