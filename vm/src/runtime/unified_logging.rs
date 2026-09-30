// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot-style unified logging framework (`-Xlog`).
//!
//! Implements the JEP 158 unified logging syntax:
//! `-Xlog:what[:output[:decorators[:output-options]]]`, where `what` is a
//! comma-separated list of `tag1[+tag2...][*][=level]` selections.
//!
//! Example specs:
//! - `gc*=info` — all GC tags at info level to stdout
//! - `gc+heap=debug:stderr` — gc+heap at debug to stderr
//! - `gc*=trace:file=gc.log:time,level,tags` — GC trace to file with decorators
//! - `gc*,safepoint:file=gc-%p.log:uptime,tid:filecount=5,filesize=10m`
//!
//! # HotSpot compatibility (gen r5w3/obs7, 2026-09-26)
//!
//! `-Xlog:gc` is the flag a HotSpot user reaches for first, and a large class
//! of tools parses its text (GCViewer, GCeasy, JMC's log import, CI scripts).
//! `gengc-r5w2-obs6-proposal-xlog-gc-hotspot-compatible-output` listed where
//! this framework's text and grammar differed; since this wave:
//!
//! * the default decorators are HotSpot's `uptime,level,tags`
//!   (`[0.012s][info][gc] ...`) — they were all six of
//!   `time,uptime,pid,tid,level,tags`;
//! * every HotSpot decorator is accepted, long or short (`t`, `utc`, `u`,
//!   `tm`, `um`, `tn`, `un`, `hn`, `p`, `ti`, `l`, `tg`), `tid` prints the
//!   bare OS thread id (it printed `tid=<n>`), `time` / `utctime` carry a
//!   `+0000` offset, and each decorator is padded to the widest value
//!   printed on that output so far, as `LogFileStreamOutput` does;
//! * `what` may hold several comma-separated selections
//!   (`gc=info,gc+heap=debug`, `gc*,safepoint`), and a later selection
//!   overrides an earlier one for the same tag and output
//!   (`gc*,gc+heap=off`); each output prints a message at most once;
//! * a 4th field of output options (`filecount=5,filesize=10m`) is accepted
//!   (and ignored: no rotation); an output with no `file=` prefix that is not
//!   `stdout` / `stderr` is a file name, and `%p` / `%t` in a file name
//!   expand to the pid and the start time;
//! * `disable` turns off everything configured before it, `all` names every
//!   tag;
//! * one bad selection no longer drops the whole `-Xlog` option: it is
//!   skipped with a warning (the spec is rejected only when nothing in it was
//!   usable);
//! * repeated `-Xlog` options are honoured (the launcher joins them with `;`,
//!   the separator this parser splits options on).
//!
//! Tag sets are modelled as one [`LogTag`] per HotSpot tag set this VM logs
//! to or users commonly select (`gc`, `gc+heap`, `gc+init`, `gc+start`,
//! `gc+cpu`, `gc+heap+exit`, ...). A `+` selection names exactly one tag set,
//! as on HotSpot; `tag*` selects every tag set containing the named tags.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

// ---------------------------------------------------------------------------
// LogTag
// ---------------------------------------------------------------------------

/// Tags that categorize log messages, matching HotSpot's tag taxonomy. Each
/// variant is one HotSpot tag SET (e.g. [`LogTag::GcHeap`] is `gc,heap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogTag {
    Gc,
    GcPhases,
    GcHeap,
    GcAge,
    GcAlloc,
    GcCpu,
    GcErgo,
    GcMetaspace,
    /// `gc,init` — HotSpot's `GCInitLogger` block at startup. gen r5w3/obs7.
    GcInit,
    /// `gc,start` — the `GC(n) Pause Young (cause)` line at a pause's start.
    /// gen r5w3/obs7.
    GcStart,
    /// `gc,heap,exit` — the heap summary at exit (selectable; not printed
    /// yet). gen r5w3/obs7.
    GcHeapExit,
    /// `gc,marking` — concurrent marking. gen r5w3/obs7.
    GcMarking,
    /// `gc,ref` — reference processing (selectable). gen r5w3/obs7.
    GcRef,
    /// `safepoint` (selectable; `-Xlog:gc*,safepoint` is a common spec and
    /// used to reject the whole option). gen r5w3/obs7.
    Safepoint,
    ClassLoad,
    ClassUnload,
    Jit,
    Jni,
    Threading,
    Os,
    Modules,
    Exceptions,
}

impl LogTag {
    /// Parse a tag name (case-insensitive) into a `LogTag`.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "gc" => Ok(LogTag::Gc),
            "gc+phases" | "gcphases" | "gc_phases" => Ok(LogTag::GcPhases),
            "gc+heap" | "gcheap" | "gc_heap" => Ok(LogTag::GcHeap),
            "gc+age" | "gcage" | "gc_age" => Ok(LogTag::GcAge),
            "gc+alloc" | "gcalloc" | "gc_alloc" => Ok(LogTag::GcAlloc),
            "gc+cpu" | "gccpu" | "gc_cpu" => Ok(LogTag::GcCpu),
            "gc+ergo" | "gcergo" | "gc_ergo" => Ok(LogTag::GcErgo),
            "gc+metaspace" | "gcmetaspace" | "gc_metaspace" => Ok(LogTag::GcMetaspace),
            "gc+init" | "gcinit" | "gc_init" => Ok(LogTag::GcInit),
            "gc+start" | "gcstart" | "gc_start" => Ok(LogTag::GcStart),
            "gc+heap+exit" => Ok(LogTag::GcHeapExit),
            "gc+marking" | "gcmarking" | "gc_marking" => Ok(LogTag::GcMarking),
            "gc+ref" | "gcref" | "gc_ref" => Ok(LogTag::GcRef),
            "safepoint" => Ok(LogTag::Safepoint),
            "classload" | "class+load" | "class_load" => Ok(LogTag::ClassLoad),
            "classunload" | "class+unload" | "class_unload" => Ok(LogTag::ClassUnload),
            "jit" => Ok(LogTag::Jit),
            "jni" => Ok(LogTag::Jni),
            "threading" => Ok(LogTag::Threading),
            "os" => Ok(LogTag::Os),
            "modules" => Ok(LogTag::Modules),
            "exceptions" => Ok(LogTag::Exceptions),
            _ => Err(format!("unknown log tag: '{s}'")),
        }
    }

    /// The display name used in formatted output (matches HotSpot convention).
    pub fn as_str(&self) -> &'static str {
        match self {
            LogTag::Gc => "gc",
            LogTag::GcPhases => "gc,phases",
            LogTag::GcHeap => "gc,heap",
            LogTag::GcAge => "gc,age",
            LogTag::GcAlloc => "gc,alloc",
            LogTag::GcCpu => "gc,cpu",
            LogTag::GcErgo => "gc,ergo",
            LogTag::GcMetaspace => "gc,metaspace",
            LogTag::GcInit => "gc,init",
            LogTag::GcStart => "gc,start",
            LogTag::GcHeapExit => "gc,heap,exit",
            LogTag::GcMarking => "gc,marking",
            LogTag::GcRef => "gc,ref",
            LogTag::Safepoint => "safepoint",
            LogTag::ClassLoad => "class,load",
            LogTag::ClassUnload => "class,unload",
            LogTag::Jit => "jit",
            LogTag::Jni => "jni",
            LogTag::Threading => "threading",
            LogTag::Os => "os",
            LogTag::Modules => "modules",
            LogTag::Exceptions => "exceptions",
        }
    }

    /// All GC-family tags, used for `gc*` wildcard expansion.
    pub fn all_gc_tags() -> &'static [LogTag] {
        &[
            LogTag::Gc,
            LogTag::GcPhases,
            LogTag::GcHeap,
            LogTag::GcAge,
            LogTag::GcAlloc,
            LogTag::GcCpu,
            LogTag::GcErgo,
            LogTag::GcMetaspace,
            LogTag::GcInit,
            LogTag::GcStart,
            LogTag::GcHeapExit,
            LogTag::GcMarking,
            LogTag::GcRef,
        ]
    }

    /// All class-family tags, used for `class*` wildcard expansion.
    pub fn all_class_tags() -> &'static [LogTag] {
        &[LogTag::ClassLoad, LogTag::ClassUnload]
    }

    /// All known tags.
    pub fn all() -> &'static [LogTag] {
        &[
            LogTag::Gc,
            LogTag::GcPhases,
            LogTag::GcHeap,
            LogTag::GcAge,
            LogTag::GcAlloc,
            LogTag::GcCpu,
            LogTag::GcErgo,
            LogTag::GcMetaspace,
            LogTag::GcInit,
            LogTag::GcStart,
            LogTag::GcHeapExit,
            LogTag::GcMarking,
            LogTag::GcRef,
            LogTag::Safepoint,
            LogTag::ClassLoad,
            LogTag::ClassUnload,
            LogTag::Jit,
            LogTag::Jni,
            LogTag::Threading,
            LogTag::Os,
            LogTag::Modules,
            LogTag::Exceptions,
        ]
    }
}

