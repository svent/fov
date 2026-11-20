//! The OS sandbox around every pipeline process (§9.3). This is the only module that
//! knows about SBPL, Landlock or seccomp. It builds commands; `exec` spawns them.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::io;
use std::path::Path;

use crate::tools::Tool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sandbox {
    On,
    Off,
}

impl Sandbox {
    /// One pipeline stage: `tool` with `args`. With the sandbox on, the process may exec only
    /// its own binary, read system locations, write only `scratch`, and has no network.
    /// `scratch` must be a real path (no symlinks).
    pub fn command(
        self,
        tool: &Tool,
        args: &[String],
        scratch: &Path,
    ) -> io::Result<duct::Expression> {
        match self {
            Sandbox::Off => Ok(duct::cmd(&tool.path, args)),
            Sandbox::On => platform_command(tool, args, scratch),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_command(tool: &Tool, args: &[String], scratch: &Path) -> io::Result<duct::Expression> {
    macos::command(tool, args, scratch)
}

#[cfg(target_os = "linux")]
fn platform_command(tool: &Tool, args: &[String], scratch: &Path) -> io::Result<duct::Expression> {
    linux::command(tool, args, scratch)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(_: &Tool, _: &[String], _: &Path) -> io::Result<duct::Expression> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sandbox for this OS",
    ))
}
