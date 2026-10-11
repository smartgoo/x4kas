//! The log files read back for the GUI's Health page: the files in the logs folder
//! ([`list`]), one file indexed by line ([`LogFile`], so a view reads only the lines it
//! shows, however big the file), and search within one file or across many
//! ([`LogFile::find`], [`search`]). Synchronous: the controller runs the slow parts
//! (opening, finding, searching) in `spawn_blocking`.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use crate::logging::{CLI_PREFIX, GUI_PREFIX, LogLevel, log_files, record_level_bytes};

/// Which frontend wrote a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSource {
    Gui,
    Cli,
}

impl LogSource {
    pub fn label(self) -> &'static str {
        match self {
            LogSource::Gui => "GUI",
            LogSource::Cli => "CLI",
        }
    }
}

/// A log file in the logs folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFileInfo {
    pub path: PathBuf,
    pub source: LogSource,
    /// The day it covers, `YYYY-MM-DD` (UTC), from its name.
    pub date: String,
    pub bytes: u64,
}

/// The log files in `dir`, newest first (the GUI's before the CLI's on the same day).
pub fn list(dir: &Path) -> std::io::Result<Vec<LogFileInfo>> {
    let mut files = Vec::new();
    for (prefix, source) in [(GUI_PREFIX, LogSource::Gui), (CLI_PREFIX, LogSource::Cli)] {
        for path in log_files(dir, prefix)? {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let date = name
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|rest| rest.strip_suffix(".log"))
                .unwrap_or_default()
                .to_string();
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            files.push(LogFileInfo {
                path,
                source,
                date,
                bytes,
            });
        }
    }
    files.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then((a.source == LogSource::Cli).cmp(&(b.source == LogSource::Cli)))
    });
    Ok(files)
}

/// Lines read from disk at a time, and kept in the cache, per block.
const BLOCK_LINES: usize = 256;
/// Blocks kept in memory.
const CACHED_BLOCKS: usize = 16;
/// A level byte for a line before any record (none to inherit).
const NO_LEVEL: u8 = u8::MAX;

/// A log file indexed by line: where each whole line starts and the level of the
/// record it belongs to (a line without one, such as a backtrace's, continues the
/// record before it). Lines are read from disk when asked for, a block at a time.
pub struct LogFile {
    path: PathBuf,
    /// The start of each whole line.
    offsets: Vec<u64>,
    /// The level of each line's record, as an index into `LogLevel::ALL`.
    levels: Vec<u8>,
    /// Bytes indexed: the end of the last whole line.
    end: u64,
    cache: Mutex<BlockCache>,
}

#[derive(Default)]
struct BlockCache {
    blocks: HashMap<usize, Arc<Vec<String>>>,
    order: VecDeque<usize>,
}

