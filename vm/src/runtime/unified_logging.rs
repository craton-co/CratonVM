// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot-style unified logging framework (`-Xlog`).
//!
//! Implements the JEP 158 unified logging syntax:
//! `-Xlog:tag1[+tag2]*[=level][:output[:decorators]]`
//!
//! Example specs:
//! - `gc*=info` — all GC tags at info level to stdout
//! - `gc+heap=debug:stderr` — gc+heap at debug to stderr
//! - `gc*=trace:file=gc.log:time,level,tags` — GC trace to file with decorators

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// ---------------------------------------------------------------------------
// LogTag
// ---------------------------------------------------------------------------

/// Tags that categorize log messages, matching HotSpot's tag taxonomy.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutput {
    Stdout,
    Stderr,
    File(PathBuf),
}

impl LogOutput {
    /// Parse an output specification.
    ///
    /// - `"stdout"` or empty → `Stdout`
    /// - `"stderr"` → `Stderr`
    /// - `"file=<path>"` → `File(path)` with security validation
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("stdout") {
            return Ok(LogOutput::Stdout);
        }
        if s.eq_ignore_ascii_case("stderr") {
            return Ok(LogOutput::Stderr);
        }
        if let Some(path_str) = s.strip_prefix("file=") {
            let path_str = path_str.trim();
            if path_str.is_empty() {
                return Err("file output requires a path: file=<path>".to_string());
            }
            validate_log_file_path(path_str)?;
            return Ok(LogOutput::File(PathBuf::from(path_str)));
        }
        Err(format!(
            "unknown output target: '{s}' (expected stdout, stderr, or file=<path>)"
        ))
    }
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

/// Which metadata fields to include in each log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDecorators {
    pub time: bool,
    pub uptime: bool,
    pub pid: bool,
    pub tid: bool,
    pub level: bool,
    pub tags: bool,
}

impl Default for LogDecorators {
    fn default() -> Self {
        Self {
            time: true,
            uptime: true,
            pid: true,
            tid: true,
            level: true,
            tags: true,
        }
    }
}

impl LogDecorators {
    /// Parse a comma-separated decorator list.
    ///
    /// Known decorators: `time`, `uptime`, `pid`, `tid`, `level`, `tags`, `none`.
    /// `none` disables all decorators.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::default());
        }

        if s.eq_ignore_ascii_case("none") {
            return Ok(Self {
                time: false,
                uptime: false,
                pid: false,
                tid: false,
                level: false,
                tags: false,
            });
        }

        // Start with all false, then enable requested decorators
        let mut dec = Self {
            time: false,
            uptime: false,
            pid: false,
            tid: false,
            level: false,
            tags: false,
        };

        for part in s.split(',') {
            let part = part.trim();
            match part.to_ascii_lowercase().as_str() {
                "time" => dec.time = true,
                "uptime" => dec.uptime = true,
                "pid" => dec.pid = true,
                "tid" => dec.tid = true,
                "level" => dec.level = true,
                "tags" => dec.tags = true,
                other => return Err(format!("unknown decorator: '{other}'")),
            }
        }

        Ok(dec)
    }
}

// ---------------------------------------------------------------------------
// LogRule
// ---------------------------------------------------------------------------

/// A single logging rule: which tags at which level go to which output.
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
// TagSelector — intermediate parse result for wildcard expansion
// ---------------------------------------------------------------------------

/// Represents a tag selector before wildcard expansion.
enum TagSelector {
    /// A single explicit tag.
    Exact(LogTag),
    /// A wildcard group like `gc*` → all GC tags.
    Wildcard(Vec<LogTag>),
    /// `*` alone → all tags.
    All,
}

/// Parse the tag portion of a spec (before `=`), handling `+` combinators
/// and `*` wildcards.
fn parse_tag_selector(s: &str) -> Result<Vec<LogTag>, String> {
    let s = s.trim();
    if s.is_empty() || s == "*" {
        // All tags
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

    // First try as a compound tag (e.g., "gc+heap" → GcHeap)
    if s.contains('+') {
        if let Ok(tag) = LogTag::from_str(s) {
            return Ok(vec![tag]);
        }
    }

    // Explicit tag list separated by `+`
    let mut tags = Vec::new();
    for part in s.split('+') {
        let part = part.trim();
        if !part.is_empty() {
            tags.push(LogTag::from_str(part)?);
        }
    }
    if tags.is_empty() {
        return Err(format!("empty tag selector: '{s}'"));
    }
    Ok(tags)
}

/// Expand a prefix wildcard like `gc` into all tags starting with that prefix.
fn expand_wildcard(prefix: &str) -> Result<Vec<LogTag>, String> {
    let prefix_lower = prefix.to_ascii_lowercase();
    match prefix_lower.as_str() {
        "gc" => Ok(LogTag::all_gc_tags().to_vec()),
        "class" => Ok(LogTag::all_class_tags().to_vec()),
        _ => {
            // Try matching as a single tag
            let tag = LogTag::from_str(prefix)?;
            Ok(vec![tag])
        }
    }
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
    /// Open file handles for `File` outputs, keyed by path for O(1) lookup.
    file_handles: Mutex<HashMap<PathBuf, File>>,
}

impl fmt::Debug for UnifiedLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnifiedLogger")
            .field("rules", &self.rules)
            .field("pid", &self.pid)
            .finish()
    }
}

