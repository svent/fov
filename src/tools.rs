//! Allowlist resolution and tool flavor detection (§9.2, §11).

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::exec::Executor;

pub const DEFAULT_ALLOW: &[&str] = &[
    "awk", "cat", "column", "cut", "grep", "head", "jq", "nl", "paste", "rev", "sed", "sort",
    "tac", "tail", "tr", "uniq", "wc",
];

const FLAVOR_BUDGET: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct Tool {
    /// The bare name the model uses.
    pub name: String,
    /// Absolute path as found on PATH. This is what gets exec'd.
    pub path: PathBuf,
    /// The same file with all symlinks resolved.
    pub real: PathBuf,
    /// First line of `--version`, or a label derived from the OS.
    pub flavor: String,
}

impl Tool {
    /// A tool at a fixed absolute path, outside the allowlist (used for the sandbox probe).
    pub fn at(path: &Path) -> std::io::Result<Tool> {
        Ok(Tool {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path: path.to_path_buf(),
            real: fs::canonicalize(path)?,
            flavor: String::from("unknown"),
        })
    }
}

/// The effective allowlist: the configured names that are installed and usable.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    tools: Vec<Tool>,
    pub warnings: Vec<String>,
}

impl Tools {
    /// Resolve `allow` against the user's PATH. Missing tools are dropped silently,
    /// scripts and multicall binaries with a warning.
    pub fn resolve(allow: &[String]) -> Tools {
        let sh = fs::metadata("/bin/sh").ok().map(|m| (m.dev(), m.ino()));
        let mut out = Tools::default();
        for name in allow {
            if out.get(name).is_some() {
                continue;
            }
            if !crate::pipeline::is_bare_name(name) {
                out.warnings.push(format!(
                    "`{name}` in [commands].allow is not a bare tool name; ignored"
                ));
                continue;
            }
            let Ok(path) = which::which(name) else {
                continue;
            };
            let Ok(real) = fs::canonicalize(&path) else {
                continue;
            };
            if is_script(&real) {
                out.warnings.push(format!(
                    "`{name}` is a script ({}); dropped, the sandbox wouldn't let its interpreter run",
                    path.display()
                ));
                continue;
            }
            let id = fs::metadata(&real).ok().map(|m| (m.dev(), m.ino()));
            if id.is_some() && id == sh {
                out.warnings.push(format!(
                    "`{name}` is the same file as /bin/sh ({}, a multicall binary such as BusyBox); dropped",
                    path.display()
                ));
                continue;
            }
            out.tools.push(Tool {
                name: name.clone(),
                path,
                real,
                flavor: String::from("unknown"),
            });
        }
        out.tools.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Tool> {
        self.tools.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Run every tool's `--version` in parallel (sandboxed) within a 50 ms budget.
    pub fn detect_flavors(&mut self, exec: &Executor) {
        let deadline = Instant::now() + FLAVOR_BUDGET;
        let lines: Vec<Option<String>> = std::thread::scope(|s| {
            let handles: Vec<_> = self
                .tools
                .iter()
                .map(|tool| s.spawn(move || version_line(exec, tool, deadline)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().ok().flatten())
                .collect()
        });
        let fallback = os_label();
        for (tool, line) in self.tools.iter_mut().zip(lines) {
            tool.flavor = line.unwrap_or_else(|| fallback.clone());
        }
    }
}

fn version_line(exec: &Executor, tool: &Tool, deadline: Instant) -> Option<String> {
    if let Some(line) = exec.version_line(tool, &["--version"], deadline) {
        return Some(line);
    }
    let is_mawk = tool.name == "mawk" || tool.real.file_name().is_some_and(|n| n == "mawk");
    if !is_mawk {
        return None;
    }
    exec.version_line(tool, &["-W", "version"], deadline)
}

fn is_script(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"#!"
}

/// The label for a tool that rejects `--version`: on macOS that's Apple's BSD userland.
pub fn os_label() -> String {
    match macos_version() {
        Some(v) => format!("BSD (macOS {v})"),
        None => String::from("unknown"),
    }
}

/// "15.7" for macOS 15.7.4; None on other systems.
pub fn macos_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 32];
        let mut len = buf.len();
        let name = c"kern.osproductversion";
        // SAFETY: buf and len describe a valid writable buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let full = String::from_utf8_lossy(&buf[..len - 1]).into_owned();
        let mut parts = full.split('.');
        return Some(match (parts.next(), parts.next()) {
            (Some(major), Some(minor)) => format!("{major}.{minor}"),
            _ => full,
        });
    }
    #[allow(unreachable_code)]
    None
}

/// "macOS 15.7 (arm64)" or "Linux 6.8.0 (aarch64)", for the system prompt.
pub fn os_description() -> String {
    let arch = std::env::consts::ARCH;
    if let Some(v) = macos_version() {
        return format!("macOS {v} ({arch})");
    }
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    format!("{} {} ({arch})", std::env::consts::OS, release.trim())
}