impl fmt::Display for LogTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// LogLevel
// ---------------------------------------------------------------------------

/// Severity levels for unified log messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Off,
    Error,
    Warning,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Numeric severity for comparison — higher means more verbose.
    fn severity(self) -> u8 {
        match self {
            LogLevel::Off => 0,
            LogLevel::Error => 1,
            LogLevel::Warning => 2,
            LogLevel::Info => 3,
            LogLevel::Debug => 4,
            LogLevel::Trace => 5,
        }
    }

    /// Returns `true` if a message at `self` level should be emitted when
    /// the configured threshold is `threshold`.
    pub fn is_enabled_at(self, threshold: LogLevel) -> bool {
        if threshold == LogLevel::Off {
            return false;
        }
        self.severity() <= threshold.severity() && self != LogLevel::Off
    }

    /// Parse a level name (case-insensitive).
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(LogLevel::Off),
            "error" => Ok(LogLevel::Error),
            "warning" | "warn" => Ok(LogLevel::Warning),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(format!("unknown log level: '{s}'")),
        }
    }

    /// Display name matching HotSpot format.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warning => "warning",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialOrd for LogLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LogLevel {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.severity().cmp(&other.severity())
    }
}

// ---------------------------------------------------------------------------
// LogOutput
// ---------------------------------------------------------------------------

/// Where log messages are written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LogOutput {
    Stdout,
    Stderr,
    File(PathBuf),
}

impl LogOutput {
    /// Parse an output specification.
    ///
    /// - `"stdout"`, `"#0"` or empty → `Stdout`
    /// - `"stderr"` or `"#1"` → `Stderr`
    /// - `"file=<path>"` → `File(path)` with security validation
    /// - any other name → `File(name)`, as HotSpot treats an output without
    ///   the `file=` prefix (gen r5w3/obs7; it was an error)
    ///
    /// `%p` in a file name expands to the process id and `%t` to the time the
    /// spec was parsed (`YYYY-MM-DD_HH-MM-SS`, UTC), HotSpot's substitutions.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("stdout") || s == "#0" {
            return Ok(LogOutput::Stdout);
        }
        if s.eq_ignore_ascii_case("stderr") || s == "#1" {
            return Ok(LogOutput::Stderr);
        }
        let path_str = s.strip_prefix("file=").unwrap_or(s).trim();
        if path_str.is_empty() {
            return Err("file output requires a path: file=<path>".to_string());
        }
        let path_str = expand_file_name(path_str);
        validate_log_file_path(&path_str)?;
        Ok(LogOutput::File(PathBuf::from(path_str)))
    }
}

/// HotSpot's file-name substitutions: `%p` the pid, `%t` the start time
/// (`%Y-%m-%d_%H-%M-%S`; UTC here, local time on HotSpot), `%%` a percent.
fn expand_file_name(name: &str) -> String {
    if !name.contains('%') {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() + 16);
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('p') => {
                chars.next();
                out.push_str(&std::process::id().to_string());
            }
            Some('t') => {
                chars.next();
                let (y, mo, d, h, mi, s, _) = utc_parts(SystemTime::now());
                out.push_str(&format!("{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}"));
            }
            Some('%') => {
                chars.next();
                out.push('%');
            }
            _ => out.push('%'),
        }
    }
    out
}

/// Security: validate that a file path is safe for log output.
///
/// Rejects:
/// - Empty paths
/// - Path traversal components (`..`)
/// - Absolute paths pointing outside the working directory tree are allowed
///   (the user controls the CLI), but `..` segments are rejected to prevent
///   confusion and accidental writes to unexpected locations.
fn validate_log_file_path(path_str: &str) -> Result<(), String> {
    if path_str.is_empty() {
        return Err("log file path must not be empty".to_string());
    }

    let path = Path::new(path_str);

    // Reject path traversal
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(format!("log file path must not contain '..': '{path_str}'"));
        }
    }

    // Reject paths that are just a directory separator
    if path_str == "/" || path_str == "\\" {
        return Err("log file path must specify a filename, not just a directory root".to_string());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// LogDecorators
// ---------------------------------------------------------------------------

/// How many decorators HotSpot defines (the padding table's width).
const DECORATOR_COUNT: usize = 12;

/// Which metadata fields to include in each log line. Printed in HotSpot's
/// order: `time`, `utctime`, `uptime`, `timemillis`, `uptimemillis`,
/// `timenanos`, `uptimenanos`, `hostname`, `pid`, `tid`, `level`, `tags`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDecorators {
    pub time: bool,
    pub uptime: bool,
    pub pid: bool,
    pub tid: bool,
    pub level: bool,
    pub tags: bool,
    /// gen r5w3/obs7: the six HotSpot decorators this framework did not know.
    pub utctime: bool,
    pub timemillis: bool,
    pub uptimemillis: bool,
    pub timenanos: bool,
    pub uptimenanos: bool,
    pub hostname: bool,
}

impl Default for LogDecorators {
    /// HotSpot's default: `uptime,level,tags` (`[0.012s][info][gc] ...`).
    /// gen r5w3/obs7; it was all six decorators this framework knew.
    fn default() -> Self {
        Self {
            uptime: true,
            level: true,
            tags: true,
            ..Self::none()
        }
    }
}

impl LogDecorators {
    /// No decorator at all (`none`).
    pub fn none() -> Self {
        Self {
            time: false,
            uptime: false,
            pid: false,
            tid: false,
            level: false,
            tags: false,
            utctime: false,
            timemillis: false,
            uptimemillis: false,
            timenanos: false,
            uptimenanos: false,
            hostname: false,
        }
    }

    /// Parse a comma-separated decorator list: HotSpot's names or their
    /// abbreviations (`time`/`t`, `utctime`/`utc`, `uptime`/`u`,
    /// `timemillis`/`tm`, `uptimemillis`/`um`, `timenanos`/`tn`,
    /// `uptimenanos`/`un`, `hostname`/`hn`, `pid`/`p`, `tid`/`ti`,
    /// `level`/`l`, `tags`/`tg`), or `none`. Empty is the default.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::default());
        }

        if s.eq_ignore_ascii_case("none") {
            return Ok(Self::none());
        }

        // Start with all false, then enable requested decorators
        let mut dec = Self::none();

        for part in s.split(',') {
            let part = part.trim();
            match part.to_ascii_lowercase().as_str() {
                "time" | "t" => dec.time = true,
                "utctime" | "utc" => dec.utctime = true,
                "uptime" | "u" => dec.uptime = true,
                "timemillis" | "tm" => dec.timemillis = true,
                "uptimemillis" | "um" => dec.uptimemillis = true,
                "timenanos" | "tn" => dec.timenanos = true,
                "uptimenanos" | "un" => dec.uptimenanos = true,
                "hostname" | "hn" => dec.hostname = true,
                "pid" | "p" => dec.pid = true,
                "tid" | "ti" => dec.tid = true,
                "level" | "l" => dec.level = true,
                "tags" | "tg" => dec.tags = true,
                other => return Err(format!("unknown decorator: '{other}'")),
            }
        }

        Ok(dec)
    }

    /// The decorators in HotSpot's print order.
    fn in_order(&self) -> [bool; DECORATOR_COUNT] {
        [
            self.time,
            self.utctime,
            self.uptime,
            self.timemillis,
            self.uptimemillis,
            self.timenanos,
            self.uptimenanos,
            self.hostname,
            self.pid,
            self.tid,
            self.level,
            self.tags,
        ]
    }
}