impl UnifiedLogger {
    /// Parse a full `-Xlog` spec string into a `UnifiedLogger`.
    ///
    /// The spec may contain multiple rules separated by `;`:
    /// `gc*=info:stdout:time,level,tags;class*=debug:stderr`
    ///
    /// Each rule has the form: `tags[=level][:output[:decorators]]`
    ///
    /// Defaults:
    /// - level: `info`
    /// - output: `stdout`
    /// - decorators: all enabled
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty -Xlog spec".to_string());
        }

        let mut rules = Vec::new();

        for rule_str in spec.split(';') {
            let rule_str = rule_str.trim();
            if rule_str.is_empty() {
                continue;
            }
            rules.push(Self::parse_single_rule(rule_str)?);
        }

        if rules.is_empty() {
            return Err("no valid rules in -Xlog spec".to_string());
        }

        Ok(Self {
            rules,
            start_time: Instant::now(),
            pid: std::process::id(),
            file_handles: Mutex::new(HashMap::new()),
        })
    }

    /// Parse a single rule segment: `tags[=level][:output[:decorators]]`
    fn parse_single_rule(s: &str) -> Result<LogRule, String> {
        // Split on `:` for output and decorators, but first handle `=` for level.
        // Format: tags_and_level:output:decorators
        // tags_and_level: tags[=level]
        //
        // On Windows, file paths contain `:` (e.g., `file=C:\path`), so when
        // the output starts with `file=`, we must be careful not to split the
        // drive letter away from the rest of the path.

        let parts: Vec<&str> = s.splitn(3, ':').collect();

        let tags_and_level = parts[0].trim();

        // Detect `file=X:\path` pattern where the split broke the drive letter
        let (output_str, decorators_str) = if parts.len() >= 3
            && parts[1].trim().starts_with("file=")
            && parts[1].trim().len() == "file=X".len()
        {
            // parts[1] is "file=C", parts[2] is "\path[:decorators]"
            // Rejoin as "file=C:\path" and split decorators at next ':'
            let rest = parts[2];
            if let Some(colon_pos) = rest.find(':') {
                let full_path = format!("{}:{}", parts[1].trim(), &rest[..colon_pos]);
                (full_path, rest[colon_pos + 1..].to_string())
            } else {
                let full_path = format!("{}:{}", parts[1].trim(), rest);
                (full_path, String::new())
            }
        } else {
            (
                parts.get(1).copied().unwrap_or("").to_string(),
                parts.get(2).copied().unwrap_or("").to_string(),
            )
        };

        // Split tags from level on `=`
        let (tags_str, level_str) = if let Some(eq_pos) = tags_and_level.rfind('=') {
            (&tags_and_level[..eq_pos], &tags_and_level[eq_pos + 1..])
        } else {
            (tags_and_level, "info")
        };

        let tags = parse_tag_selector(tags_str)?;
        let level = LogLevel::from_str(level_str)?;
        let output = LogOutput::parse(&output_str)?;
        let decorators = LogDecorators::parse(&decorators_str)?;

        Ok(LogRule {
            tags,
            level,
            output,
            decorators,
        })
    }

    /// Check if a message with the given tags and level would be emitted
    /// by any active rule.
    pub fn is_enabled(&self, tags: &[LogTag], level: LogLevel) -> bool {
        self.rules.iter().any(|rule| {
            level.is_enabled_at(rule.level) && tags.iter().any(|t| rule.tags.contains(t))
        })
    }

    /// Format and emit a log message through all matching rules.
    pub fn log(&self, tags: &[LogTag], level: LogLevel, message: &str) {
        for rule in &self.rules {
            if !level.is_enabled_at(rule.level) {
                continue;
            }
            if !tags.iter().any(|t| rule.tags.contains(t)) {
                continue;
            }

            let formatted = self.format_message(tags, level, message, &rule.decorators);

            match &rule.output {
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

    /// Format a single log line with the requested decorators.
    ///
    /// Output format: `[decorator1][decorator2]... message`
    fn format_message(
        &self,
        tags: &[LogTag],
        level: LogLevel,
        message: &str,
        decorators: &LogDecorators,
    ) -> String {
        let mut prefix = String::new();

        if decorators.time {
            let now = chrono_free_timestamp();
            prefix.push_str(&format!("[{now}]"));
        }

        if decorators.uptime {
            let elapsed = self.start_time.elapsed();
            let secs = elapsed.as_secs_f64();
            prefix.push_str(&format!("[{secs:.3}s]"));
        }

        if decorators.pid {
            // HotSpot format: bare pid number in brackets (e.g. `[45678]`).
            prefix.push_str(&format!("[{pid}]", pid = self.pid));
        }

        if decorators.tid {
            let tid = thread_id();
            prefix.push_str(&format!("[tid={tid}]"));
        }

        if decorators.level {
            prefix.push_str(&format!("[{lvl}]", lvl = level.as_str()));
        }

        if decorators.tags {
            let tag_str: Vec<&str> = tags.iter().map(|t| t.as_str()).collect();
            prefix.push_str(&format!("[{tags}]", tags = tag_str.join(",")));
        }

        if prefix.is_empty() {
            message.to_string()
        } else {
            format!("{prefix} {message}")
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
}

// ---------------------------------------------------------------------------
// Timestamp helper (no chrono dependency)
// ---------------------------------------------------------------------------

/// Produce a human-readable UTC timestamp without pulling in the `chrono` crate.
/// Format: `YYYY-MM-DDThh:mm:ss.mmmZ` (ISO 8601, millisecond precision).
fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;

    let now = SystemTime::now();
    let since_epoch = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    let total_secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();

    // Manual UTC calendar decomposition (no leap-second handling — good enough
    // for logging timestamps).
    let days = total_secs / 86400;
    let time_of_day = total_secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Date from days since epoch (1970-01-01)
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
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

/// Get a numeric thread identifier suitable for logging.
fn thread_id() -> u64 {
    // Use the thread name hash or a simple counter. For portability,
    // we use the thread::current().id() debug representation and extract
    // the numeric part.
    let id = std::thread::current().id();
    let debug = format!("{id:?}");
    debug
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0)
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
/// logger wins).
pub fn init_unified_logging(spec: &str) -> Result<(), String> {
    let logger = UnifiedLogger::parse(spec)?;
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
    }

    // -- Tag selector parsing with wildcards ---------------------------------

    #[test]
    fn tag_selector_gc_wildcard() {
        let tags = parse_tag_selector("gc*").unwrap();
        assert!(tags.contains(&LogTag::Gc));
        assert!(tags.contains(&LogTag::GcPhases));
        assert!(tags.contains(&LogTag::GcHeap));
        assert!(tags.contains(&LogTag::GcAge));
        assert!(tags.contains(&LogTag::GcAlloc));
        assert!(tags.contains(&LogTag::GcCpu));
        assert!(tags.contains(&LogTag::GcErgo));
        assert!(tags.contains(&LogTag::GcMetaspace));
        assert_eq!(tags.len(), 8);
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
    }

    #[test]
    fn tag_selector_explicit_combo() {
        let tags = parse_tag_selector("gc+heap").unwrap();
        assert_eq!(tags, vec![LogTag::GcHeap]);
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
    }

    #[test]
    fn output_parse_stderr() {
        assert_eq!(LogOutput::parse("stderr").unwrap(), LogOutput::Stderr);
        assert_eq!(LogOutput::parse("STDERR").unwrap(), LogOutput::Stderr);
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
    }

    #[test]
    fn output_parse_file_rejects_empty() {
        assert!(LogOutput::parse("file=").is_err());
    }

    #[test]
    fn output_parse_unknown() {
        assert!(LogOutput::parse("syslog").is_err());
    }

    // -- LogDecorators parsing -----------------------------------------------

    #[test]
    fn decorators_default_all_on() {
        let d = LogDecorators::default();
        assert!(d.time && d.uptime && d.pid && d.tid && d.level && d.tags);
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
    fn decorators_parse_none() {
        let d = LogDecorators::parse("none").unwrap();
        assert!(!d.time && !d.uptime && !d.pid && !d.tid && !d.level && !d.tags);
    }

    #[test]
    fn decorators_parse_empty_is_default() {
        let d = LogDecorators::parse("").unwrap();
        assert_eq!(d, LogDecorators::default());
    }

    #[test]
    fn decorators_parse_unknown() {
        assert!(LogDecorators::parse("hostname").is_err());
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
    }

    #[test]
    fn parse_gc_wildcard_with_output() {
        let logger = UnifiedLogger::parse("gc*=debug:stderr").unwrap();
        assert_eq!(logger.rule_count(), 1);
        let rule = &logger.rules()[0];
        assert_eq!(rule.tags.len(), 8); // all gc tags
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
    fn timestamp_is_iso8601() {
        let ts = chrono_free_timestamp();
        // Should match pattern: YYYY-MM-DDThh:mm:ss.mmmZ
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
        assert!(ts.contains('T'), "timestamp should contain T: {ts}");
        assert_eq!(ts.len(), 24, "timestamp length: {ts}");
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
}