impl std::fmt::Debug for LogFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogFile")
            .field("path", &self.path)
            .field("lines", &self.offsets.len())
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl LogFile {
    /// Index the file at `path`, reporting the bytes read so far; `cancel` stops it.
    pub fn open(path: &Path, cancel: &AtomicBool, progress: impl FnMut(u64, u64)) -> Result<Self> {
        let mut file = Self {
            path: path.to_path_buf(),
            offsets: Vec::new(),
            levels: Vec::new(),
            end: 0,
            cache: Mutex::new(BlockCache::default()),
        };
        file.index_from(cancel, progress)?;
        Ok(file)
    }

    /// Index the lines written since the last time. A file that shrank was replaced
    /// (rotated or truncated) and is indexed again from the start. Returns how many
    /// lines were added.
    pub fn refresh(&mut self) -> Result<usize> {
        let len = std::fs::metadata(&self.path)?.len();
        if len < self.end {
            self.offsets.clear();
            self.levels.clear();
            self.end = 0;
        }
        let before = self.offsets.len();
        self.index_from(&AtomicBool::new(false), |_, _| {})?;
        // The last block may have been read short of what was added to it.
        let mut cache = self.cache.lock().expect("log cache poisoned");
        let last = before / BLOCK_LINES;
        cache.blocks.retain(|block, _| *block < last);
        cache.order.retain(|block| *block < last);
        Ok(self.offsets.len() - before)
    }

    fn index_from(
        &mut self,
        cancel: &AtomicBool,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        let mut file = File::open(&self.path)?;
        let total = file.metadata()?.len();
        file.seek(SeekFrom::Start(self.end))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut level = self.levels.last().copied().unwrap_or(NO_LEVEL);
        let mut line = Vec::new();
        let mut pos = self.end;
        let mut since_report = 0u64;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)? as u64;
            // A last line not ended yet is left for the next refresh.
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            if let Some(l) = record_level_bytes(&line) {
                level = level_index(l);
            }
            self.offsets.push(pos);
            self.levels.push(level);
            pos += read;
            since_report += read;
            if since_report >= 8 << 20 {
                since_report = 0;
                if cancel.load(Ordering::Relaxed) {
                    return Err(anyhow!("cancelled"));
                }
                progress(pos, total);
            }
        }
        self.end = pos;
        progress(pos, total);
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whole lines indexed.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Bytes indexed.
    pub fn bytes(&self) -> u64 {
        self.end
    }

    /// The level of the record line `i` belongs to (`None` before the first record).
    pub fn level(&self, i: usize) -> Option<LogLevel> {
        self.levels
            .get(i)
            .and_then(|l| LogLevel::ALL.get(*l as usize).copied())
    }

    /// The lines whose record is at `min` or more severe, in order.
    pub fn lines_at(&self, min: LogLevel) -> Vec<u32> {
        let min = level_index(min);
        self.levels
            .iter()
            .enumerate()
            .filter(|(_, l)| **l != NO_LEVEL && **l <= min)
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Line `i` (without its newline), read from disk with the block around it unless
    /// it is cached; `None` past the end or when the file can't be read.
    pub fn line(&self, i: usize) -> Option<String> {
        if i >= self.offsets.len() {
            return None;
        }
        let block = self.block(i / BLOCK_LINES).ok()?;
        block.get(i % BLOCK_LINES).cloned()
    }

    fn block(&self, no: usize) -> Result<Arc<Vec<String>>> {
        if let Some(block) = self
            .cache
            .lock()
            .expect("log cache poisoned")
            .blocks
            .get(&no)
        {
            return Ok(block.clone());
        }
        let lines = self.read_lines(no * BLOCK_LINES..((no + 1) * BLOCK_LINES).min(self.len()))?;
        let block = Arc::new(lines);
        let mut cache = self.cache.lock().expect("log cache poisoned");
        cache.blocks.insert(no, block.clone());
        cache.order.push_back(no);
        while cache.order.len() > CACHED_BLOCKS {
            if let Some(old) = cache.order.pop_front() {
                cache.blocks.remove(&old);
            }
        }
        Ok(block)
    }

    /// Lines `range` read from disk in one go.
    pub fn read_lines(&self, range: Range<usize>) -> Result<Vec<String>> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let start = self.offsets[range.start];
        let end = self.offsets.get(range.end).copied().unwrap_or(self.end);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// The lines that match `matcher` and are at `min` or more severe, reading the
    /// file once; `cancel` stops it, `progress` gets the lines read so far.
    pub fn find(
        &self,
        matcher: &Matcher,
        min: Option<LogLevel>,
        cancel: &AtomicBool,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<Vec<u32>> {
        let min = min.map(level_index);
        let mut found = Vec::new();
        let mut start = 0;
        while start < self.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("cancelled"));
            }
            let end = (start + 8 * BLOCK_LINES).min(self.len());
            for (i, line) in self.read_lines(start..end)?.iter().enumerate() {
                let n = start + i;
                let level_ok =
                    min.is_none_or(|min| self.levels[n] != NO_LEVEL && self.levels[n] <= min);
                if level_ok && matcher.matches(line) {
                    found.push(n as u32);
                }
            }
            start = end;
            progress(start, self.len());
        }
        Ok(found)
    }
}

fn level_index(level: LogLevel) -> u8 {
    LogLevel::ALL.iter().position(|l| *l == level).unwrap_or(0) as u8
}

/// What to look for: text, or a regular expression, in either case or the one given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchText {
    pub text: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

/// A compiled [`SearchText`].
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Everything (no text).
    All,
    Text {
        needle: String,
        case_sensitive: bool,
    },
    Regex(regex::Regex),
}

