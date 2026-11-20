//! The input: a regular file, or stdin spooled to a temp file (§5).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;

use tokio::sync::watch;

pub const SPOOL_LIMIT: u64 = 1 << 30;
const CHUNK: usize = 64 << 10;

/// Which part of the input a preview ran on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Full,
    Sample,
    Partial,
}

/// A prefix of the input file: `len` bytes, or all of it.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub path: PathBuf,
    pub len: Option<u64>,
    pub scope: Scope,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Bytes available in `path`.
    pub bytes: u64,
    /// Lines counted so far.
    pub lines: u64,
    /// All data is available (EOF on stdin; always for a file).
    pub complete: bool,
    /// `lines` is the final count.
    pub counted: bool,
    pub error: Option<String>,
}

pub struct Input {
    name: String,
    path: PathBuf,
    redirect: Option<String>,
    progress: watch::Receiver<Progress>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Ndjson,
    Csv { delimiter: char, header: bool },
    Tsv { header: bool },
    Text,
}

/// The start of the input as sent to the model (§5.3).
#[derive(Clone, Debug)]
pub struct Head {
    pub text: String,
    pub bytes: usize,
    pub complete_lines: usize,
    pub covers_all: bool,
    pub cut_mid_line: bool,
    pub invalid_utf8: bool,
    pub binary: bool,
    pub format: Format,
}

impl Input {
    /// A regular file, used directly. `given` is the INPUT argument as typed.
    pub fn file(path: &Path, given: &str) -> io::Result<Input> {
        let bytes = std::fs::metadata(path)?.len();
        let name = path
            .file_name()
            .map_or_else(|| given.to_string(), |n| n.to_string_lossy().into_owned());
        let (tx, rx) = watch::channel(Progress {
            bytes,
            complete: true,
            ..Progress::default()
        });
        let file = File::open(path)?;
        thread::spawn(move || count_lines(file, &tx));
        Ok(Input {
            name,
            path: path.to_path_buf(),
            redirect: Some(given.to_string()),
            progress: rx,
        })
    }

