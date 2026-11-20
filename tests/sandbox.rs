//! Sandbox conformance tests (§9.7). Each pipeline runs through the real executor and
//! must be blocked, with the canary file and the secrets untouched.

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::*;
use fov::exec::{Executor, Outcome};
use fov::sandbox::Sandbox;

const SECRET: &str = "s3cr3t-5f2e1c";

struct Setup {
    home_secret: PathBuf,
    tmp_secret: PathBuf,
    user_tmp_secret: PathBuf,
    _files: Vec<tempfile::NamedTempFile>,
}

fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        // SAFETY: set once, before any sandboxed process is spawned by these tests.
        unsafe { std::env::set_var("FOV_TEST_SECRET", SECRET) };
        let home = std::env::var_os("HOME").expect("HOME");
        let secret_in = |dir: &Path| {
            let mut f = tempfile::Builder::new()
                .prefix(".fov-secret-")
                .tempfile_in(dir)
                .unwrap();
            std::io::Write::write_all(&mut f, format!("{SECRET}\n").as_bytes()).unwrap();
            f
        };
        let files = vec![
            secret_in(Path::new(&home)),
            secret_in(Path::new("/tmp")),
            secret_in(&std::env::temp_dir()),
        ];
        Setup {
            home_secret: files[0].path().to_path_buf(),
            tmp_secret: files[1].path().to_path_buf(),
            user_tmp_secret: files[2].path().to_path_buf(),
            _files: files,
        }
    })
}

fn sandboxed() -> (Executor, tempfile::TempDir) {
    setup();
    executor(Sandbox::On)
}

