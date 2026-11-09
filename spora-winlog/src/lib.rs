//! The human-readable log files of both executables, `spora-winui.log` and
//! `spora-wincore.log`. Each process hands env_logger the writer [`open`]
//! returns, which
//!
//! - redacts share-link secrets from every line before it reaches the file
//!   ([`redact`]). A share URL (`https://spora.to/s/<token>?r=...`) carries
//!   the tunnel's secret in its path, and URLs end up in messages (an
//!   incoming link, an error quoting one). spora-service's diagnostic log
//!   redacts them by the same rule; its function is private to that crate,
//!   so the rule is kept here as well, and the two must agree;
//! - caps the file at [`MAX_BYTES`]: a write that would take it past the cap
//!   first moves it to `<name>.1` (the previous `.1` to `.2`, and so on, the
//!   oldest past [`KEEP`] deleted), so one log never takes much more than
//!   (KEEP + 1) x MAX_BYTES on disk.
//!
//! A file left by a version that had neither (unbounded, secrets in the
//! clear) is scrubbed when it is opened: cut to its last MAX_BYTES and
//! redacted.
//!
//! Std only: the UI links it, and the UI links nothing of the core.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The size a log file rotates at.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// How many rotated files are kept beside the current one.
pub const KEEP: usize = 3;
/// The shortest run of base64url characters after `/s/` taken for a secret
/// (a share token is 60).
const MIN_SECRET: usize = 40;
/// The longest line written whole; the rest of a longer one is dropped. A
/// log record never comes near it: this only bounds the line buffer.
const MAX_LINE: usize = 64 * 1024;

/// `path` (created if need be), opened for appending, rotated at
/// [`MAX_BYTES`] with [`KEEP`] old files, every line redacted.
pub fn open(path: &Path) -> io::Result<Redacted<RotatingFile>> {
    Ok(Redacted::new(RotatingFile::open(path, MAX_BYTES, KEEP)?))
}

// ── Redaction ────────────────────────────────────────────────────────

/// Replace share-link secrets (an `s` path segment followed by 40 or more
/// base64url characters) with `<redacted>`, keeping the `/s/` before it.
/// The rule of spora-service's diagnostic log: the segment in either case
/// (`/S/`), its slashes JSON-escaped (`\/s\/`) or percent-encoded (`%2Fs%2F`,
/// `%252F` once more encoded) in any mix; the secret itself is base64url,
/// which no encoding changes. Here also right after a URI scheme
/// (`spora:s/...`, the form a `spora:` link may take). `cut` says the text
/// was truncated: a run that reaches its end may be the head of a secret, so
/// it is redacted whatever its length.
pub fn redact(s: &[u8], cut: bool) -> Cow<'_, [u8]> {
    let mut out = Vec::new();
    let mut copied = 0;
    let mut i = 0;
    while i < s.len() {
        if !matches!(s[i], b's' | b'S') || !segment_starts_at(s, i) {
            i += 1;
            continue;
        }
        let Some(after) = slash_len(&s[i + 1..]) else {
            i += 1;
            continue;
        };
        let start = i + 1 + after;
        let run = s[start..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-' || **b == b'_')
            .count();
        let at_end = start + run == s.len();
        if run >= MIN_SECRET || (cut && at_end && run > 0) {
            out.extend_from_slice(&s[copied..start]);
            out.extend_from_slice(b"<redacted>");
            copied = start + run;
            i = copied;
        } else {
            i += 1;
        }
    }
    if copied == 0 {
        return Cow::Borrowed(s);
    }
    out.extend_from_slice(&s[copied..]);
    Cow::Owned(out)
}

/// The length of the slash [`redact`] recognizes at the start of `b`: `/`
/// (after any backslashes escaping it), `%2F` or `%252F` (hex in either
/// case).
fn slash_len(b: &[u8]) -> Option<usize> {
    let mut skip = 0;
    while b.get(skip) == Some(&b'\\') {
        skip += 1;
    }
    if b.get(skip) == Some(&b'/') {
        return Some(skip + 1);
    }
    let pct = |p: &[u8]| b.len() >= p.len() && b[..p.len()].eq_ignore_ascii_case(p);
    if pct(b"%2F") {
        Some(3)
    } else if pct(b"%252F") {
        Some(5)
    } else {
        None
    }
}

/// Whether a path segment starts at `b[i]`: one of [`slash_len`]'s forms,
/// or a URI scheme's colon, ends right before it.
fn segment_starts_at(b: &[u8], i: usize) -> bool {
    let ends = |p: &[u8]| i >= p.len() && b[i - p.len()..i].eq_ignore_ascii_case(p);
    ends(b"/") || ends(b"%2F") || ends(b"%252F") || ends(b":")
}