impl SearchText {
    /// Compile it; a regular expression that doesn't parse says why.
    pub fn matcher(&self) -> Result<Matcher, String> {
        if self.text.is_empty() {
            return Ok(Matcher::All);
        }
        if self.regex {
            return regex::RegexBuilder::new(&self.text)
                .case_insensitive(!self.case_sensitive)
                .build()
                .map(Matcher::Regex)
                .map_err(|e| e.to_string());
        }
        Ok(Matcher::Text {
            needle: if self.case_sensitive {
                self.text.clone()
            } else {
                self.text.to_lowercase()
            },
            case_sensitive: self.case_sensitive,
        })
    }
}

impl Matcher {
    pub fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Text {
                needle,
                case_sensitive: true,
            } => line.contains(needle.as_str()),
            Matcher::Text { needle, .. } => line.to_lowercase().contains(needle.as_str()),
            Matcher::Regex(re) => re.is_match(line),
        }
    }
}

/// One matching line of a [`search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub file: PathBuf,
    /// 0-based.
    pub line: usize,
    pub level: Option<LogLevel>,
    pub text: String,
}

/// What a [`search`] found.
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub files_searched: usize,
    pub bytes_searched: u64,
    /// It stopped at the most hits it keeps.
    pub truncated: bool,
}

/// The lines of `files` that match `matcher` and are at `min` or more severe, at
/// most `max_hits`, in the order of `files`; `progress` gets the bytes read so far and
/// the total.
pub fn search(
    files: &[LogFileInfo],
    matcher: &Matcher,
    min: Option<LogLevel>,
    max_hits: usize,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<SearchResult> {
    let total: u64 = files.iter().map(|f| f.bytes).sum();
    let mut result = SearchResult::default();
    let mut line = Vec::new();
    for info in files {
        let file = match File::open(&info.path) {
            Ok(file) => file,
            // Rotated away since it was listed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut level = None;
        let mut n = 0;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)? as u64;
            if read == 0 {
                break;
            }
            result.bytes_searched += read;
            if let Some(l) = record_level_bytes(&line) {
                level = Some(l);
            }
            let level_ok = min.is_none_or(|min| level.is_some_and(|l| l <= min));
            if level_ok {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                if matcher.matches(text) {
                    if result.hits.len() == max_hits {
                        result.truncated = true;
                        result.files_searched += 1;
                        return Ok(result);
                    }
                    result.hits.push(SearchHit {
                        file: info.path.clone(),
                        line: n,
                        level,
                        text: text.to_string(),
                    });
                }
            }
            n += 1;
            if n % 50_000 == 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Err(anyhow!("cancelled"));
                }
                progress(result.bytes_searched, total);
            }
        }
        result.files_searched += 1;
        progress(result.bytes_searched, total);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "\
2026-10-11T19:01:02.000000Z  INFO x4kas: started
2026-10-11T19:01:03.000000Z ERROR x4kas::panic: panic: boom
   0: std::backtrace
