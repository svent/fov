//! macOS: every stage runs as `sandbox-exec -p <SBPL profile> <binary> args…` (§9.4).

use std::ffi::OsString;
use std::io;
use std::path::Path;

use crate::tools::Tool;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Readable by every stage, as real paths: system binaries, libraries and data, and the
/// package trees of Homebrew and Nix. Not `/private/etc`, and not Homebrew's `etc` and `var`,
/// where configuration and service data live.
const READ_SUBPATHS: &[&str] = &[
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/share",
    "/usr/local/Cellar",
    "/usr/local/opt",
    "/usr/local/lib",
    "/usr/local/share",
    "/bin",
    "/System",
    "/private/var/db/timezone",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/opt",
    "/opt/homebrew/lib",
    "/opt/homebrew/share",
    "/nix/store",
    // The Rosetta runtime: an x86_64 build on Apple Silicon runs its tools translated.
    "/Library/Apple/usr/libexec/oah",
];
const READ_LITERALS: &[&str] = &["/", "/dev/null", "/dev/urandom"];

/// Readable sysctls, by prefix: the hardware and its CPU features (`hw.`, `machdep.cpu.`), POSIX
/// limits for `sysconf` and `confstr` (`user.`), and the lookups of names and types, which return
/// no values, and whether the caller runs translated (`sysctl.`).
const SYSCTL_PREFIXES: &[&str] = &["hw.", "machdep.cpu.", "user.", "sysctl."];
/// And by name: the OS release and the host name (`uname`), the limits `sysconf` reads, and what
/// libSystem reads at every start. Nothing about other processes: not `kern.procargs2`, which
/// holds the arguments and environment of every process of the user.
const SYSCTL_NAMES: &[&str] = &[
    "kern.ostype",
    "kern.osrelease",
    "kern.osrevision",
    "kern.osversion",
    "kern.osproductversion",
    "kern.osproductversioncompat",
    "kern.osreleasetype",
    "kern.version",
    "kern.hostname",
    "kern.argmax",
    "kern.ngroups",
    "kern.job_control",
    "kern.saved_ids",
    "kern.posix1version",
    "kern.bootargs",
    "kern.osvariant_status",
    "kern.secure_kernel",
    "security.mac.lockdown_mode_state",
];

pub fn command(tool: &Tool, args: &[String], scratch: &Path) -> io::Result<duct::Expression> {
    if !Path::new(SANDBOX_EXEC).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SANDBOX_EXEC} not found"),
        ));
    }
    let mut argv: Vec<OsString> = vec![
        "-p".into(),
        profile(tool, scratch).into(),
        tool.path.clone().into(),
    ];
    argv.extend(args.iter().map(OsString::from));
    Ok(duct::cmd(SANDBOX_EXEC, argv))
}

fn profile(tool: &Tool, scratch: &Path) -> String {
    let bin = [lit(&tool.path), lit(&tool.real)].join(" ");
    let scratch = sbpl_str(&scratch.to_string_lossy());
    let reads: Vec<String> = READ_SUBPATHS
        .iter()
        .map(|p| format!("(subpath {})", sbpl_str(p)))
        .chain(
            READ_LITERALS
                .iter()
                .map(|p| format!("(literal {})", sbpl_str(p))),
        )
        .collect();
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|n| sbpl_str(n))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // Everything not listed is denied: network, fork, signals to other processes, Mach and
    // POSIX IPC, ioctl, other sysctls. `sysconf` needs some of the sysctls (sort, perl); a process
    // may signal itself (abort, re-raising a signal). The timezone is reached through the links
    // `/etc/localtime` and `/usr/share/zoneinfo`; following a link takes its metadata.
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec {bin})\n\
         (allow file-read* {} (subpath {scratch}) {bin})\n\
         (allow file-read-metadata (literal \"/etc\") (literal \"/var\") (literal \"/private/etc/localtime\"))\n\
         (allow file-write* (subpath {scratch}) (literal \"/dev/null\"))\n\
         (allow sysctl-read (sysctl-name-prefix {}) (sysctl-name {}))\n\
         (allow signal (target self))\n",
        reads.join(" "),
        quoted(SYSCTL_PREFIXES),
        quoted(SYSCTL_NAMES)
    )
}

fn lit(path: &Path) -> String {
    format!("(literal {})", sbpl_str(&path.to_string_lossy()))
}

fn sbpl_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
