//! Linux: Landlock + seccomp, applied in `pre_exec` (§9.5).
//!
//! `pre_exec` runs in a forked child of a multi-threaded process, so it may only make
//! syscalls. Everything is built up front: the Landlock ruleset fd with its rules, and the
//! seccomp BPF program. The child then makes three syscalls.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, Scope, path_beneath_rules,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch, sock_filter};

use crate::tools::Tool;

/// Readable by every stage: binaries, libraries, shared data, and the few files in `/etc`
/// that the loader and libc read. Not the rest of `/etc`, `/usr/local` and `/opt`, where
/// configuration and application data live. No `/proc`: see §9.3.
const READ_PATHS: &[&str] = &[
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/lib32",
    "/usr/lib64",
    "/usr/libx32",
    "/usr/libexec",
    "/usr/share",
    "/usr/local/lib",
    "/usr/local/lib64",
    "/usr/local/share",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/localtime",
    "/nix/store",
];

/// The x32 ABI of x86_64 reaches the native syscalls under numbers with this bit set.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

struct Prepared {
    ruleset: OwnedFd,
    filter: BpfProgram,
}

pub fn command(tool: &Tool, args: &[String], scratch: &Path) -> io::Result<duct::Expression> {
    let prepared = Arc::new(Prepared::new(tool, scratch)?);
    Ok(duct::cmd(&tool.path, args).before_spawn(move |cmd| {
        let p = prepared.clone();
        // SAFETY: `apply` only makes syscalls; it doesn't allocate or take locks.
        unsafe { cmd.pre_exec(move || p.apply()) };
        Ok(())
    }))
}

impl Prepared {
    fn new(tool: &Tool, scratch: &Path) -> io::Result<Prepared> {
        let abi = kernel_abi();
        if abi == ABI::Unsupported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Landlock is not available in this kernel",
            ));
        }
        // Before ABI 3 Landlock can't stop a process from truncating files it may not write.
        if abi < ABI::V3 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Landlock in this kernel is too old (Linux 6.2 or later is needed)",
            ));
        }
        let read: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::ReadDir;
        let write = AccessFs::from_write(abi);
        let mut exec_paths = vec![tool.real.clone()];
        exec_paths.extend(elf_interpreter(&tool.real));
        let mut read_paths: Vec<PathBuf> = READ_PATHS.iter().map(PathBuf::from).collect();
        // musl's library search path, if the system configures one.
        read_paths.push(format!("/etc/ld-musl-{}.path", std::env::consts::ARCH).into());
        // A tool installed under /opt reads its own package directory, not its neighbours'.
        if let Ok(rest) = tool.real.strip_prefix("/opt")
            && let Some(package) = rest.components().next()
        {
            read_paths.push(Path::new("/opt").join(package));
        }

        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(abi))
            // Since ABI 6: no signals to processes outside the sandbox, and no connections
            // to their abstract Unix sockets.
            .and_then(|r| match Scope::from_all(abi) {
                scope if scope.is_empty() => Ok(r),
                scope => r.scope(scope),
            })
            .and_then(|r| r.create())
            .and_then(|r| r.add_rules(path_beneath_rules(&read_paths, read)))
            .and_then(|r| r.add_rules(path_beneath_rules(["/dev/urandom"], AccessFs::ReadFile)))
            .and_then(|r| {
                r.add_rules(path_beneath_rules(
                    ["/dev/null"],
                    AccessFs::ReadFile | AccessFs::WriteFile,
                ))
            })
            .and_then(|r| r.add_rules(path_beneath_rules([scratch], read | write)))
            .and_then(|r| {
                r.add_rules(path_beneath_rules(
                    &exec_paths,
                    AccessFs::Execute | AccessFs::ReadFile,
                ))
            })
            .map_err(|e| io::Error::other(format!("Landlock: {e}")))?;
        let ruleset: Option<OwnedFd> = ruleset.into();
        let ruleset = ruleset.ok_or_else(|| io::Error::other("Landlock: no ruleset fd"))?;
        Ok(Prepared {
            ruleset,
            filter: network_filter()?,
        })
    }

    /// Runs in the forked child: syscalls only.
    fn apply(&self) -> io::Result<()> {
        // SAFETY: plain syscalls with valid arguments; `self` outlives the call.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            let prog = libc::sock_fprog {
                len: self.filter.len() as u16,
                filter: self.filter.as_ptr() as *mut libc::sock_filter,
            };
            if libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &prog as *const _,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// The seccomp program: `socket` and `io_uring_setup` fail with EPERM, under every ABI.