2026-10-11T19:01:04.000000Z DEBUG x4kas::b: detail Boom
2026-10-11T19:01:05.000000Z  WARN x4kas::c: careful
";

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn lists_newest_first_gui_before_cli() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "x4kas.2026-10-10.log",
            "x4kas-cli.2026-10-11.log",
            "x4kas.2026-10-11.log",
            "notes.txt",
        ] {
            write(dir.path(), name, "x\n");
        }
        let files = list(dir.path()).unwrap();
        let shown: Vec<_> = files
            .iter()
            .map(|f| (f.date.as_str(), f.source, f.bytes))
            .collect();
        assert_eq!(
            shown,
            [
                ("2026-10-11", LogSource::Gui, 2),
                ("2026-10-11", LogSource::Cli, 2),
                ("2026-10-10", LogSource::Gui, 2),
            ]
        );
    }

    #[test]
    fn indexes_lines_with_their_records_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "x4kas.2026-10-11.log", TEXT);
        let file = LogFile::open(&path, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(file.len(), 5);
        assert_eq!(file.level(0), Some(LogLevel::Info));
        // The backtrace line continues the error.
        assert_eq!(file.level(2), Some(LogLevel::Error));
        assert_eq!(file.line(2).as_deref(), Some("   0: std::backtrace"));
        assert_eq!(file.line(4).unwrap(), TEXT.lines().nth(4).unwrap());
        assert_eq!(file.line(5), None);
        assert_eq!(file.lines_at(LogLevel::Warn), [1, 2, 4]);
        assert_eq!(file.read_lines(1..3).unwrap().len(), 2);
    }

    #[test]
    fn refresh_adds_appended_lines_and_restarts_after_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "x4kas.2026-10-11.log",
            "2026-10-11T00:00:00Z INFO a: one\nhalf",
        );
        let mut file = LogFile::open(&path, &AtomicBool::new(false), |_, _| {}).unwrap();
        // An unfinished last line waits.
        assert_eq!(file.len(), 1);
        assert_eq!(
            file.line(0).as_deref(),
            Some("2026-10-11T00:00:00Z INFO a: one")
        );
        std::fs::write(
            &path,
            "2026-10-11T00:00:00Z INFO a: one\nhalf done\nthree\n",
        )
        .unwrap();
        assert_eq!(file.refresh().unwrap(), 2);
        assert_eq!(file.line(1).as_deref(), Some("half done"));
        // The continuation lines take the record's level.
        assert_eq!(file.level(2), Some(LogLevel::Info));
        std::fs::write(&path, "2026-10-11T00:00:00Z WARN a: new\n").unwrap();
        assert_eq!(file.refresh().unwrap(), 1);
        assert_eq!(file.len(), 1);
        assert_eq!(file.level(0), Some(LogLevel::Warn));
        assert_eq!(
            file.line(0).as_deref(),
            Some("2026-10-11T00:00:00Z WARN a: new")
        );
    }

    #[test]
    fn matchers() {
        let text = |t: &str, regex, case_sensitive| SearchText {
            text: t.into(),
            regex,
            case_sensitive,
        };
        assert!(
            text("", false, false)
                .matcher()
                .unwrap()
                .matches("anything")
        );
        let m = text("BOOM", false, false).matcher().unwrap();
        assert!(m.matches("panic: boom") && !m.matches("bang"));
        let m = text("BOOM", false, true).matcher().unwrap();
        assert!(!m.matches("panic: boom") && m.matches("BOOM!"));
        let m = text(r"panic:\s+\w+", true, false).matcher().unwrap();
        assert!(m.matches("x panic:  boom"));
        assert!(text("(", true, false).matcher().is_err());
    }

    #[test]
    fn finds_in_a_file_and_searches_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "x4kas.2026-10-11.log", TEXT);
        write(
            dir.path(),
            "x4kas-cli.2026-10-10.log",
            "2026-10-10T00:00:00Z ERROR x: boom again\n",
        );
        let never = AtomicBool::new(false);
        let file = LogFile::open(&path, &never, |_, _| {}).unwrap();
        let boom = SearchText {
            text: "boom".into(),
            ..Default::default()
        }
        .matcher()
        .unwrap();
        assert_eq!(file.find(&boom, None, &never, |_, _| {}).unwrap(), [1, 3]);
        assert_eq!(
            file.find(&boom, Some(LogLevel::Warn), &never, |_, _| {})
                .unwrap(),
            [1]
        );
        let files = list(dir.path()).unwrap();
        let result = search(&files, &boom, None, 100, &never, |_, _| {}).unwrap();
        let hits: Vec<_> = result
            .hits
            .iter()
            .map(|h| {
                (
                    h.file.file_name().unwrap().to_string_lossy().to_string(),
                    h.line,
                    h.level,
                )
            })
            .collect();
        assert_eq!(
            hits,
            [
                ("x4kas.2026-10-11.log".to_string(), 1, Some(LogLevel::Error)),
                ("x4kas.2026-10-11.log".to_string(), 3, Some(LogLevel::Debug)),
                (
                    "x4kas-cli.2026-10-10.log".to_string(),
                    0,
                    Some(LogLevel::Error)
                ),
            ]
        );
        assert_eq!(result.files_searched, 2);
        assert!(!result.truncated);
        let capped = search(&files, &boom, None, 2, &never, |_, _| {}).unwrap();
        assert!(capped.truncated && capped.hits.len() == 2);
        // Only the errors (and their continuation lines).
        let all = Matcher::All;
        let errors = search(&files, &all, Some(LogLevel::Error), 100, &never, |_, _| {}).unwrap();
        assert_eq!(errors.hits.len(), 3);
        let cancelled = AtomicBool::new(true);
        assert!(file.find(&boom, None, &cancelled, |_, _| {}).is_err());
    }
}