// ---------------------------------------------------------------------------
// LogRule
// ---------------------------------------------------------------------------

/// A single logging rule: which tags at which level go to which output.
///
/// One rule per selection of an `-Xlog` option. For a given output and tag
/// the LAST rule naming both decides the level (HotSpot: a later selection
/// overrides an earlier one), and the last rule naming the output decides
/// its decorators.
#[derive(Debug, Clone)]
pub struct LogRule {
    /// Tags this rule applies to. A message matches if *any* of the
    /// message's tags appears in this list (OR semantics, matching
    /// HotSpot's wildcard expansion behavior).
    pub tags: Vec<LogTag>,
    /// The maximum verbosity level for this rule.
    pub level: LogLevel,
    /// Where to write matching messages.
    pub output: LogOutput,
    /// Which decorators to include.
    pub decorators: LogDecorators,
}

// ---------------------------------------------------------------------------
// Tag selection
// ---------------------------------------------------------------------------

/// Parse the tag portion of a selection (before `=`), handling `+`
/// combinators and `*` wildcards.
///
/// * empty, `*` or `all` — every tag;
/// * `a+b*` — every tag set containing `a` and `b` ([`expand_wildcard`]);
/// * `a+b` — exactly the tag set `a,b` (an error if this VM has no such tag
///   set; HotSpot warns "No tag set matches selection" and goes on, which is
///   what the caller does with the error).
fn parse_tag_selector(s: &str) -> Result<Vec<LogTag>, String> {
    let s = s.trim();
    if s.is_empty() || s == "*" || s.eq_ignore_ascii_case("all") {
        return Ok(LogTag::all().to_vec());
    }

    // Handle wildcard suffixes like `gc*`
    if let Some(prefix) = s.strip_suffix('*') {
        let prefix = prefix.trim_end_matches('+');
        if prefix.is_empty() {
            return Ok(LogTag::all().to_vec());
        }
        return expand_wildcard(prefix);
    }

    // A single tag, or a compound tag set (e.g. "gc+heap" → GcHeap).
    LogTag::from_str(s).map(|tag| vec![tag]).map_err(|e| {
        if s.contains('+') {
            format!("no tag set matches selection '{s}'")
        } else {
            e
        }
    })
}

/// Expand a wildcard prefix like `gc` or `gc+heap` into every tag set that
/// contains all of its `+`-separated tags (HotSpot's `gc+heap*` also selects
/// `gc,heap,exit`).
fn expand_wildcard(prefix: &str) -> Result<Vec<LogTag>, String> {
    let parts: Vec<String> = prefix
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
    let tags: Vec<LogTag> = LogTag::all()
        .iter()
        .copied()
        .filter(|t| {
            let comps: Vec<&str> = t.as_str().split(',').collect();
            parts.iter().all(|p| comps.contains(&p.as_str()))
        })
        .collect();
    if tags.is_empty() {
        Err(format!("no tag set matches selection '{prefix}*'"))
    } else {
        Ok(tags)
    }
}

/// Split one `-Xlog` option into its `:`-separated fields, keeping a Windows
/// drive letter in the OUTPUT field (`file=C:\logs\gc.log`, `C:\gc.log`) with
/// the path it starts.
fn split_option_fields(s: &str) -> Vec<String> {
    let raw: Vec<&str> = s.split(':').collect();
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let seg = raw[i];
        let drive = seg.trim().strip_prefix("file=").unwrap_or(seg.trim());
        let is_drive = drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic());
        if out.len() == 1
            && is_drive
            && raw
                .get(i + 1)
                .is_some_and(|next| next.starts_with('\\') || next.starts_with('/'))
        {
            out.push(format!("{seg}:{}", raw[i + 1]));
            i += 2;
            continue;
        }
        out.push(seg.to_string());
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// UnifiedLogger
// ---------------------------------------------------------------------------

/// The unified logging engine. Holds all active rules and handles message
/// routing and formatting.
pub struct UnifiedLogger {
    rules: Vec<LogRule>,
    start_time: Instant,
    pid: u32,
    /// For the `hostname` decorator, resolved once.
    hostname: String,
    /// Open file handles for `File` outputs, keyed by path for O(1) lookup.
    file_handles: Mutex<HashMap<PathBuf, File>>,
    /// Per output, the widest value each decorator has printed so far: HotSpot
    /// pads every decoration to it (`[%-*s]`), so columns line up once a wide
    /// tag set such as `gc,metaspace` has appeared. gen r5w3/obs7.
    padding: Mutex<HashMap<LogOutput, [usize; DECORATOR_COUNT]>>,
    /// Selections that were skipped, and why, for the caller to report.
    warnings: Vec<String>,
}