// ── The redacting writer ─────────────────────────────────────────────

/// Passes whole lines to `W`, each redacted ([`redact`]) and in one
/// `write_all`, so no secret is ever split between two writes and `W` sees
/// line boundaries (a file rotates between lines, never inside one). A
/// partial line waits for its newline (env_logger ends every record with
/// one, and writes it in one go); a line longer than 64 KiB is cut there
/// and the rest of it dropped.
pub struct Redacted<W: Write> {
    inner: W,
    line: Vec<u8>,
    /// Dropping the rest of an over-long line, up to its newline.
    skipping: bool,
}

impl<W: Write> Redacted<W> {
    pub fn new(inner: W) -> Self {
        Redacted {
            inner,
            line: Vec::new(),
            skipping: false,
        }
    }
}

impl<W: Write> Write for Redacted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == b'\n').map(|p| p + 1);
            let (chunk, tail) = rest.split_at(end.unwrap_or(rest.len()));
            rest = tail;
            if self.skipping {
                self.skipping = end.is_none();
                continue;
            }
            self.line.extend_from_slice(chunk);
            if end.is_some() {
                let written = self.inner.write_all(&redact(&self.line, false));
                self.line.clear();
                written?;
            } else if self.line.len() > MAX_LINE {
                self.line.truncate(MAX_LINE);
                let mut cut = redact(&self.line, true).into_owned();
                cut.extend_from_slice(b" [cut]\n");
                self.line.clear();
                self.skipping = true;
                self.inner.write_all(&cut)?;
            }
        }
        Ok(buf.len())
    }

    /// Flushes what has been passed on; a partial line stays until its
    /// newline comes.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for Redacted<W> {
    fn drop(&mut self) {
        if !self.line.is_empty() {
            let _ = self.inner.write_all(&redact(&self.line, true));
            let _ = self.inner.flush();
        }
    }
}

// ── The rotating file ────────────────────────────────────────────────

/// A log file capped at `max_bytes`. A write that would take it past the
/// cap first moves it to `<name>.1`, shifting the older ones up and
/// deleting the one past `keep`, and starts an empty file (should the move
/// fail, the file is emptied instead: the cap holds either way). A single
/// write is never split, so a file exceeds the cap only by one write larger
/// than the cap.
pub struct RotatingFile {
    path: PathBuf,
    /// `None` after a failed rotation: the next write opens it again.
    file: Option<File>,
    len: u64,
    max_bytes: u64,
    keep: usize,
}