    /// Copy `source` into `dir/stdin` on a background thread, up to `limit` bytes.
    pub fn spool(
        source: File,
        name: &str,
        redirect: Option<String>,
        dir: &Path,
        limit: u64,
    ) -> io::Result<Input> {
        let path = dir.join("stdin");
        let out = File::create(&path)?;
        let (tx, rx) = watch::channel(Progress::default());
        thread::spawn(move || spool(source, out, &tx, limit));
        Ok(Input {
            name: name.to_string(),
            path,
            redirect,
            progress: rx,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file the pipelines read: the input file or the spool.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The INPUT argument, for `< FILE` in the printed command.
    pub fn redirect(&self) -> Option<&str> {
        self.redirect.as_deref()
    }

    pub fn progress(&self) -> Progress {
        self.progress.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// Wait until all data is available (or the spool failed).
    pub async fn wait_complete(&self) -> Progress {
        let mut rx = self.progress.clone();
        let p = rx
            .wait_for(|p| p.complete || p.error.is_some())
            .await
            .map(|p| p.clone());
        p.unwrap_or_else(|_| self.progress())
    }

    /// Wait until the head can be cut, then cut it.
    pub async fn head(&self, head_bytes: usize, head_max: usize) -> io::Result<Head> {
        let mut rx = self.progress.clone();
        let mut sender_gone = false;
        loop {
            let p = rx.borrow_and_update().clone();
            if let Some(e) = p.error {
                return Err(io::Error::other(e));
            }
            let buf = read_prefix(&self.path, (head_max as u64).min(p.bytes))?;
            if let Some((len, cut_mid_line)) = cut_head(&buf, p.complete, head_bytes, head_max) {
                return Ok(Head::new(
                    &buf[..len],
                    cut_mid_line,
                    p.complete && len as u64 == p.bytes,
                ));
            }
            if sender_gone {
                return Err(io::Error::other("input ended unexpectedly"));
            }
            sender_gone = rx.changed().await.is_err();
        }
    }

    /// Whether a prefix of the input that ends at a line boundary is still valid input (§8.3).
    pub fn cuttable(&self, head: &Head, sample_bytes: u64) -> bool {
        if head.format == Format::Json {
            return false;
        }
        if head.complete_lines > 0 {
            return true;
        }
        let p = self.progress();
        let Ok(buf) = read_prefix(&self.path, sample_bytes.min(p.bytes)) else {
            return false;
        };
        memchr::memchr(b'\n', &buf).is_some()
    }

    /// What the next preview runs on: the full input (plus a sample to fall back to), or the
    /// PARTIAL input so far. None means: wait for EOF first.
    pub fn preview_plan(
        &self,
        cuttable: bool,
        sample_bytes: u64,
    ) -> io::Result<Option<(Snapshot, Option<Snapshot>)>> {
        let p = self.progress();
        let (primary, available) = if p.complete {
            (
                Snapshot {
                    path: self.path.clone(),
                    len: None,
                    scope: Scope::Full,
                },
                p.bytes,
            )
        } else if cuttable {
            let end = last_line_end(&self.path, p.bytes)?;
            (
                Snapshot {
                    path: self.path.clone(),
                    len: Some(end),
                    scope: Scope::Partial,
                },
                end,
            )
        } else {
            return Ok(None);
        };
        let sample = if cuttable && available > sample_bytes {
            let buf = read_prefix(&self.path, sample_bytes)?;
            memchr::memrchr(b'\n', &buf).map(|i| Snapshot {
                path: self.path.clone(),
                len: Some(i as u64 + 1),
                scope: Scope::Sample,
            })
        } else {
            None
        };
        Ok(Some((primary, sample)))
    }

    /// The start of the input for the input pane: at most `max_lines` lines or `max_bytes`.
    pub fn view(&self, max_lines: usize, max_bytes: usize) -> io::Result<Vec<u8>> {
        let p = self.progress();
        let mut buf = read_prefix(&self.path, (max_bytes as u64).min(p.bytes))?;
        if let Some(i) = memchr::memchr_iter(b'\n', &buf).nth(max_lines.saturating_sub(1)) {
            buf.truncate(i + 1);
        }
        Ok(buf)
    }
}

impl Head {
    fn new(raw: &[u8], cut_mid_line: bool, covers_all: bool) -> Head {
        let text = String::from_utf8_lossy(raw);
        Head {
            invalid_utf8: matches!(text, std::borrow::Cow::Owned(_)),
            text: text.into_owned(),
            bytes: raw.len(),
            complete_lines: memchr::memchr_iter(b'\n', raw).count(),
            covers_all,
            cut_mid_line,
            binary: memchr::memchr(0, raw).is_some(),
            format: guess_format(raw, covers_all),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header = |h: bool| if h { "header row" } else { "no header row" };
        match self {
            Format::Json => write!(f, "json (a single document)"),
            Format::Ndjson => write!(f, "ndjson (one JSON value per line)"),
            Format::Csv {
                delimiter,
                header: h,
            } => write!(f, "csv (delimiter {delimiter:?}, {})", header(*h)),
            Format::Tsv { header: h } => write!(f, "tsv ({})", header(*h)),
            Format::Text => write!(f, "text"),
        }
    }
}

/// Where to cut the head: `(length, cut_mid_line)`, or None to wait for more data.
fn cut_head(buf: &[u8], complete: bool, head: usize, head_max: usize) -> Option<(usize, bool)> {
    if complete && buf.len() <= head {
        return Some((buf.len(), false));
    }
    if buf.len() < head {
        return None;
    }
    if let Some(i) = memchr::memrchr(b'\n', &buf[..head]) {
        return Some((i + 1, false));
    }
    if let Some(i) = memchr::memchr(b'\n', &buf[..buf.len().min(head_max)]) {
        return Some((i + 1, false));
    }
    if buf.len() >= head_max {
        return Some((utf8_floor(buf, head_max), true));
    }
    complete.then_some((buf.len(), false))
}

/// The largest cut ≤ `n` that doesn't split a UTF-8 sequence.
fn utf8_floor(buf: &[u8], n: usize) -> usize {
    let mut start = n;
    while start > 0 && start > n.saturating_sub(4) && (buf[start - 1] & 0xC0) == 0x80 {
        start -= 1;
    }
    if start == 0 {
        return n;
    }
    let lead = buf[start - 1];
    let len = match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return n, // continuation run without a lead byte: not UTF-8 anyway
    };
    if start - 1 + len <= n { n } else { start - 1 }
}

fn guess_format(head: &[u8], covers_all: bool) -> Format {
    let trimmed = head.trim_ascii_start();
    if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
        let first_end = memchr::memchr(b'\n', trimmed);
        let first_parses = first_end
            .is_some_and(|i| serde_json::from_slice::<serde_json::Value>(&trimmed[..i]).is_ok());
        if !first_parses {
            return Format::Json;
        }
        let more = memchr::memchr_iter(b'\n', trimmed).count() > 1 || !covers_all;
        return if more { Format::Ndjson } else { Format::Json };
    }
    let lines: Vec<&[u8]> = head
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .take(20)
        .collect();
    // Skip the last line when it's cut off.
    let lines = if lines.len() > 2 && !head.ends_with(b"\n") {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    if lines.len() < 2 {
        return Format::Text;
    }
    for delimiter in *b"\t,;|" {
        let count = |l: &[u8]| memchr::memchr_iter(delimiter, l).count();
        let n = count(lines[0]);
        if n == 0 || !lines.iter().all(|l| count(l) == n) {
            continue;
        }
        let numeric = |l: &[u8]| {
            l.split(|&b| b == delimiter)
                .any(|f| std::str::from_utf8(f).is_ok_and(|s| s.trim().parse::<f64>().is_ok()))
        };
        let header = !numeric(lines[0]) && lines[1..].iter().any(|l| numeric(l));
        return if delimiter == b'\t' {
            Format::Tsv { header }
        } else {
            Format::Csv {
                delimiter: delimiter as char,
                header,
            }
        };
    }
    Format::Text
}

fn read_prefix(path: &Path, len: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(len as usize);
    File::open(path)?.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// The offset just after the last newline before `end` (0 if there is none).
fn last_line_end(path: &Path, end: u64) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut pos = end;
    let mut buf = vec![0u8; CHUNK];
    while pos > 0 {
        let n = (CHUNK as u64).min(pos);
        f.seek(SeekFrom::Start(pos - n))?;
        f.read_exact(&mut buf[..n as usize])?;
        if let Some(i) = memchr::memrchr(b'\n', &buf[..n as usize]) {
            return Ok(pos - n + i as u64 + 1);
        }
        pos -= n;
    }
    Ok(0)
}

fn count_lines(mut file: File, tx: &watch::Sender<Progress>) {
    let mut buf = vec![0u8; 1 << 20];
    let (mut lines, mut last) = (0u64, b'\n');
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                lines += memchr::memchr_iter(b'\n', &buf[..n]).count() as u64;
                last = buf[n - 1];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    lines += u64::from(last != b'\n');
    tx.send_modify(|p| {
        p.lines = lines;
        p.counted = true;
    });
}

fn spool(mut source: File, mut out: File, tx: &watch::Sender<Progress>, limit: u64) {
    let mut buf = vec![0u8; CHUNK];
    let (mut bytes, mut last) = (0u64, b'\n');
    loop {
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return tx.send_modify(|p| p.error = Some(format!("reading the input: {e}"))),
        };
        if bytes + n as u64 > limit {
            let size = if limit >= 1 << 30 {
                format!("{} GiB", limit >> 30)
            } else {
                format!("{limit} bytes")
            };
            let msg = format!(
                "the input is larger than the {size} stdin limit; save it to a file and pass that as INPUT instead"
            );
            return tx.send_modify(|p| p.error = Some(msg));
        }
        if let Err(e) = out.write_all(&buf[..n]) {
            return tx.send_modify(|p| p.error = Some(format!("writing the spool file: {e}")));
        }
        bytes += n as u64;
        last = buf[n - 1];
        let lines = memchr::memchr_iter(b'\n', &buf[..n]).count() as u64;
        tx.send_modify(|p| {
            p.bytes += n as u64;
            p.lines += lines;
        });
    }
    tx.send_modify(|p| {
        p.lines += u64::from(p.bytes > 0 && last != b'\n');
        p.complete = true;
        p.counted = true;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: usize = 1024;

    #[test]
    fn head_small_input_is_all_of_it() {
        assert_eq!(cut_head(b"a\nb", true, 16 * K, 64 * K), Some((3, false)));
        assert_eq!(
            cut_head(b"a\nb", false, 16 * K, 64 * K),
            None,
            "wait while stdin may grow"
        );
    }

    #[test]
    fn head_cuts_after_last_newline_in_window() {
        let mut buf = vec![b'x'; 20 * K];
        buf[100] = b'\n';
        buf[16 * K - 1] = b'\n';
        buf[16 * K + 5] = b'\n';
        assert_eq!(cut_head(&buf, false, 16 * K, 64 * K), Some((16 * K, false)));
        buf[16 * K - 1] = b'x';
        assert_eq!(cut_head(&buf, false, 16 * K, 64 * K), Some((101, false)));
    }

    #[test]
    fn head_extends_to_first_newline_within_max() {
        let mut buf = vec![b'x'; 64 * K];
        buf[16_570] = b'\n'; // github-events.ndjson: first record is 16,571 B
        assert_eq!(cut_head(&buf, false, 16 * K, 64 * K), Some((16_571, false)));
        assert_eq!(cut_head(&buf[..10_000], false, 16 * K, 64 * K), None);
        assert_eq!(
            cut_head(&buf[..20_000], false, 16 * K, 64 * K),
            Some((16_571, false))
        );
        // Custom sizes.
        assert_eq!(
            cut_head(&buf, false, 4 * K, 8 * K),
            Some((utf8_floor(&buf, 8 * K), true))
        );
    }

    #[test]
    fn head_cut_mid_line_at_utf8_boundary() {
        let mut buf = vec![b'x'; 64 * K - 1];
        buf.extend("é".as_bytes()); // 2 bytes straddling the 64 KiB mark
        buf.extend(vec![b'x'; 100]);
        let (len, mid) = cut_head(&buf[..64 * K], false, 16 * K, 64 * K).unwrap();
        assert!(mid);
        assert_eq!(len, 64 * K - 1);
        assert!(std::str::from_utf8(&buf[..len]).is_ok());
        // Without a newline but at EOF: all of it, not mid-line.
        assert_eq!(
            cut_head(&buf[..30 * K], true, 16 * K, 64 * K),
            Some((30 * K, false))
        );
    }

    #[test]
    fn head_flags() {
        let h = Head::new(b"a\0b\xff\n", false, true);
        assert!(h.binary && h.invalid_utf8);
        assert_eq!(h.complete_lines, 1);
        assert!(h.text.contains('\u{fffd}'));
    }

    #[test]
    fn format_guess() {
        let csv = b"order_id,region,qty\nORD-1,North,2\nORD-2,South,3\n";
        assert_eq!(
            guess_format(csv, true),
            Format::Csv {
                delimiter: ',',
                header: true
            }
        );
        assert_eq!(
            guess_format(b"a,b\nc,d\n", true),
            Format::Csv {
                delimiter: ',',
                header: false
            }
        );
        assert_eq!(
            guess_format(b"x\ty\n1\t2\n", true),
            Format::Tsv { header: true }
        );
        assert_eq!(
            guess_format(b"{\"a\":1}\n{\"a\":2}\n", true),
            Format::Ndjson
        );
        assert_eq!(
            guess_format(b"{\"a\":1}\n", false),
            Format::Ndjson,
            "one record, more follow"
        );
        assert_eq!(guess_format(b"{\"a\":1}\n", true), Format::Json);
        assert_eq!(guess_format(b"[\n  {\"a\": 1},\n", false), Format::Json);
        assert_eq!(guess_format(b"GET / 200\nGET /x 404\n", true), Format::Text);
    }

    #[test]
    fn fixtures() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let head = |f: &str| {
            let input = Input::file(&root.join(f), f).unwrap();
            rt.block_on(input.head(16 * K, 64 * K)).unwrap()
        };
        let h = head("github-events.ndjson");
        assert_eq!(
            (h.bytes, h.complete_lines, h.format),
            (16_571, 1, Format::Ndjson)
        );
        let h = head("github-events.json");
        assert_eq!(h.format, Format::Json);
        assert!(h.bytes <= 16 * K && h.text.ends_with('\n'));
        assert_eq!(
            head("sales.csv").format,
            Format::Csv {
                delimiter: ',',
                header: true
            }
        );
        assert_eq!(head("access.log").format, Format::Text);
    }
}