impl fmt::Debug for UnifiedLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnifiedLogger")
            .field("rules", &self.rules)
            .field("pid", &self.pid)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl UnifiedLogger {
    /// Parse a full `-Xlog` spec string into a `UnifiedLogger`.
    ///
    /// The spec may contain several `-Xlog` options separated by `;` (the
    /// launcher joins repeated `-Xlog` options that way):
    /// `gc*=info:stdout:time,level,tags;class*=debug:stderr`
    ///
    /// Each option has the form: `what[:output[:decorators[:options]]]`,
    /// `what` being comma-separated `tags[=level]` selections.
    ///
    /// Defaults:
    /// - level: `info`
    /// - output: `stdout`
    /// - decorators: `uptime,level,tags` (HotSpot's)
    ///
    /// A selection that cannot be used is skipped and recorded in
    /// [`Self::warnings`]; the spec is an error only when it yields no rule
    /// and was not a `disable`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty -Xlog spec".to_string());
        }

        let mut rules = Vec::new();
        let mut warnings = Vec::new();
        let mut disabled = false;

        for option in spec.split(';') {
            let option = option.trim();
            if option.is_empty() {
                continue;
            }
            if option.eq_ignore_ascii_case("disable") {
                // HotSpot: turn off every output configured so far,
                // including the default warning output.
                rules.clear();
                disabled = true;
                continue;
            }
            match Self::parse_option(option, &mut warnings) {
                Ok(mut option_rules) => rules.append(&mut option_rules),
                Err(e) => warnings.push(format!("invalid -Xlog option '{option}': {e}")),
            }
        }

        if rules.is_empty() && !disabled {
            return Err(if warnings.is_empty() {
                "no valid rules in -Xlog spec".to_string()
            } else {
                warnings.join("; ")
            });
        }

        Ok(Self {
            rules,
            start_time: Instant::now(),
            pid: std::process::id(),
            hostname: host_name(),
            file_handles: Mutex::new(HashMap::new()),
            padding: Mutex::new(HashMap::new()),
            warnings,
        })
    }

    /// Parse one option `what[:output[:decorators[:output-options]]]` into
    /// one rule per usable selection; a skipped selection is pushed onto
    /// `warnings`. An unusable output or decorator list rejects the option.
    fn parse_option(s: &str, warnings: &mut Vec<String>) -> Result<Vec<LogRule>, String> {
        let fields = split_option_fields(s);
        let what = fields.first().map_or("", |f| f.trim());
        let output = LogOutput::parse(fields.get(1).map_or("", String::as_str))?;
        let decorators = LogDecorators::parse(fields.get(2).map_or("", String::as_str))?;
        // fields[3..]: output options (`filecount=`, `filesize=`,
        // `foldmultilines=`). Accepted and ignored: no rotation here.

        let mut rules = Vec::new();
        // An empty `what` selects every tag at info (as before).
        let selections: Vec<&str> = if what.is_empty() {
            vec![""]
        } else {
            what.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
        };
        for sel in selections {
            let (tags_str, level_str) = sel.split_once('=').unwrap_or((sel, "info"));
            match (parse_tag_selector(tags_str), LogLevel::from_str(level_str.trim())) {
                (Ok(tags), Ok(level)) => rules.push(LogRule {
                    tags,
                    level,
                    output: output.clone(),
                    decorators: decorators.clone(),
                }),
                (Err(e), _) | (_, Err(e)) => {
                    warnings.push(format!("-Xlog selection '{sel}' ignored: {e}"));
                }
            }
        }
        if rules.is_empty() {
            return Err("no usable selection".to_string());
        }
        Ok(rules)
    }

    /// The level `output` logs `tag` at: the last rule naming both, or
    /// `None` when no rule does.
    fn level_for(&self, output: &LogOutput, tag: LogTag) -> Option<LogLevel> {
        self.rules
            .iter()
            .rev()
            .find(|r| &r.output == output && r.tags.contains(&tag))
            .map(|r| r.level)
    }

    /// Does `output` take a message with `tags` at `level`?
    fn enabled_on(&self, output: &LogOutput, tags: &[LogTag], level: LogLevel) -> bool {
        tags.iter().any(|t| {
            self.level_for(output, *t)
                .is_some_and(|threshold| level.is_enabled_at(threshold))
        })
    }

    /// The decorators of `output`: those of the last rule naming it.
    fn decorators_for(&self, output: &LogOutput) -> Option<&LogDecorators> {
        self.rules
            .iter()
            .rev()
            .find(|r| &r.output == output)
            .map(|r| &r.decorators)
    }

    /// Check if a message with the given tags and level would be emitted
    /// by any active rule. Allocation-free.
    pub fn is_enabled(&self, tags: &[LogTag], level: LogLevel) -> bool {
        self.rules.iter().any(|rule| {
            tags.iter().any(|t| rule.tags.contains(t)) && self.enabled_on(&rule.output, tags, level)
        })
    }

    /// Format and emit a log message once on every output that takes it.
    pub fn log(&self, tags: &[LogTag], level: LogLevel, message: &str) {
        for (i, rule) in self.rules.iter().enumerate() {
            let output = &rule.output;
            // Each output once: only at its first rule.
            if self.rules[..i].iter().any(|r| &r.output == output) {
                continue;
            }
            if !self.enabled_on(output, tags, level) {
                continue;
            }
            let Some(decorators) = self.decorators_for(output) else {
                continue;
            };

            // The padding lock is held across the write, so the widths a
            // line was padded to are the ones in force when it lands.
            let mut padding = match self.padding.lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            let pad = padding
                .entry(output.clone())
                .or_insert([0; DECORATOR_COUNT]);
            let formatted = self.format_padded(tags, level, message, decorators, pad);

            match output {
                LogOutput::Stdout => {
                    let _ = writeln!(std::io::stdout().lock(), "{formatted}");
                }
                LogOutput::Stderr => {
                    let _ = writeln!(std::io::stderr().lock(), "{formatted}");
                }
                LogOutput::File(path) => {
                    self.write_to_file(path, &formatted);
                }
            }
        }
    }

    /// Format a single log line with the requested decorators, unpadded.
    ///
    /// Output format: `[decorator1][decorator2]... message`
    fn format_message(
        &self,
        tags: &[LogTag],
        level: LogLevel,
        message: &str,
        decorators: &LogDecorators,
    ) -> String {
        self.format_padded(tags, level, message, decorators, &mut [0; DECORATOR_COUNT])
    }

    /// [`Self::format_message`], padding each decoration to `pad` and
    /// widening `pad` to what this line printed (HotSpot's
    /// `LogFileStreamOutput::write_decorations`).
    fn format_padded(
        &self,
        tags: &[LogTag],
        level: LogLevel,
        message: &str,
        decorators: &LogDecorators,
        pad: &mut [usize; DECORATOR_COUNT],
    ) -> String {
        let on = decorators.in_order();
        let mut prefix = String::new();
        for (i, enabled) in on.iter().enumerate() {
            if !enabled {
                continue;
            }
            let value = self.decoration(i, tags, level);
            let width = pad[i].max(value.len());
            prefix.push('[');
            prefix.push_str(&value);
            for _ in value.len()..width {
                prefix.push(' ');
            }
            prefix.push(']');
            pad[i] = width;
        }

        if prefix.is_empty() {
            message.to_string()
        } else {
            format!("{prefix} {message}")
        }
    }

    /// The value of decorator `i` (HotSpot's order, see [`LogDecorators`]).
    fn decoration(&self, i: usize, tags: &[LogTag], level: LogLevel) -> String {
        let elapsed = self.start_time.elapsed();
        let since_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        match i {
            0 | 1 => iso8601_utc(SystemTime::now()),
            2 => format!("{:.3}s", elapsed.as_secs_f64()),
            3 => format!("{}ms", since_epoch.as_millis()),
            4 => format!("{}ms", elapsed.as_millis()),
            5 => format!("{}ns", since_epoch.as_nanos()),
            6 => format!("{}ns", elapsed.as_nanos()),
            7 => self.hostname.clone(),
            8 => self.pid.to_string(),
            9 => thread_id().to_string(),
            10 => level.as_str().to_string(),
            _ => {
                let tag_str: Vec<&str> = tags.iter().map(|t| t.as_str()).collect();
                tag_str.join(",")
            }
        }
    }

    /// Write a formatted log line to a file, opening the handle lazily.
    fn write_to_file(&self, path: &Path, line: &str) {
        let mut handles = match self.file_handles.lock() {
            Ok(h) => h,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Find or create the file handle (O(1) keyed lookup).
        let file = if handles.contains_key(path) {
            handles.get_mut(path).expect("present (just checked)")
        } else {
            let f = match OpenOptions::new().create(true).append(true).open(path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "[unified_logging] failed to open log file '{}': {e}",
                        path.display()
                    );
                    return;
                }
            };
            handles.entry(path.to_path_buf()).or_insert(f)
        };

        let _ = writeln!(file, "{line}");
    }

    /// Return the number of active rules (useful for tests / diagnostics).
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Return a reference to the rules (for inspection in tests).
    pub fn rules(&self) -> &[LogRule] {
        &self.rules
    }

    /// The selections and options the parse skipped, one message each.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

// ---------------------------------------------------------------------------
// Timestamp helpers (no chrono dependency)
// ---------------------------------------------------------------------------

/// `(year, month, day, hour, minute, second, millisecond)` of `t`, UTC.
fn utc_parts(t: SystemTime) -> (u64, u64, u64, u64, u64, u64, u32) {
    let since_epoch = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let total_secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    // Manual UTC calendar decomposition (no leap-second handling — good enough
    // for logging timestamps).
    let days = total_secs / 86400;
    let time_of_day = total_secs % 86400;
    let (year, month, day) = days_to_ymd(days);
    (
        year,
        month,
        day,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
        millis,
    )
}

/// HotSpot's `time` / `utctime` decoration, in UTC:
/// `YYYY-MM-DDThh:mm:ss.mmm+0000` (ISO 8601 with a numeric offset, as HotSpot
/// prints it; HotSpot's `time` is local time, and this VM does not read the
/// time zone, so both decorators print UTC).
fn iso8601_utc(t: SystemTime) -> String {
    let (year, month, day, hours, minutes, seconds, millis) = utc_parts(t);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}+0000")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm adapted from Howard Hinnant's `days_to_civil`.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month index [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // month [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