impl RotatingFile {
    /// `path`, created if need be, to append to. A file already there that
    /// is larger than the cap or holds a secret (written before either
    /// rule existed) is scrubbed first: cut to its last `max_bytes`, from a
    /// line start, and redacted. The scrub is best effort; only failing to
    /// open the file is an error.
    pub fn open(path: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let _ = scrub(path, max_bytes);
        let mut f = RotatingFile {
            path: path.to_path_buf(),
            file: None,
            len: 0,
            max_bytes,
            keep,
        };
        f.reopen()?;
        Ok(f)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.len = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    /// `<name>.<n>`.
    fn numbered(&self, n: usize) -> PathBuf {
        suffixed(&self.path, &format!(".{n}"))
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Closed first: Windows moves an open file only if every handle on
        // it allows that.
        self.file = None;
        let moved = self.shift();
        self.reopen()?;
        if moved.is_err() {
            // Could not move it (or nothing is kept): start it over rather
            // than let it grow.
            if let Some(f) = &self.file {
                f.set_len(0)?;
            }
            self.len = 0;
        }
        Ok(())
    }

    /// Moves the current file to `.1` and every older one up by one.
    fn shift(&self) -> io::Result<()> {
        if self.keep == 0 {
            return Err(io::Error::other("no rotated files kept"));
        }
        let gone = |r: io::Result<()>| match r {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        gone(fs::remove_file(self.numbered(self.keep)))?;
        for n in (1..self.keep).rev() {
            gone(fs::rename(self.numbered(n), self.numbered(n + 1)))?;
        }
        fs::rename(&self.path, self.numbered(1))
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.reopen()?;
        }
        if self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let Some(f) = self.file.as_mut() else {
            return Err(io::Error::other("log file not open"));
        };
        f.write_all(buf)?;
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    s.into()
}

/// Cuts `path` to its last `max_bytes` (from a line start) and redacts it,
/// rewriting it only when either changes something.
fn scrub(path: &Path, max_bytes: u64) -> io::Result<()> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let too_long = len > max_bytes;
    if too_long {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut data = Vec::new();
    file.take(max_bytes).read_to_end(&mut data)?;
    let mut body = data.as_slice();
    if too_long {
        // What precedes the first newline is the tail of a line.
        body = match body.iter().position(|&b| b == b'\n') {
            Some(p) => &body[p + 1..],
            None => &[],
        };
    }
    // A file that does not end with a newline was cut mid-line.
    let cut = !body.is_empty() && !body.ends_with(b"\n");
    let clean = redact(body, cut);
    if !too_long && matches!(clean, Cow::Borrowed(_)) {
        return Ok(());
    }
    let tmp = suffixed(path, ".tmp");
    fs::write(&tmp, &clean)
        .and_then(|()| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A share token: 60 base64url characters.
    const SECRET: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdEfGhIjKlMnOpQrStUv";

    fn r(s: &str, cut: bool) -> String {
        String::from_utf8(redact(s.as_bytes(), cut).into_owned()).unwrap()
    }

    #[test]
    fn redaction() {
        assert_eq!(SECRET.len(), 60);
        let url = format!("https://spora.to/s/{SECRET}?r=relay.example:443");
        assert_eq!(
            r(&format!("Handling incoming URL: {url}"), false),
            "Handling incoming URL: https://spora.to/s/<redacted>?r=relay.example:443"
        );
        // Two in one message.
        assert_eq!(
            r(&format!("{url} {url}"), false),
            "https://spora.to/s/<redacted>?r=relay.example:443 \
             https://spora.to/s/<redacted>?r=relay.example:443"
        );
        // Shorter runs are not secrets.
        let short = format!("/s/{}", &SECRET[..39]);
        assert_eq!(r(&short, false), short);
        assert!(matches!(redact(b"no urls here", false), Cow::Borrowed(_)));
        // An earlier `/s/` must not hide a later one.
        assert_eq!(r(&format!("/s/s/{SECRET}"), false), "/s/s/<redacted>");
        // At a cut end, any run may be the head of a secret.
        assert_eq!(r("see /s/AbCd", true), "see /s/<redacted>");
        assert_eq!(r("see /s/AbCd", false), "see /s/AbCd");
        assert_eq!(r("see /s/AbCd x", true), "see /s/AbCd x");
    }

    #[test]
    fn redaction_of_encoded_urls() {
        // JSON-escaped, once and twice over.
        assert_eq!(
            r(
                &format!(r#"{{"url":"https:\/\/spora.to\/s\/{SECRET}?r=x"}}"#),
                false
            ),
            r#"{"url":"https:\/\/spora.to\/s\/<redacted>?r=x"}"#
        );
        assert_eq!(
            r(&format!(r"https:\\/\\/spora.to\\/s\\/{SECRET}"), false),
            r"https:\\/\\/spora.to\\/s\\/<redacted>"
        );
        // Percent-encoded, either case, once or twice, and mixed.
        for (form, want) in [
            (
                format!("u=https%3A%2F%2Fspora.to%2Fs%2F{SECRET}%3Fr"),
                "u=https%3A%2F%2Fspora.to%2Fs%2F<redacted>%3Fr",
            ),
            (
                format!("u=spora.to%2fs%2f{SECRET}"),
                "u=spora.to%2fs%2f<redacted>",
            ),
            (
                format!("u=spora.to%252Fs%252F{SECRET}"),
                "u=spora.to%252Fs%252F<redacted>",
            ),
            (format!("got /s%2F{SECRET}"), "got /s%2F<redacted>"),
            (format!("got %2Fs/{SECRET}"), "got %2Fs/<redacted>"),
        ] {
            assert_eq!(r(&form, false), want);
        }
        // Case.
        assert_eq!(
            r(&format!("HTTPS://SPORA.TO/S/{SECRET}"), false),
            "HTTPS://SPORA.TO/S/<redacted>"
        );
        // The spora: scheme, with and without slashes.
        assert_eq!(r(&format!("spora:s/{SECRET}"), false), "spora:s/<redacted>");
        assert_eq!(
            r(&format!("spora://s/{SECRET}"), false),
            "spora://s/<redacted>"
        );
        // Not a path segment: a word ending in s.
        let plain = format!("bytes/{SECRET}");
        assert_eq!(r(&plain, false), plain);
        // A module path is no scheme.
        let module = "[INFO spora_winui::settings] saved";
        assert_eq!(r(module, false), module);
    }

    /// Collects what a [`Redacted`] passes on, one entry per write.
    #[derive(Default)]
    struct Writes(Vec<Vec<u8>>);

    impl Write for &mut Writes {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_secret_split_between_writes_is_still_redacted() {
        let mut got = Writes::default();
        {
            let mut w = Redacted::new(&mut got);
            let line = format!("[INFO] link https://spora.to/s/{SECRET}?r=x\n");
            let (a, b) = line.as_bytes().split_at(40);
            w.write_all(a).unwrap();
            w.flush().unwrap();
            w.write_all(b).unwrap();
            w.write_all(b"second\nthird ").unwrap();
            w.write_all(b"line\n").unwrap();
        }
        let lines: Vec<String> = got
            .0
            .iter()
            .map(|l| String::from_utf8(l.clone()).unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                "[INFO] link https://spora.to/s/<redacted>?r=x\n",
                "second\n",
                "third line\n",
            ]
        );
    }

    #[test]
    fn an_endless_line_is_cut() {
        let mut got = Writes::default();
        {
            let mut w = Redacted::new(&mut got);
            w.write_all(b"x /s/").unwrap();
            w.write_all(&vec![b'A'; MAX_LINE * 2]).unwrap();
            w.write_all(b"AAAA\nnext\n").unwrap();
        }
        assert_eq!(got.0.len(), 2);
        assert_eq!(got.0[0], b"x /s/<redacted> [cut]\n");
        assert_eq!(got.0[1], b"next\n");
    }

    fn scratch(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spora-winlog-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn size(p: &Path) -> u64 {
        fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    #[test]
    fn rotates_at_the_cap_and_keeps_so_many() {
        let dir = scratch("rotate");
        let path = dir.join("test.log");
        let mut w = Redacted::new(RotatingFile::open(&path, 1000, 2).unwrap());
        for i in 0..200 {
            writeln!(w, "line {i:04} {}", "x".repeat(80)).unwrap();
        }
        drop(w);
        // "line 0000 " + 80 x + newline.
        let line = 91;
        assert!(size(&path) <= 1000 && size(&path) > 0);
        assert!(size(&dir.join("test.log.1")) > 1000 - line);
        assert!(size(&dir.join("test.log.1")) <= 1000);
        assert!(size(&dir.join("test.log.2")) <= 1000);
        assert!(!dir.join("test.log.3").exists());
        // Lines are never split: every file starts with a whole line, and
        // the newest lines are in the current file.
        for f in ["test.log", "test.log.1", "test.log.2"] {
            let text = fs::read_to_string(dir.join(f)).unwrap();
            assert!(text.lines().all(|l| l.len() as u64 == line - 1), "{f}");
        }
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.ends_with(&format!("line 0199 {}\n", "x".repeat(80))));
        let older = fs::read_to_string(dir.join("test.log.1")).unwrap();
        let first = |t: &str| t[5..9].parse::<u32>().unwrap();
        assert!(first(&older) < first(&current));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_old_files_the_log_starts_over() {
        let dir = scratch("keep0");
        let path = dir.join("test.log");
        let mut w = Redacted::new(RotatingFile::open(&path, 100, 0).unwrap());
        for i in 0..50 {
            writeln!(w, "line {i}").unwrap();
        }
        drop(w);
        assert!(size(&path) <= 100);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_log_is_scrubbed_when_opened() {
        let dir = scratch("scrub");
        // An unbounded log from before, secrets in the clear.
        let path = dir.join("old.log");
        let mut old = String::new();
        let mut i = 0;
        while old.len() < 5000 {
            old.push_str(&format!("line {i} https://spora.to/s/{SECRET}?r=x\n"));
            i += 1;
        }
        fs::write(&path, &old).unwrap();
        let mut w = Redacted::new(RotatingFile::open(&path, 1000, 2).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(SECRET));
        assert!(text.len() <= 1000);
        // Whole lines, the newest kept.
        assert!(text.starts_with("line "));
        assert!(text.ends_with(&format!(
            "line {} https://spora.to/s/<redacted>?r=x\n",
            i - 1
        )));
        writeln!(w, "new").unwrap();
        drop(w);
        assert!(!dir.join("old.log.tmp").exists());

        // A small one with nothing to redact is left alone.
        let clean = dir.join("clean.log");
        fs::write(&clean, "fine\n").unwrap();
        let before = fs::metadata(&clean).unwrap().modified().unwrap();
        drop(RotatingFile::open(&clean, 1000, 2).unwrap());
        assert_eq!(fs::metadata(&clean).unwrap().modified().unwrap(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_writes_redacted_lines_through_env_logger_shapes() {
        let dir = scratch("open");
        let path = dir.join("spora-winui.log");
        let mut w = open(&path).unwrap();
        // env_logger writes a record and then flushes, in one go.
        w.write_all(format!("[2026-11-09T12:00:00Z INFO  spora_winui] Handling incoming URL: https://spora.to/s/{SECRET}?r=x\n").as_bytes()).unwrap();
        w.flush().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(SECRET));
        assert!(text.contains("https://spora.to/s/<redacted>?r=x"));
        drop(w);
        fs::remove_dir_all(&dir).unwrap();
    }
}