fn canary() -> PathBuf {
    let p = PathBuf::from(format!(
        "/tmp/fov-canary-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Run `stages` on sales.csv; the output must not contain the secret.
fn run_clean(stages: &[(&str, &[&str])]) -> fov::exec::Run {
    let (exec, _dir) = sandboxed();
    let run = preview(&exec, &pipeline(stages), &fixture("sales.csv"));
    let all = format!("{}{}", stdout(&run), String::from_utf8_lossy(&run.stderr));
    assert!(!all.contains(SECRET), "secret leaked by {stages:?}: {all}");
    run
}

#[test]
fn awk_cannot_exec() {
    let c = canary();
    run_clean(&[("awk", &[&format!(r#"BEGIN{{system("touch {}")}}"#, s(&c))])]);
    assert!(!c.exists());
    let run = run_clean(&[("awk", &[r#"BEGIN{"id" | getline x; print x}"#])]);
    assert!(!stdout(&run).contains("uid="), "{}", stdout(&run));
}

#[test]
fn awk_cannot_write() {
    let c = canary();
    run_clean(&[("awk", &[&format!(r#"BEGIN{{print 1 > "{}"}}"#, s(&c))])]);
    assert!(!c.exists());
}

#[test]
fn awk_cannot_read_secrets() {
    let st = setup();
    let spool_dir = tempfile::tempdir().unwrap();
    let spool = spool_dir.path().join("stdin");
    std::fs::write(&spool, format!("{SECRET}\n")).unwrap();
    for f in [&st.home_secret, &st.tmp_secret, &st.user_tmp_secret, &spool] {
        let run = run_clean(&[(
            "awk",
            &[&format!(
                r#"BEGIN{{while((getline l < "{}") > 0) print l}}"#,
                s(f)
            )],
        )]);
        assert!(stdout(&run).is_empty(), "read {}", f.display());
    }
    if cfg!(target_os = "linux") {
        for f in [
            "/proc/self/environ",
            &format!("/proc/{}/environ", std::process::id()),
        ] {
            run_clean(&[(
                "awk",
                &[&format!(
                    r#"BEGIN{{RS="\0"; while((getline l < "{f}") > 0) print l}}"#
                )],
            )]);
        }
    }
}

#[test]
fn awk_cannot_read_a_terminal() {
    let mut master = 0;
    let mut slave = 0;
    // SAFETY: openpty fills in two fds; the name buffer is unused.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0);
    // SAFETY: slave is a valid fd; ttyname returns a NUL-terminated static buffer.
    let tty = unsafe { std::ffi::CStr::from_ptr(libc::ttyname(slave)) }
        .to_string_lossy()
        .into_owned();
    let line = format!("{SECRET}\n");
    // SAFETY: writing a valid buffer to the pty master.
    unsafe { libc::write(master, line.as_ptr().cast(), line.len()) };
    let run = run_clean(&[(
        "awk",
        &[&format!(
            r#"BEGIN{{if ((getline l < "{tty}") > 0) print "READ:" l}}"#
        )],
    )]);
    assert!(!stdout(&run).contains("READ:"));
    // SAFETY: closing our own fds.
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
}

#[test]
fn sed_cannot_exec_or_write() {
    let c = canary();
    run_clean(&[("sed", &["1e id"])]);
    run_clean(&[("sed", &["-n", &format!("1w {}", s(&c))])]);
    assert!(!c.exists());
    let target = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    std::fs::write(target.path(), "keep\n").unwrap();
    run_clean(&[("sed", &["-i", "", "s/keep/gone/", &s(target.path())])]);
    run_clean(&[("sed", &["-i", "s/keep/gone/", &s(target.path())])]);
    assert_eq!(std::fs::read_to_string(target.path()).unwrap(), "keep\n");
}

#[test]
fn jq_cannot_read_files_or_env() {
    let st = setup();
    let run = run_clean(&[("jq", &["--rawfile", "s", &s(&st.home_secret), "-n", "$s"])]);
    assert_ne!(run.outcome, Outcome::Ok);
    let run = run_clean(&[("jq", &["-n", "$ENV"])]);
    assert_eq!(run.outcome, Outcome::Ok);
    assert!(!stdout(&run).contains("FOV_TEST_SECRET"));
}

#[test]
fn grep_cannot_read_pattern_files() {
    let st = setup();
    let run = run_clean(&[("grep", &["-f", &s(&st.home_secret)])]);
    assert!(
        matches!(run.outcome, Outcome::Failed { .. }),
        "{:?}",
        run.outcome
    );
}

#[test]
fn system_configuration_is_not_readable() {
    // Real paths: on macOS `/etc` is a symlink to `/private/etc`.
    let file = if cfg!(target_os = "macos") {
        "/private/etc/hosts"
    } else {
        "/etc/passwd"
    };
    assert!(Path::new(file).exists());
    let run = run_clean(&[("cat", &[file])]);
    assert!(matches!(run.outcome, Outcome::Failed { .. }));
    assert!(stdout(&run).is_empty(), "read {file}");
}

/// On macOS a stage reads only the listed sysctls (§9.4), not e.g. `kern.procargs2`, the
/// arguments and environment of every process of the user. The probe reads the machine's
/// `kern.uuid`, which isn't listed, and `hw.ncpu`, which is.
#[cfg(target_os = "macos")]
#[test]
fn only_listed_sysctls_are_readable() {
    let uuid: &[&str] = &["-n", "kern.uuid"];
    let (off, _dir) = executor(Sandbox::Off);
    let run = preview(&off, &pipeline(&[("sysctl", uuid)]), &fixture("sales.csv"));
    assert!(!stdout(&run).is_empty(), "the probe works unsandboxed");
    let run = run_clean(&[("sysctl", uuid)]);
    assert!(stdout(&run).is_empty());
    let run = run_clean(&[("sysctl", &["-n", "hw.ncpu"])]);
    assert_eq!(run.outcome, Outcome::Ok);
    assert!(!stdout(&run).is_empty());
}

/// Local time in a preview is the local time of the reused command. Only telling on a
/// machine whose timezone isn't UTC.
#[test]
fn local_time_is_the_same_as_outside() {
    let probe: &[&str] = &["-rn", r#"1750000000 | strflocaltime("%H:%M")"#];
    let (off, _dir) = executor(Sandbox::Off);
    let outside = preview(&off, &pipeline(&[("jq", probe)]), &fixture("sales.csv"));
    let inside = run_clean(&[("jq", probe)]);
    assert_eq!(inside.outcome, Outcome::Ok);
    assert_eq!(stdout(&inside), stdout(&outside));
}

/// A tool that can make any syscall (here perl) can't signal a process outside the sandbox.
/// On Linux that takes Landlock ABI 6 (Linux 6.12).
#[test]
fn cannot_signal_other_processes() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: querying the Landlock ABI version takes no pointers.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0,
                1,
            )
        };
        if abi < 6 {
            return;
        }
    }
    if !installed("perl") {
        return;
    }
    // Signal 0 only checks the permission. The parent is this test process.
    let probe = r#"print kill(0, getppid()) ? "ALLOWED" : "denied""#;
    let (off, _dir) = executor(Sandbox::Off);
    let p = pipeline(&[("perl", &["-e", probe])]);
    let run = preview(&off, &p, &fixture("sales.csv"));
    assert_eq!(stdout(&run), "ALLOWED", "the probe works unsandboxed");
    let run = run_clean(&[("perl", &["-e", probe])]);
    assert_eq!(stdout(&run), "denied");
}

#[test]
fn gawk_has_no_network() {
    if !installed("gawk") {
        return;
    }
    let run = run_clean(&[(
        "gawk",
        &[
            r#"BEGIN{s="/inet/tcp/0/example.com/80"; print "GET / HTTP/1.0\r\n\r" |& s; if ((s |& getline l) > 0) print "NET:" l}"#,
        ],
    )]);
    assert!(!stdout(&run).contains("NET:"));
}

#[test]
fn sort_spills_to_scratch() {
    let (exec, dir) = sandboxed();
    let big = dir.path().join("big.txt");
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&big).unwrap());
        let mut x: u64 = 0x2545F4914F6CDD1D;
        while f.get_ref().metadata().unwrap().len() < 100 << 20 {
            for _ in 0..10_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                writeln!(f, "{x:020}").unwrap();
            }
            f.flush().unwrap();
        }
    }
    let p = pipeline(&[("sort", &[]), ("tail", &["-1"])]);
    // This checks that spilling works, not speed: BSD sort takes 10–30 s here.
    let timeout = std::time::Duration::from_secs(600);
    let run = exec.run_preview(&p, &full(&big), None, timeout, &Default::default());
    assert_eq!(
        run.outcome,
        Outcome::Ok,
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout_lines, 1);
}

#[test]
fn every_default_tool_runs() {
    let (exec, _dir) = sandboxed();
    let cases: &[(&str, &[&str])] = &[
        ("awk", &["-F,", "{print $3}"]),
        ("cat", &[]),
        ("column", &["-t", "-s,"]),
        ("cut", &["-d,", "-f3"]),
        ("grep", &["North"]),
        ("head", &["-3"]),
        ("nl", &[]),
        ("paste", &["-", "-"]),
        ("rev", &[]),
        ("sed", &["-n", "2p"]),
        ("sort", &["-t,", "-k3"]),
        ("tac", &[]),
        ("tail", &["-3"]),
        ("tr", &["a-z", "A-Z"]),
        ("uniq", &["-c"]),
        ("wc", &["-l"]),
    ];
    for (tool, args) in cases {
        if !installed(tool) {
            continue;
        }
        let run = preview(&exec, &pipeline(&[(tool, args)]), &fixture("sales.csv"));
        assert_eq!(
            run.outcome,
            Outcome::Ok,
            "{tool}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(!run.stdout.is_empty(), "{tool} printed nothing");
    }
    let run = preview(
        &exec,
        &pipeline(&[("jq", &["-r", ".type"])]),
        &fixture("github-events.ndjson"),
    );
    assert_eq!(
        run.outcome,
        Outcome::Ok,
        "jq: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn probe_passes() {
    let (exec, _dir) = sandboxed();
    exec.probe_sandbox().unwrap();
}