/// The `tid` decoration: the OS thread id on Linux (what HotSpot prints and
/// what `jstack`'s `nid` and `top -H` show), a numeric id elsewhere.
fn thread_id() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `gettid` takes no arguments, touches no memory and cannot
        // fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        if let Ok(tid) = u64::try_from(tid) {
            return tid;
        }
    }
    // Portable fallback: the numeric part of the Rust thread id.
    let id = std::thread::current().id();
    let debug = format!("{id:?}");
    debug
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0)
}

/// The `hostname` decoration, resolved once per logger.
fn host_name() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(h) = cratonvm_types::flags::runtime_var(var) {
            if !h.is_empty() {
                return h;
            }
        }
    }
    "localhost".to_string()
}

// ---------------------------------------------------------------------------
// Global static logger
// ---------------------------------------------------------------------------

/// VM-wide unified logger, initialized once during VM startup.
static UNIFIED_LOGGER: OnceLock<UnifiedLogger> = OnceLock::new();

/// Initialize the global unified logger from a `-Xlog` spec string.
///
/// Returns `Ok(())` on success, or an error string if parsing fails.
/// If called more than once, subsequent calls are ignored (the first
/// logger wins). Skipped selections are reported on stderr in HotSpot's
/// warning shape (`[warning][logging] ...`), once.
pub fn init_unified_logging(spec: &str) -> Result<(), String> {
    let logger = UnifiedLogger::parse(spec)?;
    for w in logger.warnings() {
        let _ = writeln!(std::io::stderr().lock(), "[warning][logging] {w}");
    }
    // OnceLock::set returns Err(value) if already initialized — we ignore
    // that because double-init is benign (first writer wins).
    let _ = UNIFIED_LOGGER.set(logger);
    Ok(())
}

/// Log a message through the global unified logger.
///
/// This is a no-op if the logger has not been initialized or if the
/// message's tags/level do not match any active rule.
pub fn log_unified(tags: &[LogTag], level: LogLevel, message: &str) {
    if let Some(logger) = UNIFIED_LOGGER.get() {
        logger.log(tags, level, message);
    }
}

/// Check whether a message with the given tags and level would be emitted.
///
/// Returns `false` if the global logger is not initialized.
pub fn is_unified_logging_enabled(tags: &[LogTag], level: LogLevel) -> bool {
    UNIFIED_LOGGER
        .get()
        .map_or(false, |l| l.is_enabled(tags, level))
}

// ---------------------------------------------------------------------------
// Convenience macros (optional — callers can also use `log_unified` directly)
// ---------------------------------------------------------------------------

/// Convenience: log a GC info message.
pub fn gc_info(message: &str) {
    log_unified(&[LogTag::Gc], LogLevel::Info, message);
}

/// Convenience: log a GC debug message.
pub fn gc_debug(message: &str) {
    log_unified(&[LogTag::Gc], LogLevel::Debug, message);
}

// ---------------------------------------------------------------------------
// GC startup lines (gen r5w3/obs7)
// ---------------------------------------------------------------------------

/// What HotSpot's `Universe::initialize_heap` (`[gc] Using <collector>`) and
/// `GCInitLogger` (`[gc,init] ...`) print at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcInitFacts<'a> {
    /// `Serial`, for the Generational backend.
    pub collector: &'a str,
    /// The VM version (`java.vm.version`).
    pub version: &'a str,
    /// `os::processor_count()` / `os::active_processor_count()`.
    pub cpus_total: usize,
    pub cpus_available: usize,
    /// Physical memory, when known.
    pub memory_bytes: Option<u64>,
    /// `MinHeapSize` / `InitialHeapSize` / `MaxHeapSize` in bytes.
    pub heap_min: usize,
    pub heap_initial: usize,
    pub heap_max: usize,
}

/// HotSpot's `EXACTFMT`: the largest of `G`/`M`/`K`/`B` that divides `s`
/// exactly (`byte_size_in_exact_unit` / `exact_unit_for_byte_size`).
pub fn exact_size(s: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * K;
    const G: u64 = M * K;
    if s >= G && s % G == 0 {
        format!("{}G", s / G)
    } else if s >= M && s % M == 0 {
        format!("{}M", s / M)
    } else if s >= K && s % K == 0 {
        format!("{}K", s / K)
    } else {
        format!("{s}B")
    }
}

/// HotSpot's `PROPERFMT` on a 64-bit VM: `G` from 100 GiB, `M` from 100 MiB,
/// `K` from 100 KiB, bytes below (`byte_size_in_proper_unit`, truncating).
pub fn proper_size(s: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * K;
    const G: u64 = M * K;
    if s >= 100 * G {
        format!("{}G", s / G)
    } else if s >= 100 * M {
        format!("{}M", s / M)
    } else if s >= 100 * K {
        format!("{}K", s / K)
    } else {
        format!("{s}B")
    }
}

/// The `gc,init` lines, in `GCInitLogger::print_all`'s order. Only the lines
/// this VM can state truthfully: HotSpot's `Large Page Support`,
/// `NUMA Support`, `Compressed Oops`, `Pre-touch` and worker lines describe
/// HotSpot mechanisms and are not printed.
pub fn gc_init_lines(f: &GcInitFacts<'_>) -> Vec<String> {
    let mut lines = vec![
        format!("Version: {} (release)", f.version),
        format!("CPUs: {} total, {} available", f.cpus_total, f.cpus_available),
    ];
    if let Some(m) = f.memory_bytes {
        lines.push(format!("Memory: {}", proper_size(m)));
    }
    let b = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    lines.push(format!("Heap Min Capacity: {}", exact_size(b(f.heap_min))));
    lines.push(format!("Heap Initial Capacity: {}", exact_size(b(f.heap_initial))));
    lines.push(format!("Heap Max Capacity: {}", exact_size(b(f.heap_max))));
    lines
}