fn network_filter() -> io::Result<BpfProgram> {
    let rules = BTreeMap::from([
        (libc::SYS_socket, vec![]),
        (libc::SYS_io_uring_setup, vec![]),
    ]);
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| io::Error::other(format!("seccomp: {e}")))?;
    let by_number: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .and_then(|f| f.try_into())
    .map_err(|e| io::Error::other(format!("seccomp: {e}")))?;
    // seccompiler matches exact syscall numbers, and an x32 number is a different number for
    // the same syscall. So first refuse every number with the x32 bit; no other ABI has one.
    let insn = |code: u32, jt, jf, k| sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    let mut filter = vec![
        // The syscall number is the first field of `seccomp_data`.
        insn(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        insn(
            libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K,
            0,
            1,
            X32_SYSCALL_BIT,
        ),
        insn(
            libc::BPF_RET | libc::BPF_K,
            0,
            0,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ),
    ];
    filter.extend(by_number);
    Ok(filter)
}

fn kernel_abi() -> ABI {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: querying the ABI version takes no pointers.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    ABI::from(v as i32)
}

/// The dynamic loader named in the binary's PT_INTERP header, if any (ELF64 little-endian).
fn elf_interpreter(binary: &Path) -> Option<PathBuf> {
    let mut data = Vec::new();
    fs::File::open(binary)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let u16_at = |o: usize| Some(u16::from_le_bytes(data.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(data.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_le_bytes(data.get(o..o + 8)?.try_into().ok()?));
    let phoff = u64_at(0x20)? as usize;
    let phentsize = u16_at(0x36)? as usize;
    let phnum = u16_at(0x38)? as usize;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(ph)? != 3 {
            continue; // not PT_INTERP
        }
        let off = u64_at(ph + 8)? as usize;
        let size = u64_at(ph + 32)? as usize;
        let raw = data.get(off..off + size)?;
        let path =
            PathBuf::from(String::from_utf8_lossy(raw.split(|&b| b == 0).next()?).into_owned());
        return fs::canonicalize(&path).ok().or(Some(path));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    const NATIVE: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const NATIVE: u32 = 0xC000_00B7;
    const OTHER: u32 = 0x4000_0003; // AUDIT_ARCH_I386

    /// What the kernel's seccomp would return for this syscall: a classic BPF interpreter
    /// for the instructions the filter uses.
    fn verdict(filter: &[sock_filter], arch: u32, nr: u32) -> u32 {
        let (mut pc, mut a) = (0, 0);
        loop {
            let i = &filter[pc];
            pc += 1;
            match u32::from(i.code) {
                c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    a = match i.k {
                        0 => nr,
                        4 => arch,
                        k => panic!("load of seccomp_data offset {k}"),
                    }
                }
                c if c == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => {
                    pc += usize::from(if a == i.k { i.jt } else { i.jf })
                }
                c if c == libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K => {
                    pc += usize::from(if a >= i.k { i.jt } else { i.jf })
                }
                c if c == libc::BPF_JMP | libc::BPF_JA => pc += i.k as usize,
                c if c == libc::BPF_RET | libc::BPF_K => return i.k,
                c => panic!("unexpected BPF instruction {c:#x}"),
            }
        }
    }

    #[test]
    fn network_syscalls_are_denied_under_every_abi() {
        let filter = network_filter().unwrap();
        let denied = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        for nr in [libc::SYS_socket as u32, libc::SYS_io_uring_setup as u32] {
            assert_eq!(verdict(&filter, NATIVE, nr), denied);
            assert_eq!(verdict(&filter, NATIVE, nr | X32_SYSCALL_BIT), denied);
        }
        let read = libc::SYS_read as u32;
        assert_eq!(verdict(&filter, NATIVE, read), libc::SECCOMP_RET_ALLOW);
        assert_eq!(verdict(&filter, NATIVE, read | X32_SYSCALL_BIT), denied);
        assert_eq!(
            verdict(&filter, OTHER, read),
            libc::SECCOMP_RET_KILL_PROCESS
        );
    }
}