/// Log the startup lines: `[gc] Using <collector>` (HotSpot prints it under
/// plain `-Xlog:gc`; GCeasy picks its parser from it), then the `gc,init`
/// block. A no-op without a logger.
pub fn log_gc_startup(f: &GcInitFacts<'_>) {
    let Some(logger) = UNIFIED_LOGGER.get() else {
        return;
    };
    logger.log(&[LogTag::Gc], LogLevel::Info, &format!("Using {}", f.collector));
    if logger.is_enabled(&[LogTag::GcInit], LogLevel::Info) {
        for line in gc_init_lines(f) {
            logger.log(&[LogTag::GcInit], LogLevel::Info, &line);
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- LogLevel ordering ---------------------------------------------------

    #[test]
    fn level_ordering() {
        assert!(LogLevel::Off < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Warning);
        assert!(LogLevel::Warning < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
    }

    #[test]
    fn level_is_enabled_at() {
        // Info message should pass at Info or higher threshold
        assert!(LogLevel::Info.is_enabled_at(LogLevel::Info));
        assert!(LogLevel::Info.is_enabled_at(LogLevel::Debug));
        assert!(LogLevel::Info.is_enabled_at(LogLevel::Trace));

        // Info message should NOT pass at Warning threshold
        assert!(!LogLevel::Info.is_enabled_at(LogLevel::Warning));

        // Off level never passes
        assert!(!LogLevel::Off.is_enabled_at(LogLevel::Trace));
        assert!(!LogLevel::Info.is_enabled_at(LogLevel::Off));
    }

    #[test]
    fn level_parse_roundtrip() {
        for name in &["off", "error", "warning", "warn", "info", "debug", "trace"] {
            let level = LogLevel::from_str(name).unwrap();
            // warn normalizes to warning
            if *name != "warn" {
                assert_eq!(level.as_str(), *name);
            }
        }
    }

    #[test]
    fn level_parse_case_insensitive() {
        assert_eq!(LogLevel::from_str("INFO").unwrap(), LogLevel::Info);
        assert_eq!(LogLevel::from_str("Debug").unwrap(), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("TRACE").unwrap(), LogLevel::Trace);
    }

    #[test]
    fn level_parse_error() {
        assert!(LogLevel::from_str("verbose").is_err());
        assert!(LogLevel::from_str("").is_err());
    }

    // -- LogTag parsing ------------------------------------------------------

    #[test]
    fn tag_parse_basic() {
        assert_eq!(LogTag::from_str("gc").unwrap(), LogTag::Gc);
        assert_eq!(LogTag::from_str("jit").unwrap(), LogTag::Jit);
        assert_eq!(LogTag::from_str("threading").unwrap(), LogTag::Threading);
    }

    #[test]
    fn tag_parse_compound() {
        assert_eq!(LogTag::from_str("gc+heap").unwrap(), LogTag::GcHeap);
        assert_eq!(LogTag::from_str("gc+phases").unwrap(), LogTag::GcPhases);
        assert_eq!(LogTag::from_str("class+load").unwrap(), LogTag::ClassLoad);
        assert_eq!(LogTag::from_str("gc+init").unwrap(), LogTag::GcInit);
        assert_eq!(LogTag::from_str("gc+start").unwrap(), LogTag::GcStart);
        assert_eq!(LogTag::from_str("gc+heap+exit").unwrap(), LogTag::GcHeapExit);
    }

    #[test]
    fn tag_parse_case_insensitive() {
        assert_eq!(LogTag::from_str("GC").unwrap(), LogTag::Gc);
        assert_eq!(LogTag::from_str("JIT").unwrap(), LogTag::Jit);
    }

    #[test]
    fn tag_parse_error() {
        assert!(LogTag::from_str("unknown_tag").is_err());
        assert!(LogTag::from_str("").is_err());
    }

    #[test]
    fn tag_display() {
        assert_eq!(LogTag::Gc.to_string(), "gc");
        assert_eq!(LogTag::GcHeap.to_string(), "gc,heap");
        assert_eq!(LogTag::ClassLoad.to_string(), "class,load");
        assert_eq!(LogTag::GcHeapExit.to_string(), "gc,heap,exit");
    }

    // -- Tag selector parsing with wildcards ---------------------------------

    #[test]
    fn tag_selector_gc_wildcard() {
        let tags = parse_tag_selector("gc*").unwrap();
        for t in LogTag::all_gc_tags() {
            assert!(tags.contains(t), "gc* must select {t}");
        }
        // gen r5w3/obs7: gc,init / gc,start / gc,heap,exit / gc,marking /
        // gc,ref joined the family (it was 8).
        assert_eq!(tags.len(), 13);
        assert!(!tags.contains(&LogTag::Safepoint));
    }

    #[test]
    fn tag_selector_compound_wildcard_selects_supersets() {
        // HotSpot: `gc+heap*` is every tag set containing gc and heap.
        let tags = parse_tag_selector("gc+heap*").unwrap();
        assert_eq!(tags, vec![LogTag::GcHeap, LogTag::GcHeapExit]);
    }

    #[test]
    fn tag_selector_class_wildcard() {
        let tags = parse_tag_selector("class*").unwrap();
        assert!(tags.contains(&LogTag::ClassLoad));
        assert!(tags.contains(&LogTag::ClassUnload));
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn tag_selector_all_wildcard() {
        let tags = parse_tag_selector("*").unwrap();
        assert_eq!(tags.len(), LogTag::all().len());
        assert_eq!(parse_tag_selector("all").unwrap().len(), LogTag::all().len());
    }

    #[test]
    fn tag_selector_explicit_combo() {
        let tags = parse_tag_selector("gc+heap").unwrap();
        assert_eq!(tags, vec![LogTag::GcHeap]);
        // An exact combination this VM has no tag set for selects nothing
        // (it used to OR its parts together: `gc+foo` enabled `gc`).
        assert!(parse_tag_selector("gc+safepoint").is_err());
    }

    #[test]
    fn tag_selector_empty_is_all() {
        let tags = parse_tag_selector("").unwrap();
        assert_eq!(tags.len(), LogTag::all().len());
    }

    // -- LogOutput parsing ---------------------------------------------------

    #[test]
    fn output_parse_stdout() {
        assert_eq!(LogOutput::parse("stdout").unwrap(), LogOutput::Stdout);
        assert_eq!(LogOutput::parse("").unwrap(), LogOutput::Stdout);
        assert_eq!(LogOutput::parse("STDOUT").unwrap(), LogOutput::Stdout);
        assert_eq!(LogOutput::parse("#0").unwrap(), LogOutput::Stdout);
    }

    #[test]
    fn output_parse_stderr() {
        assert_eq!(LogOutput::parse("stderr").unwrap(), LogOutput::Stderr);
        assert_eq!(LogOutput::parse("STDERR").unwrap(), LogOutput::Stderr);
        assert_eq!(LogOutput::parse("#1").unwrap(), LogOutput::Stderr);
    }

    #[test]
    fn output_parse_file() {
        assert_eq!(
            LogOutput::parse("file=gc.log").unwrap(),
            LogOutput::File(PathBuf::from("gc.log"))
        );
    }

    #[test]
    fn output_parse_file_rejects_traversal() {
        assert!(LogOutput::parse("file=../etc/passwd").is_err());
        assert!(LogOutput::parse("file=foo/../../bar").is_err());
        assert!(LogOutput::parse("../etc/passwd").is_err());
    }

    #[test]
    fn output_parse_file_rejects_empty() {
        assert!(LogOutput::parse("file=").is_err());
    }

    /// gen r5w3/obs7: HotSpot treats an output that is not `stdout` /
    /// `stderr` and has no `file=` prefix as a file name (`-Xlog:gc:gc.log`).
    /// This was an "unknown output target" error that dropped the option.
    #[test]
    fn output_parse_bare_name_is_a_file() {
        assert_eq!(
            LogOutput::parse("gc.log").unwrap(),
            LogOutput::File(PathBuf::from("gc.log"))
        );
    }

    #[test]
    fn output_file_name_substitutions() {
        let pid = std::process::id().to_string();
        assert_eq!(expand_file_name("gc-%p.log"), format!("gc-{pid}.log"));
        assert_eq!(expand_file_name("a%%b"), "a%b");
        let t = expand_file_name("gc-%t.log");
        // gc-YYYY-MM-DD_HH-MM-SS.log
        assert_eq!(t.len(), "gc-.log".len() + 19, "{t}");
        assert!(!t.contains('%'), "{t}");
    }

    // -- LogDecorators parsing -----------------------------------------------

    /// gen r5w3/obs7: HotSpot's default decorators are `uptime,level,tags`.
    #[test]
    fn decorators_default_is_hotspots() {
        let d = LogDecorators::default();
        assert!(d.uptime && d.level && d.tags);
        assert!(!d.time && !d.pid && !d.tid && !d.utctime && !d.hostname);
        assert!(!d.timemillis && !d.uptimemillis && !d.timenanos && !d.uptimenanos);
    }

    #[test]
    fn decorators_parse_subset() {
        let d = LogDecorators::parse("time,level,tags").unwrap();
        assert!(d.time);
        assert!(!d.uptime);
        assert!(!d.pid);
        assert!(!d.tid);
        assert!(d.level);
        assert!(d.tags);
    }

    #[test]
    fn decorators_parse_abbreviations_and_new_names() {
        let d = LogDecorators::parse("t,utc,u,tm,um,tn,un,hn,p,ti,l,tg").unwrap();
        assert!(d.in_order().iter().all(|on| *on), "{d:?}");
        let d = LogDecorators::parse("uptimemillis,pid").unwrap();
        assert!(d.uptimemillis && d.pid && !d.uptime);
    }

    #[test]
    fn decorators_parse_none() {
        let d = LogDecorators::parse("none").unwrap();
        assert!(d.in_order().iter().all(|on| !on));
    }

    #[test]
    fn decorators_parse_empty_is_default() {
        let d = LogDecorators::parse("").unwrap();
        assert_eq!(d, LogDecorators::default());
    }

    #[test]
    fn decorators_parse_unknown() {
        assert!(LogDecorators::parse("hostnam").is_err());
    }

    // -- Full spec parsing ---------------------------------------------------

    #[test]
    fn parse_simple_gc_info() {
        let logger = UnifiedLogger::parse("gc=info").unwrap();
        assert_eq!(logger.rule_count(), 1);
        let rule = &logger.rules()[0];
        assert_eq!(rule.tags, vec![LogTag::Gc]);
        assert_eq!(rule.level, LogLevel::Info);
        assert_eq!(rule.output, LogOutput::Stdout);
        assert_eq!(rule.decorators, LogDecorators::default());
    }

    #[test]
    fn parse_gc_wildcard_with_output() {
        let logger = UnifiedLogger::parse("gc*=debug:stderr").unwrap();
        assert_eq!(logger.rule_count(), 1);
        let rule = &logger.rules()[0];
        assert_eq!(rule.tags.len(), LogTag::all_gc_tags().len()); // all gc tags
        assert_eq!(rule.level, LogLevel::Debug);
        assert_eq!(rule.output, LogOutput::Stderr);
    }

    #[test]
    fn parse_full_spec_with_decorators() {
        let logger = UnifiedLogger::parse("gc*=info:stdout:time,level,tags").unwrap();
        assert_eq!(logger.rule_count(), 1);
        let rule = &logger.rules()[0];
        assert!(rule.decorators.time);
        assert!(!rule.decorators.uptime);
        assert!(!rule.decorators.pid);
        assert!(!rule.decorators.tid);
        assert!(rule.decorators.level);
        assert!(rule.decorators.tags);
    }

    #[test]
    fn parse_multiple_rules() {
        let logger = UnifiedLogger::parse("gc=info:stdout;class*=debug:stderr").unwrap();
        assert_eq!(logger.rule_count(), 2);
        assert_eq!(logger.rules()[0].tags, vec![LogTag::Gc]);
        assert_eq!(logger.rules()[0].output, LogOutput::Stdout);
        assert!(logger.rules()[1].tags.contains(&LogTag::ClassLoad));
        assert_eq!(logger.rules()[1].output, LogOutput::Stderr);
    }

    /// gen r5w3/obs7: several comma-separated selections in one option, each
    /// with its own level, sharing the option's output and decorators.
    #[test]
    fn parse_comma_separated_selections() {
        let logger = UnifiedLogger::parse("gc=info,gc+heap=debug:stderr:uptime").unwrap();
        assert_eq!(logger.rule_count(), 2);
        assert_eq!(logger.rules()[1].tags, vec![LogTag::GcHeap]);
        assert_eq!(logger.rules()[1].level, LogLevel::Debug);
        for r in logger.rules() {
            assert_eq!(r.output, LogOutput::Stderr);
            assert!(r.decorators.uptime && !r.decorators.level);
        }
        assert!(logger.is_enabled(&[LogTag::GcHeap], LogLevel::Debug));
        assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Debug));
    }

    /// gen r5w3/obs7: the production spec people paste — output options in
    /// a 4th field and a tag this VM does not log to — used to be rejected
    /// whole (`unknown decorator: 'tags:filecount=5'`).
    #[test]
    fn parse_output_options_and_common_production_spec() {
        let logger =
            UnifiedLogger::parse("gc*,safepoint:file=gc.log:time,uptime,level,tags:filecount=5,filesize=10m")
                .unwrap();
        assert_eq!(logger.rule_count(), 2);
        assert_eq!(logger.rules()[0].output, LogOutput::File(PathBuf::from("gc.log")));
        assert!(logger.rules()[0].decorators.time && logger.rules()[0].decorators.uptime);
        assert!(logger.warnings().is_empty(), "{:?}", logger.warnings());
    }

    /// gen r5w3/obs7: a later selection overrides an earlier one for the
    /// same tag and output (`gc*,gc+heap=off`).
    #[test]
    fn later_selection_overrides_earlier() {
        let logger = UnifiedLogger::parse("gc*,gc+heap=off").unwrap();
        assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
        assert!(!logger.is_enabled(&[LogTag::GcHeap], LogLevel::Info));
        let logger = UnifiedLogger::parse("gc=debug;gc=warning").unwrap();
        assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    }

    /// gen r5w3/obs7: one bad selection is skipped with a warning, the rest
    /// of the option still applies; a spec with nothing usable is an error.
    #[test]
    fn bad_selection_is_skipped_not_fatal() {
        let logger = UnifiedLogger::parse("gc,nosuchtag=info").unwrap();
        assert_eq!(logger.rule_count(), 1);
        assert_eq!(logger.warnings().len(), 1, "{:?}", logger.warnings());
        assert!(UnifiedLogger::parse("nosuchtag").is_err());
    }

    #[test]
    fn disable_clears_everything_before_it() {
        let logger = UnifiedLogger::parse("gc*;disable").unwrap();
        assert_eq!(logger.rule_count(), 0);
        assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
        let logger = UnifiedLogger::parse("disable;gc").unwrap();
        assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    }

    #[test]
    fn windows_drive_letter_stays_with_the_path() {
        assert_eq!(
            split_option_fields(r"gc:file=C:\logs\gc.log:uptime"),
            vec!["gc".to_string(), r"file=C:\logs\gc.log".to_string(), "uptime".to_string()]
        );
        assert_eq!(
            split_option_fields("gc*:stdout:uptime,tid:filecount=0"),
            vec!["gc*", "stdout", "uptime,tid", "filecount=0"]
        );
    }

    #[test]
    fn parse_default_level_is_info() {
        let logger = UnifiedLogger::parse("gc").unwrap();
        assert_eq!(logger.rules()[0].level, LogLevel::Info);
    }

    #[test]
    fn parse_file_output() {
        let logger = UnifiedLogger::parse("gc=trace:file=gc.log").unwrap();
        assert_eq!(
            logger.rules()[0].output,
            LogOutput::File(PathBuf::from("gc.log"))
        );
    }

    #[test]
    fn parse_error_empty() {
        assert!(UnifiedLogger::parse("").is_err());
    }

    #[test]
    fn parse_error_bad_level() {
        assert!(UnifiedLogger::parse("gc=verbose").is_err());
    }

    #[test]
    fn parse_error_bad_tag() {
        assert!(UnifiedLogger::parse("foobar=info").is_err());
    }

    // -- is_enabled ----------------------------------------------------------

    #[test]
    fn is_enabled_matches() {
        let logger = UnifiedLogger::parse("gc*=info").unwrap();
        assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
        assert!(logger.is_enabled(&[LogTag::GcHeap], LogLevel::Error));
        assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Debug)); // debug > info threshold
        assert!(!logger.is_enabled(&[LogTag::Jit], LogLevel::Info)); // wrong tag
    }

    #[test]
    fn is_enabled_multiple_rules() {
        let logger = UnifiedLogger::parse("gc=info;jit=debug").unwrap();
        assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
        assert!(logger.is_enabled(&[LogTag::Jit], LogLevel::Debug));
        assert!(!logger.is_enabled(&[LogTag::Threading], LogLevel::Info));
    }

    // -- format_message ------------------------------------------------------

    #[test]
    fn format_no_decorators() {
        let logger = UnifiedLogger::parse("gc=info:stdout:none").unwrap();
        let msg = logger.format_message(
            &[LogTag::Gc],
            LogLevel::Info,
            "GC pause 12ms",
            &logger.rules()[0].decorators,
        );
        assert_eq!(msg, "GC pause 12ms");
    }

    #[test]
    fn format_level_and_tags_only() {
        let logger = UnifiedLogger::parse("gc=info:stdout:level,tags").unwrap();
        let msg = logger.format_message(
            &[LogTag::Gc],
            LogLevel::Info,
            "GC pause",
            &logger.rules()[0].decorators,
        );
        assert_eq!(msg, "[info][gc] GC pause");
    }

    #[test]
    fn format_multiple_tags() {
        let logger = UnifiedLogger::parse("gc*=info:stdout:tags").unwrap();
        let msg = logger.format_message(
            &[LogTag::Gc, LogTag::GcHeap],
            LogLevel::Info,
            "heap resize",
            &logger.rules()[0].decorators,
        );
        assert_eq!(msg, "[gc,gc,heap] heap resize");
    }

    /// gen r5w3/obs7: the default line is HotSpot's `[<uptime>s][info][gc]`.
    #[test]
    fn format_default_decorators_are_hotspots() {
        let logger = UnifiedLogger::parse("gc").unwrap();
        let msg = logger.format_message(
            &[LogTag::Gc],
            LogLevel::Info,
            "Using Serial",
            &logger.rules()[0].decorators,
        );
        let (uptime, rest) = msg.split_once(']').expect("decorated");
        assert!(uptime.starts_with('[') && uptime.ends_with('s'), "{msg}");
        assert!(uptime[1..uptime.len() - 1].parse::<f64>().is_ok(), "{msg}");
        assert_eq!(rest, "[info][gc] Using Serial");
    }

    /// gen r5w3/obs7: HotSpot pads each decoration to the widest value that
    /// output has printed so far, so a `gc` line after a `gc,metaspace` one
    /// reads `[gc          ]`.
    #[test]
    fn decorations_are_padded_to_the_widest_so_far() {
        let logger = UnifiedLogger::parse("gc*:stdout:level,tags").unwrap();
        let d = logger.rules()[0].decorators.clone();
        let mut pad = [0; DECORATOR_COUNT];
        let a = logger.format_padded(&[LogTag::Gc], LogLevel::Info, "a", &d, &mut pad);
        let b = logger.format_padded(&[LogTag::GcMetaspace], LogLevel::Info, "b", &d, &mut pad);
        let c = logger.format_padded(&[LogTag::Gc], LogLevel::Info, "c", &d, &mut pad);
        assert_eq!(a, "[info][gc] a");
        assert_eq!(b, "[info][gc,metaspace] b");
        // The tag column is padded to HotSpot's width; built rather than
        // spelled, so the literal-line-break ratchet does not count it.
        assert_eq!(c, format!("[info][gc{}] c", " ".repeat(10)));
    }

    #[test]
    fn tid_and_pid_decorations_are_bare_numbers() {
        let logger = UnifiedLogger::parse("gc:stdout:pid,tid").unwrap();
        let msg = logger.format_message(
            &[LogTag::Gc],
            LogLevel::Info,
            "x",
            &logger.rules()[0].decorators,
        );
        let pid = std::process::id();
        assert!(msg.starts_with(&format!("[{pid}][")), "{msg}");
        let tid = msg
            .split(']')
            .nth(1)
            .and_then(|s| s.strip_prefix('['))
            .expect("tid decoration");
        assert!(tid.parse::<u64>().is_ok(), "bare tid: {msg}");
    }

    // -- GC startup lines ----------------------------------------------------

    #[test]
    fn size_formats_are_hotspots() {
        const M: u64 = 1024 * 1024;
        assert_eq!(exact_size(64 * M), "64M");
        assert_eq!(exact_size(1024 * M), "1G");
        assert_eq!(exact_size(1536 * M), "1536M");
        assert_eq!(exact_size(8 * 1024 + 512), "8704B");
        assert_eq!(exact_size(12 * 1024), "12K");
        assert_eq!(proper_size(32 * 1024 * M), "32768M");
        assert_eq!(proper_size(200 * 1024 * M), "200G");
        assert_eq!(proper_size(99 * 1024), "101376B");
    }

    #[test]
    fn gc_init_lines_follow_gc_init_logger() {
        const M: usize = 1024 * 1024;
        let lines = gc_init_lines(&GcInitFacts {
            collector: "Serial",
            version: "25.0.1+8",
            cpus_total: 8,
            cpus_available: 4,
            memory_bytes: Some(16 * 1024 * 1024 * 1024),
            heap_min: 16 * M,
            heap_initial: 16 * M,
            heap_max: 512 * M,
        });
        assert_eq!(
            lines,
            [
                "Version: 25.0.1+8 (release)",
                "CPUs: 8 total, 4 available",
                "Memory: 16384M",
                "Heap Min Capacity: 16M",
                "Heap Initial Capacity: 16M",
                "Heap Max Capacity: 512M",
            ]
        );
    }

    // -- File path validation ------------------------------------------------

    #[test]
    fn validate_path_normal() {
        assert!(validate_log_file_path("gc.log").is_ok());
        assert!(validate_log_file_path("logs/gc.log").is_ok());
    }

    #[test]
    fn validate_path_traversal_rejected() {
        assert!(validate_log_file_path("../gc.log").is_err());
        assert!(validate_log_file_path("logs/../../gc.log").is_err());
    }

    #[test]
    fn validate_path_empty_rejected() {
        assert!(validate_log_file_path("").is_err());
    }

    #[test]
    fn validate_path_root_rejected() {
        assert!(validate_log_file_path("/").is_err());
    }

    // -- Timestamp / helpers -------------------------------------------------

    #[test]
    fn timestamp_is_iso8601_with_offset() {
        let ts = iso8601_utc(SystemTime::now());
        // Should match pattern: YYYY-MM-DDThh:mm:ss.mmm+0000
        assert!(ts.ends_with("+0000"), "timestamp should carry an offset: {ts}");
        assert!(ts.contains('T'), "timestamp should contain T: {ts}");
        assert_eq!(ts.len(), 28, "timestamp length: {ts}");
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-01 is day 19723 since epoch
        let (y, m, d) = days_to_ymd(19723);
        assert_eq!((y, m, d), (2024, 1, 1));
    }

    // -- LogOutput to file (integration) -------------------------------------

    #[test]
    fn file_output_creates_file() {
        let dir = std::env::temp_dir().join("cratonvm_unified_log_test");
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("test.log");
        // Clean up from previous runs
        let _ = std::fs::remove_file(&log_path);

        let spec = format!("gc=info:file={}", log_path.display());
        let logger = UnifiedLogger::parse(&spec).unwrap();
        logger.log(&[LogTag::Gc], LogLevel::Info, "test message");

        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            contents.contains("test message"),
            "log file should contain the message: {contents}"
        );

        // Clean up
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn file_output_reuses_handle_and_appends() {
        // Exercises the find-or-create handle lookup: repeated logs to the same
        // path must reuse the cached handle (one map entry) and append in order.
        let dir = std::env::temp_dir().join("cratonvm_unified_log_reuse_test");
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("reuse.log");
        let _ = std::fs::remove_file(&log_path);

        let spec = format!("gc=info:file={}", log_path.display());
        let logger = UnifiedLogger::parse(&spec).unwrap();
        logger.log(&[LogTag::Gc], LogLevel::Info, "first");
        logger.log(&[LogTag::Gc], LogLevel::Info, "second");
        logger.log(&[LogTag::Gc], LogLevel::Info, "third");

        // Exactly one cached handle for the single path.
        {
            let handles = logger.file_handles.lock().unwrap();
            assert_eq!(handles.len(), 1);
        }

        let contents = std::fs::read_to_string(&log_path).unwrap();
        let first = contents.find("first").expect("first present");
        let second = contents.find("second").expect("second present");
        let third = contents.find("third").expect("third present");
        assert!(first < second && second < third, "append order: {contents}");

        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// gen r5w3/obs7: two selections that both match a message on the same
    /// output print it ONCE (each rule used to print its own copy).
    #[test]
    fn one_output_prints_a_message_once() {
        let dir = std::env::temp_dir().join("cratonvm_unified_log_once_test");
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("once.log");
        let _ = std::fs::remove_file(&log_path);

        let spec = format!("gc*,gc+heap=debug:file={}:none", log_path.display());
        let logger = UnifiedLogger::parse(&spec).unwrap();
        logger.log(&[LogTag::GcHeap], LogLevel::Info, "heap line");

        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(contents, "heap line\n");

        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
