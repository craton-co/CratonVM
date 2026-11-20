// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//!   (gci i46/lg: and applied -- a file output is archived to `<name>.<n>`
//!   at VM start and rotated at `filesize`, HotSpot 25's `LogFileOutput`;
//!   see "File outputs: options and rotation"); an output with no `file=` prefix that is not
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
use std::sync::{Arc, Mutex};
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
    /// `gc,heap,exit` — selectable, and prints nothing, exactly as on
    /// HotSpot 25: there the exit heap summary is `gc,exit`
    /// ([`LogTag::GcExit`]) and `-Xlog:gc+heap+exit` matches no line
    /// (measured on 25.0.3+9, gcf f1/r). gen r5w3/obs7 assumed `gc,heap,exit`.
    GcHeapExit,
    /// `gc,exit` — HotSpot 25's exit heap summary (`Heap`, ` DefNew total ..`,
    /// ` Tenured total ..`, ` Metaspace used ..`), selected by `gc*` and
    /// `gc+exit`. gcf f1/r; printed once at exit since gcf f2/r
    /// ([`log_gc_exit_summary`], Generational backend only).
    GcExit,
    /// `gc,marking` — concurrent marking. gen r5w3/obs7.
    GcMarking,
    /// `gc,ref` — reference processing. gen r5w3/obs7; HotSpot's debug lines
    /// are rendered by [`xlog_reference_lines`] since gcg g3/x.
    GcRef,
    /// `gc,phases,ref` — HotSpot's per-pause reference-processing phase
    /// times and census at debug ([`xlog_reference_lines`]). gcg g3/x: the
    /// selection `gc+phases+ref` was refused as "no tag set matches".
    GcPhasesRef,
    /// `gc,phases,start` — HotSpot's phase-start lines. Selectable (gcg g3/x);
    /// nothing logs to it yet.
    GcPhasesStart,
    /// `gc,promotion` — Serial's `Promotion failed` (info). Selectable (gcg
    /// g3/x); nothing logs to it yet
    /// (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full-FIXED-20261001`).
    GcPromotion,
    /// `gc,tlab` — HotSpot's TLAB statistics (debug / trace). Selectable
    /// (gcg g3/x); nothing logs to it yet.
    GcTlab,
    /// `gc,jni` — HotSpot's GC-locker stall line at debug. Selectable (gcg
    /// g4/yg: `-Xlog:gc+jni` warned "no tag set matches", HotSpot accepts
    /// it); nothing logs to it yet.
    GcJni,
    /// `gc,periodic` -- G1's periodic collection check
    /// (`G1PeriodicGCTask::check_for_periodic_gc`, debug). zt t31/x10.
    GcPeriodic,
    /// `gc,load` -- ZGC's per-generation `Load:` line (`ZStatLoad::print`,
    /// the system load averages). zt t31/x10 (x10 page item 2).
    GcLoad,
    /// `gc,mmu` -- ZGC's per-generation `MMU:` line (`ZStatMMU::print`,
    /// minimum mutator utilisation over this VM's pauses). zt t31/x10.
    GcMmu,
    /// `gc,reloc` -- ZGC's relocation tables (selectable, nothing logs to
    /// it: this collector has no page-size classes or age table; GC.md).
    GcReloc,
    /// `gc,task` -- ZGC's `Using N Workers` lines (selectable, nothing logs
    /// to it: no per-generation worker pools; GC.md).
    GcTask,
    /// `gc,nmethod` -- ZGC's `NMethods:` line (selectable, nothing logs to
    /// it; GC.md).
    GcNmethod,
    /// `gc,stats` -- ZGC's periodic `ZStat` table (selectable, nothing logs
    /// to it: no 10 s / 10 m / 10 h sampler; GC.md).
    GcStats,
    /// `safepoint` (selectable; `-Xlog:gc*,safepoint` is a common spec and
    /// used to reject the whole option). gen r5w3/obs7.
    Safepoint,
    /// `safepoint,stats` -- HotSpot 25's per-safepoint statistics table and
    /// its exit summary (`SafepointTracing::statistics_log` /
    /// `statistics_exit_log`, info). Selectable (gci i46/lg,
    /// `gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual 5: the
    /// selection warned `No tag set matches selection`). gcj j1/ob: the
    /// Generational collector logs the table and the exit summary to it
    /// (`CRATONVM_GEN_XLOG_SAFEPOINT_STATS`); G1 and ZGC log nothing here.
    SafepointStats,
    /// `interpreter,safepoint` -- a HotSpot 25 tag set (selectable, nothing
    /// logs to it). gci i47w/j46: it replaces i46/lg's `safepoint,cleanup`,
    /// which HotSpot 25 does NOT have: `-Xlog:safepoint+cleanup=off` warns
    /// `No tag set matches selection: safepoint+cleanup. Did you mean any of
    /// the following? safepoint* safepoint+stats interpreter+safepoint` there
    /// (row `lg46_p_tags`), and this set completes that suggestion list.
    InterpreterSafepoint,
    ClassLoad,
    ClassUnload,
    Jit,
    Jni,
    Threading,
    Os,
    Modules,
    Exceptions,
    /// `stringtable`: HotSpot's String table lines (`Start size`, each
    /// collection's `Uncleaned items`, the concurrent work's trigger, grow and
    /// clean; gcj j1/st, `memory::string_table::hotspot_gc_notification`).
    /// Selectable since gcg g2/w item 6 (GC wave 9 lane gcD).
    StringTable,
    /// gcj j1/st: `stringtable,perf` (the concurrent work's and the
    /// statistics' timings).
    StringTablePerf,
    /// gcj j1/st: `symboltable` (`Start size: 32768 (15)`).
    SymbolTable,
    /// gcj j1/st: `symboltable,perf` (the statistics' timing).
    SymbolTablePerf,
    /// `valuebasedclasses`: `-XX:DiagnoseSyncOnValueBasedClasses=2`'s
    /// `Synchronizing on object ...` line and the locking stack (info;
    /// interpreter round i5 wave 102, lane L6,
    /// `vm_exec::diagnose_sync_on_value_based_class`).
    ValueBasedClasses,
    /// `jfr,startup` -- JFR's start-up recording banner (gcj j2/ob2,
    /// `CRATONVM_JFR_STARTUP_BANNER`, opt-in): HotSpot turns it on at info on
    /// stdout when `-XX:StartFlightRecording` is given. Written through
    /// [`log_jfr_startup_lines`] only; not a selectable tag set here (not in
    /// [`LogTag::all`] nor [`LogTag::from_str`]), so no `-Xlog` selection
    /// changes.
    Jfr,
    JfrStartup,
    /// `pagesize` -- `os::large_page_init`'s and ZGC's start-up warnings
    /// (`UseLargePages disabled, ...`, `Shared memory transparent huge pages
    /// are not enabled ...`; [`log_pagesize_warning`]). zt t26/x21: a HotSpot
    /// tag set, so `-Xlog:pagesize=off` / `-Xlog:pagesize:stderr` route it.
    Pagesize,
    /// `jni,resolve` -- `jni_RegisterNatives`'s `Re-registering of platform
    /// native method: ...` warning (j27 wave 8, lane W8DSP,
    /// `native::jni::jni_platform_rebind`; [`log_warning_line`]). A HotSpot
    /// tag set: `-Xlog:jni+resolve=off` silences it, `jni*` selects it.
    JniResolve,
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
            "gc+exit" => Ok(LogTag::GcExit),
            "gc+marking" | "gcmarking" | "gc_marking" => Ok(LogTag::GcMarking),
            "gc+ref" | "gcref" | "gc_ref" => Ok(LogTag::GcRef),
            "gc+phases+ref" => Ok(LogTag::GcPhasesRef),
            "gc+phases+start" => Ok(LogTag::GcPhasesStart),
            "gc+promotion" => Ok(LogTag::GcPromotion),
            "gc+tlab" => Ok(LogTag::GcTlab),
            "gc+jni" => Ok(LogTag::GcJni),
            "gc+periodic" => Ok(LogTag::GcPeriodic),
            "gc+load" => Ok(LogTag::GcLoad),
            "gc+mmu" => Ok(LogTag::GcMmu),
            "gc+reloc" => Ok(LogTag::GcReloc),
            "gc+task" => Ok(LogTag::GcTask),
            "gc+nmethod" => Ok(LogTag::GcNmethod),
            "gc+stats" => Ok(LogTag::GcStats),
            "safepoint" => Ok(LogTag::Safepoint),
            "safepoint+stats" => Ok(LogTag::SafepointStats),
            "interpreter+safepoint" => Ok(LogTag::InterpreterSafepoint),
            "classload" | "class+load" | "class_load" => Ok(LogTag::ClassLoad),
            "classunload" | "class+unload" | "class_unload" => Ok(LogTag::ClassUnload),
            "jit" => Ok(LogTag::Jit),
            "jni" => Ok(LogTag::Jni),
            "threading" => Ok(LogTag::Threading),
            "os" => Ok(LogTag::Os),
            "modules" => Ok(LogTag::Modules),
            "exceptions" => Ok(LogTag::Exceptions),
            "stringtable" => Ok(LogTag::StringTable),
            "stringtable+perf" => Ok(LogTag::StringTablePerf),
            "symboltable" => Ok(LogTag::SymbolTable),
            "symboltable+perf" => Ok(LogTag::SymbolTablePerf),
            "valuebasedclasses" => Ok(LogTag::ValueBasedClasses),
            "jfr" => Ok(LogTag::Jfr),
            "jfr+startup" => Ok(LogTag::JfrStartup),
            "pagesize" => Ok(LogTag::Pagesize),
            "jni+resolve" => Ok(LogTag::JniResolve),
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
            LogTag::GcExit => "gc,exit",
            LogTag::GcMarking => "gc,marking",
            LogTag::GcRef => "gc,ref",
            LogTag::GcPhasesRef => "gc,phases,ref",
            LogTag::GcPhasesStart => "gc,phases,start",
            LogTag::GcPromotion => "gc,promotion",
            LogTag::GcTlab => "gc,tlab",
            LogTag::GcJni => "gc,jni",
            LogTag::GcPeriodic => "gc,periodic",
            LogTag::GcLoad => "gc,load",
            LogTag::GcMmu => "gc,mmu",
            LogTag::GcReloc => "gc,reloc",
            LogTag::GcTask => "gc,task",
            LogTag::GcNmethod => "gc,nmethod",
            LogTag::GcStats => "gc,stats",
            LogTag::Safepoint => "safepoint",
            LogTag::SafepointStats => "safepoint,stats",
            LogTag::InterpreterSafepoint => "interpreter,safepoint",
            LogTag::ClassLoad => "class,load",
            LogTag::ClassUnload => "class,unload",
            LogTag::Jit => "jit",
            LogTag::Jni => "jni",
            LogTag::Threading => "threading",
            LogTag::Os => "os",
            LogTag::Modules => "modules",
            LogTag::Exceptions => "exceptions",
            LogTag::StringTable => "stringtable",
            LogTag::StringTablePerf => "stringtable,perf",
            LogTag::SymbolTable => "symboltable",
            LogTag::SymbolTablePerf => "symboltable,perf",
            LogTag::ValueBasedClasses => "valuebasedclasses",
            LogTag::Jfr => "jfr",
            LogTag::JfrStartup => "jfr,startup",
            LogTag::Pagesize => "pagesize",
            LogTag::JniResolve => "jni,resolve",
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
            LogTag::GcExit,
            LogTag::GcMarking,
            LogTag::GcRef,
            LogTag::GcPhasesRef,
            LogTag::GcPhasesStart,
            LogTag::GcPromotion,
            LogTag::GcTlab,
            LogTag::GcJni,
            LogTag::GcPeriodic,
            LogTag::GcLoad,
            LogTag::GcMmu,
            LogTag::GcReloc,
            LogTag::GcTask,
            LogTag::GcNmethod,
            LogTag::GcStats,
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
            LogTag::GcExit,
            LogTag::GcMarking,
            LogTag::GcRef,
            LogTag::GcPhasesRef,
            LogTag::GcPhasesStart,
            LogTag::GcPromotion,
            LogTag::GcTlab,
            LogTag::GcJni,
            LogTag::GcPeriodic,
            LogTag::GcLoad,
            LogTag::GcMmu,
            LogTag::GcReloc,
            LogTag::GcTask,
            LogTag::GcNmethod,
            LogTag::GcStats,
            LogTag::Safepoint,
            LogTag::SafepointStats,
            LogTag::InterpreterSafepoint,
            LogTag::ClassLoad,
            LogTag::ClassUnload,
            LogTag::Jit,
            LogTag::Jni,
            LogTag::Threading,
            LogTag::Os,
            LogTag::Modules,
            LogTag::Exceptions,
            LogTag::StringTable,
            LogTag::StringTablePerf,
            LogTag::SymbolTable,
            LogTag::SymbolTablePerf,
            LogTag::ValueBasedClasses,
            LogTag::Jfr,
            LogTag::JfrStartup,
            LogTag::Pagesize,
            LogTag::JniResolve,
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
// File outputs: options and rotation (gci i46/lg)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`:
// every `file=` output was opened `create + append` at its first line, and
// the `filecount=` / `filesize=` options were parsed and dropped. HotSpot 25
// (`logFileOutput.cpp`):
//
// * `LogFileOutput::initialize`, at `-Xlog` parse time: an existing regular
//   file is archived to `<name>.<n>` before the new one is opened when
//   `filecount` (default 5) is above 0 -- `n` the first index in
//   `0..filecount` with no file, else the index of the oldest archive,
//   zero-padded to the digits of `filecount - 1` -- and is TRUNCATED when
//   `filecount=0`. A FIFO under the default count is never rotated.
// * `LogFileOutput::write_blocking`: after a line, when the bytes written
//   since the open reach `filesize` (default 20M; 0 = never) and `filecount`
//   is above 0, the file is rotated: closed, archived to the next index
//   (wrapping at `filecount`), and opened anew.
//
// Kill switch `CRATONVM_XLOG_FILE_ROTATION=0`: options ignored, every file
// appended to unrotated, as before.

/// gci i46/lg: `CRATONVM_XLOG_FILE_ROTATION`, default ON (`=0` kills). Read
/// once per `-Xlog` parse ([`UnifiedLogger::parse`]).
pub fn xlog_file_rotation_enabled() -> bool {
    true
}

/// HotSpot 25's `LogFileOutput::DefaultFileCount`.
const LOG_FILE_DEFAULT_COUNT: u32 = 5;
/// HotSpot 25's `LogFileOutput::DefaultFileSize` (20 MiB).
const LOG_FILE_DEFAULT_SIZE: u64 = 20 * 1024 * 1024;
/// HotSpot 25's `MaxRotationFileCount`.
const LOG_FILE_MAX_COUNT: u32 = 1000;

/// gci i46/lg: HotSpot 25's warning when an `-Xlog` option gives output
/// options to an output an earlier option (or the VM: `stdout`, `stderr`)
/// already created (`LogConfiguration::parse_log_arguments`). Printed on
/// stdout as `[<uptime>][warning][logging] ...` with the no-match warnings.
const XLOG_EXISTING_OUTPUT_OPTIONS_IGNORED: &str = "Output options for existing outputs are ignored.";

/// gci i46/lg: a file output's rotation options, HotSpot 25 `LogFileOutput`'s
/// `_file_count`, `_rotate_size` and `_is_default_file_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOutputOptions {
    /// `filecount=`, `0..=1000`; 0 = no archive (the file is truncated at
    /// open and never rotated).
    pub file_count: u32,
    /// `filesize=` in bytes; 0 = no size rotation.
    pub file_size: u64,
    /// `filecount=` was not given (a FIFO is then never rotated).
    pub default_count: bool,
}

impl Default for FileOutputOptions {
    fn default() -> Self {
        Self {
            file_count: LOG_FILE_DEFAULT_COUNT,
            file_size: LOG_FILE_DEFAULT_SIZE,
            default_count: true,
        }
    }
}

/// gci i46/lg: HotSpot's `Arguments::atojulong` -- decimal or `0x` hex, with
/// an optional `k` / `m` / `g` / `t` suffix (either case). `None` when
/// malformed or above `u64::MAX`. Pure.
fn hotspot_atojulong(s: &str) -> Option<u64> {
    let (digits, shift) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 10),
        'm' | 'M' => (&s[..s.len() - 1], 20),
        'g' | 'G' => (&s[..s.len() - 1], 30),
        't' | 'T' => (&s[..s.len() - 1], 40),
        _ => (s, 0),
    };
    let n = if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()?
    } else {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<u64>().ok()?
    };
    n.checked_mul(1u64 << shift)
}

/// gci i46/lg: a file output's options text (`filecount=5,filesize=10m`,
/// `foldmultilines=true`) as HotSpot 25's `LogFileOutput::parse_options`
/// reads it; empty = the defaults. `Err` carries HotSpot's error text (HotSpot
/// then refuses the `-Xlog` option; this VM keeps the defaults and warns).
/// Pure.
fn parse_file_output_options(s: &str) -> Result<FileOutputOptions, String> {
    let mut o = FileOutputOptions::default();
    if s.trim().is_empty() {
        return Ok(o);
    }
    for pos in s.split(',') {
        let Some((key, value)) = pos.split_once('=') else {
            return Err(format!("Invalid option '{pos}' for log file output."));
        };
        match key {
            "foldmultilines" => {
                if value != "true" && value != "false" {
                    return Err(format!("Invalid option: {key} must be 'true' or 'false'."));
                }
            }
            "filecount" => {
                let n = if value.bytes().next().is_some_and(|b| b.is_ascii_digit())
                    && value.bytes().all(|b| b.is_ascii_digit())
                {
                    value.parse::<u64>().unwrap_or(u64::MAX)
                } else {
                    u64::MAX
                };
                if n > u64::from(LOG_FILE_MAX_COUNT) {
                    return Err(format!(
                        "Invalid option: filecount must be in range [0, {LOG_FILE_MAX_COUNT}]"
                    ));
                }
                o.file_count = n as u32;
                o.default_count = false;
            }
            "filesize" => match hotspot_atojulong(value) {
                Some(n) => o.file_size = n,
                None => {
                    return Err(format!(
                        "Invalid option: filesize must be in range [0, {}]",
                        u64::MAX
                    ))
                }
            },
            _ => return Err(format!("Invalid option '{key}' for log file output.")),
        }
    }
    Ok(o)
}

/// gci i46/lg: HotSpot's `number_of_digits(filecount - 1)`, the zero-padding
/// of an archive index. Pure.
fn log_archive_digits(file_count: u32) -> usize {
    let top = file_count.saturating_sub(1);
    if top < 10 {
        1
    } else if top < 100 {
        2
    } else {
        3
    }
}

/// gci i46/lg: `<path>.<index>`, the index zero-padded
/// ([`log_archive_digits`]). Pure.
fn log_archive_name(path: &Path, index: u32, file_count: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index:0width$}", width = log_archive_digits(file_count)));
    PathBuf::from(name)
}

/// gci i46/lg: HotSpot's `next_file_number` -- the first index in
/// `0..file_count` whose archive does not exist, else the index of the
/// oldest archive (by modification time; the first on a tie or when the
/// time is unknown). `None` when an archive name exists and is not a
/// regular file (HotSpot refuses the output; this VM then appends).
fn next_log_archive(path: &Path, file_count: u32) -> Option<u32> {
    let mut oldest: Option<(u32, Option<SystemTime>)> = None;
    for i in 0..file_count {
        let meta = match std::fs::metadata(log_archive_name(path, i, file_count)) {
            Ok(m) => m,
            Err(_) => return Some(i),
        };
        if !meta.is_file() {
            return None;
        }
        let modified = meta.modified().ok();
        let older = match oldest {
            None => true,
            Some((_, Some(best))) => modified.is_some_and(|m| m < best),
            Some((_, None)) => false,
        };
        if older {
            oldest = Some((i, modified));
        }
    }
    oldest.map(|(i, _)| i)
}

/// gci i46/lg: HotSpot's `LogFileOutput::archive` -- remove a stale
/// `<path>.<index>`, then rename `path` to it. A failed rename is reported
/// on stderr in HotSpot's words and the file is then appended to.
fn archive_log_file(path: &Path, index: u32, file_count: u32) {
    let archive = log_archive_name(path, index, file_count);
    let _ = std::fs::remove_file(&archive);
    if let Err(e) = std::fs::rename(path, &archive) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "Could not rename log file '{}' to '{}' ({e}).",
            path.display(),
            archive.display()
        );
    }
}

/// gci i46/lg: an open file output and its rotation state (HotSpot's
/// `_current_file`, `_current_size`).
struct LogFile {
    /// `None` between a rotation's close and its reopen, or after a reopen
    /// failed (the next line retries).
    file: Option<File>,
    /// Bytes written since this file was opened.
    written: u64,
    /// The archive index the next rotation uses.
    next_archive: u32,
    /// The effective `filecount` (0: never rotate).
    file_count: u32,
    /// `filesize` (0: never rotate by size).
    file_size: u64,
}

impl LogFile {
    /// Write `line` and a newline; then rotate if the file reached its size.
    fn write_line(&mut self, path: &Path, line: &str) {
        if self.file.is_none() {
            self.file = OpenOptions::new().create(true).append(true).open(path).ok();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if writeln!(file, "{line}").is_err() {
            return;
        }
        self.written = self.written.saturating_add(line.len() as u64 + 1);
        if self.file_count > 0 && self.file_size > 0 && self.written >= self.file_size {
            // Closed before the rename (Windows cannot rename an open file).
            self.file = None;
            archive_log_file(path, self.next_archive, self.file_count);
            self.next_archive = (self.next_archive + 1) % self.file_count;
            self.written = 0;
            self.file = OpenOptions::new().create(true).append(true).open(path).ok();
        }
    }
}

/// gci i46/lg: open the file output `path`. `options` `None` (the kill
/// switch, or a path no option defined): `create + append`, never rotated,
/// as before. `Some`: HotSpot 25's `LogFileOutput::initialize` -- an existing
/// regular file is archived ([`next_log_archive`], [`archive_log_file`]) when
/// `filecount` is above 0 and truncated when it is 0; an existing file that
/// is not regular (a FIFO, a device) is opened as it is and never rotated
/// (HotSpot does that for a FIFO under the default count and refuses the
/// output otherwise).
fn open_log_file(path: &Path, options: Option<FileOutputOptions>) -> std::io::Result<LogFile> {
    let Some(o) = options else {
        return Ok(LogFile {
            file: Some(OpenOptions::new().create(true).append(true).open(path)?),
            written: 0,
            next_archive: 0,
            file_count: 0,
            file_size: 0,
        });
    };
    let meta = std::fs::metadata(path).ok();
    let regular = meta.as_ref().is_some_and(|m| m.is_file());
    let mut file_count = if meta.is_some() && !regular { 0 } else { o.file_count };
    let mut next_archive = 0;
    if file_count > 0 && regular {
        match next_log_archive(path, file_count) {
            Some(n) => {
                archive_log_file(path, n, file_count);
                next_archive = (n + 1) % file_count;
            }
            // An archive slot that is not a regular file: append, unrotated.
            None => file_count = 0,
        }
    }
    // `filecount=0`: HotSpot opens in append mode and then `ftruncate`s a
    // regular file; opening it for writing with truncation is the same file.
    let file = if o.file_count == 0 && regular {
        OpenOptions::new().write(true).truncate(true).open(path)?
    } else {
        OpenOptions::new().create(true).append(true).open(path)?
    };
    Ok(LogFile {
        file: Some(file),
        written: 0,
        next_archive,
        file_count,
        file_size: o.file_size,
    })
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
// HotSpot's refusal of a bad `-Xlog` selection (gci i27/ob, g3x item 11)
// ---------------------------------------------------------------------------

/// HotSpot 25's log tag names (`logTag.hpp`, `LOG_TAG_LIST`, in its sorted
/// order), the universe [`xlog_option_refusal`] checks a selection's tags
/// against. Deliberately GENEROUS: a name HotSpot does not have only makes
/// this VM keep its older tolerance (warn and run) for that name, while a
/// name HotSpot has and this list lacked would refuse a command line HotSpot
/// runs -- so when in doubt a name is listed. The order matters only for the
/// `Did you mean` suggestion ([`hotspot_tag_fuzzy_match`]).
const HOTSPOT_LOG_TAGS: &[&str] = &[
    "abstractmethoderror",
    "addresses",
    "age",
    "aging",
    "alloc",
    "annotation",
    "aot",
    "arguments",
    "array",
    "attach",
    "barrier",
    "blocks",
    "bot",
    "breakpoint",
    "bytecode",
    "cause",
    "cds",
    "census",
    "class",
    "classhisto",
    "cleanup",
    "codecache",
    "codestrings",
    "commit",
    "compaction",
    "compilation",
    "condy",
    "constantpool",
    "constraints",
    "container",
    "continuations",
    "coops",
    "cpu",
    "cset",
    "data",
    "datacreation",
    "dcmd",
    "decoder",
    "defaultmethods",
    "deoptimization",
    "dependencies",
    "director",
    "downcall",
    "dump",
    "dynamic",
    "emergency",
    "ergo",
    "event",
    "exceptions",
    "exit",
    "finalizer",
    "fingerprint",
    "foreign",
    "free",
    "freelist",
    "gc",
    "generate",
    "handshake",
    "hashtables",
    "heap",
    "heapdump",
    "heapsampling",
    "humongous",
    "ihop",
    "iklass",
    "indy",
    "init",
    "inlining",
    "install",
    "interpreter",
    "itables",
    "jfr",
    "jit",
    "jmethod",
    "jni",
    "jvmci",
    "jvmti",
    "lambda",
    "library",
    "link",
    "liveness",
    "load",
    "loader",
    "lockstack",
    "logging",
    "loom",
    "malloc",
    "map",
    "mark",
    "marking",
    "membername",
    "memops",
    "metadata",
    "metaspace",
    "methodcomparator",
    "methodhandles",
    "methodtrace",
    "mirror",
    "mmu",
    "module",
    "monitorinflation",
    "monitormismatch",
    "monitortable",
    "native",
    "nestmates",
    "nmethod",
    "nmt",
    "normalize",
    "numa",
    "objecttagging",
    "obsolete",
    "oldobject",
    "oom",
    "oopmap",
    "oops",
    "oopstorage",
    "os",
    "owner",
    "page",
    "pagesize",
    "parser",
    "patch",
    "path",
    "perf",
    "periodic",
    "phases",
    "placeholders",
    "plab",
    "preorder",
    "preview",
    "promotion",
    "protectiondomain",
    "ptrqueue",
    "purge",
    "record",
    "redefine",
    "ref",
    "refine",
    "region",
    "reloc",
    "relocate",
    "remap",
    "remset",
    "resolve",
    "safepoint",
    "sampling",
    "scavenge",
    "sealed",
    "setting",
    "shutdown",
    "signal",
    "smr",
    "stackbarrier",
    "stackchunk",
    "stackmap",
    "stacktrace",
    "stackwalk",
    "start",
    "startup",
    "startuptime",
    "state",
    "stats",
    "streaming",
    "stringdedup",
    "stringtable",
    "subclass",
    "survivor",
    "suspend",
    "sweep",
    "symboltable",
    "system",
    "table",
    "task",
    "thread",
    "throttle",
    "timer",
    "tlab",
    "tracking",
    "training",
    "trimnative",
    "uncommit",
    "unload",
    "unmap",
    "unshareable",
    "upcall",
    "update",
    "valuebasedclasses",
    "verification",
    "verify",
    "vmmutex",
    "vmoperation",
    "vmthread",
    "vtables",
    "vtablestubs",
    "workgang",
];

/// HotSpot 25's level names (`LogLevel::_name`, `Off` first); `from_string`
/// matches any of them, `fuzzy_match` suggests from index 1 on.
const HOTSPOT_LOG_LEVELS: [&str; 6] = ["off", "trace", "debug", "info", "warning", "error"];

/// HotSpot's `StringUtils::similarity`: twice the number of `a`'s bigrams
/// found anywhere in `b` (each bigram of `a` counted once), over the summed
/// lengths. Byte-wise and case-sensitive, as there. Pure.
fn hotspot_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let total = a.len() + b.len();
    let hit = a
        .windows(2)
        .filter(|pa| b.windows(2).any(|pb| pa == &pb))
        .count();
    2.0 * hit as f64 / total as f64
}

/// HotSpot's `LogTag::fuzzy_match`: the tag whose name scores at least 0.5
/// against `s` (the LAST one at the best score: the loop keeps a tag whose
/// score is `>=` the best so far). Pure.
fn hotspot_tag_fuzzy_match(s: &str) -> Option<&'static str> {
    let mut best = 0.5;
    let mut found = None;
    for &tag in HOTSPOT_LOG_TAGS {
        let score = hotspot_similarity(tag, s);
        if score >= best {
            best = score;
            found = Some(tag);
        }
    }
    found
}

/// HotSpot's `LogLevel::fuzzy_match` (threshold 0.4, `Off` never suggested).
/// Pure.
fn hotspot_level_fuzzy_match(s: &str) -> Option<&'static str> {
    let mut best = 0.4;
    let mut found = None;
    for &level in &HOTSPOT_LOG_LEVELS[1..] {
        let score = hotspot_similarity(s, level);
        if score >= best {
            best = score;
            found = Some(level);
        }
    }
    found
}

/// Why HotSpot 25 refuses ONE `-Xlog` option (`option` is the text after
/// `-Xlog:`), as the lines it writes to its error stream
/// (`LogSelection::parse`: `Invalid level '<l>' in log selection.` /
/// `Invalid tag '<t>' in log selection.`, each with HotSpot's ` Did you mean
/// '<x>'?` when one scores high enough); `None` when HotSpot accepts the
/// option as far as this VM can tell. Only the two refusals this VM can state
/// exactly are modelled -- an unknown level and an unknown tag name; every
/// other shape (an output or decorator error, `all` combined with a tag, a
/// misplaced `*`, a duplicate tag, an empty tag) answers `None` and keeps the
/// VM's tolerant parse. `help`, `disable` and `async[...]` are not
/// selections. Pure.
///
/// HotSpot parses the selections before the output and the decorators, the
/// level of a selection before its tags, and stops at the first bad
/// selection -- so does this.
pub fn xlog_option_refusal(option: &str) -> Option<Vec<String>> {
    let option = option.trim();
    if option.eq_ignore_ascii_case("help")
        || option.eq_ignore_ascii_case("disable")
        || option.starts_with("async")
    {
        return None;
    }
    let fields = split_option_fields(option);
    let what = fields.first().map_or("", |f| f.as_str());
    if what.is_empty() {
        return None;
    }
    for sel in what.split(',') {
        if sel.is_empty() {
            return None;
        }
        let (tags, level) = match sel.split_once('=') {
            Some((tags, level)) => (tags, Some(level)),
            None => (sel, None),
        };
        if let Some(level) = level {
            if !HOTSPOT_LOG_LEVELS.iter().any(|l| l.eq_ignore_ascii_case(level)) {
                let mut line = format!("Invalid level '{level}' in log selection.");
                if let Some(m) = hotspot_level_fuzzy_match(level) {
                    line.push_str(&format!(" Did you mean '{m}'?"));
                }
                return Some(vec![line]);
            }
        }
        if tags.eq_ignore_ascii_case("all") || tags == "*" {
            continue;
        }
        let names: Vec<&str> = tags.split('+').collect();
        for (i, &raw) in names.iter().enumerate() {
            let last = i + 1 == names.len();
            let name = if last {
                raw.strip_suffix('*').unwrap_or(raw)
            } else {
                raw
            };
            if name.is_empty() || name.contains('*') || name.eq_ignore_ascii_case("all") {
                // A shape this VM does not model exactly: keep the tolerant parse.
                return None;
            }
            if !HOTSPOT_LOG_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                let mut line = format!("Invalid tag '{name}' in log selection.");
                if let Some(m) = hotspot_tag_fuzzy_match(name) {
                    line.push_str(&format!(" Did you mean '{m}'?"));
                }
                return Some(vec![line]);
            }
        }
    }
    stream_output_options_refusal(&fields)
}

/// gci i47w/j46 (`lg46_i_stdout_options`): HotSpot 25 parses output options
/// given to `stdout` / `stderr` (`LogFileStreamOutput`) and refuses every key
/// but `foldmultilines`, and an option with no `=`: `Invalid option '<key>'
/// for log output (<stdout|stderr>).`, then the whole `-Xlog` option (as
/// [`xlog_option_refusal`]). `fields` are the option's
/// [`split_option_fields`]. A `foldmultilines` value, a file output's options
/// and an output given by index (`#0`) are not modelled (`None`: the tolerant
/// parse stays). Pure.
fn stream_output_options_refusal(fields: &[String]) -> Option<Vec<String>> {
    let output = fields.get(1).map(|f| f.trim())?;
    if output != "stdout" && output != "stderr" {
        return None;
    }
    let options = fields.get(3..).map_or(String::new(), |rest| rest.join(":"));
    if options.trim().is_empty() {
        return None;
    }
    for pos in options.split(',') {
        let key = match pos.split_once('=') {
            Some((key, _)) => key,
            None => pos,
        };
        if pos.contains('=') && key == "foldmultilines" {
            continue;
        }
        return Some(vec![format!("Invalid option '{key}' for log output ({output}).")]);
    }
    None
}

/// gci i28/dg (g3x item 11 residual): a selection's tags as HotSpot's
/// `LogSelection::parse` reads them -- each `+`-separated name looked up
/// case-insensitively in [`HOTSPOT_LOG_TAGS`] (the canonical lowercase name
/// kept, in the order given) and a trailing `*` as the wildcard. `None` for a
/// shape this VM does not model exactly (an unknown name, an empty name, a
/// duplicate, `all`, a misplaced `*`): the caller keeps its older handling.
/// Pure.
fn hotspot_selection_tags(tags_str: &str) -> Option<(Vec<&'static str>, bool)> {
    let s = tags_str.trim();
    let (body, wildcard) = match s.strip_suffix('*') {
        Some(b) => (b, true),
        None => (s, false),
    };
    if body.is_empty() || body.contains('*') {
        return None;
    }
    let mut out: Vec<&'static str> = Vec::new();
    for raw in body.split('+') {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("all") {
            return None;
        }
        let canon = HOTSPOT_LOG_TAGS
            .iter()
            .copied()
            .find(|t| t.eq_ignore_ascii_case(raw))?;
        if out.contains(&canon) {
            return None;
        }
        out.push(canon);
    }
    Some((out, wildcard))
}

/// gci i28/dg: the tag sets this VM logs to, each as its HotSpot tag names in
/// HotSpot's order (`gc,heap,exit` -> `["gc", "heap", "exit"]`), in
/// [`LogTag::all`]'s order -- the stand-in for HotSpot's `LogTagSet` list.
/// A set naming a tag HotSpot does not have (this VM's invented `threading`,
/// `modules`) is left out: HotSpot could never suggest or select it.
fn xlog_tag_set_universe() -> Vec<(LogTag, Vec<&'static str>)> {
    LogTag::all()
        .iter()
        .map(|t| (*t, t.as_str().split(',').collect::<Vec<&'static str>>()))
        .filter(|(_, names)| names.iter().all(|n| HOTSPOT_LOG_TAGS.contains(n)))
        .collect()
}

/// HotSpot's `LogSelection::selects`: a wildcard selection takes every tag set
/// containing all its tags, a plain one only the tag set of exactly its tags
/// (in any order). Pure.
fn hotspot_selection_selects(tags: &[&str], wildcard: bool, ts: &[&str]) -> bool {
    (wildcard || ts.len() == tags.len()) && tags.iter().all(|t| ts.contains(t))
}

/// HotSpot's `LogSelection::similarity`: the Soerensen-Dice coefficient of two
/// tag lists. Pure.
fn hotspot_selection_similarity(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let hit = a.iter().filter(|t| b.contains(t)).count();
    2.0 * hit as f64 / (a.len() + b.len()) as f64
}

/// HotSpot's `LogSelection::describe_tags`: `gc+heap`, `gc+heap*`.
fn hotspot_selection_describe(tags: &[&str], wildcard: bool) -> String {
    let mut s = tags.join("+");
    if wildcard {
        s.push('*');
    }
    s
}

/// gci i28/dg (g3x item 11 residual): HotSpot 25's
/// `LogSelectionList::verify_selections` text for a selection whose names are
/// all HotSpot tags but which selects no tag set of `universe`: `No tag set
/// matches selection: <tags>. ` followed, when any qualifies, by `Did you mean
/// any of the following?` and ` <suggestion>` each (`suggest_similar_matching`:
/// the selection with a wildcard added when that matches something, then each
/// tag set -- as a wildcard unless that selects only itself -- scoring at
/// least 0.3, at most 10, a better one replacing the least similar). `None`
/// when the selection matches a tag set or is not modelled. HotSpot walks ITS
/// tag-set list; this VM's is smaller and in another order, so the suggestion
/// tail is never byte-identical (rows strip it); the head is. Pure.
fn xlog_no_tag_set_match_line(
    tags_str: &str,
    universe: &[(LogTag, Vec<&'static str>)],
) -> Option<String> {
    const SIMILARITY_REQUIREMENT: f64 = 0.3;
    const SUGGESTIONS_CAP: usize = 10;
    let (tags, wildcard) = hotspot_selection_tags(tags_str)?;
    let selected = |t: &[&str], w: bool| {
        universe
            .iter()
            .filter(|(_, ts)| hotspot_selection_selects(t, w, ts))
            .count()
    };
    if selected(tags.as_slice(), wildcard) > 0 {
        return None;
    }
    let mut suggestions: Vec<(Vec<&'static str>, bool)> = Vec::new();
    if !wildcard && selected(tags.as_slice(), true) > 0 {
        suggestions.push((tags.clone(), true));
    }
    for (_, ts) in universe {
        let w = selected(ts.as_slice(), true) != 1;
        let score = hotspot_selection_similarity(&tags, ts);
        if score < SIMILARITY_REQUIREMENT {
            continue;
        }
        if suggestions.len() < SUGGESTIONS_CAP {
            suggestions.push((ts.clone(), w));
            continue;
        }
        let mut min = 1.0;
        let mut pos = None;
        for (i, (s, _)) in suggestions.iter().enumerate() {
            let sc = hotspot_selection_similarity(&tags, s);
            if sc < min {
                min = sc;
                pos = Some(i);
            }
        }
        if let Some(pos) = pos {
            if score > min {
                suggestions[pos] = (ts.clone(), w);
            }
        }
    }
    let mut line = format!(
        "No tag set matches selection: {}. ",
        hotspot_selection_describe(&tags, wildcard)
    );
    if !suggestions.is_empty() {
        line.push_str("Did you mean any of the following?");
        for (s, w) in &suggestions {
            line.push(' ');
            line.push_str(&hotspot_selection_describe(s, *w));
        }
    }
    Some(line)
}

/// gci i28/dg: the tag sets of `universe` a selection HotSpot can read
/// ([`hotspot_selection_tags`]) selects -- so `heap+gc` selects `gc,heap` as
/// on HotSpot (tag order is free there), where [`LogTag::from_str`] knows only
/// its own spellings. `None` when the selection is not modelled or selects
/// nothing. Pure.
fn hotspot_selection_tag_sets(
    tags_str: &str,
    universe: &[(LogTag, Vec<&'static str>)],
) -> Option<Vec<LogTag>> {
    let (tags, wildcard) = hotspot_selection_tags(tags_str)?;
    let out: Vec<LogTag> = universe
        .iter()
        .filter(|(_, ts)| hotspot_selection_selects(&tags, wildcard, ts))
        .map(|(t, _)| *t)
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// gci i28/dg: a `verify_selections` warning as HotSpot 25 prints it on its
/// default output (`all=warning:stdout`, decorators `uptime,level,tags`):
/// `[<uptime>s][warning][logging] <text>`. Pure.
fn xlog_logging_warning_line(uptime_secs: f64, text: &str) -> String {
    format!("[{uptime_secs:.3}s][warning][logging] {text}")
}

/// gci i27/ob (g3x item 11): does the launcher refuse a bad `-Xlog` selection
/// at startup as HotSpot 25 does (`[error][logging]` line on stdout, `Invalid
/// -Xlog option ...` and the launcher's two lines on stderr, exit 1)? Default
/// ON, `CRATONVM_XLOG_REFUSE_INVALID_SELECTION=0` restores the warn-and-run
/// tolerance of gen r5w3/obs7. Read once by the launcher, before the VM.
pub fn xlog_refuse_invalid_selection_enabled() -> bool {
    true
}

/// The full HotSpot 25 refusal for the launcher's `-Xlog` options, in order:
/// the first option HotSpot refuses gives `(stdout_lines, stderr_lines)`,
/// stdout carrying the `[<uptime>][error][logging] ...` lines (the default
/// `all=warning:stdout` output with its `uptime,level,tags` decorators; the
/// uptime is `uptime_secs`) and stderr `Invalid -Xlog option '-Xlog:<o>', see
/// error log for details.` and the launcher's `Error: Could not create the
/// Java Virtual Machine.` / `Error: A fatal exception has occurred. Program
/// will exit.`. An earlier `disable` option has turned the stdout output off
/// (HotSpot's `-Xlog:disable` also silences warnings and errors), so then
/// only the stderr lines remain. `None` when every option is accepted. Pure.
pub fn xlog_refusal_lines(
    options: &[String],
    uptime_secs: f64,
) -> Option<(Vec<String>, Vec<String>)> {
    let mut stdout_on = true;
    for option in options {
        if option.trim().eq_ignore_ascii_case("disable") {
            stdout_on = false;
            continue;
        }
        let Some(errors) = xlog_option_refusal(option) else {
            continue;
        };
        let out = if stdout_on {
            errors
                .iter()
                .map(|e| format!("[{uptime_secs:.3}s][error][logging] {e}"))
                .collect()
        } else {
            Vec::new()
        };
        let err = vec![
            format!("Invalid -Xlog option '-Xlog:{option}', see error log for details."),
            "Error: Could not create the Java Virtual Machine.".to_string(),
            "Error: A fatal exception has occurred. Program will exit.".to_string(),
        ];
        return Some((out, err));
    }
    None
}

// ---------------------------------------------------------------------------
// UnifiedLogger
// ---------------------------------------------------------------------------

/// The unified logging engine. Holds all active rules and handles message
/// routing and formatting.
pub struct UnifiedLogger {
    rules: Vec<LogRule>,
    /// JFR enables startup info on the default stdout unless -Xlog:disable
    /// removed that output; explicit tag selections still take precedence.
    default_jfr_startup: bool,
    start_time: Instant,
    pid: u32,
    /// For the `hostname` decorator, resolved once.
    hostname: String,
    /// Open file handles for `File` outputs, keyed by path for O(1) lookup.
    /// gci i46/lg: with each file's rotation state ([`LogFile`]).
    file_handles: Mutex<HashMap<PathBuf, LogFile>>,
    /// Per output, the widest value each decorator has printed so far: HotSpot
    /// pads every decoration to it (`[%-*s]`), so columns line up once a wide
    /// tag set such as `gc,metaspace` has appeared. gen r5w3/obs7.
    padding: Mutex<HashMap<LogOutput, [usize; DECORATOR_COUNT]>>,
    /// Selections that were skipped, and why, for the caller to report.
    warnings: Vec<String>,
    /// gci i28/dg: HotSpot's `No tag set matches selection: ...` texts
    /// ([`xlog_no_tag_set_match_line`]) of selections HotSpot accepts but
    /// that select nothing, in order; [`init_unified_logging`] prints them on
    /// stdout as `[<uptime>][warning][logging] ...`. A selection parsed after
    /// a `disable` adds none (its output is off on HotSpot too).
    no_match_warnings: Vec<String>,
    /// Claimed by the first of the launcher's exit arms to print the
    /// `gc,exit` heap summary (gcf f2/r): HotSpot prints it once, from
    /// `before_exit`, and both the normal-return and the `System.exit` arm
    /// reach the summary call.
    exit_summary_claimed: std::sync::atomic::AtomicBool,
    /// gcg g3/x: lines rendered INSIDE a pause (the reference-processing
    /// block, [`defer_xlog_reference_processing`]) and printed after it by
    /// the collection's own `-Xlog` lines ([`flush_deferred_gc_lines`]), so
    /// they land in HotSpot's order (after `gc,start`, before `gc,age` /
    /// `gc,heap`) and no I/O happens inside the pause. `(gc_id, tag, level,
    /// text)`; bounded by [`DEFERRED_GC_LINES_CAP`].
    deferred: Mutex<Vec<(u64, LogTag, LogLevel, String)>>,
    /// gci i45/sl: the `-Xlog:safepoint` lines' state -- the pause each
    /// initiator thread has open and the barrier figures the previous pause
    /// began with ([`safepoint_line_begin`]). A field of the logger, not a
    /// process static; empty unless `safepoint` is selected at info.
    safepoint: Mutex<SafepointLineState>,
    /// gci i46/lg (`gci-i45sl-xlog-file-output-appends-to-an-existing-log-FIXED-20261016`):
    /// each file output's options (`filecount`, `filesize`) from its first
    /// definition after the last `disable`, as HotSpot's `LogFileOutput`
    /// keeps them. Empty under `CRATONVM_XLOG_FILE_ROTATION=0`: every file
    /// then appends, unrotated, as before.
    file_options: HashMap<PathBuf, FileOutputOptions>,
}

/// gcg g3/x: the most lines [`UnifiedLogger::deferred`] holds. One
/// reference-processing block is at most 17 lines; a pause that defers and is
/// never flushed (a door without `gc_event_finish`) must not grow it.
const DEFERRED_GC_LINES_CAP: usize = 64;

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
        let mut no_match_warnings = Vec::new();
        let mut accepted_without_rules = false;
        let mut disabled = false;
        // gci i46/lg: HotSpot 25's file-output options and rotation
        // (`CRATONVM_XLOG_FILE_ROTATION`, default ON; off, the options are
        // ignored and every file output appends, as before).
        let rotation = xlog_file_rotation_enabled();
        let mut file_options: HashMap<PathBuf, FileOutputOptions> = HashMap::new();

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
                // gci i46/lg: and delete every file output, so a later
                // option defines its file (and its options) anew.
                file_options.clear();
                continue;
            }
            let mut option_no_match = Vec::new();
            let mut output_options = None;
            match Self::parse_option(option, &mut warnings, &mut option_no_match) {
                Ok((mut option_rules, output, options)) => {
                    rules.append(&mut option_rules);
                    output_options = Some((output, options));
                }
                Err(e) => warnings.push(format!("invalid -Xlog option '{option}': {e}")),
            }
            // gci i28/dg: HotSpot accepts a selection that matches no tag
            // set (a warning, not an error), so the spec is not "unusable".
            if !option_no_match.is_empty() {
                accepted_without_rules = true;
                if !disabled {
                    no_match_warnings.append(&mut option_no_match);
                }
            }
            // gci i46/lg: the output's options -- a file's first definition
            // takes them (`LogFileOutput::initialize`); on an output that
            // exists already (stdout, stderr, a file an earlier option
            // defined) HotSpot ignores them with a `logging` warning.
            if let (true, Some((output, options))) = (rotation, output_options) {
                let defined = match &output {
                    LogOutput::File(path) => file_options.contains_key(path),
                    LogOutput::Stdout | LogOutput::Stderr => true,
                };
                if defined {
                    if !options.trim().is_empty() && !disabled {
                        no_match_warnings.push(XLOG_EXISTING_OUTPUT_OPTIONS_IGNORED.to_string());
                    }
                } else if let LogOutput::File(path) = output {
                    let parsed = parse_file_output_options(&options).unwrap_or_else(|e| {
                        warnings.push(format!("-Xlog output options '{options}' ignored: {e}"));
                        FileOutputOptions::default()
                    });
                    file_options.insert(path, parsed);
                }
            }
        }

        if rules.is_empty() && !disabled && !accepted_without_rules {
            return Err(if warnings.is_empty() {
                "no valid rules in -Xlog spec".to_string()
            } else {
                warnings.join("; ")
            });
        }

        Ok(Self {
            default_jfr_startup: !disabled,
            rules,
            start_time: Instant::now(),
            pid: std::process::id(),
            hostname: host_name(),
            file_handles: Mutex::new(HashMap::new()),
            padding: Mutex::new(HashMap::new()),
            warnings,
            no_match_warnings,
            exit_summary_claimed: std::sync::atomic::AtomicBool::new(false),
            deferred: Mutex::new(Vec::new()),
            safepoint: Mutex::new(SafepointLineState::default()),
            file_options,
        })
    }

    /// Parse one option `what[:output[:decorators[:output-options]]]` into
    /// one rule per usable selection; a skipped selection is pushed onto
    /// `warnings`. An unusable output or decorator list rejects the option.
    ///
    /// gci i28/dg: a selection this VM's [`LogTag::from_str`] cannot read but
    /// HotSpot can ([`hotspot_selection_tags`]) selects the tag sets HotSpot's
    /// `LogSelection::selects` would (`heap+gc` = `gc,heap`); one that selects
    /// none pushes HotSpot's `No tag set matches selection: ...` text onto
    /// `no_match` instead of a warning, and does not make the option unusable.
    ///
    /// gci i46/lg: also returns the option's output and its output-options
    /// text (`filecount=5,filesize=10m`; empty when none), which
    /// [`Self::parse`] applies to a file's first definition.
    fn parse_option(
        s: &str,
        warnings: &mut Vec<String>,
        no_match: &mut Vec<String>,
    ) -> Result<(Vec<LogRule>, LogOutput, String), String> {
        let fields = split_option_fields(s);
        let what = fields.first().map_or("", |f| f.trim());
        let output = LogOutput::parse(fields.get(1).map_or("", String::as_str))?;
        let decorators = LogDecorators::parse(fields.get(2).map_or("", String::as_str))?;
        // fields[3..]: output options (`filecount=`, `filesize=`,
        // `foldmultilines=`), HotSpot's text after the third `:`. gci i46/lg:
        // a file output's `filecount` / `filesize` apply
        // ([`parse_file_output_options`]); `foldmultilines` is accepted.
        let options = fields.get(3..).map_or(String::new(), |rest| rest.join(":"));

        let mut rules = Vec::new();
        // An empty `what` selects every tag at info (as before).
        let selections: Vec<&str> = if what.is_empty() {
            vec![""]
        } else {
            what.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        };
        let mut universe: Option<Vec<(LogTag, Vec<&'static str>)>> = None;
        let mut option_no_match = 0usize;
        for sel in selections {
            let (tags_str, level_str) = sel.split_once('=').unwrap_or((sel, "info"));
            let level = LogLevel::from_str(level_str.trim());
            let tags = parse_tag_selector(tags_str).or_else(|e| {
                if level.is_err() {
                    return Err(e);
                }
                let u: &[(LogTag, Vec<&'static str>)] =
                    universe.get_or_insert_with(xlog_tag_set_universe);
                if let Some(sets) = hotspot_selection_tag_sets(tags_str, u) {
                    return Ok(sets);
                }
                if let Some(line) = xlog_no_tag_set_match_line(tags_str, u) {
                    no_match.push(line);
                    option_no_match += 1;
                    return Ok(Vec::new());
                }
                Err(e)
            });
            match (tags, level) {
                (Ok(tags), Ok(_)) if tags.is_empty() => {}
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
        if rules.is_empty() && option_no_match == 0 {
            return Err("no usable selection".to_string());
        }
        Ok((rules, output, options))
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
        self.log_at(tags, level, message, None);
    }

    /// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 7): [`Self::log`]
    /// with the time decorations (`uptime`, `time`, `utctime`, `timemillis`,
    /// `uptimemillis`, `timenanos`, `uptimenanos`) taken at `at` instead of
    /// now -- the line of an event that happened earlier and is written late,
    /// as a collection's `gc,start` line is (no I/O inside the pause).
    /// `None`: now, i.e. [`Self::log`].
    pub fn log_at(&self, tags: &[LogTag], level: LogLevel, message: &str, at: Option<Instant>) {
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
            let formatted = self.format_padded_at(tags, level, message, decorators, pad, at);

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

    /// gci i47w/xl47 (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item
    /// 2's residual): `message` as a line of HotSpot's default
    /// `all=warning:stdout` output -- decorated with STDOUT's decorators (those
    /// of the last option naming stdout: an `-Xlog:...:stdout:<decorators>`
    /// re-decorates the default output too, as HotSpot's one stdout output
    /// is; HotSpot's `uptime,level,tags` when no option names it), padded to
    /// and widening stdout's padding like every other line written there.
    /// Whether the default output takes the message is the caller's
    /// (HotSpot's selection rules). The padding lock is released before the
    /// caller writes; called at startup, single-threaded.
    fn default_stdout_line(&self, tags: &[LogTag], level: LogLevel, message: &str) -> String {
        let default = LogDecorators::default();
        let decorators = self
            .decorators_for(&LogOutput::Stdout)
            .unwrap_or(&default);
        let mut padding = match self.padding.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pad = padding
            .entry(LogOutput::Stdout)
            .or_insert([0; DECORATOR_COUNT]);
        self.format_padded_at(tags, level, message, decorators, pad, None)
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
        self.format_padded_at(tags, level, message, decorators, pad, None)
    }

    /// [`Self::format_padded`] with the time decorations taken at `at`
    /// ([`Self::log_at`]).
    fn format_padded_at(
        &self,
        tags: &[LogTag],
        level: LogLevel,
        message: &str,
        decorators: &LogDecorators,
        pad: &mut [usize; DECORATOR_COUNT],
        at: Option<Instant>,
    ) -> String {
        let on = decorators.in_order();
        let mut prefix = String::new();
        for (i, enabled) in on.iter().enumerate() {
            if !enabled {
                continue;
            }
            let value = self.decoration(i, tags, level, at);
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
    /// gci i30/ob: the time decorations at `at` (`None`: now).
    fn decoration(
        &self,
        i: usize,
        tags: &[LogTag],
        level: LogLevel,
        at: Option<Instant>,
    ) -> String {
        let (elapsed, wall) = decoration_instants(self.start_time, at);
        let since_epoch = wall
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        match i {
            0 | 1 => iso8601_utc(wall),
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
    ///
    /// gci i46/lg: the first open applies HotSpot 25's `LogFileOutput`
    /// initialisation ([`open_log_file`]: an existing file is archived, or
    /// truncated under `filecount=0`) when [`Self::open_file_outputs`] has not
    /// opened the file already, and a file that reaches its `filesize` is
    /// rotated after the line that reached it (`LogFileOutput::write_blocking`
    /// -> `rotate`).
    fn write_to_file(&self, path: &Path, line: &str) {
        let mut handles = match self.file_handles.lock() {
            Ok(h) => h,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Find or create the file handle (O(1) keyed lookup).
        let file = if handles.contains_key(path) {
            handles.get_mut(path).expect("present (just checked)")
        } else {
            let f = match open_log_file(path, self.file_options.get(path).copied()) {
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

        file.write_line(path, line);
    }

    /// gci i46/lg (`gci-i45sl-xlog-file-output-appends-to-an-existing-log-FIXED-20261016`):
    /// open every file output now, as HotSpot 25 does while it parses `-Xlog`
    /// (`LogFileOutput::initialize`): the archive / truncation of an existing
    /// file happens at VM start even when the run writes no line to it. A
    /// file that fails to open is left to the first write, which reports it
    /// as before. Called by [`init_unified_logging`]; a no-op under
    /// `CRATONVM_XLOG_FILE_ROTATION=0` (no file has options then).
    fn open_file_outputs(&self) {
        if self.file_options.is_empty() {
            return;
        }
        let mut handles = match self.file_handles.lock() {
            Ok(h) => h,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Map order: each file's archive names are its own, so the order
        // the files open in changes nothing.
        for (path, options) in &self.file_options {
            if handles.contains_key(path) {
                continue;
            }
            if let Ok(f) = open_log_file(path, Some(*options)) {
                handles.insert(path.clone(), f);
            }
        }
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

    /// gci i28/dg: HotSpot's `No tag set matches selection: ...` texts, in
    /// order (undecorated; [`init_unified_logging`] decorates them).
    pub fn no_match_warnings(&self) -> &[String] {
        &self.no_match_warnings
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

/// gci i30/ob (g3x item 7): `(uptime, wall clock)` of a line decorated at
/// `at` (`None`: now) for a logger started at `start`: the uptime is
/// `at - start` (0 for an `at` before the logger existed) and the wall clock
/// is now moved back by `now - at` (the monotonic distance; a wall-clock step
/// in between is not modelled).
fn decoration_instants(start: Instant, at: Option<Instant>) -> (std::time::Duration, SystemTime) {
    let now = Instant::now();
    let wall_now = SystemTime::now();
    match at {
        None => (now.saturating_duration_since(start), wall_now),
        Some(at) => {
            let back = now.saturating_duration_since(at);
            (
                at.saturating_duration_since(start),
                wall_now.checked_sub(back).unwrap_or(wall_now),
            )
        }
    }
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

/// The `-Xlog` logger of the VM that started last, installed by its
/// startup ([`init_unified_logging`] / [`reset_unified_logging`]).
///
/// gci i30/ob (`i28-dg-report.md` review item 4): it was a process `OnceLock`,
/// first writer wins, so a second VM in the process (`cratonvm-embed`'s
/// sequential VMs, a test that builds two) printed its own `-Xlog` warnings
/// and then silently kept the FIRST VM's rules and outputs -- or, started
/// without `-Xlog`, the first VM's logger. Each VM's startup now replaces it
/// (same declaration count: `per_vm_state_statics_ratchet`). Residual: two
/// VMs alive at once share the later one's logger (HotSpot has one VM per
/// process); a per-VM handle needs `SharedVm` at every log call (proposal
/// `gci-i30ob-proposal-per-vm-xlog-logger-handle-20261015`).
static UNIFIED_LOGGER: parking_lot::RwLock<Option<Arc<UnifiedLogger>>> =
    parking_lot::RwLock::new(None);

/// gci i30/ob: the installed logger, if any (one uncontended read lock and
/// one reference-count increment).
fn current_logger() -> Option<Arc<UnifiedLogger>> {
    UNIFIED_LOGGER.read().clone()
}

/// gci i30/ob: install `logger` as the process's `-Xlog` logger, replacing
/// any earlier VM's.
fn install_unified_logger(logger: Option<UnifiedLogger>) {
    *UNIFIED_LOGGER.write() = logger.map(Arc::new);
}

/// gci i30/ob (`i28-dg-report.md` review item 4): a VM started without
/// `-Xlog` has no logger, whatever an earlier VM in the process installed.
/// Called by VM startup when its configuration names no `-Xlog` spec.
pub fn reset_unified_logging() {
    install_unified_logger(None);
}

/// Initialize the unified logger from a `-Xlog` spec string.
///
/// Returns `Ok(())` on success, or an error string if parsing fails.
/// gci i30/ob: each call (one per VM startup) REPLACES the installed logger
/// (it used to be first-writer-wins for the process); a spec that fails to
/// parse leaves no logger. Skipped selections are reported on stderr in
/// HotSpot's warning shape (`[warning][logging] ...`), once.
pub fn init_unified_logging(spec: &str) -> Result<(), String> {
    let logger = match UnifiedLogger::parse(spec) {
        Ok(logger) => logger,
        Err(e) => {
            install_unified_logger(None);
            return Err(e);
        }
    };
    for w in logger.warnings() {
        let _ = writeln!(std::io::stderr().lock(), "[warning][logging] {w}");
    }
    // gci i28/dg (g3x item 11 residual): a selection HotSpot accepts but that
    // matches no tag set is HotSpot's `verify_selections` warning, written on
    // its default `all=warning:stdout` output -- STDOUT, decorated
    // `[<uptime>][warning][logging]` -- not the stderr shape above. Not
    // modelled: a user option that re-decorates stdout.
    let uptime = logger.start_time.elapsed().as_secs_f64();
    for w in logger.no_match_warnings() {
        let _ = writeln!(
            std::io::stdout().lock(),
            "{}",
            xlog_logging_warning_line(uptime, w)
        );
    }
    // gci i46/lg: HotSpot opens (and archives or truncates) every file output
    // while it parses `-Xlog`, before the VM runs a line of Java.
    logger.open_file_outputs();
    install_unified_logger(Some(logger));
    Ok(())
}

/// gci i47w/xl47: write `message` on stdout as a line of HotSpot's default
/// `all=warning:stdout` output, decorated by the installed logger's stdout
/// decorators ([`UnifiedLogger::default_stdout_line`]). `false` -- nothing
/// written -- when no logger is installed (no `-Xlog`, or a spec that did
/// not parse); the caller then prints HotSpot's default shape itself. The
/// caller has decided that the default output takes the message.
pub fn log_on_default_stdout(tags: &[LogTag], level: LogLevel, message: &str) -> bool {
    let Some(logger) = current_logger() else {
        return false;
    };
    let line = logger.default_stdout_line(tags, level, message);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    true
}

/// gcj j2/ob2 (`gci-i48-start-flight-recording-prints-no-jfr-startup-banner`,
/// `CRATONVM_JFR_STARTUP_BANNER`, opt-in): JFR's `[jfr,startup]` start-up
/// banner, one line per text, at info on stdout -- decorated and padded as the
/// installed logger's stdout lines ([`log_on_default_stdout`]), or, with no
/// `-Xlog` logger, in HotSpot's default stdout decorations
/// `[<uptime>s][info][jfr,startup] <text>` with `uptime_secs` (an empty text
/// keeps the blank after the decorations, as HotSpot's empty middle line
/// does). The caller decided that HotSpot prints the banner.
pub fn log_jfr_startup_lines(texts: &[String], uptime_secs: f64) {
    let logger = current_logger();
    for text in texts {
        if let Some(logger) = logger.as_deref() {
            logger.log(&[LogTag::JfrStartup], LogLevel::Info, text);
            // Selecting stderr/file does not remove HotSpot's default stdout.
            if logger.default_jfr_startup
                && logger.level_for(&LogOutput::Stdout, LogTag::JfrStartup).is_none() {
                log_on_default_stdout(&[LogTag::JfrStartup], LogLevel::Info, text);
            }
        } else {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "[{uptime_secs:.3}s][info][jfr,startup] {text}");
            let _ = out.flush();
        }
    }
}

/// A DCmdStart failure uses HotSpot's startup tag at error level, before
/// the launcher's initialization-failure lines. Honor explicit -Xlog routing.
pub fn log_jfr_startup_error(text: &str, uptime_secs: f64) {
    let logger = current_logger();
    if let Some(logger) = logger.as_deref() {
        logger.log(&[LogTag::JfrStartup], LogLevel::Error, text);
        if logger.default_jfr_startup
            && logger.level_for(&LogOutput::Stdout, LogTag::JfrStartup).is_none() {
            log_on_default_stdout(&[LogTag::JfrStartup], LogLevel::Error, text);
        }
    } else {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "[{uptime_secs:.3}s][error][jfr,startup] {text}");
        let _ = out.flush();
    }
}

/// zt t26/x21 (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`,
/// `zgc-zt13x10-review-residuals-*` item 30j): HotSpot's start-up
/// `log_warning(pagesize)` line ([`SHMEM_THP_NOT_ENABLED_WARNING`],
/// [`NO_LARGE_PAGES_WARNING`], [`THP_UNSUPPORTED_WARNING`]) through the
/// installed `-Xlog` logger: every output whose selection takes `pagesize` at
/// `warning`, plus HotSpot's default `all=warning:stdout` output unless an
/// option set `pagesize` on stdout (`-Xlog:pagesize=off`) or a `disable` turned
/// the default output off -- the rule [`log_jfr_startup_lines`] follows. The
/// stdout line is decorated and padded as the logger's stdout lines, on the
/// logger's clock. `false` -- nothing written -- when no logger is installed
/// (no `-Xlog`, or a spec that did not parse): the caller prints HotSpot's
/// default shape `[<uptime>s][warning][pagesize] <text>` itself.
pub fn log_pagesize_warning(text: &str) -> bool {
    log_warning_through_logger(LogTag::Pagesize, text)
}

/// [`log_pagesize_warning`]'s routing for any one-tag-set `log_warning(..)`
/// line: `false` when no logger is installed. j27 wave 8 (lane W8DSP).
fn log_warning_through_logger(tag: LogTag, text: &str) -> bool {
    let Some(logger) = current_logger() else {
        return false;
    };
    logger.log(&[tag], LogLevel::Warning, text);
    if logger.default_jfr_startup && logger.level_for(&LogOutput::Stdout, tag).is_none() {
        log_on_default_stdout(&[tag], LogLevel::Warning, text);
    }
    true
}

/// j27 wave 8 (lane W8DSP): a HotSpot `log_warning(<tag set>)` line written
/// while the VM runs (first user: `jni,resolve`, `jni_RegisterNatives`):
/// through the `-Xlog` logger as [`log_pagesize_warning`] routes it, or, with
/// no logger, on stdout in HotSpot's default shape
/// ([`warning_default_line`]) at `uptime_secs`.
pub fn log_warning_line(tag: LogTag, text: &str, uptime_secs: f64) {
    if log_warning_through_logger(tag, text) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", warning_default_line(uptime_secs, tag, text));
    let _ = out.flush();
}

/// HotSpot's default stdout shape (`uptime,level,tags`) of a warning line,
/// unpadded: `[<uptime>s][warning][<tags>] <text>`. Pure.
pub fn warning_default_line(uptime_secs: f64, tag: LogTag, text: &str) -> String {
    format!("[{uptime_secs:.3}s][warning][{}] {text}", tag.as_str())
}

/// zt t26/x21: HotSpot's default shape of a start-up `[warning][pagesize]`
/// line (`uptime,level,tags` on stdout), for a VM without an `-Xlog` logger.
/// Pure.
pub fn pagesize_warning_default_line(uptime_secs: f64, text: &str) -> String {
    format!("[{uptime_secs:.3}s][warning][pagesize] {text}")
}

/// zt t26/x21: the `pagesize` tag set is selectable, and the default-stdout
/// rule of [`log_pagesize_warning`] reads as HotSpot's `all=warning:stdout`:
/// an option naming another tag set or another output leaves it, `disable`
/// and `pagesize=off` take it away.
#[cfg(test)]
mod zt26x21_pagesize_tag_tests;

/// Log a message through the global unified logger.
///
/// This is a no-op if the logger has not been initialized or if the
/// message's tags/level do not match any active rule.
pub fn log_unified(tags: &[LogTag], level: LogLevel, message: &str) {
    log_unified_at(tags, level, message, None);
}

/// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 7):
/// [`log_unified`] with the time decorations taken at `at`
/// ([`UnifiedLogger::log_at`]) -- a collection's `gc,start` line, written
/// after the pause (no I/O inside it) but decorated with the pause's start,
/// where HotSpot writes it. `None`: now.
pub fn log_unified_at(tags: &[LogTag], level: LogLevel, message: &str, at: Option<Instant>) {
    let xlog = current_logger();
    if let Some(logger) = xlog.as_deref() {
        if logger.is_enabled(tags, level) {
            logger.log_at(tags, level, message, at);
        }
    }
    // gci i1/ob (`gch-h4fl-young-sizing-and-xlog-review-findings` item 6): the
    // JVMTI verbose output is HotSpot's stdout output with its level raised
    // (`LogConfiguration::configure_stdout`), so it takes the line as well
    // unless the `-Xlog` logger already printed it on stdout. It used to be
    // skipped whenever `-Xlog` printed the line anywhere (a file, stderr).
    if VERBOSE_FLAGS.load(std::sync::atomic::Ordering::Relaxed) != 0
        && verbose_output_also_logs(xlog.as_deref(), tags, level)
    {
        if let Some(extra) = VERBOSE_LOGGER.read().as_ref() {
            extra.log_at(tags, level, message, at);
        }
    }
}

/// gci i1/ob: does the JVMTI verbose logger (always `stdout`) still have to
/// print a line the `-Xlog` logger `xlog` has had? Only when `xlog` did not
/// print it on `stdout` itself: one output, one line, as on HotSpot.
fn verbose_output_also_logs(
    xlog: Option<&UnifiedLogger>,
    tags: &[LogTag],
    level: LogLevel,
) -> bool {
    !xlog.is_some_and(|l| l.enabled_on(&LogOutput::Stdout, tags, level))
}

/// JVMTI `SetVerboseFlag`'s output (interpreter round i2 wave 63, lane L1):
/// HotSpot's `LogConfiguration::configure_stdout` for `gc` (exactly) at
/// info, and for `class+load` and `class+unload` at info, while an agent has
/// the flag on. A second logger beside the `-Xlog` one, which is built once
/// at startup; [`VERBOSE_FLAGS`] keeps the cost of the unset case one
/// relaxed load.
static VERBOSE_LOGGER: parking_lot::RwLock<Option<UnifiedLogger>> = parking_lot::RwLock::new(None);
/// The bits of [`set_jvmti_verbose`]: 1 `gc`, 2 `class`.
static VERBOSE_FLAGS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// JVMTI `SetVerboseFlag(JVMTI_VERBOSE_GC | JVMTI_VERBOSE_CLASS, value)`:
/// `gc` true for the `gc` flag, false for the `class` one.
pub fn set_jvmti_verbose(gc: bool, on: bool) {
    use std::sync::atomic::Ordering;
    let bit = if gc { 1 } else { 2 };
    let flags = if on {
        VERBOSE_FLAGS.load(Ordering::Relaxed) | bit
    } else {
        VERBOSE_FLAGS.load(Ordering::Relaxed) & !bit
    };
    let mut spec = Vec::new();
    if flags & 1 != 0 {
        spec.push("gc=info");
    }
    if flags & 2 != 0 {
        spec.push("class+load=info,class+unload=info");
    }
    let logger = if spec.is_empty() {
        None
    } else {
        UnifiedLogger::parse(&format!("{}:stdout", spec.join(","))).ok()
    };
    *VERBOSE_LOGGER.write() = logger;
    VERBOSE_FLAGS.store(flags, Ordering::Relaxed);
}

/// Check whether a message with the given tags and level would be emitted.
///
/// Returns `false` if the global logger is not initialized.
pub fn is_unified_logging_enabled(tags: &[LogTag], level: LogLevel) -> bool {
    UNIFIED_LOGGER
        .read()
        .as_ref()
        .map_or(false, |l| l.is_enabled(tags, level))
        || (VERBOSE_FLAGS.load(std::sync::atomic::Ordering::Relaxed) != 0
            && VERBOSE_LOGGER
                .read()
                .as_ref()
                .is_some_and(|l| l.is_enabled(tags, level)))
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
    /// HotSpot's `CollectedHeap::name()` of the collector shape:
    /// [`GC_NAME_SERIAL`] for the Generational backend, [`GC_NAME_G1`],
    /// [`GC_NAME_ZGC`].
    pub collector: &'a str,
    /// The VM version (`java.vm.version`).
    pub version: &'a str,
    /// `os::processor_count()` / `os::active_processor_count()`.
    pub cpus_total: usize,
    pub cpus_available: usize,
    /// Physical memory, when known.
    pub memory_bytes: Option<u64>,
    /// `MinHeapSize` / `InitialHeapSize` / `MaxHeapSize` in bytes. gcg g3/x:
    /// `heap_min` is HotSpot's `MinHeapSize` ([`hotspot_min_heap_size`]),
    /// not the initial size (the two printed the same figure).
    pub heap_min: usize,
    pub heap_initial: usize,
    pub heap_max: usize,
    /// The value of HotSpot's `Compressed Oops:` line
    /// ([`compressed_oops_mode`]): `Disabled`, or `Enabled (<mode>)`. gcg g3/x.
    pub compressed_oops: &'a str,
    /// G1's region size in bytes (`Heap Region Size:`); `None` elsewhere.
    pub region_size: Option<usize>,
    /// zr z1/tr: ZGC's `-XX:SoftMaxHeapSize` as the heap applied it
    /// (`Soft Max Capacity:`); `None` = the max capacity, HotSpot's default.
    pub soft_max: Option<usize>,
    /// zr z1/tr: ZGC's uncommit policy `(ZUncommit, ZUncommitDelay seconds)`
    /// for the `Uncommit:` / `Uncommit Delay:` lines; `None` elsewhere.
    pub zgc_uncommit: Option<(bool, u64)>,
    /// zr z3/mt: `-XX:ParallelGCThreads` as ZGC applied it, for the
    /// `Runtime Workers:` line; `None` when the flag was not given (or off
    /// ZGC). See [`zgc_init_lines`] for why only an explicit value is printed.
    pub zgc_runtime_workers: Option<usize>,
    /// zki (page `zgc-zt23hunt4-*` item 4): the ZGC heap's uncommit probe on
    /// explicit hugetlbfs pages was refused: `Uncommit: Implicitly Disabled
    /// (Not supported by operating system)`.
    pub zgc_uncommit_unsupported: bool,
    /// zt t23/hunt4: `-XX:+AlwaysPreTouch` is in force (the heap touches what
    /// it commits), for the `Pre-touch:` line. The ZGC heap applies the option
    /// itself (`ZgcRealHeap::set_always_pre_touch`); zt t29/g1: G1 and
    /// Generational through `VmHeap::set_always_pre_touch_non_zgc`.
    pub pre_touch: bool,
    /// zt t24/hunt7: ZGC's `Large Page Support:` value as the heap decided it
    /// ([`zgc_large_pages`]: `-XX:+UseTransparentHugePages` and the kernel's
    /// THP modes); `None` = `Disabled`, the value with the switch
    /// `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES=0` and off Linux. zki (item 5(c)):
    /// G1 and Serial state theirs too ([`gc_init_large_pages_line`]).
    pub large_pages: Option<&'a str>,
    /// G1 evacuation pool size, as applied by the heap.
    pub g1_parallel_workers: Option<usize>,
    /// zt t31/x10: G1's `-XX:G1PeriodicGCInterval` at start-up (ms; `0` =
    /// `Periodic GC: Disabled`).
    pub g1_periodic_interval_ms: u64,
    /// zt t27/lg1: the Linux facts HotSpot 25 ZGC's backing states in its
    /// block (address-space limit, `max_map_count`; [`ZgcLinuxBacking`]);
    /// `None` off ZGC and off Linux, where its lines and the `max_map_count`
    /// warning are not printed.
    pub zgc_linux_backing: Option<ZgcLinuxBacking>,
}

/// zt t27/lg1 (page `zgc-zt12tg5-*` item 5, host row `tg10_h3`; page
/// `zgc-zt13x10-review-residuals-*` item 23i, rows `tg8_s2`, `x15_T2`): the
/// two Linux facts HotSpot 25 ZGC's heap reservation and backing read at heap
/// initialisation -- the address-space limit (`os::has_allocatable_memory_limit`,
/// `RLIMIT_AS`) and `/proc/sys/vm/max_map_count`
/// (`ZPhysicalMemoryBacking::warn_max_map_count`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZgcLinuxBacking {
    /// The `RLIMIT_AS` soft limit in bytes; `None` = unlimited.
    pub address_space_limit: Option<u64>,
    /// `/proc/sys/vm/max_map_count`; `None` when it cannot be read (HotSpot
    /// then skips its check).
    pub max_map_count: Option<u64>,
}

/// `CRATONVM_ZGC_INIT_LINUX_BACKING_LINES` (zt t27/lg1, default ON, `=0`
/// kill): on Linux, ZGC's `gc,init` block carries HotSpot 25's five reservation
/// and backing lines (`Address Space Size`, `Reserved Space Type` / `Size`,
/// `Heap Backing File` / `Filesystem`; [`zgc_linux_backing_lines`]) between
/// `Large Page Support` and `Min Capacity`. `=0`: they are left out, as
/// before. Read per call (once per `gc,init` block).
pub fn zgc_init_linux_backing_lines_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_MAX_MAP_COUNT_WARNING` (zt t27/lg1, default ON, `=0` kill):
/// on Linux, a ZGC VM whose maximum heap needs more memory mappings than
/// `/proc/sys/vm/max_map_count` allows prints HotSpot 25's five
/// `[warning][gc]` lines ([`zgc_max_map_count_warning_lines`]) at heap
/// initialisation, `-version` included. `=0`: nothing, as before.
pub fn zgc_max_map_count_warning_enabled() -> bool {
    true
}

/// ZGC's granule (`ZGranuleSize`, 2 MiB): HotSpot aligns the maximum capacity
/// to it, and the `max_map_count` rule counts granules.
const ZGC_GRANULE_BYTES: u64 = 2 * 1024 * 1024;

/// zt t27/lg1: HotSpot 25 ZGC's largest supported heap on x86-64 and AArch64
/// (`ZAddressOffsetMax` at its 44-bit ceiling, 16 TiB): a larger maximum heap
/// ends the launch with `Java heap too large (max supported heap size is
/// 16384G)` (`ZGlobalsPointers::initialize`; host rows `x15_T1` / `_T1c`).
pub const ZGC_MAX_SUPPORTED_HEAP_BYTES: u64 = 1 << 44;

/// zt t27/lg1: `ZAddressOffsetMax` for a maximum capacity -- the smallest
/// power of two holding `ZVirtualToPhysicalRatio` (16) times the capacity, its
/// bit count clamped to `[42, 44]` (`ZPlatformAddressOffsetBits`, recalled).
/// Pure.
pub fn zgc_address_offset_max(max_capacity: u64) -> u64 {
    let desired = max_capacity.saturating_mul(16);
    let bits = desired
        .checked_next_power_of_two()
        .map_or(64, u64::trailing_zeros)
        .clamp(42, 44);
    1u64 << bits
}

/// zt t27/lg1: HotSpot 25 ZGC's five Linux reservation and backing lines of
/// the `gc,init` block, measured on the host (25.0.4, rows `tg10_h3`,
/// `h7_T7`, `h7_T8`: `Address Space Size: unlimited`, `Reserved Space Type:
/// Contiguous/Unrestricted/Complete`, `Reserved Space Size: 31776M` at the
/// ergonomic 1986M and `4G` at `-Xmx256m`, `Heap Backing File:
/// /memfd:java_heap`, `Heap Backing Filesystem: tmpfs (0x1021994)`). The
/// reservation is 16 times the maximum capacity (aligned to the granule),
/// limited by `ZAddressOffsetMax` ([`zgc_address_offset_max`]) and half the
/// address-space limit (`MaxVirtMemFraction`); `Restricted` when the
/// address-space limit is the binding one. The limited-address-space
/// spelling and the restricted arm are recalled, not measured (no host row
/// sets `RLIMIT_AS`). HotSpot's heap is a `memfd` on `tmpfs`; this VM states
/// the reservation and backing HotSpot's ZGC makes for the same command
/// line, as its other `gc,init` lines do. Pure.
pub fn zgc_linux_backing_lines(
    max_heap: u64,
    backing: &ZgcLinuxBacking,
    hugetlbfs: bool,
) -> Vec<String> {
    let align_up = |v: u64| v.div_ceil(ZGC_GRANULE_BYTES).saturating_mul(ZGC_GRANULE_BYTES);
    let max_capacity = align_up(max_heap);
    let offset_max = zgc_address_offset_max(max_capacity);
    let limit = match backing.address_space_limit {
        Some(as_limit) => align_up(as_limit / 2).min(offset_max),
        None => offset_max,
    };
    let reserved = max_capacity.saturating_mul(16).min(limit);
    let address_space = match backing.address_space_limit {
        None => "Address Space Size: unlimited".to_string(),
        Some(as_limit) => format!("Address Space Size: limited ({})", exact_size(as_limit)),
    };
    vec![
        address_space,
        format!(
            "Reserved Space Type: Contiguous/{}/Complete",
            if limit == offset_max {
                "Unrestricted"
            } else {
                "Restricted"
            }
        ),
        format!("Reserved Space Size: {}", exact_size(reserved)),
        // zki (page `zgc-zt23hunt4-*` item 4, host `zki_hs1` b1 / b2 / b7 /
        // b8): the `Explicit` state's heap is a hugetlbfs memfd.
        if hugetlbfs {
            "Heap Backing File: /memfd:java_heap.hugetlb".to_string()
        } else {
            "Heap Backing File: /memfd:java_heap".to_string()
        },
        if hugetlbfs {
            "Heap Backing Filesystem: hugetlbfs (0x958458f6)".to_string()
        } else {
            "Heap Backing Filesystem: tmpfs (0x1021994)".to_string()
        },
    ]
}

/// zt t27/lg1: HotSpot 25 ZGC's `max_map_count` warning
/// (`ZPhysicalMemoryBacking::warn_max_map_count`), measured on the host
/// (25.0.4, `-Xmx1t`, rows `tg8_s2`, `tg8_s2h`, `x15_T2`: five `[warning][gc]`
/// lines on stdout): ZGC may map each granule three times, plus 20 % for the
/// rest of the process, so it needs `(max capacity / 2 MiB) * 3 * 1.2`
/// mappings (truncated); below that, the five lines. Empty when the limit
/// is enough or unknown. Pure.
pub fn zgc_max_map_count_warning_lines(max_heap: u64, max_map_count: Option<u64>) -> Vec<String> {
    let Some(actual) = max_map_count else {
        return Vec::new();
    };
    let max_capacity = max_heap
        .div_ceil(ZGC_GRANULE_BYTES)
        .saturating_mul(ZGC_GRANULE_BYTES);
    // HotSpot: `(max_capacity / ZGranuleSize) * 3 * 1.2`, a `double`
    // truncated to `size_t`. Cast: the granule count is far below 2^52.
    let required = ((max_capacity / ZGC_GRANULE_BYTES) as f64 * 3.0 * 1.2) as u64;
    if actual >= required {
        return Vec::new();
    }
    vec![
        "***** WARNING! INCORRECT SYSTEM CONFIGURATION DETECTED! *****".to_string(),
        "The system limit on number of memory mappings per process might be too low for the given"
            .to_string(),
        format!(
            "max Java heap size ({}M). Please adjust /proc/sys/vm/max_map_count to allow for at",
            max_capacity / (1024 * 1024)
        ),
        format!(
            "least {required} mappings (current limit is {actual}). Continuing execution with the current"
        ),
        "limit could lead to a premature OutOfMemoryError being thrown, due to failure to map memory."
            .to_string(),
    ]
}

/// zt t27/lg1: [`zgc_max_map_count_warning_lines`] for a VM, under
/// `CRATONVM_ZGC_MAX_MAP_COUNT_WARNING`; empty without Linux facts.
pub fn zgc_max_map_count_warning(max_heap: u64, backing: Option<&ZgcLinuxBacking>) -> Vec<String> {
    match backing {
        Some(b) if zgc_max_map_count_warning_enabled() => {
            zgc_max_map_count_warning_lines(max_heap, b.max_map_count)
        }
        _ => Vec::new(),
    }
}

/// zt t27/lg1: the [`ZgcLinuxBacking`] facts of this process, read now
/// (`getrlimit(RLIMIT_AS)` and `/proc/sys/vm/max_map_count`); `None` off
/// Linux and when neither of their two switches is on. Read once per ZGC VM
/// start (and once by a `-version` that prints the warning).
pub fn zgc_linux_backing_facts() -> Option<ZgcLinuxBacking> {
    #[cfg(target_os = "linux")]
    {
        if !zgc_init_linux_backing_lines_enabled() && !zgc_max_map_count_warning_enabled() {
            return None;
        }
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` writes the one `rlimit` it is given, a live
        // local.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut rl) };
        // `rlim_t` is `u64` on Linux.
        let address_space_limit =
            (rc == 0 && rl.rlim_cur != libc::RLIM_INFINITY).then_some(rl.rlim_cur);
        let max_map_count = std::fs::read_to_string("/proc/sys/vm/max_map_count")
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok());
        Some(ZgcLinuxBacking {
            address_space_limit,
            max_map_count,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// zt t27/lg1: HotSpot's default shape of a start-up `[warning][gc]` line
/// (`uptime,level,tags` on stdout) for a VM without an `-Xlog` logger, the
/// tags column padded to `tag_width` (HotSpot pads it to the widest tag set
/// its stdout has printed: 8 after a `[warning][pagesize]` line). Pure.
pub fn gc_warning_default_line(uptime_secs: f64, tag_width: usize, text: &str) -> String {
    format!("[{uptime_secs:.3}s][warning][{:<tag_width$}] {text}", "gc")
}

/// zki (page `zgc-zt23hunt4-*` item 4): an `[error][gc]` line written while
/// the VM runs: through the `-Xlog` logger (every output taking `gc` at
/// `error`, plus HotSpot's default stdout output), or, with no logger, in
/// HotSpot's default stdout shape at `uptime_secs`.
pub fn log_gc_error_line(text: &str, uptime_secs: f64) {
    if let Some(logger) = current_logger() {
        log_gc_startup_level(&logger, LogLevel::Error, text);
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", gc_error_default_line(uptime_secs, 5, 2, text));
    let _ = out.flush();
}

/// zki (page `zgc-zt23hunt4-*` item 4): HotSpot 25 ZGC's `[error][gc]` line
/// when a refused commit lowers its maximum (`ZPageAllocator`, `Forced to
/// lower max Java heap size from %zuM(%.0f%%) to %zuM(%.0f%%)`, both sizes
/// against the maximum capacity). Measured: `from 512M(100%) to 218M(43%)`.
/// Pure.
pub fn zgc_forced_lower_max_text(max_capacity: u64, lowered_to: u64) -> String {
    const M: u64 = 1024 * 1024;
    let pct = |v: u64| {
        if max_capacity == 0 {
            0.0
        } else {
            v as f64 * 100.0 / max_capacity as f64
        }
    };
    format!(
        "Forced to lower max Java heap size from {}M({:.0}%) to {}M({:.0}%)",
        max_capacity / M,
        pct(max_capacity),
        lowered_to / M,
        pct(lowered_to)
    )
}

/// zki (page `zgc-zt23hunt4-*` item 4): an `[error][gc]` line in HotSpot's
/// default stdout shape, the level and tag columns padded to the widest ones
/// stdout printed before it (`[error  ][gc      ]` after a
/// `[warning][pagesize]` line, `[error][gc]` alone). Pure.
pub fn gc_error_default_line(
    uptime_secs: f64,
    level_width: usize,
    tag_width: usize,
    text: &str,
) -> String {
    format!(
        "[{uptime_secs:.3}s][{:<level_width$}][{:<tag_width$}] {text}",
        "error", "gc"
    )
}

/// zt t27/lg1: a start-up `[warning][gc]` line through `logger`: every output
/// that takes `gc` at `warning`, plus HotSpot's default `all=warning:stdout`
/// unless an option set `gc` on stdout or a `disable` removed that output (the
/// rule of [`log_pagesize_warning`]).
fn log_gc_startup_warning(logger: &UnifiedLogger, text: &str) {
    log_gc_startup_level(logger, LogLevel::Warning, text);
}

/// [`log_gc_startup_warning`] at `level` (zki: `error` for the heap's
/// refused hugetlbfs commits).
fn log_gc_startup_level(logger: &UnifiedLogger, level: LogLevel, text: &str) {
    logger.log(&[LogTag::Gc], level, text);
    if logger.default_jfr_startup && logger.level_for(&LogOutput::Stdout, LogTag::Gc).is_none() {
        let line = logger.default_stdout_line(&[LogTag::Gc], level, text);
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// zt t27/lg1: the Linux reservation / backing lines and the
/// `max_map_count` warning against the host's HotSpot 25.0.4 text (rows
/// `tg10_h3`, `h7_T7`, `tg8_s2`), the limit arithmetic worked out by hand.
#[cfg(test)]
mod zt27lg1_zgc_linux_backing_tests;

/// zt t23/hunt4: the value of HotSpot's `Pre-touch:` line
/// (`GCInitLogger::print_heap` / ZGC's `ZPageAllocator`: `AlwaysPreTouch`).
fn pre_touch_line(on: bool) -> String {
    format!("Pre-touch: {}", if on { "Enabled" } else { "Disabled" })
}

/// `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES` (zt t24/hunt7, default ON, `=0`
/// kill): on Linux, HotSpot 25 ZGC's large-page decision ([`zgc_large_pages`])
/// -- `-XX:+UseTransparentHugePages` advises the heap `MADV_HUGEPAGE` when the
/// kernel's modes let HotSpot use transparent huge pages, `os::large_page_init`'s
/// `[warning][pagesize]` line when they do not support them at all, and the
/// `gc,init` block's `Large Page Support:` value for the kernel's modes, with
/// or without the flag. `=0`: the flag is accepted and ignored and the line is
/// the constant `Disabled`, as before. Read once per VM start (ZGC only).
pub fn zgc_transparent_huge_pages_enabled() -> bool {
    true
}

/// zt t24/hunt7: HotSpot 25's `os::large_page_init` warning when
/// `-XX:+UseTransparentHugePages` is given and the kernel's THP mode
/// (`/sys/kernel/mm/transparent_hugepage/enabled`) is neither `always` nor
/// `madvise` (`log_warning(pagesize)`, default output stdout). Recalled, not
/// measured: only a host whose mode is `never` prints it.
pub const THP_UNSUPPORTED_WARNING: &str =
    "UseTransparentHugePages disabled, transparent huge pages are not supported by the operating system.";

/// zt t25/hunt7b: HotSpot 25's `[warning][pagesize]` line when
/// `-XX:+UseTransparentHugePages` is in force and the kernel's SHMEM THP mode
/// (`/sys/kernel/mm/transparent_hugepage/shmem_enabled`) is `never`: ZGC maps
/// its heap as shared memory. Measured on 25.0.4 (host rows `h7_T1`, `h7_T4`,
/// `h7_T6`, `h7_T7`: `enabled=madvise`, `shmem_enabled=never`), printed on
/// stdout before the `gc,init` block, which then says `Disabled (OS
/// enforced)`.
pub const SHMEM_THP_NOT_ENABLED_WARNING: &str =
    "Shared memory transparent huge pages are not enabled in the OS. Set /sys/kernel/mm/transparent_hugepage/shmem_enabled to 'advise' to enable them.";

/// zt t25/hunt7b: HotSpot 25's `[warning][pagesize]` line for an explicit
/// `-XX:+UseLargePages` (without transparent huge pages) on a host with no
/// hugetlbfs page available (`warn_no_large_pages_configured`). Measured on
/// 25.0.4 (host row `h7_T8`); the `gc,init` block then says `Disabled`.
pub const NO_LARGE_PAGES_WARNING: &str =
    "UseLargePages disabled, no large pages configured and available on the system.";

/// zt t24/hunt7: HotSpot 25 ZGC's large-page decision on Linux
/// ([`zgc_large_pages`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZgcLargePages {
    /// The `gc,init` block's `Large Page Support:` value
    /// (`ZLargePages::to_string`).
    pub line: &'static str,
    /// The heap is advised `MADV_HUGEPAGE`: HotSpot's state is `Transparent`,
    /// because the flag asked for it or (zki, page item 5(b)) because the
    /// kernel's shmem mode is `always` (`Enabled (Transparent, OS
    /// enforced)`): the kernel then backs HotSpot's shmem heap huge, and this
    /// heap is anonymous memory, which only the advice backs huge under the
    /// anonymous mode `madvise`.
    pub advise: bool,
    /// The `[warning][pagesize]` text HotSpot prints at startup, if any
    /// ([`THP_UNSUPPORTED_WARNING`], [`SHMEM_THP_NOT_ENABLED_WARNING`],
    /// [`NO_LARGE_PAGES_WARNING`]).
    pub warning: Option<&'static str>,
    /// zki (page item 4): HotSpot's `Explicit` state -- `-XX:+UseLargePages`
    /// without the THP flag on a host whose hugetlbfs pool passed
    /// `os::large_page_init`'s sanity mapping: the heap is backed by
    /// hugetlbfs pages (`ZgcRealHeap::with_capacity_initial_and_large_pages`)
    /// and its backing lines name `/memfd:java_heap.hugetlb` on `hugetlbfs`.
    pub explicit: bool,
}

/// zt t24/hunt7: the selected word of a THP sysfs file, the bracketed one
/// (`always [madvise] never` -> `madvise`), as HotSpot's `HugePages` reads
/// it; `None` when there is none. Pure.
pub fn thp_sysfs_mode(text: &str) -> Option<&str> {
    let open = text.find('[')?;
    let rest = &text[open + 1..];
    let close = rest.find(']')?;
    Some(&rest[..close])
}

/// zt t24/hunt7: the kernel's THP modes HotSpot 25 reads at startup --
/// `(enabled, shmem_enabled)` under `/sys/kernel/mm/transparent_hugepage/`
/// ([`thp_sysfs_mode`]) -- `None` for a file that is missing or unreadable,
/// and both `None` off Linux.
pub fn kernel_thp_modes() -> (Option<String>, Option<String>) {
    #[cfg(target_os = "linux")]
    {
        let read = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| thp_sysfs_mode(&text).map(str::to_string))
        };
        (
            read("/sys/kernel/mm/transparent_hugepage/enabled"),
            read("/sys/kernel/mm/transparent_hugepage/shmem_enabled"),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None)
    }
}

/// zt t24/hunt7 (`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`):
/// HotSpot 25 ZGC's large-page decision on Linux, recalled from
/// `os::large_page_init` and `ZLargePages::pd_initialize` /
/// `ZLargePages::to_string` (not measured here; rows `h7_T*` decide):
///
/// * `os::large_page_init`: `-XX:+UseTransparentHugePages` (`use_thp`) on a
///   kernel whose THP mode (`anon_mode`) is neither `always` nor `madvise`
///   prints [`THP_UNSUPPORTED_WARNING`] and turns the flag off.
/// * ZGC's heap is shared memory (`memfd`), so its state follows the shmem
///   THP mode (`shmem_mode`): with the flag in force, `never` makes it
///   `Disabled (OS enforced)`, anything else `Enabled (Transparent)` (the
///   heap is advised); without it, `always` makes it `Enabled (Transparent,
///   OS enforced)`, anything else `Disabled`.
///
/// zt t25/hunt7b, measured on 25.0.4 (host `madvise` / `never`): the flag in
/// force with a `never` shmem mode also prints
/// [`SHMEM_THP_NOT_ENABLED_WARNING`]; an explicit `-XX:+UseLargePages`
/// (`use_large_pages`) without the THP flag prints [`NO_LARGE_PAGES_WARNING`]
/// when the hugetlbfs pool cannot map one page (`explicit_pages` false:
/// [`hugetlb_sanity_check`]) and the state is `Disabled`.
///
/// zki (page item 4, host rows `zki_hs*`, 25.0.4 with a hugetlbfs pool): when
/// the pool can map a page, the state is `Enabled (Explicit)` and the heap is
/// backed by hugetlbfs pages (`explicit`); no warning. zki (item 5(b)): the
/// OS-enforced transparent state advises the heap too
/// ([`zgc_os_enforced_thp_advice_enabled`]). Pure but for that switch.
pub fn zgc_large_pages(
    use_thp: bool,
    use_large_pages: bool,
    anon_mode: Option<&str>,
    shmem_mode: Option<&str>,
    explicit_pages: bool,
) -> ZgcLargePages {
    let supported = matches!(anon_mode, Some("always" | "madvise"));
    let mut warning = (use_thp && !supported).then_some(THP_UNSUPPORTED_WARNING);
    if !use_thp && use_large_pages && !explicit_pages {
        warning = Some(NO_LARGE_PAGES_WARNING);
    }
    let mut explicit = false;
    let (line, advise) = if use_thp && supported {
        if shmem_mode == Some("never") {
            warning = Some(SHMEM_THP_NOT_ENABLED_WARNING);
            ("Disabled (OS enforced)", false)
        } else {
            ("Enabled (Transparent)", true)
        }
    } else if !use_thp && use_large_pages && explicit_pages {
        explicit = true;
        ("Enabled (Explicit)", false)
    } else if shmem_mode == Some("always") {
        (
            "Enabled (Transparent, OS enforced)",
            zgc_os_enforced_thp_advice_enabled(),
        )
    } else {
        ("Disabled", false)
    };
    ZgcLargePages {
        line,
        advise,
        warning,
        explicit,
    }
}

/// `CRATONVM_ZGC_OS_ENFORCED_THP_ADVICE` (zki, page
/// `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 5(b); default ON,
/// `=0` kill): in HotSpot's `Enabled (Transparent, OS enforced)` state (shmem
/// THP mode `always`, no flag) the ZGC heap is advised `MADV_HUGEPAGE`, as
/// the kernel backs HotSpot's shmem heap huge in that state. `=0`: the line
/// only, the heap backed as the anonymous THP mode decides.
pub fn zgc_os_enforced_thp_advice_enabled() -> bool {
    true
}

/// `CRATONVM_LARGE_PAGES_EXPLICIT` (zki, page item 4; default ON, `=0`
/// kill): an explicit `-XX:+UseLargePages` on a Linux host whose hugetlbfs
/// pool passes HotSpot's sanity mapping ([`hugetlb_sanity_check`]) is
/// HotSpot 25's `Explicit` state: `Large Page Support: Enabled (Explicit)`
/// on every collector, and the ZGC heap backed by hugetlbfs pages (each
/// refused commit prints [`ZGC_COMMIT_FAILED_ERROR`]; an initial heap the
/// pool cannot hold fails VM creation as HotSpot's does). `=0`: the request
/// is ignored silently when the pool has pages, as before.
pub fn large_pages_explicit_enabled() -> bool {
    true
}

/// zki (page item 4): HotSpot 25's `[error][gc]` line for each hugetlbfs
/// commit of the ZGC heap the pool refuses (`ZPhysicalMemoryBacking::commit_inner`,
/// `os::strerror(ENOMEM)`), measured on the host (25.0.4, rows `zki_hs*`).
pub const ZGC_COMMIT_FAILED_ERROR: &str = "Failed to commit memory (Not enough space)";

/// zki (page item 4): HotSpot 25 ZGC's `[error][gc]` line and
/// `vm_exit_during_initialization` detail when the initial heap could not be
/// committed whole: `Failed to allocate initial Java heap (<n>M)`, `<n>` the
/// initial capacity in whole MiB. Pure.
pub fn zgc_initial_heap_failure_text(initial_bytes: u64) -> String {
    format!(
        "Failed to allocate initial Java heap ({}M)",
        initial_bytes / (1024 * 1024)
    )
}

/// zki (page item 4): `os::Linux::hugetlbfs_sanity_check` for the default
/// 2 MiB page size -- can one private anonymous `MAP_HUGETLB` page be mapped
/// (which reserves a page of the pool) right now? HotSpot keeps
/// `UseLargePages` only when it can, else it warns [`NO_LARGE_PAGES_WARNING`].
/// `false` off Linux.
pub fn hugetlb_sanity_check() -> bool {
    #[cfg(target_os = "linux")]
    {
        const HUGE: usize = 2 * 1024 * 1024;
        // SAFETY: a fresh private anonymous mapping at a kernel-chosen
        // address aliases nothing; it is unmapped before returning.
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                HUGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return false;
            }
            libc::munmap(p, HUGE);
        }
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// zki (page item 5(c)): the `Large Page Support:` value of the G1 and Serial
/// `gc,init` blocks (`GCInitLogger::large_pages_support`, Linux): `Enabled
/// (Transparent)` when `-XX:+UseTransparentHugePages` survived
/// `os::large_page_init` (the anonymous THP mode is `always` or `madvise`),
/// `Enabled (Explicit)` for an explicit `-XX:+UseLargePages` whose pool
/// passed the sanity mapping (`explicit_pages`), else `Disabled`. Measured on
/// 25.0.4 (rows `zki_hs1` a2 / a3 / a4 / b3 / b4 / c2 / c4: the shmem mode
/// plays no part). Pure.
pub fn gc_init_large_pages_line(
    use_thp: bool,
    use_large_pages: bool,
    anon_mode: Option<&str>,
    explicit_pages: bool,
) -> &'static str {
    if use_thp {
        if matches!(anon_mode, Some("always" | "madvise")) {
            "Enabled (Transparent)"
        } else {
            "Disabled"
        }
    } else if use_large_pages && explicit_pages {
        "Enabled (Explicit)"
    } else {
        "Disabled"
    }
}

/// zt t26/x21: the collector-independent half of [`zgc_large_pages`]'s
/// warning -- `os::large_page_init`'s lines, which HotSpot 25 prints on every
/// collector: [`THP_UNSUPPORTED_WARNING`] for `-XX:+UseTransparentHugePages`
/// on a kernel whose THP mode is neither `always` nor `madvise`, else
/// [`NO_LARGE_PAGES_WARNING`] for an explicit `-XX:+UseLargePages` whose
/// hugetlbfs pool cannot map a page (`explicit_pages` false,
/// [`hugetlb_sanity_check`]). Pure.
pub fn os_large_page_warning(
    use_thp: bool,
    use_large_pages: bool,
    anon_mode: Option<&str>,
    explicit_pages: bool,
) -> Option<&'static str> {
    if use_thp {
        return (!matches!(anon_mode, Some("always" | "madvise"))).then_some(THP_UNSUPPORTED_WARNING);
    }
    (use_large_pages && !explicit_pages).then_some(NO_LARGE_PAGES_WARNING)
}

/// `[gc] Using Serial` -- the Generational backend's HotSpot shape.
pub const GC_NAME_SERIAL: &str = "Serial";
/// `[gc] Using G1`.
pub const GC_NAME_G1: &str = "G1";
/// `[gc] Using The Z Garbage Collector` (HotSpot's `ZName`).
pub const GC_NAME_ZGC: &str = "The Z Garbage Collector";

/// gcg g3/x: HotSpot's default `MinHeapSize` floor when neither `-Xms` nor
/// `-XX:MinHeapSize` is given -- measured on 25.0.3 Serial: `Heap Min
/// Capacity: 8M` at `-Xmx10m` .. `-Xmx40g`, and `min(8M, -Xmx)` below
/// (`-Xmx6m` prints `6M`).
pub const HOTSPOT_DEFAULT_MIN_HEAP_FLOOR: usize = 8 * 1024 * 1024;

/// gcg g3/x (`gcg-g1c-xlog-gc-init-block-is-not-hotspots`): the `MinHeapSize`
/// HotSpot reports for this command line. `explicit` is `-XX:MinHeapSize`, or
/// `-Xms` (which sets both the minimum and the initial size on HotSpot);
/// without it, HotSpot's ergonomic minimum is `min(8M, InitialHeapSize)`
/// (`Arguments::set_heap_size`: `MIN2(reasonable_minimum, InitialHeapSize)`).
/// Never above `initial` (HotSpot refuses `MinHeapSize > InitialHeapSize`;
/// this VM clamps, as it clamps `-Xms` to `-Xmx`).
pub fn hotspot_min_heap_size(explicit: Option<usize>, initial: usize) -> usize {
    explicit
        .unwrap_or(HOTSPOT_DEFAULT_MIN_HEAP_FLOOR)
        .min(initial)
}

/// gcg g3/x: the value of HotSpot's `[gc,init] Compressed Oops:` line
/// (`GCInitLogger::print_compressed_oops`, `CompressedOops::mode_to_string`)
/// for this process's narrow-oop state: `Disabled`, or `Enabled (32-bit)` /
/// `(Zero based)` / `(Non-zero disjoint base)` / `(Non-zero based)` from the
/// base and shift. HotSpot's disjoint test is a base aligned to
/// `4G << LogMinObjAlignmentInBytes` (32 GiB). This VM runs with compressed
/// oops OFF by default, so the default line is `Disabled` -- what HotSpot
/// prints under `-XX:-UseCompressedOops` (the HotSpot twin of a row must pass
/// it: HotSpot's own mode depends on where its OS placed the heap).
pub fn compressed_oops_mode(enabled: bool, base: u64, shift: usize) -> &'static str {
    const DISJOINT_ALIGN: u64 = 1 << 35;
    if !enabled {
        "Disabled"
    } else if base == 0 && shift == 0 {
        "Enabled (32-bit)"
    } else if base == 0 {
        "Enabled (Zero based)"
    } else if base % DISJOINT_ALIGN == 0 {
        "Enabled (Non-zero disjoint base)"
    } else {
        "Enabled (Non-zero based)"
    }
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

/// The `gc,init` lines, in `GCInitLogger::print_all`'s order (HotSpot 25.0.3
/// Serial, measured by gcg g3/x):
///
/// ```text
/// Version: 25.0.3+9-LTS (release)
/// CPUs: 8 total, 8 available
/// Memory: 16233M
/// Large Page Support: Disabled
/// NUMA Support: Disabled
/// Compressed Oops: Disabled
/// Heap Min Capacity: 8M
/// Heap Initial Capacity: 64M
/// Heap Max Capacity: 64M
/// Pre-touch: Disabled
/// ```
///
/// gcg g3/x added the four lines the block lacked: this VM uses no large
/// pages, binds no memory to NUMA nodes and pre-touches nothing (all three are
/// true statements, the lines HotSpot prints by default), and the oops line
/// states this process's narrow-oop mode ([`compressed_oops_mode`]).
/// Serial has no worker line (`Parallel Workers` is G1's / Parallel's).
///
/// gcd d10/o retirement (2026-10-04): G1 prints the same block with
/// `Heap Region Size:` after the oops line and `Periodic GC: Disabled` last
/// (25.0.4 `-XX:+UseG1GC -Xlog:gc+init`; this G1 runs no periodic
/// collection). The parallel count comes from G1's actual evacuation pool.
/// Concurrent/refinement worker lines remain omitted. ZGC's block is
/// [`zgc_init_lines`].
pub fn gc_init_lines(f: &GcInitFacts<'_>) -> Vec<String> {
    if f.collector == GC_NAME_ZGC {
        return zgc_init_lines(f);
    }
    let mut lines = vec![
        format!("Version: {} (release)", f.version),
        format!(
            "CPUs: {} total, {} available",
            f.cpus_total, f.cpus_available
        ),
    ];
    if let Some(m) = f.memory_bytes {
        lines.push(format!("Memory: {}", proper_size(m)));
    }
    // zki (page `zgc-zt23hunt4-*` item 5(c)): the VM's large-page decision
    // for this collector ([`gc_init_large_pages_line`]); `None` = `Disabled`.
    lines.push(format!(
        "Large Page Support: {}",
        f.large_pages.unwrap_or("Disabled")
    ));
    lines.push("NUMA Support: Disabled".to_string());
    lines.push(format!("Compressed Oops: {}", f.compressed_oops));
    let b = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    if let Some(region) = f.region_size {
        lines.push(format!("Heap Region Size: {}", exact_size(b(region))));
    }
    lines.push(format!("Heap Min Capacity: {}", exact_size(b(f.heap_min))));
    lines.push(format!(
        "Heap Initial Capacity: {}",
        exact_size(b(f.heap_initial))
    ));
    lines.push(format!("Heap Max Capacity: {}", exact_size(b(f.heap_max))));
    lines.push(pre_touch_line(f.pre_touch));
    if f.collector == GC_NAME_G1 {
        if let Some(n) = f.g1_parallel_workers {
            lines.push(format!("Parallel Workers: {n}"));
        }
        // zt t31/x10: HotSpot 25's `G1InitLogger::print_periodic_gc`.
        if f.g1_periodic_interval_ms == 0 || !crate::config::hotspot_flags::full_gc_heap_dumps_enabled() {
            lines.push("Periodic GC: Disabled".to_string());
        } else {
            lines.push("Periodic GC: Enabled".to_string());
            lines.push(format!("Periodic GC Interval: {}ms", f.g1_periodic_interval_ms));
        }
    }
    lines
}







/// gcd d10/o retirement (2026-10-04): HotSpot 25 ZGC's `gc,init` block, in
/// its order (25.0.4, `-XX:+UseZGC -Xmx256m -Xlog:gc+init`), as far as this
/// VM can state it truthfully:
///
/// ```text
/// Initializing The Z Garbage Collector
/// Version: 25.0.4+7-LTS (release)
/// NUMA Support: Disabled
/// CPUs: 8 total, 8 available
/// Memory: 32086M
/// Large Page Support: Disabled
/// Min Capacity: 8M
/// Initial Capacity: 256M
/// Max Capacity: 256M
/// Soft Max Capacity: 256M
/// Pre-touch: Disabled
/// Available space on backing filesystem: N/A
/// Uncommit: Enabled
/// Uncommit Delay: 300s
/// ```
///
/// HotSpot's address-space, reserved-space, backing-file / filesystem,
/// page-size and worker lines describe its own heap backing and thread pools
/// and are not printed. The uncommit lines (zr z1/tr) describe this VM's own
/// policy and are.
/// zr z3/t2: the backing-filesystem line's gate -- HotSpot prints it on Linux
/// only (`linux`), and `CRATONVM_ZGC_INIT_BACKING_LINE=0` drops it there.
fn zgc_backing_line_applies(linux: bool) -> bool {
    linux
}

/// zt t7/x4b: HotSpot ZGC's `gc,init` capacity figure, `%zuM` (whole MiB,
/// truncating). Pure.
fn zgc_init_mib(bytes: u64) -> String {
    format!("{}M", bytes / (1024 * 1024))
}



pub fn zgc_init_lines(f: &GcInitFacts<'_>) -> Vec<String> {
    let b = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let mut lines = vec![
        format!("Initializing {GC_NAME_ZGC}"),
        format!("Version: {} (release)", f.version),
        "NUMA Support: Disabled".to_string(),
        format!(
            "CPUs: {} total, {} available",
            f.cpus_total, f.cpus_available
        ),
    ];
    if let Some(m) = f.memory_bytes {
        lines.push(format!("Memory: {}", proper_size(m)));
    }
    // zt t24/hunt7: the heap's decision (`GcInitFacts::large_pages`,
    // `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`); it was the constant `Disabled`.
    lines.push(format!(
        "Large Page Support: {}",
        f.large_pages.unwrap_or("Disabled")
    ));
    // zt t27/lg1 (`CRATONVM_ZGC_INIT_LINUX_BACKING_LINES`, default ON, `=0`
    // kill; host row `tg10_h3`): HotSpot 25's Linux reservation and backing
    // lines come next, before the capacities ([`zgc_linux_backing_lines`]).
    if let Some(backing) = f.zgc_linux_backing.as_ref() {
        if zgc_init_linux_backing_lines_enabled() {
            lines.extend(zgc_linux_backing_lines(
                b(f.heap_max),
                backing,
                f.large_pages == Some("Enabled (Explicit)"),
            ));
        }
    }
    // zt t7/x4b (`CRATONVM_ZGC_INIT_CAPACITY_MIB`, default ON, `=0` kill):
    // HotSpot 25's ZGC prints these four as `%zuM` (local 25.0.3: `-Xmx1g`
    // gives `Max Capacity: 1024M`, `-XX:SoftMaxHeapSize=100k` gives `Soft Max
    // Capacity: 0M`); `exact_size` printed `1G` and `100K`.
    let size: fn(u64) -> String = {
        zgc_init_mib
    };
    lines.push(format!("Min Capacity: {}", size(b(f.heap_min))));
    lines.push(format!("Initial Capacity: {}", size(b(f.heap_initial))));
    lines.push(format!("Max Capacity: {}", size(b(f.heap_max))));
    lines.push(format!(
        "Soft Max Capacity: {}",
        size(b(f.soft_max.unwrap_or(f.heap_max).min(f.heap_max)))
    ));
    // zt t23/hunt4: `Enabled` under an `-XX:+AlwaysPreTouch` the heap
    // applied (`GcInitFacts::pre_touch`); it was the constant `Disabled`.
    lines.push(pre_touch_line(f.pre_touch));
    // zr z2/t2: HotSpot 25 prints its backing filesystem's free space between
    // `Pre-touch` and `Uncommit` (host order, z1/tr). This heap is anonymous
    // memory with no backing filesystem, so `N/A` -- HotSpot's own spelling
    // when it has no figure -- is a true statement here. Printed only with the
    // uncommit facts (a real ZGC heap), `CRATONVM_ZGC_INIT_BACKING_LINE=0`
    // kills it. The other HotSpot-only lines stay unprinted: see
    // `docs/feature-designs/zgc-architecture.md` (address and
    // reserved space, backing file / filesystem, medium page size, the worker
    // pools describe HotSpot structures this VM does not have).
    // zr z3/t2 (rv3 finding 3): Linux only, as on HotSpot, whose line comes
    // from its Linux backing (`zPhysicalMemoryBacking_linux.cpp`); its Windows
    // and BSD backings print no such line.
    if f.zgc_uncommit.is_some() && zgc_backing_line_applies(cfg!(target_os = "linux")) {
        lines.push("Available space on backing filesystem: N/A".to_string());
    }
    // zr z1/tr: the uncommit lines describe THIS VM's policy (`ZUncommit`,
    // `ZUncommitDelay`, the `-Xms` floor), so unlike the address-space,
    // backing-file, page-size and worker lines they are stated. HotSpot 25
    // prints them after `Pre-touch` (and its backing-filesystem line, which is
    // not); host-observed on 25.0.4 `-Xmx512m`: `Uncommit: Enabled`,
    // `Uncommit Delay: 300s`. `Implicitly Disabled` is HotSpot's spelling
    // when the minimum heap equals the maximum.
    if let Some((enabled, delay_secs)) = f.zgc_uncommit {
        // zt t22/hunt3: HotSpot's `try_enable_uncommit` tests `!ZUncommit`
        // BEFORE `max_capacity == min_capacity`, so `-XX:-ZUncommit -Xms256m
        // -Xmx256m` prints `Uncommit: Disabled`, not the implicit line
        // (`CRATONVM_ZGC_INIT_UNCOMMIT_DISABLED_FIRST`, default ON, `=0`
        // kill: the implicit line first, as before).
        if !enabled {
            lines.push("Uncommit: Disabled".to_string());
        } else if f.heap_min >= f.heap_max {
            lines.push("Uncommit: Implicitly Disabled (-Xms equals -Xmx)".to_string());
        } else if f.zgc_uncommit_unsupported {
            // zki: `try_enable_uncommit`'s probe failed (explicit hugetlbfs
            // pages, a pool with no granule to spare; host row `zki_h3`).
            lines.push(
                "Uncommit: Implicitly Disabled (Not supported by operating system)".to_string(),
            );
        } else {
            lines.push("Uncommit: Enabled".to_string());
            lines.push(format!("Uncommit Delay: {delay_secs}s"));
        }
    }
    // zr z3/mt: HotSpot 25 ends the block with `Runtime Workers: N`, N being
    // `ParallelGCThreads` (25.0.3, 8 CPUs: `5` by default, `3` under
    // `-XX:ParallelGCThreads=3`, `1` at `-Xmx64m`). This VM's pause pools are
    // sized by that flag since zr z3/mt (`ZgcRealHeap::set_gc_worker_threads`),
    // so an EXPLICIT value is a true statement and is printed; without the
    // flag the pools keep their own measured defaults (the compaction runs 8
    // workers, the mark and sweep are serial), none of which is HotSpot's
    // ergonomic, so no figure is printed rather than a false one. The three
    // `GC Workers ...` lines that precede it on HotSpot stay unprinted: they
    // say `(dynamic)` (this VM's pools run a fixed count per cycle) and name a
    // young generation the default mode does not have
    // (`zgc-zr2t2-gc-init-lines-not-printed-CLOSED-20261019.md`).
    // `CRATONVM_ZGC_INIT_WORKER_LINE=0` kills the line.
    if let Some(n) = f.zgc_runtime_workers {
        {
            lines.push(format!("Runtime Workers: {n}"));
        }
    }
    lines
}

/// gcg g3/x: HotSpot's `[debug][gc,heap]` startup line
/// (`GenArguments::initialize_size_info`, measured on 25.0.3 Serial, two
/// spaces between the pairs, bytes):
/// `Minimum heap 8388608  Initial heap 67108864  Maximum heap 67108864`.
pub fn gc_heap_sizes_debug_line(f: &GcInitFacts<'_>) -> String {
    format!(
        "Minimum heap {}  Initial heap {}  Maximum heap {}",
        f.heap_min, f.heap_initial, f.heap_max
    )
}

/// Log the startup lines: `[gc] Using <collector>` (HotSpot prints it under
/// plain `-Xlog:gc`; GCeasy picks its parser from it), and the `gc,init`
/// block, in HotSpot 25's order for the collector ([`startup_line_order`]).
/// A no-op without a logger.
///
/// gcd d10/o retirement (2026-10-04): every backend now prints its line --
/// `Using G1`, `Using The Z Garbage Collector` -- where only the
/// Generational backend's `Using Serial` was printed before.
pub fn log_gc_startup(f: &GcInitFacts<'_>) {
    log_gc_startup_with_errors(f, &[]);
}

/// zki (page `zgc-zt23hunt4-*` item 4): [`log_gc_startup`] plus `[error][gc]`
/// lines HotSpot logs from the heap's initialisation, after the ZGC
/// `gc,init` block and before `Using The Z Garbage Collector` (25.0.4,
/// `-Xlog:gc*:stdout` with a hugetlbfs pool too small for the initial heap:
/// the `Failed to commit memory` lines, then `Failed to allocate initial Java
/// heap`). Through every output that takes `gc` at `error`, plus HotSpot's
/// default stdout output (the rule of [`log_pagesize_warning`]).
pub fn log_gc_startup_with_errors(f: &GcInitFacts<'_>, errors: &[String]) {
    let Some(logger) = current_logger() else {
        return;
    };
    let init_on = logger.is_enabled(&[LogTag::GcInit], LogLevel::Info);
    for line in startup_line_order(f) {
        match line {
            StartupLine::CardTable => logger.log(
                &[LogTag::GcInit],
                LogLevel::Info,
                &format!("CardTable entry size: {}", cratonvm_types::CARD_SIZE_BYTES),
            ),
            StartupLine::HeapSizesDebug => {
                if logger.is_enabled(&[LogTag::GcHeap], LogLevel::Debug) {
                    logger.log(
                        &[LogTag::GcHeap],
                        LogLevel::Debug,
                        &gc_heap_sizes_debug_line(f),
                    );
                }
            }
            StartupLine::Using => {
                logger.log(
                    &[LogTag::Gc],
                    LogLevel::Info,
                    &format!("Using {}", f.collector),
                );
            }
            StartupLine::InitBlock => {
                // zt t27/lg1 (`CRATONVM_ZGC_MAX_MAP_COUNT_WARNING`): ZGC's
                // `[warning][gc]` `max_map_count` lines come from its Linux
                // backing after the backing-filesystem line, before the
                // uncommit lines, whatever `gc+init` selects.
                let warning = if f.collector == GC_NAME_ZGC {
                    let max = u64::try_from(f.heap_max).unwrap_or(u64::MAX);
                    zgc_max_map_count_warning(max, f.zgc_linux_backing.as_ref())
                } else {
                    Vec::new()
                };
                let lines = if init_on { gc_init_lines(f) } else { Vec::new() };
                let split = if warning.is_empty() {
                    lines.len()
                } else {
                    lines
                        .iter()
                        .position(|l| l.starts_with("Uncommit:") || l.starts_with("Runtime Workers:"))
                        .unwrap_or(lines.len())
                };
                // zki: a refused uncommit probe's error is logged where
                // HotSpot's probe runs, just before its `Uncommit:` line (the
                // first of `errors`); the startup commit's after the block.
                let mut errors = errors.iter();
                let mut log_init = |line: &String| {
                    if f.zgc_uncommit_unsupported
                        && line.starts_with("Uncommit: Implicitly Disabled (Not supported")
                    {
                        if let Some(text) = errors.next() {
                            log_gc_startup_level(&logger, LogLevel::Error, text);
                        }
                    }
                    logger.log(&[LogTag::GcInit], LogLevel::Info, line);
                };
                for line in &lines[..split] {
                    log_init(line);
                }
                for text in &warning {
                    log_gc_startup_warning(&logger, text);
                }
                for line in &lines[split..] {
                    log_init(line);
                }
                for text in errors {
                    log_gc_startup_level(&logger, LogLevel::Error, text);
                }
            }
        }
    }
}

/// One group of [`log_gc_startup`]'s lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupLine {
    /// `[gc,init] CardTable entry size: 512` (`CardTable::initialize_card_size`).
    CardTable,
    /// gcg g3/x: `[debug][gc,heap] Minimum heap ...` (Serial only).
    HeapSizesDebug,
    /// `[gc] Using <name>`.
    Using,
    /// The `gc,init` block ([`gc_init_lines`]).
    InitBlock,
}

/// HotSpot 25's startup order per collector (measured on 25.0.3 Serial,
/// gcf f2/r and gcg g3/x; on 25.0.4 G1 and ZGC, gcd d10/o retirement):
/// Serial and G1 print the card size, then `Using`, then the block (Serial's
/// debug heap sizes come before `Using`); ZGC has no card table and prints
/// its block -- opening with `Initializing The Z Garbage Collector` -- BEFORE
/// `Using`.
pub fn startup_line_order(f: &GcInitFacts<'_>) -> &'static [StartupLine] {
    use StartupLine::*;
    match f.collector {
        GC_NAME_SERIAL => &[CardTable, HeapSizesDebug, Using, InitBlock],
        GC_NAME_ZGC => &[InitBlock, Using],
        _ => &[CardTable, Using, InitBlock],
    }
}

// ---------------------------------------------------------------------------
// GC exit heap summary (gcf f2/r)
// ---------------------------------------------------------------------------

/// One space (or generation) of the exit heap summary: its capacity and
/// occupancy in bytes and its `[bottom, top, end)` address triple (for a
/// generation, HotSpot's `VirtualSpace` triple `[low_boundary, high,
/// high_boundary)`: reserved start, committed end, reserved end).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitSpace {
    pub capacity: u64,
    pub used: u64,
    pub bounds: [u64; 3],
}

/// What HotSpot Serial's `SerialHeap::print_on` prints at exit, as this VM's
/// Generational backend can state it (see `gc_events::serial_exit_heap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SerialExitHeap {
    /// `DefNew`: eden + from capacity (the to-space is listed, not counted).
    pub young: ExitSpace,
    pub eden: ExitSpace,
    pub from: ExitSpace,
    pub to: ExitSpace,
    /// `Tenured` and its one space.
    pub old: ExitSpace,
    pub the: ExitSpace,
    /// `Metaspace used / committed / reserved` in bytes (both spaces).
    pub metaspace: (u64, u64, u64),
    /// gci i33/ms: the `class space` line's `used / committed / reserved`
    /// (the compressed class space's part of `metaspace`, from
    /// `vm_init::metaspace_stats`); `None` prints no such line, HotSpot's
    /// `-XX:-UseCompressedClassPointers` shape (this VM's before i33, and its
    /// `CRATONVM_METASPACE_HOTSPOT_SPACES=0` arm).
    pub class_space: Option<(u64, u64, u64)>,
}

/// gci i33/ms (`gci-i33ms-metaspace-spaces-and-limit-FIXED-20261016`): HotSpot 25's
/// `MetaspaceUtils::print_on` -- the ` Metaspace` line, then, with a
/// compressed class space, its indented `class space` line -- in KiB,
/// truncated: what the `gc,exit` summaries, `GC.heap_info`, the SIGQUIT
/// `Heap` section and the debug `Metaspace Before|After GC` blocks print.
pub fn metaspace_print_on_lines(
    metaspace: (u64, u64, u64),
    class_space: Option<(u64, u64, u64)>,
) -> Vec<String> {
    let (used, committed, reserved) = metaspace;
    let mut lines = vec![format!(
        " Metaspace       used {}K, committed {}K, reserved {}K",
        used / 1024,
        committed / 1024,
        reserved / 1024
    )];
    if let Some((used, committed, reserved)) = class_space {
        lines.push(format!(
            "  class space    used {}K, committed {}K, reserved {}K",
            used / 1024,
            committed / 1024,
            reserved / 1024
        ));
    }
    lines
}

/// gci i33/ms: is `line` one of [`metaspace_print_on_lines`]'s (the debug
/// blocks file them under `gc,metaspace`, the rest under `gc,heap`)?
pub fn is_metaspace_print_on_line(line: &str) -> bool {
    line.starts_with(" Metaspace") || line.starts_with("  class space")
}

/// gci i33/ms: the exit summaries' metaspace figures now --
/// `vm_init::metaspace_stats`, one class-store walk:
/// `(used, committed, reserved)` and the class space's.
fn exit_metaspace_figures(
    shared: &crate::vm::SharedVm,
) -> ((u64, u64, u64), Option<(u64, u64, u64)>) {
    let stats = crate::vm::vm_init::metaspace_stats(&shared.classes.class_manager.read());
    (stats.triple(), stats.class_space)
}

/// `PTR_FORMAT` on a 64-bit VM: `0x` and sixteen lower-case hex digits.
fn ptr_format(p: u64) -> String {
    format!("{p:#018x}")
}

/// `ContiguousSpace::print_on`'s `%3d%%`: `(int)((double)used * 100 / capacity)`,
/// truncated; 0 for an empty space (HotSpot divides by zero there).
fn space_percent(s: &ExitSpace) -> u64 {
    if s.capacity == 0 {
        0
    } else {
        // Truncating, as the C cast; u128 so `used * 100` cannot wrap.
        u64::try_from(u128::from(s.used) * 100 / u128::from(s.capacity)).unwrap_or(u64::MAX)
    }
}

/// The `gc,exit` lines, in `Universe::print_on` / `SerialHeap::print_on`'s
/// order and with their exact spacing (HotSpot 25.0.3, gcf f1/r measurement):
///
/// ```text
/// Heap
///  DefNew     total 19648K, used 174K [0x00000000fc000000, 0x00000000fd550000, 0x00000000fd550000)
///   eden space 17472K,   1% used [0x00000000fc000000, 0x00000000fc02bb50, 0x00000000fd110000)
///   from space 2176K,   0% used [..]
///   to   space 2176K,   0% used [..]
///  Tenured    total 43712K, used 1279K [..]
///   the  space 43712K,   2% used [..]
///  Metaspace       used 94K, committed 320K, reserved 1114112K
/// ```
pub fn serial_exit_heap_lines(h: &SerialExitHeap) -> Vec<String> {
    let triple = |b: &[u64; 3]| {
        format!(
            "[{}, {}, {})",
            ptr_format(b[0]),
            ptr_format(b[1]),
            ptr_format(b[2])
        )
    };
    let generation = |name: &str, g: &ExitSpace| {
        format!(
            " {name:<10} total {}K, used {}K {}",
            g.capacity / 1024,
            g.used / 1024,
            triple(&g.bounds)
        )
    };
    let space = |prefix: &str, s: &ExitSpace| {
        format!(
            "  {prefix} space {}K, {:>3}% used {}",
            s.capacity / 1024,
            space_percent(s),
            triple(&s.bounds)
        )
    };
    let mut lines = vec![
        "Heap".to_string(),
        generation("DefNew", &h.young),
        space("eden", &h.eden),
        space("from", &h.from),
        space("to  ", &h.to),
        generation("Tenured", &h.old),
        space("the ", &h.the),
    ];
    // gci i33/ms: and the `class space` line when there is one.
    lines.extend(metaspace_print_on_lines(h.metaspace, h.class_space));
    lines
}

/// The exit summary's figures on the Generational backend; `None` on G1 and
/// ZGC (HotSpot prints those summaries in their own grammar, not attempted).
///
/// The sizes are the JMX pools' (`GenerationalHeap::jmx_memory_pools`), so the
/// summary agrees with the per-collection `gc,heap` lines and with
/// `MemoryPoolMXBean`: `DefNew` = `Eden Space` + `Survivor Space`, `from` = the
/// survivors, `Tenured` = `Tenured Gen`. The addresses come from the arenas
/// (`GenerationalHeap::exit_summary_arenas`). This collector has no separate
/// survivor space: the survivors sit inside the active semispace. How the
/// spaces are laid over the two semispaces (non-overlapping, `to` = `from`)
/// is [`serial_exit_heap_from`]'s (gcg g1/c). A GCeasy / GCViewer parse, not
/// a byte diff, is the oracle for the addresses.
///
/// Metaspace is the class-metadata figure the heap summary and the non-heap
/// `MemoryUsage` report; gci i33/ms: with HotSpot's committed, reserved and
/// `class space` figures (`vm_init::metaspace_stats`; used = committed =
/// reserved and no class space under `CRATONVM_METASPACE_HOTSPOT_SPACES=0`).
pub fn serial_exit_heap(shared: &crate::vm::SharedVm) -> Option<SerialExitHeap> {
    use cratonvm_gc::gc_metrics::{JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED};
    let Some(pools) = shared.mem.heap.gen_jmx_memory_pools() else {
        return None;
    };
    let h_committed = shared.mem.heap.gen_jmx_collection_committed().unwrap_or([None; 3]);
    let pool = |name: &str| {
        pools
            .iter()
            .find(|p| p.name == name)
            .map(|p| (p.used, p.committed))
    };
    let (eden_used, eden_committed) = pool(JMX_POOL_EDEN)?;
    let (surv_used, surv_committed) = pool(JMX_POOL_SURVIVOR)?;
    let (old_used, old_committed) = pool(JMX_POOL_TENURED)?;
    // `(base, used, committed, capacity)` per arena; widening casts.
    let widen = |(base, used, committed, cap): (usize, usize, usize, usize)| {
        (base as u64, used as u64, committed as u64, cap as u64)
    };
    let h = &shared.mem.heap;
    let [from, to, old] = h.exit_summary_arenas().map(widen);
    // gci i33/ms: HotSpot's metaspace figures (committed by granule, the
    // reservations, the class space), one walk; the heap summary's
    // `metaspace_used` is their `used`.
    let (metaspace, class_space) = exit_metaspace_figures(shared);
    let mut heap = serial_exit_heap_from(
        ExitPools {
            eden: (eden_used, eden_committed),
            survivor: (surv_used, surv_committed),
            tenured: (old_used, old_committed),
        },
        from,
        to,
        old,
        metaspace.0,
    );
    heap.metaspace = metaspace;
    heap.class_space = class_space;
    Some(heap)
}

/// The JMX pools' `(used, committed)` the exit summary is built from
/// ([`serial_exit_heap_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitPools {
    pub eden: (u64, u64),
    pub survivor: (u64, u64),
    pub tenured: (u64, u64),
}

/// Round `n` up to a whole KiB.
fn kib_ceil(n: u64) -> u64 {
    n.div_ceil(1024).saturating_mul(1024)
}

/// gcg g1/c: the young generation's `(used, capacity)` split into HotSpot
/// Serial's `[DefNew, eden, from]` so the printed KiB add up: the survivors'
/// capacity is their committed bytes rounded UP to a KiB (and never below
/// their used bytes), eden's is the young commit floored to a KiB less that,
/// and `DefNew`'s is the sum. The JMX pools carve the survivors out of the
/// young commit byte for byte (`GenerationalHeap::jmx_memory_pools`), so
/// `eden/1024 + from/1024` printed one KiB short of `(eden + from)/1024`
/// whenever the survivor count was not a KiB multiple -- the vf2 r2-2
/// `DefNew total 12288K` over `eden 12231K + from 56K`. HotSpot's spaces are
/// aligned, so its three figures always add up. Used by the exit summary
/// ([`serial_exit_heap_from`]) and the per-collection `gc,heap` line
/// (`gc_events::render_xlog_heap_lines`). Each `used` is clamped to its
/// capacity.
pub fn serial_young_kib_split(eden: (u64, u64), survivor: (u64, u64)) -> [(u64, u64); 3] {
    let (eden_used, eden_committed) = eden;
    let (surv_used, surv_committed) = survivor;
    let young_k = eden_committed.saturating_add(surv_committed) / 1024 * 1024;
    let surv_cap = kib_ceil(surv_committed.max(surv_used)).min(young_k);
    let eden_cap = young_k - surv_cap;
    let surv_used = surv_used.min(surv_cap);
    let eden_used = eden_used.min(eden_cap);
    [
        (eden_used.saturating_add(surv_used), young_k),
        (eden_used, eden_cap),
        (surv_used, surv_cap),
    ]
}

/// The exit summary's figures from the JMX pools and the three arenas'
/// `(base, used, committed, capacity)`. Pure, so the arithmetic is
/// unit-tested without a VM ([`serial_exit_heap`] is the wrapper).
///
/// gcg g1/c (`gcf-f1r-xlog-exit-heap-summary-is-tag-gc-exit-and-not-printed`,
/// the vf2 r2-2 sanity check): HotSpot's spaces never overlap, `DefNew total`
/// is exactly `eden + from` in the printed KiB, every space's `top` agrees with
/// its `% used`, and `to` has `from`'s capacity. The f2/r version broke all
/// four: it printed `DefNew total 12288K` over `eden 12231K + from 56K` (two
/// truncations of unaligned byte counts), eden's `top` was the arena's bump
/// high-water (about 97 % of the span after a non-moving sweep, against a
/// `0% used` read from the live pool), `from` started at eden's bottom, and
/// `to` was the whole inactive semispace (16384K against `from` 56K).
///
/// The layout now: the active semispace holds the survivors at its base (a
/// moving cycle copies them there), so `from` is `[base, base + survivors,
/// base + S)` with `S` the survivors rounded up to a KiB, and `eden` is the
/// rest of the young commit above it, `[base + S, base + S + eden_used,
/// base + S + eden_capacity)`. Eden's capacity is the young commit (floored to
/// a KiB) less `S`, so `DefNew`'s KiB total is the sum of the two printed
/// KiB figures. `to` is `S` bytes at the inactive semispace's base: HotSpot's
/// `to` always equals `from`, and the rest of the inactive semispace is this
/// collector's copy reserve, which no HotSpot space and no JMX pool counts.
/// A space's `top` is `bottom + used`, as the old generation's already was
/// (a free list makes the bump top an over-statement).
pub fn serial_exit_heap_from(
    pools: ExitPools,
    from: (u64, u64, u64, u64),
    to: (u64, u64, u64, u64),
    old: (u64, u64, u64, u64),
    metaspace: u64,
) -> SerialExitHeap {
    let at = |base: u64, off: u64| base.saturating_add(off);
    let (old_used, old_committed) = pools.tenured;
    let (fb, _, _, fcap) = from;
    let (tb, _, _, _) = to;
    let (ob, _, oc, ocap) = old;
    let [(young_used, young_total_k), (eden_used, eden_cap), (surv_used, surv_cap)] =
        serial_young_kib_split(pools.eden, pools.survivor);
    let eb = at(fb, surv_cap);
    SerialExitHeap {
        young: ExitSpace {
            capacity: young_total_k,
            used: young_used,
            bounds: [fb, at(fb, young_total_k), at(fb, fcap.max(young_total_k))],
        },
        eden: ExitSpace {
            capacity: eden_cap,
            used: eden_used,
            bounds: [eb, at(eb, eden_used), at(eb, eden_cap)],
        },
        from: ExitSpace {
            capacity: surv_cap,
            used: surv_used,
            bounds: [fb, at(fb, surv_used), at(fb, surv_cap)],
        },
        to: ExitSpace {
            capacity: surv_cap,
            used: 0,
            bounds: [tb, tb, at(tb, surv_cap)],
        },
        old: ExitSpace {
            capacity: old_committed,
            used: old_used,
            bounds: [ob, at(ob, oc.min(ocap)), at(ob, ocap)],
        },
        the: ExitSpace {
            capacity: old_committed,
            used: old_used,
            bounds: [ob, at(ob, old_used), at(ob, oc.min(ocap))],
        },
        metaspace: (metaspace, metaspace, metaspace),
        // gci i33/ms: `serial_exit_heap` sets HotSpot's figures over these.
        class_space: None,
    }
}

/// gci i28/tl (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 9): may this
/// collection print HotSpot's `gc,tlab` debug `TLAB totals` line? `events`
/// is the collection's own "emit the events" verdict; Serial-shaped
/// (Generational) and G1 heaps only (ZGC is not this round's).
fn tlab_totals_wanted(shared: &crate::vm::SharedVm, events: bool) -> bool {
    events
        && (shared.mem.heap.is_generational() || shared.mem.heap.is_g1())
        && is_unified_logging_enabled(&[LogTag::GcTlab], LogLevel::Debug)
}

/// gci i28/tl: at a collection's start, with the world stopped (`gc_event_start`,
/// HotSpot's `ensure_parsability` point): difference every live thread's TLAB
/// cycle counters against the previous collection's marks
/// (`TlabTotalsState::publish`) and stash HotSpot's
/// `ThreadLocalAllocStats::publish` line for [`log_tlab_totals`]. Clears the
/// stash otherwise, so a line no writer took never prints under a later
/// collection. Nothing but that clear unless `-Xlog:gc+tlab=debug` is on.
pub fn stash_tlab_totals(shared: &crate::vm::SharedVm, events: bool) {
    let state = &shared.mem.tlab_wedge.tlab_totals;
    let line = if tlab_totals_wanted(shared, events) {
        state
            .publish(shared.threads.thread_registry.tlab_cycle_snapshots())
            .line()
    } else {
        None
    };
    state.stash(line);
}

/// gci i30/ob (`i28-tl-report.md` review item 6): take the line
/// [`stash_tlab_totals`] just computed, at the collection's start, so it
/// travels with that collection's start record (`GcEventStart::tlab_totals`)
/// instead of waiting in the per-VM stash, which the next collection's start
/// overwrites -- a G1 young report deferred past another pause's start lost
/// its line, or printed the later pause's.
pub fn take_tlab_totals(shared: &crate::vm::SharedVm) -> Option<String> {
    shared.mem.tlab_wedge.tlab_totals.take()
}

/// gci i28/tl: print the line [`stash_tlab_totals`] computed for collection
/// `gc_id`, at `gc,tlab` debug -- after the `Heap Before GC` block, as
/// HotSpot Serial's `gc_prologue` (`ensure_parsability`) runs after
/// `print_heap_before_gc`. gci i30/ob: the line comes from the collection's
/// own start record ([`take_tlab_totals`]); `None` prints nothing.
pub fn log_tlab_totals(line: Option<&str>, gc_id: u64) {
    if let Some(line) = line {
        log_unified(&[LogTag::GcTlab], LogLevel::Debug, &tlab_totals_line(gc_id, line));
    }
}

/// `GC(<n>) <line>`: the GC-id prefix HotSpot's `gc` tag sets carry.
fn tlab_totals_line(gc_id: u64, line: &str) -> String {
    format!("GC({gc_id}) {line}")
}

/// zr z9/hp: what HotSpot 25 ZGC prints under `gc,exit` at VM exit, as far as
/// this VM can state it (25.0.3, `-XX:+UseZGC -Xmx512m -Xlog:gc+exit`):
///
/// ```text
/// Stopping ZGC
/// Heap
///  ZHeap           used 8M, capacity 254M, max capacity 512M
///   Cache          246M (1)
///    size classes  128M (1)
///  Metaspace       used 140K, committed 256K, reserved 1114112K
///   class space    used 5K, committed 64K, reserved 1048576K
/// ```
///
/// `Stopping ZGC` is `ZCollectedHeap::stop`'s line; the `ZHeap` line is
/// `ZHeap::print_on` (used, committed capacity and maximum, each truncated to
/// MiB). The two `Cache` lines describe HotSpot's page cache (committed pages
/// not in use, by size class), a structure this heap does not have, and the
/// `class space` line its compressed class space: the two `Cache` lines are
/// not printed; gci i33/ms: the `class space` line is, with the model's
/// class space (`class_space`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZgcExitHeap {
    /// `Runtime.totalMemory() - freeMemory()`: the heap's allocated bytes.
    pub used: u64,
    /// The committed heap (`Runtime.totalMemory()`).
    pub capacity: u64,
    /// `-Xmx`.
    pub max_capacity: u64,
    /// `Metaspace used / committed / reserved` in bytes (the Serial summary's
    /// figures).
    pub metaspace: (u64, u64, u64),
    /// gci i33/ms: the `class space` line's figures (as
    /// [`SerialExitHeap::class_space`]).
    pub class_space: Option<(u64, u64, u64)>,
}

/// [`ZgcExitHeap`]'s lines, HotSpot's spacing.
pub fn zgc_exit_heap_lines(h: &ZgcExitHeap) -> Vec<String> {
    const M: u64 = 1024 * 1024;
    let mut lines = vec![
        "Stopping ZGC".to_string(),
        "Heap".to_string(),
        format!(
            " {:<16}used {}M, capacity {}M, max capacity {}M",
            "ZHeap",
            h.used / M,
            h.capacity / M,
            h.max_capacity / M
        ),
    ];
    lines.extend(metaspace_print_on_lines(h.metaspace, h.class_space));
    lines
}



/// The ZGC exit summary's figures; `None` off ZGC or with
/// `CRATONVM_ZGC_EXIT_HEAP_SUMMARY=0`.
pub fn zgc_exit_heap(shared: &crate::vm::SharedVm) -> Option<ZgcExitHeap> {
    let heap = &shared.mem.heap;
    if heap.is_generational() || heap.is_g1() {
        return None;
    }
    // zt t23/x19 (page item 28d; `CRATONVM_ZGC_EXIT_FINISHES_SWEEP`, default
    // ON, `=0` kill): a deferred sweep still pending at exit (an exit from
    // another thread while the collecting thread sweeps) is finished first,
    // so `used` is the cycle's end, not the unswept heap. One load otherwise.
    let _ = heap.zgc_finish_pending_sweep_at_exit();
    let w = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let (metaspace, class_space) = exit_metaspace_figures(shared);
    Some(ZgcExitHeap {
        used: w(heap.allocated_bytes()),
        capacity: w(heap.committed_bytes()),
        max_capacity: w(shared.config.max_heap_size),
        metaspace,
        class_space,
    })
}

/// The `gc,exit` heap summary's lines for this VM's backend: Serial's
/// ([`serial_exit_heap`]) on Generational, G1's ([`g1_exit_heap_lines`], gci
/// i14/hd, through [`exit_heap_lines`]) on G1, ZGC's ([`zgc_exit_heap`], zr
/// z9/hp) on ZGC. The 2026-10-15 merge of dev joined the two sides' backends.
pub fn gc_exit_heap_lines(shared: &crate::vm::SharedVm) -> Option<Vec<String>> {
    if let Some(lines) = exit_heap_lines(shared) {
        return Some(lines);
    }
    zgc_exit_heap(shared).map(|h| zgc_exit_heap_lines(&h))
}

/// Print the exit heap summary under `gc,exit` (`-Xlog:gc+exit`, `gc*`), once
/// per process, from whichever launcher exit arm gets here first. `build` runs
/// only when the tag set is selected and this call won the claim; `None` (a
/// backend whose HotSpot summary has another grammar) prints nothing. A no-op
/// without a logger. zr z9/hp: `build` yields the lines
/// ([`gc_exit_heap_lines`]), so ZGC's summary goes through the same claim.
pub fn log_gc_exit_summary(build: impl FnOnce() -> Option<Vec<String>>) {
    log_gc_exit_summary_lines(build);
}

/// gci i14/hd: [`log_gc_exit_summary`] for any backend's lines (the gci
/// launcher passed [`exit_heap_lines`]: Serial's, and G1's since i14; dev's
/// zr z9/hp made `log_gc_exit_summary` itself take lines, so since the
/// 2026-10-15 merge the two are one function). The same once-per-process
/// claim; `build` runs only when the tag set is selected and this call won it.
pub fn log_gc_exit_summary_lines(build: impl FnOnce() -> Option<Vec<String>>) {
    let Some(logger) = current_logger() else {
        return;
    };
    if !logger.is_enabled(&[LogTag::GcExit], LogLevel::Info) {
        return;
    }
    if logger
        .exit_summary_claimed
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let Some(lines) = build() else {
        return;
    };
    for line in lines {
        logger.log(&[LogTag::GcExit], LogLevel::Info, &line);
    }
}

/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 6): what HotSpot 25 G1's `G1CollectedHeap::print_heap_on` prints,
/// under `Universe::print_on`'s `Heap` line and before the Metaspace line:
/// the `gc,exit` summary, `jcmd GC.heap_info` and the SIGQUIT `Heap`
/// section. G1 printed no exit block at all, and the other two printed this
/// VM's own `Young Generation: ...` / `java heap total ...` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct G1ExitHeap {
    /// `total reserved`: the reservation (`-Xmx` on the region grid).
    pub reserved: u64,
    /// `committed`: the committed prefix of the reservation.
    pub committed: u64,
    /// `used`: every non-free region's used bytes (a humongous object once).
    pub used: u64,
    /// The reservation's `[start, end)`.
    pub bounds: [u64; 2],
    pub region_size: u64,
    /// Eden plus survivor regions (`young_regions_count`).
    pub young_regions: u64,
    pub survivor_regions: u64,
    /// `Metaspace used / committed / reserved` in bytes (as Serial's).
    pub metaspace: (u64, u64, u64),
    /// gci i33/ms: the `class space` line's figures (as
    /// [`SerialExitHeap::class_space`]).
    pub class_space: Option<(u64, u64, u64)>,
}

/// The G1 lines in HotSpot 25.0.4's text (measured SIGQUIT section,
/// `interpreter-i59-L1-sigquit-terminates-the-vm`: ` garbage-first heap   total
/// reserved 8216576K, committed 520192K, used 3133K [...)`):
///
/// ```text
/// Heap
///  garbage-first heap   total reserved 65536K, committed 4096K, used 1024K [0x.., 0x..)
///   region size 1024K, 2 young (2048K), 0 survivors (0K)
///  Metaspace       used 94K, committed 94K, reserved 94K
/// ```
///
/// gci i33/ms: then the `class space` line when `class_space` is set (as
/// [`serial_exit_heap_lines`]). Pure.
pub fn g1_exit_heap_lines(h: &G1ExitHeap) -> Vec<String> {
    let k = |b: u64| b / 1024;
    let mut lines = vec![
        "Heap".to_string(),
        format!(
            " {:<20} total reserved {}K, committed {}K, used {}K [{}, {})",
            "garbage-first heap",
            k(h.reserved),
            k(h.committed),
            k(h.used),
            ptr_format(h.bounds[0]),
            ptr_format(h.bounds[1])
        ),
        format!(
            "  region size {}K, {} young ({}K), {} survivors ({}K)",
            k(h.region_size),
            h.young_regions,
            k(h.young_regions.saturating_mul(h.region_size)),
            h.survivor_regions,
            k(h.survivor_regions.saturating_mul(h.region_size))
        ),
    ];
    lines.extend(metaspace_print_on_lines(h.metaspace, h.class_space));
    lines
}

/// The G1 figures of [`g1_exit_heap_lines`]; `None` off G1. One region-table
/// walk (`G1Collector::region_census`), the arena's reservation and commit,
/// and the measured class metadata.
pub fn g1_exit_heap(shared: &crate::vm::SharedVm) -> Option<G1ExitHeap> {
    let h = &shared.mem.heap;
    let census = h.region_census()?;
    let (start, end) = h.reserved_span();
    let used = census.eden.0 + census.survivor.0 + census.old.0 + census.humongous.0;
    let (metaspace, class_space) = exit_metaspace_figures(shared);
    Some(G1ExitHeap {
        reserved: h.reserved_bytes() as u64,
        committed: h.committed_bytes() as u64,
        used: used as u64,
        bounds: [start as u64, end as u64],
        region_size: census.region_size as u64,
        young_regions: (census.eden.1 + census.survivor.1) as u64,
        survivor_regions: census.survivor.1 as u64,
        metaspace,
        class_space,
    })
}

/// gci i14/hd: HotSpot's `Universe::print_on` lines for this VM's collector
/// -- Serial's on Generational ([`serial_exit_heap_lines`]), G1's
/// ([`g1_exit_heap_lines`]); `None` on ZGC (another grammar, not attempted).
pub fn exit_heap_lines(shared: &crate::vm::SharedVm) -> Option<Vec<String>> {
    if let Some(heap) = serial_exit_heap(shared) {
        return Some(serial_exit_heap_lines(&heap));
    }
    g1_exit_heap(shared).map(|heap| g1_exit_heap_lines(&heap))
}

// ---------------------------------------------------------------------------
// Reference processing: `gc,ref` and `gc,phases,ref` at debug (gcg g3/x)
// ---------------------------------------------------------------------------

/// One pause's reference-processing census in HotSpot's terms
/// (`ReferenceProcessorPhaseTimes::print_all_references` /
/// `print_phase`). Index order of the four-kind arrays: soft, weak, final,
/// phantom.
///
/// The semantics are HotSpot's, and the producer must honour them
/// (`gcg-g2r-xlog-gc-ref-debug-and-phases-ref-print-nothing`, "a semantic
/// trap"): `discovered` counts the references discovered BY THIS collection
/// (referent not marked; for a soft reference, also clearable by the LRU
/// policy) -- NOT `ReferenceProcessingStats::*_discovered`, which is the
/// length of each list when the round began; `dropped` the discovered ones
/// whose referent turned out live; `processed` the ones cleared / enqueued
/// by this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefProcCensus {
    pub discovered: [u64; 4],
    pub dropped: [u64; 4],
    pub processed: [u64; 4],
    /// The whole of reference processing, nanoseconds.
    pub total_ns: u64,
    /// The three HotSpot phases: notify soft/weak (and final) references,
    /// keep finalizable objects alive, notify phantom references.
    pub phase_ns: [u64; 3],
    /// Phase 1's per-kind times (soft, weak, final).
    pub phase1_kind_ns: [u64; 3],
}

/// HotSpot's `%.1fms` of a nanosecond figure.
fn ref_ms(ns: u64) -> String {
    format!("{:.1}ms", ns as f64 / 1_000_000.0)
}

/// gcg g3/x: the debug lines HotSpot 25 prints for one pause's reference
/// processing, in its order (measured on 25.0.3 Serial, young and full
/// collections): first the `gc,ref` lines -- per phase, either
/// `ReferenceProcessor::execute queues: 1, RefProcThreadModel::Single,
/// marks_oops_alive: <b>` (the phase ran; `true` only for the keep-alive
/// phase) or `Skipped <Phase> of Reference Processing: no references` --
/// then the `gc,phases,ref` block:
///
/// ```text
/// GC(0) Reference Processing: 0.1ms
/// GC(0)   Notify Soft/WeakReferences: 0.1ms
/// GC(0)     SoftRef: 0.0ms            (or `skipped` when phase 1 did not run)
/// GC(0)     WeakRef: 0.0ms
/// GC(0)     FinalRef: 0.0ms
/// GC(0)     Total: 0.0ms
/// GC(0)   Notify and keep alive finalizable: 0.0ms
/// GC(0)     FinalRef: skipped
/// GC(0)   Notify PhantomReferences: 0.0ms
/// GC(0)     PhantomRef: skipped
/// GC(0)   SoftReference Discovered: 0, Dropped: 0, Processed: 0
/// GC(0)   WeakReference Discovered: 6, Dropped: 6, Processed: 0
/// GC(0)   FinalReference Discovered: 0, Dropped: 0, Processed: 0
/// GC(0)   PhantomReference Discovered: 0, Dropped: 0, Processed: 0
/// ```
///
/// A phase runs when it has discovered references: phase 1 for soft + weak +
/// final, phase 2 for final, phase 3 for phantom (HotSpot's
/// `ReferenceProcessor::process_*` early returns). Pure.
pub fn xlog_reference_lines(gc_id: u64, c: &RefProcCensus) -> Vec<(LogTag, LogLevel, String)> {
    xlog_reference_lines_at(gc_id, c, 0)
}

/// gci i15/rf (`gci-i14gq-g1-reference-processing-and-finalization-review-findings-FIXED-20261016`
/// G-7): HotSpot G1's evacuation pause prints the `gc,phases,ref` block
/// inside `Post Evacuate Collection Set`
/// (`G1GCPhaseTimes::print_post_evacuate_collection_set`:
/// `debug_time_for_reference("Reference Processing", ..)` at `Indents[2]`,
/// then `print_all_references(2, false)`), so every line of the block sits
/// two `Indents` steps (4 spaces) deeper than Serial's / a G1 Full GC's /
/// G1's remark's (`print_all_references()`, base 0). The block of
/// [`xlog_reference_lines`] with each `gc,phases,ref` line shifted by
/// `indent_levels` HotSpot `Indents` steps (2 spaces each); the `gc,ref`
/// lines are not indented. `0` is [`xlog_reference_lines`]. Pure.
pub fn xlog_reference_lines_at(
    gc_id: u64,
    c: &RefProcCensus,
    indent_levels: usize,
) -> Vec<(LogTag, LogLevel, String)> {
    let base = "  ".repeat(indent_levels);
    let [soft, weak, fin, phantom] = c.discovered;
    let ran = [
        soft.saturating_add(weak).saturating_add(fin) > 0,
        fin > 0,
        phantom > 0,
    ];
    let names = [
        "SoftWeakFinalRefsPhase",
        "KeepAliveFinalRefsPhase",
        "PhantomRefsPhase",
    ];
    let mut out = Vec::with_capacity(17);
    let d = LogLevel::Debug;
    for (i, name) in names.iter().enumerate() {
        let text = if ran[i] {
            format!(
                "GC({gc_id}) ReferenceProcessor::execute queues: 1, RefProcThreadModel::Single, \
                 marks_oops_alive: {}",
                i == 1
            )
        } else {
            format!("GC({gc_id}) Skipped {name} of Reference Processing: no references")
        };
        out.push((LogTag::GcRef, d, text));
    }
    let p = |text: String| {
        (
            LogTag::GcPhasesRef,
            LogLevel::Debug,
            format!("GC({gc_id}) {base}{text}"),
        )
    };
    let sub = |ran: bool, ns: u64| {
        if ran {
            ref_ms(ns)
        } else {
            "skipped".to_string()
        }
    };
    out.push(p(format!("Reference Processing: {}", ref_ms(c.total_ns))));
    out.push(p(format!(
        "  Notify Soft/WeakReferences: {}",
        ref_ms(c.phase_ns[0])
    )));
    for (k, name) in ["SoftRef", "WeakRef", "FinalRef"].iter().enumerate() {
        out.push(p(format!(
            "    {name}: {}",
            sub(ran[0], c.phase1_kind_ns[k])
        )));
    }
    let phase1_total = c
        .phase1_kind_ns
        .iter()
        .fold(0u64, |a, &b| a.saturating_add(b));
    out.push(p(format!("    Total: {}", sub(ran[0], phase1_total))));
    out.push(p(format!(
        "  Notify and keep alive finalizable: {}",
        ref_ms(c.phase_ns[1])
    )));
    out.push(p(format!("    FinalRef: {}", sub(ran[1], c.phase_ns[1]))));
    out.push(p(format!(
        "  Notify PhantomReferences: {}",
        ref_ms(c.phase_ns[2])
    )));
    out.push(p(format!("    PhantomRef: {}", sub(ran[2], c.phase_ns[2]))));
    for (k, kind) in ["Soft", "Weak", "Final", "Phantom"].iter().enumerate() {
        out.push(p(format!(
            "  {kind}Reference Discovered: {}, Dropped: {}, Processed: {}",
            c.discovered[k], c.dropped[k], c.processed[k]
        )));
    }
    out
}

/// gcg g3/x: is `-Xlog` taking the reference-processing block (`gc,ref` or
/// `gc,phases,ref` at debug)? A producer checks this before building a
/// [`RefProcCensus`] (the census is per-pause work).
pub fn xlog_reference_lines_wanted() -> bool {
    is_unified_logging_enabled(&[LogTag::GcRef, LogTag::GcPhasesRef], LogLevel::Debug)
}

/// gcg g3/x: render this pause's reference-processing block
/// ([`xlog_reference_lines`]) and hold it for [`flush_deferred_gc_lines`] --
/// called INSIDE the pause, by the reference-processing step; nothing is
/// written there. `gc_id` is the id the collection's own lines will carry
/// (the VM's `gc_cycle_count` before `gc_event_finish` advances it; a G1
/// remark passes its concurrent cycle's id, gci i12/ge,
/// `gc_and_alloc::remark_reference_xlog_id`). A no-op unless
/// [`xlog_reference_lines_wanted`].
pub fn defer_xlog_reference_processing(gc_id: u64, census: &RefProcCensus) {
    defer_xlog_reference_processing_at(gc_id, census, 0);
}

/// gci i15/rf (G-7): [`defer_xlog_reference_processing`] with the
/// `gc,phases,ref` block shifted by `indent_levels` HotSpot `Indents` steps
/// ([`xlog_reference_lines_at`]; `2` for a G1 evacuation pause's block).
pub fn defer_xlog_reference_processing_at(
    gc_id: u64,
    census: &RefProcCensus,
    indent_levels: usize,
) {
    let Some(logger) = current_logger() else {
        return;
    };
    if !logger.is_enabled(&[LogTag::GcRef, LogTag::GcPhasesRef], LogLevel::Debug) {
        return;
    }
    let block = xlog_reference_lines_at(gc_id, census, indent_levels);
    let mut q = match logger.deferred.lock() {
        Ok(q) => q,
        Err(poisoned) => poisoned.into_inner(),
    };
    deferred_gc_lines_push(&mut q, gc_id, block, DEFERRED_GC_LINES_CAP);
}

/// One held line: `(gc_id, tag, level, text)`.
type DeferredGcLine = (u64, LogTag, LogLevel, String);

/// gci i28/dg (i27/ob review item 3): add collection `gc_id`'s block to the
/// held lines, keeping every other id's block (each id is its own slot: a
/// pause whose block is held must not lose it because another thread's pause
/// flushed first). Bounded by `cap`: when the block does not fit, the OLDEST
/// other id's whole block is evicted first (a pause whose door never reached
/// [`flush_deferred_gc_lines`] can therefore not pin the cap), and only when
/// nothing else is held is this id's own block cut at the cap.
fn deferred_gc_lines_push(
    q: &mut Vec<DeferredGcLine>,
    gc_id: u64,
    block: Vec<(LogTag, LogLevel, String)>,
    cap: usize,
) {
    while q.len() + block.len() > cap {
        let Some(victim) = q.iter().map(|l| l.0).find(|&id| id != gc_id) else {
            break;
        };
        q.retain(|l| l.0 != victim);
    }
    for (tag, level, text) in block {
        if q.len() >= cap {
            break;
        }
        q.push((gc_id, tag, level, text));
    }
}

/// gci i28/dg: remove and return collection `gc_id`'s held lines, in the
/// order they were deferred; every other id's lines stay held.
fn deferred_gc_lines_take(
    q: &mut Vec<DeferredGcLine>,
    gc_id: u64,
) -> Vec<(LogTag, LogLevel, String)> {
    let mut mine = Vec::new();
    let mut rest = Vec::with_capacity(q.len());
    for (id, tag, level, text) in q.drain(..) {
        if id == gc_id {
            mine.push((tag, level, text));
        } else {
            rest.push((id, tag, level, text));
        }
    }
    *q = rest;
    mine
}

/// gcg g3/x: print the lines deferred for collection `gc_id` (each on the
/// outputs that take its tag set and level). Called by the collection's
/// `-Xlog` lines after `gc,start`. A no-op without a logger.
///
/// gci i28/dg: other ids' lines are KEPT for their own flush (before, every
/// other id's block was dropped here, so a pause whose reference block was
/// deferred lost it when another thread's pause flushed first -- widened by
/// i27's deferred G1 concurrent-start report). A block whose flush never
/// comes is evicted by [`deferred_gc_lines_push`] when the cap is reached.
pub fn flush_deferred_gc_lines(gc_id: u64) {
    let Some(logger) = current_logger() else {
        return;
    };
    let lines = {
        let mut q = match logger.deferred.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        if q.is_empty() {
            return;
        }
        deferred_gc_lines_take(&mut q, gc_id)
    };
    for (tag, level, text) in lines {
        logger.log(&[tag], level, &text);
    }
}

/// gcg g4/yg (`gengc-r5w3-obs7-proposal-xlog-remaining-serial-lines` item 3):
/// HotSpot Serial's four full-collection phases (`SerialFullGC`'s
/// `GCTraceTime(Info, gc, phases)` scopes), in order.
pub const SERIAL_FULL_GC_PHASES: [&str; 4] = [
    "Phase 1: Mark live objects",
    "Phase 2: Compute new object addresses",
    "Phase 3: Adjust pointers",
    "Phase 4: Move objects",
];

/// gcg g4/yg: one full-collection phase as HotSpot 25 prints it at info --
/// `(gc,phases,start) GC(n) <phase>` when the scope opens and `(gc,phases)
/// GC(n) <phase> <t>ms` (`%.3fms`) when it closes. `index` is 0..=3 into
/// [`SERIAL_FULL_GC_PHASES`]. Returns `(start_line, end_line)`.
pub fn xlog_full_gc_phase_lines(gc_id: u64, index: usize, phase_ns: u64) -> (String, String) {
    let name = SERIAL_FULL_GC_PHASES[index.min(SERIAL_FULL_GC_PHASES.len() - 1)];
    (
        format!("GC({gc_id}) {name}"),
        format!("GC({gc_id}) {name} {:.3}ms", phase_ns as f64 / 1_000_000.0),
    )
}

/// gcg g4/yg: `CRATONVM_XLOG_GC_FULL_PHASES`: print Serial's four
/// `gc,phases` pairs on every Generational full collection. Read once per
/// collection, only when `gc,phases` or `gc,phases,start` is on at info.
/// gcg g5/yg: default ON (vg4 `yg4_phases_*`: eight lines in HotSpot's
/// order on a full collection, none on young pauses or on G1 / ZGC); `=0`
/// is the kill switch.
pub fn xlog_full_gc_phases_enabled() -> bool {
    true
}

// ---------------------------------------------------------------------------
// `safepoint` (gci i45/sl)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`:
// the tag parsed, had a name and was in `all`, and nothing emitted on it.
// HotSpot 25 prints one info line per safepoint from `SafepointTracing::end`
// (`safepoint.cpp`):
//
//   Safepoint "<VM operation>", Time since last: N ns, Reaching safepoint: N ns,
//   At safepoint: N ns, Leaving safepoint: N ns, Total: N ns,
//   Threads: R runnable, T total
//
// (one line; wrapped here; gci i47w/j45: HotSpot 25 has no "Cleanup" phase --
// the JDK 21 shape i45/sl printed -- and ends with the leave phase and the
// thread counts, read from Temurin 25.0.3's own `-Xlog:safepoint` file in
// rows `vi45-sl.list` S). The figures come from the VM's GC barrier,
// read by the pause's initiator INSIDE the pause (`gc_events.rs`
// `safepoint_line_begin_for`): "Time since last" is the growth of the
// barrier's application-time sum (`GcBarrier::safepoint_stats`, gci i44/sp)
// since the previous pause began -- the interval between the previous
// `complete_gc` and this request, which the barrier adds when the request
// wins; "Reaching safepoint" is this pause's time-to-safepoint
// (`TtspStats::last_ns`: request -> quota met); "At safepoint" is quota
// met -> the pause's seal, i.e. the take-over's post-quota time
// (`gc_quiescence::xt_post_quota_ns`) plus begin -> seal; "Leaving
// safepoint" is 0 (HotSpot's leave -> end, a few hundred ns, is not timed
// here); "Total" is their sum, as HotSpot's `end - begin` is. "Threads":
// 0 runnable (every thread was brought to the pause; HotSpot's count of
// threads still running when the sync began is not kept) and the registry's
// alive threads at the begin (HotSpot's total also counts its hidden
// JavaThreads -- compiler, service threads -- so it is larger; a VM-internal
// figure, like the timings). The line is printed after the release (no I/O inside the pause),
// decorated with the seal instant, where HotSpot writes it.

/// `CRATONVM_XLOG_SAFEPOINT_LINES` (default ON, gci i45/sl, `=0` kills): the
/// `safepoint` info line per pause. Read once per pause, and only when the
/// installed `-Xlog` logger takes `safepoint` at info
/// ([`xlog_safepoint_lines_wanted`]).
pub fn xlog_safepoint_lines_enabled() -> bool {
    true
}

/// gci i45/sl: does this pause print a `safepoint` line? The installed `-Xlog`
/// logger takes `safepoint` at info (`-Xlog:safepoint`, `safepoint*`, `all`)
/// and the kill switch is not set. One read lock and a rule scan; the flag is
/// read only when the tag is on. The JVMTI verbose output never takes the tag.
pub fn xlog_safepoint_lines_wanted() -> bool {
    current_logger().is_some_and(|l| l.is_enabled(&[LogTag::Safepoint], LogLevel::Info))
        && xlog_safepoint_lines_enabled()
}

/// gci i45/sl: one pause's barrier figures as its initiator reads them inside
/// the pause, after the quota was met ([`safepoint_line_begin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafepointBegin {
    /// The VM (`SharedVm::vm_identity`): the previous-pause mark is kept for
    /// one VM at a time (two live VMs share the process logger).
    pub vm: usize,
    /// `SafepointStats::application_ns`: cumulative, this pause's interval
    /// included (the barrier adds it when the request wins).
    pub application_ns: u64,
    /// `TtspStats::pauses`: moved since the previous mark iff this pause
    /// recorded a time-to-safepoint.
    pub ttsp_pauses: u64,
    /// `TtspStats::last_ns`: this pause's request -> quota met.
    pub ttsp_last_ns: u64,
    /// `gc_quiescence::xt_post_quota_ns`: quota met -> take-over returned
    /// (0 without a take-over; every pause's ledger opens at 0).
    pub post_quota_ns: u64,
    /// gci i47w/j45: the VM's alive registered threads at the begin (the
    /// line's `Threads: .. total`).
    pub threads_total: u32,
    /// The HotSpot VM operation, when the begin knows it (a non-collection
    /// pause); a collection names it at its finish (its door). `None` at both
    /// ends: no line (a pause HotSpot runs no safepoint for).
    pub name: Option<&'static str>,
}

/// gci i45/sl: the barrier figures the previous pause began with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SafepointMark {
    vm: usize,
    application_ns: u64,
    ttsp_pauses: u64,
}

/// gci i45/sl: a pause between its begin and its finish, keyed by its
/// initiator thread (begin, seal and finish run on that thread; a thread
/// initiates one pause at a time, so a second begin replaces a first whose
/// finish never came -- an unwound pause).
#[derive(Debug)]
struct SafepointOpen {
    thread: std::thread::ThreadId,
    name: Option<&'static str>,
    since_last_ns: u64,
    reaching_ns: u64,
    post_quota_ns: u64,
    threads_total: u32,
    begun: Instant,
    sealed: Option<Instant>,
    /// gci i47w/xl47 (residual 1 of
    /// `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`): the
    /// rendered line of the first of two HotSpot operations this pause ran
    /// back to back (G1's `G1PauseRemark`, then `G1PauseCleanup`), split off
    /// at their cut ([`safepoint_line_split`]) with its end instant; printed
    /// before this pause's own (the second operation's) line. `None` unless
    /// split.
    head: Option<(String, Instant)>,
    /// zt t22/hunt3: the rendered lines of the leading operations of a ZGC
    /// collection named at its seal ([`safepoint_line_name_ops`]), with their
    /// end instants, oldest first; printed before this pause's own (the last
    /// operation's) line. Empty unless named so.
    cuts: Vec<(String, Instant)>,
    /// zt t22/hunt3: [`safepoint_line_name_ops`] named this pause: its own
    /// name wins over the finish's.
    named_at_seal: bool,
    /// gcj j1/ob: print this pause's `safepoint` line at its finish (the tag
    /// is on and `CRATONVM_XLOG_SAFEPOINT_LINES` is not killed). `true` for
    /// every pause opened by [`safepoint_line_begin`].
    print_line: bool,
    /// gcj j1/ob: and its `safepoint,stats` row before it
    /// ([`safepoint_line_begin_with_stats`]; Generational only).
    stats: bool,
}

/// gcj j1/ob (residual 8 of
/// `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`):
/// HotSpot 25's `SafepointTracing` statistics for one logger --
/// `_cur_stat_index` (the header every 30 rows), `_op_count` per operation,
/// `_max_sync_time` and `_max_vmop_time` -- and whether the exit summary was
/// printed.
#[derive(Debug, Default)]
struct SafepointStatsState {
    cur_stat_index: u64,
    op_counts: Vec<(&'static str, u64)>,
    max_sync_ns: u64,
    max_vmop_ns: u64,
    exit_printed: bool,
}

/// gcj j1/ob: what [`UnifiedLogger::safepoint_take_full_on`] hands the
/// finish: the rendered `safepoint` line and its end instant, whether that
/// line prints, and the `safepoint,stats` rows that go before it.
#[derive(Debug)]
struct SafepointTaken {
    line: String,
    end: Instant,
    print_line: bool,
    stats_rows: Vec<String>,
}

/// gci i45/sl: [`UnifiedLogger::safepoint`].
#[derive(Debug, Default)]
struct SafepointLineState {
    last: Option<SafepointMark>,
    open: Vec<SafepointOpen>,
    /// gci i46/lg (residual 3): rendered lines waiting for a parked G1 young
    /// report to print first, `(requester key, text, end instant)`, oldest
    /// first; bounded by [`SAFEPOINT_OPEN_CAP`] (the oldest is printed past
    /// it).
    held: Vec<(u64, String, Instant)>,
    /// gcj j1/ob: the `safepoint,stats` figures.
    stats: SafepointStatsState,
}

/// gcj j1/ob: HotSpot 25's `safepoint,stats` header
/// (`SafepointTracing::statistics_log`, every 30 rows), byte for byte as the
/// local 25.0.3 oracle `xl47_r_stats_capture.hs` prints it.
pub const SAFEPOINT_STATS_HEADER: &str = "VM Operation                 [ threads: total initial_running ][ time:       sync    vmop      total ] page_trap_count";

/// gcj j1/ob: one `safepoint,stats` row, HotSpot 25's
/// `SafepointTracing::statistics_log` format (`%-28s [       %8d        %8d
/// ][       %10ld %10ld %10ld ]%16d`): the operation, the thread total and
/// the threads still running at the sync's start, sync (begin -> synced),
/// vmop (synced -> end), their sum, and the polling-page trap count (0: this
/// VM has no polling page). Pure.
pub fn safepoint_stats_row(
    name: &str,
    threads_total: u32,
    initial_running: u32,
    sync_ns: u64,
    vmop_ns: u64,
) -> String {
    let total_ns = sync_ns.saturating_add(vmop_ns);
    format!(
        "{name:<28} [       {threads_total:>8}        {initial_running:>8} ][       {sync_ns:>10} {vmop_ns:>10} {total_ns:>10} ]{:>16}",
        0
    )
}

/// gcj j1/ob: HotSpot 25's VM operations in `VM_OPS_DO` order, the order
/// `SafepointTracing::statistics_exit_log` prints its counts in. Only the
/// relative order of the names this VM's Generational pauses carry matters
/// (`Halt` first, as the oracle shows; the rest from reading `vmOperation.hpp`,
/// unverified by a row with two distinct operations -- residual page). A name
/// not listed prints after them, in first-seen order.
const HOTSPOT_VM_OPERATION_ORDER: &[&str] = &[
    "Halt",
    "SafepointALot",
    "ThreadDump",
    "PrintThreads",
    "FindDeadlocks",
    "ClearICs",
    "ForceSafepoint",
    "DeoptimizeFrame",
    "DeoptimizeAll",
    "Verify",
    "HeapDumper",
    "CollectForMetadataAllocation",
    "CollectForCodeCacheAllocation",
    "GC_HeapInspection",
    "SerialCollectForAllocation",
    "SerialGCCollect",
    "RedefineClasses",
    "Exit",
];

/// gcj j1/ob: HotSpot 25's `safepoint,stats` exit summary
/// (`SafepointTracing::statistics_exit_log`): `%-28s%10lu` per operation run
/// at least once, in [`HOTSPOT_VM_OPERATION_ORDER`], then the two maxima.
/// `counts` in first-seen order. Pure.
pub fn safepoint_stats_exit_lines(
    counts: &[(&'static str, u64)],
    max_sync_ns: u64,
    max_vmop_ns: u64,
) -> Vec<String> {
    let rank = |name: &str| {
        HOTSPOT_VM_OPERATION_ORDER
            .iter()
            .position(|n| *n == name)
            .unwrap_or(HOTSPOT_VM_OPERATION_ORDER.len())
    };
    let mut sorted: Vec<(&'static str, u64)> =
        counts.iter().copied().filter(|&(_, c)| c > 0).collect();
    // Stable: unlisted names keep their first-seen order.
    sorted.sort_by_key(|&(name, _)| rank(name));
    let mut out: Vec<String> = sorted
        .into_iter()
        .map(|(name, count)| format!("{name:<28}{count:>10}"))
        .collect();
    out.push(format!("Maximum sync time  {max_sync_ns} ns"));
    out.push(format!(
        "Maximum vm operation time (except for Exit VM operation)  {max_vmop_ns} ns"
    ));
    out
}

/// gci i45/sl: the most open pauses [`SafepointLineState::open`] holds (one
/// per initiating thread; the oldest is dropped past it).
const SAFEPOINT_OPEN_CAP: usize = 64;

/// gci i45/sl: `(time since last, reaching safepoint)` of the pause `b`
/// begins, against the previous pause's mark `last`. The first pause of a VM
/// (no mark, or another VM's) counts from the barrier's creation, as
/// HotSpot's first counts from VM start. A pause that recorded no
/// time-to-safepoint (the TTSP count did not move) reaches it in 0 ns rather
/// than reporting an earlier pause's. Pure.
fn safepoint_begin_figures(last: Option<SafepointMark>, b: &SafepointBegin) -> (u64, u64) {
    let same = last.filter(|m| m.vm == b.vm);
    let since_last = match same {
        Some(m) => b.application_ns.saturating_sub(m.application_ns),
        None => b.application_ns,
    };
    let fresh_ttsp = match same {
        Some(m) => b.ttsp_pauses != m.ttsp_pauses,
        None => b.ttsp_pauses != 0,
    };
    (since_last, if fresh_ttsp { b.ttsp_last_ns } else { 0 })
}

/// gci i45/sl: "At safepoint" -- quota met -> `end`: the take-over's
/// post-quota time plus begin -> `end`. Saturating. Pure.
fn safepoint_at_ns(post_quota_ns: u64, begun: Instant, end: Instant) -> u64 {
    let span = u64::try_from(end.saturating_duration_since(begun).as_nanos()).unwrap_or(u64::MAX);
    post_quota_ns.saturating_add(span)
}

/// gci i45/sl: HotSpot 25's `safepoint` info line (`SafepointTracing::end`);
/// "Total" is the sum of the three phases (reaching, at, leaving), as
/// HotSpot's `end - begin` is. gci i47w/j45: HotSpot 25's shape -- no
/// "Cleanup" (JDK 21's), then "Leaving safepoint" and "Threads: R runnable,
/// T total". Pure.
pub fn safepoint_line(
    name: &str,
    since_last_ns: u64,
    reaching_ns: u64,
    at_safepoint_ns: u64,
    leaving_ns: u64,
    (threads_running, threads_total): (u32, u32),
) -> String {
    let total = reaching_ns
        .saturating_add(at_safepoint_ns)
        .saturating_add(leaving_ns);
    format!(
        "Safepoint \"{name}\", Time since last: {since_last_ns} ns, Reaching safepoint: {reaching_ns} ns, At safepoint: {at_safepoint_ns} ns, Leaving safepoint: {leaving_ns} ns, Total: {total} ns, Threads: {threads_running} runnable, {threads_total} total"
    )
}

impl UnifiedLogger {
    /// gci i45/sl: [`safepoint_line_begin`] on this logger, for `thread` at
    /// `now`.
    fn safepoint_begin_on(&self, b: SafepointBegin, thread: std::thread::ThreadId, now: Instant) {
        self.safepoint_begin_with_on(b, thread, now, true, false);
    }

    /// gcj j1/ob: [`Self::safepoint_begin_on`], saying whether the pause's
    /// `safepoint` line prints (`print_line`) and whether its
    /// `safepoint,stats` row does (`stats`).
    fn safepoint_begin_with_on(
        &self,
        b: SafepointBegin,
        thread: std::thread::ThreadId,
        now: Instant,
        print_line: bool,
        stats: bool,
    ) {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (since_last_ns, reaching_ns) = safepoint_begin_figures(st.last, &b);
        st.last = Some(SafepointMark {
            vm: b.vm,
            application_ns: b.application_ns,
            ttsp_pauses: b.ttsp_pauses,
        });
        st.open.retain(|o| o.thread != thread);
        if st.open.len() >= SAFEPOINT_OPEN_CAP {
            st.open.remove(0);
        }
        st.open.push(SafepointOpen {
            thread,
            name: b.name,
            since_last_ns,
            reaching_ns,
            post_quota_ns: b.post_quota_ns,
            threads_total: b.threads_total,
            begun: now,
            sealed: None,
            head: None,
            cuts: Vec::new(),
            named_at_seal: false,
            print_line,
            stats,
        });
    }

    /// gci i45/sl: [`safepoint_line_seal`] on this logger. The first seal
    /// wins.
    fn safepoint_seal_on(&self, thread: std::thread::ThreadId, now: Instant) {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(o) = st.open.iter_mut().find(|o| o.thread == thread) {
            o.sealed.get_or_insert(now);
        }
    }

    /// gci i45/sl: close `thread`'s open pause and render its line --
    /// `(text, end instant)`, `None` when the thread has no open pause or
    /// neither end named a VM operation. An unsealed pause ends at `now`.
    fn safepoint_take_on(
        &self,
        thread: std::thread::ThreadId,
        name: Option<&'static str>,
        now: Instant,
    ) -> Option<(String, Instant)> {
        self.safepoint_take_full_on(thread, name, now)
            .map(|t| (t.line, t.end))
    }

    /// gcj j1/ob: [`Self::safepoint_take_on`] with what the finish prints
    /// besides the line: whether the line prints at all, and the
    /// `safepoint,stats` rows (header + row) of a pause opened with stats,
    /// which this records in the logger's statistics.
    fn safepoint_take_full_on(
        &self,
        thread: std::thread::ThreadId,
        name: Option<&'static str>,
        now: Instant,
    ) -> Option<SafepointTaken> {
        let open = {
            let mut st = match self.safepoint.lock() {
                Ok(s) => s,
                Err(poisoned) => poisoned.into_inner(),
            };
            let at = st.open.iter().position(|o| o.thread == thread)?;
            st.open.remove(at)
        };
        // zt t22/hunt3: a pause named at its seal keeps its own (last
        // operation's) name.
        let name = if open.named_at_seal {
            open.name
        } else {
            name.or(open.name)
        }?;
        let end = open.sealed.unwrap_or(now);
        let at_ns = safepoint_at_ns(open.post_quota_ns, open.begun, end);
        let stats_rows = if open.stats {
            self.safepoint_stats_rows_on(name, open.threads_total, open.reaching_ns, at_ns)
        } else {
            Vec::new()
        };
        Some(SafepointTaken {
            line: safepoint_line(
                name,
                open.since_last_ns,
                open.reaching_ns,
                at_ns,
                0,
                (0, open.threads_total),
            ),
            end,
            print_line: open.print_line,
            stats_rows,
        })
    }

    /// gcj j1/ob: record one safepoint in the logger's `safepoint,stats`
    /// figures, as HotSpot 25's `SafepointTracing::begin` / `end` /
    /// `statistics_log` do, and answer its rows: the header when
    /// `_cur_stat_index % 30 == 0` (index then 1, else +1), then the row.
    /// "sync" is the pause's reaching time, "vmop" its at-safepoint time
    /// (leaving is 0 here, as on the `safepoint` line).
    fn safepoint_stats_rows_on(
        &self,
        name: &'static str,
        threads_total: u32,
        sync_ns: u64,
        vmop_ns: u64,
    ) -> Vec<String> {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let s = &mut st.stats;
        let mut rows = Vec::with_capacity(2);
        if s.cur_stat_index % 30 == 0 {
            rows.push(SAFEPOINT_STATS_HEADER.to_string());
            s.cur_stat_index = 1;
        } else {
            s.cur_stat_index += 1;
        }
        match s.op_counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => s.op_counts.push((name, 1)),
        }
        s.max_sync_ns = s.max_sync_ns.max(sync_ns);
        s.max_vmop_ns = s.max_vmop_ns.max(vmop_ns);
        rows.push(safepoint_stats_row(name, threads_total, 0, sync_ns, vmop_ns));
        rows
    }

    /// gcj j1/ob: the exit summary's lines, once per logger (`None` the
    /// second time). The VM thread's terminating `Halt` operation is counted
    /// first, as HotSpot's `VMThread::run` runs it before the summary.
    fn safepoint_stats_exit_on(&self, requested_exit: bool) -> Option<Vec<String>> {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let s = &mut st.stats;
        if s.exit_printed {
            return None;
        }
        s.exit_printed = true;
        // HotSpot's explicit VM_Exit bypasses VMThread's terminating Halt.
        // Neither operation emits a statistics row before this summary.
        let terminal = if requested_exit { "Exit" } else { "Halt" };
        match s.op_counts.iter_mut().find(|(n, _)| *n == terminal) {
            Some((_, count)) => *count += 1,
            None => s.op_counts.push((terminal, 1)),
        }
        Some(safepoint_stats_exit_lines(&s.op_counts, s.max_sync_ns, s.max_vmop_ns))
    }

    /// gci i47w/xl47: [`safepoint_line_split`] on this logger. The open
    /// pause's figures up to `at` become the head line (its own name, "At
    /// safepoint" to `at`); the rest of the pause is the `tail` operation,
    /// from `at` (time since last and reaching 0: the two operations run back
    /// to back inside one pause here). No effect without an open pause, on a
    /// pause that prints no line (no name at its begin), or a second time.
    fn safepoint_split_on(&self, thread: std::thread::ThreadId, at: Instant, tail: &'static str) {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(o) = st.open.iter_mut().find(|o| o.thread == thread) else {
            return;
        };
        let Some(name) = o.name else {
            return;
        };
        if o.head.is_some() {
            return;
        }
        let at_ns = safepoint_at_ns(o.post_quota_ns, o.begun, at);
        o.head = Some((
            safepoint_line(
                name,
                o.since_last_ns,
                o.reaching_ns,
                at_ns,
                0,
                (0, o.threads_total),
            ),
            at,
        ));
        o.name = Some(tail);
        o.since_last_ns = 0;
        o.reaching_ns = 0;
        o.post_quota_ns = 0;
        o.begun = at;
    }

    /// zt t22/hunt3: [`safepoint_line_name_ops`] on this logger. `names` has
    /// one more entry than `cuts`: operation `i` runs to `cuts[i]` (clamped
    /// into the pause, in order), the last one to the pause's end. Each
    /// leading operation's line is rendered now into [`SafepointOpen::cuts`]
    /// -- the first with the pause's since-last, reaching and post-quota
    /// figures, the rest with 0 (they run back to back inside one pause
    /// here) -- and the pause takes the last name. No effect without an
    /// open pause, with no names, with a length mismatch, or a second time.
    fn safepoint_name_ops_on(
        &self,
        thread: std::thread::ThreadId,
        names: &[&'static str],
        cuts: &[Instant],
    ) {
        let Some((&last, leading)) = names.split_last() else {
            return;
        };
        if leading.len() != cuts.len() {
            return;
        }
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(o) = st.open.iter_mut().find(|o| o.thread == thread) else {
            return;
        };
        if o.named_at_seal {
            return;
        }
        for (&name, &cut) in leading.iter().zip(cuts) {
            let at = cut.max(o.begun);
            let at_ns = safepoint_at_ns(o.post_quota_ns, o.begun, at);
            o.cuts.push((
                safepoint_line(
                    name,
                    o.since_last_ns,
                    o.reaching_ns,
                    at_ns,
                    0,
                    (0, o.threads_total),
                ),
                at,
            ));
            o.since_last_ns = 0;
            o.reaching_ns = 0;
            o.post_quota_ns = 0;
            o.begun = at;
        }
        o.name = Some(last);
        o.named_at_seal = true;
    }

    /// zt t22/hunt3: take `thread`'s open pause's leading-operation lines
    /// ([`Self::safepoint_name_ops_on`]), oldest first.
    fn safepoint_take_cuts_on(&self, thread: std::thread::ThreadId) -> Vec<(String, Instant)> {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        st.open
            .iter_mut()
            .find(|o| o.thread == thread)
            .map(|o| std::mem::take(&mut o.cuts))
            .unwrap_or_default()
    }

    /// gci i47w/xl47: take `thread`'s open pause's split-off head line, if
    /// any ([`safepoint_line_flush_head`]).
    fn safepoint_take_head_on(&self, thread: std::thread::ThreadId) -> Option<(String, Instant)> {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        st.open
            .iter_mut()
            .find(|o| o.thread == thread)
            .and_then(|o| o.head.take())
    }

    /// gci i46/lg: [`safepoint_line_hold`] on this logger -- close `thread`'s
    /// open pause and keep its rendered line under `key`. Returns the line a
    /// full hold list evicts (the oldest), for the caller to print.
    fn safepoint_hold_on(
        &self,
        thread: std::thread::ThreadId,
        name: Option<&'static str>,
        key: u64,
        now: Instant,
    ) -> Option<(String, Instant)> {
        let (line, end) = self.safepoint_take_on(thread, name, now)?;
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        let evicted = if st.held.len() >= SAFEPOINT_OPEN_CAP {
            let (_, text, at) = st.held.remove(0);
            Some((text, at))
        } else {
            None
        };
        st.held.push((key, line, end));
        evicted
    }

    /// gci i46/lg: take the lines held under `key`, oldest first.
    fn safepoint_release_on(&self, key: u64) -> Vec<(String, Instant)> {
        let mut st = match self.safepoint.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        if st.held.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        st.held.retain(|(k, text, at)| {
            if *k == key {
                out.push((text.clone(), *at));
                false
            } else {
                true
            }
        });
        out
    }
}

/// gci i45/sl: open the calling thread's pause on the installed logger. Call
/// INSIDE the pause, on its initiator, once the quota was met, and only when
/// [`xlog_safepoint_lines_wanted`]. No I/O; one uncontended lock.
pub fn safepoint_line_begin(b: SafepointBegin) {
    if let Some(logger) = current_logger() {
        logger.safepoint_begin_on(b, std::thread::current().id(), Instant::now());
    }
}

/// gci i45/sl: stamp the calling thread's open pause's end -- call inside the
/// pause, right before `complete_gc` (beside `gc_event_seal` /
/// `non_collection_pause_seal`). A no-op without an open pause.
pub fn safepoint_line_seal() {
    if let Some(logger) = current_logger() {
        logger.safepoint_seal_on(std::thread::current().id(), Instant::now());
    }
}

/// gci i45/sl: close the calling thread's open pause and print its
/// `safepoint` line, after the release. `name` (a collection's VM operation)
/// overrides the begin's; with neither, nothing is printed. A no-op without
/// an open pause.
pub fn safepoint_line_finish(name: Option<&'static str>) {
    let Some(logger) = current_logger() else {
        return;
    };
    // gci i47w/xl47: a split-off head not printed yet goes first.
    if let Some((line, end)) = logger.safepoint_take_head_on(std::thread::current().id()) {
        logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &line, Some(end));
    }
    // zt t22/hunt3: a ZGC collection's leading operations, in order.
    for (line, end) in logger.safepoint_take_cuts_on(std::thread::current().id()) {
        logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &line, Some(end));
    }
    if let Some(taken) =
        logger.safepoint_take_full_on(std::thread::current().id(), name, Instant::now())
    {
        // gcj j1/ob: HotSpot's `SafepointTracing::end` logs the
        // `safepoint,stats` row (Generational) before the `safepoint` line.
        let end = Some(taken.end);
        for row in &taken.stats_rows {
            logger.log_at(&[LogTag::SafepointStats], LogLevel::Info, row, end);
        }
        if taken.print_line {
            logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &taken.line, end);
        }
    }
}

/// gcj j1/ob: [`safepoint_line_begin`] for a pause whose `safepoint` line
/// prints only when `print_line` ([`xlog_safepoint_lines_wanted`]) and whose
/// `safepoint,stats` row prints when `stats` ([`xlog_safepoint_stats_wanted`],
/// Generational only). Same contract otherwise.
pub fn safepoint_line_begin_with_stats(b: SafepointBegin, print_line: bool, stats: bool) {
    if let Some(logger) = current_logger() {
        logger.safepoint_begin_with_on(
            b,
            std::thread::current().id(),
            Instant::now(),
            print_line,
            stats,
        );
    }
}

/// `CRATONVM_GEN_XLOG_SAFEPOINT_STATS` (gcj j1/ob; default ON, `=0` kill:
/// nothing is logged to `safepoint,stats`, as before). On Generational,
/// `-Xlog:safepoint+stats` (`safepoint*`, `all`) prints HotSpot 25's
/// statistics table -- the header every 30 safepoints, one row before each
/// `safepoint` line -- and its exit summary. G1 and ZGC never print it.
pub fn xlog_gen_safepoint_stats_enabled() -> bool {
    true
}

/// gcj j1/ob: does the installed `-Xlog` logger take `safepoint,stats` at
/// info, with `CRATONVM_GEN_XLOG_SAFEPOINT_STATS` on? The caller adds the
/// collector test (Generational only). One read lock and a rule scan; the
/// flag is read only when the tag is on.
pub fn xlog_safepoint_stats_wanted() -> bool {
    current_logger().is_some_and(|l| l.is_enabled(&[LogTag::SafepointStats], LogLevel::Info))
        && xlog_gen_safepoint_stats_enabled()
}

/// gcj j1/ob: HotSpot 25's `safepoint,stats` exit summary
/// (`SafepointTracing::statistics_exit_log`) on the installed logger, once:
/// `<operation> <count>` per operation run (the terminating `Halt` included),
/// `Maximum sync time`, `Maximum vm operation time (except for Exit VM
/// operation)`. `generational`: the VM runs the Generational collector (the
/// summary is never printed for G1 or ZGC). A no-op without a logger, with
/// the tag off or the switch killed, or the second time.
pub fn log_safepoint_stats_exit_summary(generational: bool) {
    log_safepoint_stats_requested_exit_summary(generational, false);
}

/// The explicit `System.exit` arm uses HotSpot's VM_Exit terminal operation.
pub fn log_safepoint_stats_requested_exit_summary(generational: bool, requested_exit: bool) {
    if !generational || !xlog_safepoint_stats_wanted() {
        return;
    }
    let Some(logger) = current_logger() else {
        return;
    };
    if let Some(lines) = logger.safepoint_stats_exit_on(requested_exit) {
        for line in lines {
            logger.log(&[LogTag::SafepointStats], LogLevel::Info, &line);
        }
    }
}

/// zt t22/hunt3 (`CRATONVM_ZGC_XLOG_SAFEPOINT_LINES`): name the calling
/// thread's open pause as a run of HotSpot operations back to back --
/// `names[i]` up to `cuts[i]`, the last name to the pause's end -- so its
/// finish prints one `safepoint` line per operation, whatever name the finish
/// passes. A ZGC collection is HotSpot's `ZMarkStartYoungAndOld`,
/// `ZMarkEndOld`, `ZRelocateStartOld` here, cut where its pause rows are.
/// Call inside the pause, on its initiator, before the seal. A no-op without
/// a logger or an open pause (the tag is not selected), or a second time.
/// One lock.
pub fn safepoint_line_name_ops(names: &[&'static str], cuts: &[Instant]) {
    if let Some(logger) = current_logger() {
        logger.safepoint_name_ops_on(std::thread::current().id(), names, cuts);
    }
}

/// gci i47w/xl47 (residual 1 of
/// `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`): the calling
/// thread's open pause ran two HotSpot operations back to back, cut at `at`:
/// its line becomes two -- the begin's operation up to `at`, then `tail` from
/// `at` to the pause's end. G1's remark pause here is HotSpot's
/// `VM_G1PauseRemark` and `VM_G1PauseCleanup` (`G1PauseRemark`, then
/// `G1PauseCleanup`; `sl45_i_g1_conc` HotSpot names `{G1PauseCleanup=5,
/// G1PauseRemark=5, ..}`). Call inside the pause, on its initiator, before
/// the seal. A no-op without a logger, without an open pause (the tag is not
/// selected), or on a pause that prints no line. One lock.
pub fn safepoint_line_split(at: Instant, tail: &'static str) {
    if let Some(logger) = current_logger() {
        logger.safepoint_split_on(std::thread::current().id(), at, tail);
    }
}

/// gci i47w/xl47: print the calling thread's split-off head line now (after
/// the release, between the two operations' own `gc` lines, as HotSpot
/// orders them); [`safepoint_line_finish`] prints it otherwise. A no-op
/// without one.
pub fn safepoint_line_flush_head() {
    let Some(logger) = current_logger() else {
        return;
    };
    if let Some((line, end)) = logger.safepoint_take_head_on(std::thread::current().id()) {
        logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &line, Some(end));
    }
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 3): [`safepoint_line_finish`], except that the line is kept under `key`
/// (the requester's `ThreadId.0`) instead of printed, until
/// [`safepoint_line_release_held`] for the same key -- for a G1 young pause
/// whose `gc` lines are parked until its mark-cycle drive labels them
/// (HotSpot writes those lines inside the pause, before its `safepoint`
/// line). Call after the release. A no-op without an open pause.
pub fn safepoint_line_hold(name: Option<&'static str>, key: u64) {
    let Some(logger) = current_logger() else {
        return;
    };
    if let Some((line, end)) =
        logger.safepoint_hold_on(std::thread::current().id(), name, key, Instant::now())
    {
        logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &line, Some(end));
    }
}

/// gci i46/lg: print the lines [`safepoint_line_hold`] kept under `key`,
/// oldest first -- right after the parked report's lines. With the world
/// running. One read lock and one uncontended lock; nothing held, nothing
/// printed.
pub fn safepoint_line_release_held(key: u64) {
    let Some(logger) = current_logger() else {
        return;
    };
    for (line, end) in logger.safepoint_release_on(key) {
        logger.log_at(&[LogTag::Safepoint], LogLevel::Info, &line, Some(end));
    }
}

// ---------------------------------------------------------------------------
// `class,load` (gch h4/fl)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`:
// the tag parsed and had a name, and nothing emitted it. HotSpot 25 prints one
// info line per class created from class-file bytes
// (`InstanceKlass::print_class_load_logging`, at the end of class-file
// parsing, after its supertypes were loaded): `<external name> source: <src>`.
// The emit point here is the class manager's deferred class-load hook
// (`vm_init::class_load_adapter`), which runs once per class defined from
// bytes, in define order (a supertype resolved during the define is pushed
// first), after the defining write guard is released and before the class
// can be linked or run. Array classes and VM-internal shapes are not class
// files and print nothing, as on HotSpot.

/// What [`class_load_message`] needs to know of a just-defined class. Pure
/// data, so the format is unit-testable without a VM.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClassLoadFacts<'a> {
    /// Internal name (`java/lang/Object`; a hidden class carries its
    /// `/0x<hex>` suffix).
    pub internal_name: &'a str,
    /// A JEP 371 hidden class (`Lookup.defineHiddenClass`).
    pub hidden: bool,
    /// Defined by the bootstrap loader.
    pub bootstrap: bool,
    /// The named module the class is a member of, if any.
    pub module: Option<&'a str>,
    /// `CodeSource` location URL the define recorded, if any.
    pub code_source: Option<&'a str>,
    /// Internal name of the nest host a hidden class joined (`NESTMATE`).
    pub nest_host: Option<&'a str>,
    /// gci i11/cl: internal name of the LOOKUP class a `NESTMATE` hidden
    /// class was defined through, when its define recorded one
    /// (`ClassManager::hidden_lookup_class_names`).
    pub lookup_class: Option<&'a str>,
    /// A `java.lang.reflect.Proxy` class.
    pub dynamic_proxy: bool,
    /// An array class or a VM-internal shape: no class file, no line.
    pub not_a_class_file: bool,
}

/// The URL prefix the class manager synthesises for a class defined with no
/// code source (`ClassManager::define_class_shared_parsed`,
/// `file:/runtime-defined/<name>.class`): not where the bytes came from.
const RUNTIME_DEFINED_CODE_SOURCE: &str = "file:/runtime-defined/";

/// HotSpot's `Klass::external_name()` for the log: `/` becomes `.`, except
/// that a hidden class keeps the `/0x<hex>` suffix that separates its name
/// from its address (`java.lang.invoke.LambdaForm$MH/0x00007f...`).
pub fn class_load_external_name(internal: &str, hidden: bool) -> String {
    if hidden {
        if let Some(cut) = internal.rfind("/0x") {
            let (base, suffix) = internal.split_at(cut);
            let digits = &suffix[3..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return format!("{}{suffix}", base.replace('/', "."));
            }
        }
    }
    internal.replace('/', ".")
}

/// The `source:` field HotSpot 25 prints for a class, or `None` when it
/// prints none (a boot class with no known module: HotSpot's "source is
/// unknown" arm).
///
/// * hidden: the lookup class's name when it joined a nest (`NESTMATE`,
///   a lambda proxy), else `__JVM_LookupDefineClass__` (`LambdaForm$MH`).
///   gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): HotSpot's
///   `jvm_lookup_define_class` passes `lookup_k->external_name()`, the
///   LOOKUP class, which is not the nest host when the lookup class is
///   itself a nest member (a lambda spun in `Outer$Inner` joins `Outer`'s
///   nest). A lambda proxy's name carries its lookup class
///   (`InnerClassLambdaMetafactory.lambdaClassName`:
///   `<lookup>$$Lambda`), so it is read from there
///   ([`hidden_lambda_lookup_class`]). Every nestmate define door records
///   its lookup class (`DefineClassFull::lookup_class_name` ->
///   `ClassManager::hidden_lookup_class_names`, [`ClassLoadFacts::lookup_class`]),
///   which wins over both; the nest host is the last fallback;

/// * a dynamic proxy: `__dynamic_proxy__` (the source `Proxy` passes);
/// * a real code-source URL (a class-path directory `file:/dir/`, a jar
///   `file:/x.jar`, a platform class's `jrt:/java.sql`): the URL itself;
/// * a module-image class: `jrt:/<module>` (HotSpot without CDS; with CDS
///   it prints `shared objects file`, which this VM has no counterpart for);
/// * any other class defined from bytes with no code source:
///   `__JVM_DefineClass__` (`ClassLoader.defineClass` with a null domain).
pub fn class_load_source(f: &ClassLoadFacts<'_>) -> Option<String> {
    if f.hidden {
        return Some(match f.nest_host {
            Some(host) => match f.lookup_class {
                Some(lookup) => class_load_external_name(lookup, true),
                None => hidden_lambda_lookup_class(f.internal_name)
                    .unwrap_or_else(|| class_load_external_name(host, false)),
            },
            None => "__JVM_LookupDefineClass__".to_string(),
        });
    }
    if f.dynamic_proxy {
        return Some("__dynamic_proxy__".to_string());
    }
    let real_url = f
        .code_source
        .filter(|url| !url.is_empty() && !url.starts_with(RUNTIME_DEFINED_CODE_SOURCE));
    if let Some(url) = real_url {
        return Some(url.to_string());
    }
    if let Some(module) = f.module.filter(|m| !m.is_empty()) {
        return Some(format!("jrt:/{module}"));
    }
    if f.bootstrap {
        None
    } else {
        Some("__JVM_DefineClass__".to_string())
    }
}

/// The `[class,load]` info message (decorations are the logger's), or `None`
/// for a class HotSpot logs nothing for.
pub fn class_load_message(f: &ClassLoadFacts<'_>) -> Option<String> {
    if f.not_a_class_file {
        return None;
    }
    let name = class_load_external_name(f.internal_name, f.hidden);
    Some(match class_load_source(f) {
        Some(source) => format!("{name} source: {source}"),
        None => name,
    })
}

/// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): the external
/// name of the lookup class a lambda proxy's hidden class was spun for, read
/// from its internal name, or `None` when the name is not a lambda proxy's.
/// JDK 25 `InnerClassLambdaMetafactory.lambdaClassName` names the class
/// `<lookup class>$$Lambda` (a HIDDEN lookup class's `/0x<hex>` written
/// `_0x<hex>`), and HotSpot prints the lookup class as the `source:`. The
/// `_0x<16 hex digits>` tail is turned back into the hidden lookup class's
/// own `/0x<hex>` name (HotSpot's mangled hidden names, like this VM's,
/// always carry 16 digits). Pure.
pub fn hidden_lambda_lookup_class(internal: &str) -> Option<String> {
    let cut = internal.rfind("$$Lambda")?;
    let prefix = &internal[..cut];
    if prefix.is_empty() || prefix.ends_with('/') {
        return None;
    }
    let external = prefix.replace('/', ".");
    if let Some(at) = external.rfind("_0x") {
        let digits = &external[at + 3..];
        if digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(format!("{}/{}", &external[..at], &external[at + 1..]));
        }
    }
    Some(external)
}

/// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 4): what HotSpot
/// 25's `-Xlog:class+load=debug` second line needs to know of a class
/// (`InstanceKlass::print_class_load_logging`, the `Debug` half). Pure data.
#[derive(Debug, Clone, Copy)]
pub struct ClassLoadDebugFacts<'a> {
    /// The class's id (its `klass:` stand-in, [`class_load_klass_token`]).
    pub class_id: u32,
    /// The superclass's id; `None` prints HotSpot's null `super:` (Object).
    pub superclass: Option<u32>,
    /// The directly declared interfaces (`local_interfaces()`), in order.
    pub interfaces: &'a [crate::classloading::ClassId],
    /// The defining loader.
    pub loader: crate::classloading::ClassLoaderId,
    /// The class-file bytes the class was parsed from, when still cached:
    /// `bytes:` and `checksum:` print only then (HotSpot prints them
    /// whenever it parsed a stream, i.e. always without CDS).
    pub class_file: Option<&'a [u8]>,
}

/// The base of [`class_load_klass_token`]: inside the range HotSpot's
/// compressed class space prints (`0x0000000800000000` and up).
const CLASS_LOAD_KLASS_TOKEN_BASE: u64 = 0x0000_0008_0000_0000;

/// gci i11/cl: the `klass:` / `super:` / `interfaces:` value printed for
/// class `class_id`. This VM has no `Klass*`; the token is a stable stand-in
/// derived from the id, so a class's `super:` equals the `klass:` its
/// superclass printed (the one relation the debug line carries). Never 0,
/// which HotSpot prints only for a null super. Pure.
pub fn class_load_klass_token(class_id: u32) -> u64 {
    CLASS_LOAD_KLASS_TOKEN_BASE + u64::from(class_id) * 8
}

/// gci i11/cl: the text inside `loader: [..]`, in the shape of HotSpot's
/// `ClassLoaderData::print_value_on`: `loader data: 0x.. of 'bootstrap'` for
/// the null loader, else `loader data: 0x.. for instance a '<loader class>'
/// {0x..}`. The addresses are stand-ins (this VM has no `ClassLoaderData`);
/// a user-defined loader's class is not known here, so it prints
/// `java/lang/ClassLoader` (a known gap, recorded on the page). Pure.
pub fn class_load_loader_data(loader: crate::classloading::ClassLoaderId) -> String {
    use crate::classloading::ClassLoaderId;
    let n = u64::from(loader.to_native_id());
    let cld = 0x0000_0001_0000_0000u64 + n * 0x100;
    let class = match loader {
        ClassLoaderId::Bootstrap => return format!("loader data: 0x{cld:016x} of 'bootstrap'"),
        ClassLoaderId::Extension => "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
        ClassLoaderId::Application => "jdk/internal/loader/ClassLoaders$AppClassLoader",
        ClassLoaderId::UserDefined(_) => "java/lang/ClassLoader",
    };
    let oop = 0x0000_0007_0000_0000u64 + n * 0x100;
    format!("loader data: 0x{cld:016x} for instance a '{class}'{{0x{oop:016x}}}")
}

/// The IEEE CRC-32 table (zlib's `crc32`, which HotSpot's
/// `ClassLoader::crc32` calls), built at compile time: no process state.
const CLASS_FILE_CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            let mask = (c & 1).wrapping_neg();
            c = (c >> 1) ^ (0xEDB8_8320 & mask);
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table};

/// gci i11/cl: zlib's `crc32(0, bytes, len)`, the debug line's `checksum:`
/// (`%08x`). Pure.
pub fn class_file_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = CLASS_FILE_CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// gci i11/cl (item 4): HotSpot 25's `[class,load]` debug message, which
/// starts with a space (` klass: 0x.. super: 0x..[ interfaces: 0x..] loader:
/// [..][ bytes: N checksum: xxxxxxxx]`). Pure.
pub fn class_load_debug_message(f: &ClassLoadDebugFacts<'_>) -> String {
    let mut s = format!(
        " klass: 0x{:016x} super: 0x{:016x}",
        class_load_klass_token(f.class_id),
        f.superclass.map_or(0, class_load_klass_token)
    );
    if !f.interfaces.is_empty() {
        s.push_str(" interfaces:");
        for iface in f.interfaces {
            s.push_str(&format!(" 0x{:016x}", class_load_klass_token(iface.as_u32())));
        }
    }
    s.push_str(&format!(" loader: [{}]", class_load_loader_data(f.loader)));
    if let Some(bytes) = f.class_file {
        s.push_str(&format!(
            " bytes: {} checksum: {:08x}",
            bytes.len(),
            class_file_crc32(bytes)
        ));
    }
    s}

/// gci i11/cl (item 4): is `class+load` on at debug? One logger probe.
pub(crate) fn class_load_debug_logging_enabled() -> bool {
    is_unified_logging_enabled(&[LogTag::ClassLoad], LogLevel::Debug)
}

/// gci i11/cl (item 4): the debug message of a stored class of `cm`
/// ([`class_load_debug_message`]); its bytes come from the class-bytes cache
/// (FIFO, 16 MiB by default: a class evicted before it is logged prints no
/// `bytes:`, which only the registration walk of a very large boot set can
/// meet).
fn class_load_debug_message_of(
    cm: &cratonvm_classloading::ClassManager,
    class: &crate::classloading::Class,
) -> String {
    class_load_debug_message(&ClassLoadDebugFacts {
        class_id: class.id.as_u32(),
        superclass: class.superclass.map(|s| s.as_u32()),
        interfaces: &class.interfaces,
        loader: class.loader_id,
        class_file: cm.class_bytes_cache.get(&class.id).map(|b| &**b),
    })
}

/// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 6): the
/// `[class,load]` info message of a lambda proxy this VM answered an
/// `invokedynamic` (or a reflective `LambdaMetafactory` call) with, where
/// HotSpot defines a hidden class: `<host>$$Lambda/0x<id> source: <host>`
/// -- the name `Class.getName()` answers for it
/// (`interpreter::lambda_proxy_display_name`) and HotSpot's NESTMATE source,
/// the lookup class. `host` is internal. Pure.
pub fn lambda_proxy_class_load_message(host: &str, proxy_class_id: u32) -> String {
    let host = host.replace('/', ".");
    format!("{host}$$Lambda/0x{proxy_class_id:016x} source: {host}")
}

/// gci i11/cl (item 6): log the `[class,load]` line(s) of lambda proxy
/// `proxy_class_id`, registered in `shared`'s proxy tables just now. No class
/// file reaches `define_class_shared_parsed` for it, so the class-load hook
/// never sees it; the two registration sites call this instead
/// (`invokedynamic::bootstrap_lambda`, `register_lambda_proxy` of the
/// native context). One logger probe when the tag is off. The caller holds
/// no class-manager guard.
pub(crate) fn log_lambda_proxy_class_load(
    shared: &crate::vm::SharedVm,
    proxy_class_id: crate::classloading::ClassId,
) {
    if !class_load_logging_enabled() {
        return;
    }
    // Only once the VM is registered for hooks, i.e. after its registration
    // walk printed `java.lang.Object` and the classes before it (gci i5/hd):
    // a proxy minted earlier would print ahead of them. Such a proxy prints
    // nothing (the walk covers the class store, which holds no proxy).
    let domain = shared.jit.class_layout_domain;
    let entry = crate::vm::vm_init::hook_vm_and_class_log_cutoff(domain);
    if !entry.is_some_and(|(vm, _)| std::ptr::eq(&*vm, shared)) {
        return;
    }
    let host = crate::runtime::interpreter::lambda_proxy_host(shared, proxy_class_id);
    let Some((host_id, host)) = host else {
        return;
    };
    let info = lambda_proxy_class_load_message(&host, proxy_class_id.as_u32());
    log_unified(&[LogTag::ClassLoad], LogLevel::Info, &info);
    if !class_load_debug_logging_enabled() {
        return;
    }
    let interfaces: Vec<crate::classloading::ClassId> = shared
        .classes
        .lambda_call_site_for(proxy_class_id)
        .and_then(|site| site.functional_interface_id)
        .into_iter()
        .collect();
    let (object, loader) = {
        let cm = shared.classes.class_manager.read();
        let object = cm.find_bootstrap_class_by_name("java/lang/Object");
        let loader = host_id
            .and_then(|id| cm.get_class(id))
            .map_or(crate::classloading::ClassLoaderId::Bootstrap, |c| c.loader_id);
        (object, loader)
    };
    let debug = class_load_debug_message(&ClassLoadDebugFacts {
        class_id: proxy_class_id.as_u32(),
        superclass: object.map(|o| o.as_u32()),
        interfaces: &interfaces,
        loader,
        class_file: None,
    });
    log_unified(&[LogTag::ClassLoad], LogLevel::Debug, &debug);
}

/// gch h4/fl: the `-Xlog:class+load` emitter, called once per class defined
/// from bytes by the class-load hook (`vm_init::class_load_adapter`) with
/// the defining store's layout domain. One logger probe when the tag is off.
///
/// gci i5/hd (`gch-h4fl-xlog-class-load-residuals` items 1-2): a class whose
/// VM is not registered for hooks yet prints NOTHING here (it printed its
/// name alone, with no source); the VM's registration
/// (`vm_init::set_global_shared_vm_for_hooks`) prints every class its store
/// already holds, in define (id) order, primordial classes included, and
/// records the first id it did not print. A class below that cutoff is the
/// walk's: a hook event still queued from before the registration
/// (`PENDING_CLASS_HOOKS`) would print it a second time, so it is skipped
/// ([`class_load_hook_prints`]).
pub(crate) fn log_class_load(owner_domain: u32, class_id: u32, internal_name: &str) {
    if !class_load_logging_enabled() {
        return;
    }
    let entry = crate::vm::vm_init::hook_vm_and_class_log_cutoff(owner_domain);
    let Some((shared, cutoff)) = entry else {
        return;
    };
    if !class_load_hook_prints(class_id, cutoff) {
        return;
    }
    // gci i11/cl (item 4): HotSpot's second, debug-level line follows the
    // info line when `class+load=debug` is on.
    let debug = class_load_debug_logging_enabled();
    let (info, debug_line) = class_load_messages_for(&shared, class_id, internal_name, debug);
    if let Some(message) = info {
        log_unified(&[LogTag::ClassLoad], LogLevel::Info, &message);
        if let Some(line) = debug_line {
            log_unified(&[LogTag::ClassLoad], LogLevel::Debug, &line);
        }
    }
}

/// gci i5/hd: is `-Xlog:class+load` (or JVMTI `SetVerboseFlag(CLASS)`) on at
/// info? One logger probe.
pub(crate) fn class_load_logging_enabled() -> bool {
    is_unified_logging_enabled(&[LogTag::ClassLoad], LogLevel::Info)
}

/// gci i5/hd: does the class-load hook print class `class_id` of a VM whose
/// registration walk printed every id below `walk_cutoff`? (A VM registered
/// while the tag was off has cutoff 0: the hook prints everything.) Pure.
pub(crate) fn class_load_hook_prints(class_id: u32, walk_cutoff: u32) -> bool {
    class_id >= walk_cutoff
}

/// gci i5/hd: does the registration walk print a class of `origin`? The hook
/// fires only for classes defined from parsed bytes
/// (`ClassManager::define_class_shared_parsed`, whose
/// `classify_defined_origin` never yields a stub, an array or a VM-internal
/// shape), so the walk prints exactly those: never a compatibility stub
/// (no class file; HotSpot has no such class), an array class or a
/// VM-internal shape. Pure.
pub(crate) fn class_load_walk_includes(origin: &cratonvm_classloading::ClassOrigin) -> bool {
    use cratonvm_classloading::ClassOrigin;
    !matches!(
        origin,
        ClassOrigin::CompatibilityStub { .. } | ClassOrigin::VmArray | ClassOrigin::VmInternal
    )
}

/// gci i5/hd: the `[class,load]` messages of every class of `cm`'s store
/// below `cutoff`, in id order (the define order), for the registration
/// walk. The caller holds the class-manager read guard and logs the lines
/// after it has dropped it ([`log_class_load_walk`]).
///
/// gci i11/cl (item 4): each line carries its level; with `class+load=debug`
/// on, a class's debug line follows its info line.
pub(crate) fn class_load_walk_messages(
    cm: &cratonvm_classloading::ClassManager,
    cutoff: u32,
) -> Vec<(LogLevel, String)> {
    let debug = class_load_debug_logging_enabled();
    let mut lines = Vec::new();
    for id in 0..cutoff {
        let Some(class) = cm.get_class(crate::classloading::ClassId::new(id)) else {
            continue;
        };
        if !class_load_walk_includes(&class.origin) {
            continue;
        }
        if let Some(message) = class_load_message(&class_load_facts(cm, class)) {
            lines.push((LogLevel::Info, message));
            if debug {
                lines.push((LogLevel::Debug, class_load_debug_message_of(cm, class)));
            }
        }
    }
    lines
}

/// gci i5/hd: write the registration walk's lines (see
/// [`class_load_walk_messages`]), each at its own level.
pub(crate) fn log_class_load_walk(lines: &[(LogLevel, String)]) {
    for (level, line) in lines {
        log_unified(&[LogTag::ClassLoad], *level, line);
    }
}

/// The [`ClassLoadFacts`] of a stored class.
///
/// gci i11/cl: `cm` supplies the lookup class a NESTMATE hidden define
/// recorded (item 7).
fn class_load_facts<'a>(
    cm: &'a cratonvm_classloading::ClassManager,
    class: &'a crate::classloading::Class,
) -> ClassLoadFacts<'a> {
    use cratonvm_classloading::ClassOrigin;
    ClassLoadFacts {
        lookup_class: cm.hidden_lookup_class_names.get(&class.id).map(|n| &**n),
        internal_name: &*class.name,
        hidden: class.hidden,
        bootstrap: matches!(class.loader_id, crate::classloading::ClassLoaderId::Bootstrap),
        module: class.module_name.as_deref(),
        code_source: class.code_source.as_ref().and_then(|cs| cs.url.as_deref()),
        nest_host: class.nest_host.as_deref(),
        dynamic_proxy: matches!(class.origin, ClassOrigin::GeneratedProxy { .. }),
        not_a_class_file: class.name.starts_with('[')
            || matches!(class.origin, ClassOrigin::VmArray | ClassOrigin::VmInternal),
    }
}

/// [`class_load_message`] for class `class_id` of `shared`'s class store,
/// and (gci i11/cl, item 4) its [`class_load_debug_message`] when `debug`.
/// The class-manager read guard is dropped before the caller writes the
/// lines. A class the read cannot reach prints its name alone and no debug
/// line.
fn class_load_messages_for(
    shared: &crate::vm::SharedVm,
    class_id: u32,
    internal_name: &str,
    debug: bool,
) -> (Option<String>, Option<String>) {
    let lock = &shared.classes.class_manager;
    // The hook runs after this thread released its own write guard, but it
    // may still hold lower-level locks a concurrent writer waits for, so it
    // never blocks: a non-blocking read retried for a bounded time (another
    // thread's define is short), then the name alone. Untracked: a try adds
    // no wait-for edge (`class_name_adapter`'s reasoning).
    let deadline = Instant::now() + std::time::Duration::from_millis(100);
    let cm = loop {
        if let Some(cm) = lock.try_read_untracked() {
            break cm;
        }
        if Instant::now() >= deadline {
            return (Some(class_load_external_name(internal_name, true)), None);
        }
        std::thread::yield_now();
    };
    let Some(class) = cm.get_class(crate::classloading::ClassId::new(class_id)) else {
        return (Some(class_load_external_name(internal_name, true)), None);
    };
    let info = class_load_message(&class_load_facts(&cm, class));
    let debug_line = (debug && info.is_some()).then(|| class_load_debug_message_of(&cm, class));
    (info, debug_line)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

/// gci i28/dg: the deferred reference-processing lines keep one slot per
/// collection id (i27/ob review item 3), and HotSpot's `No tag set matches
/// selection` warning (g3x item 11 residual).
#[cfg(test)]
mod gci_i28_dg_tests;

/// gci i28/tl (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 9): the `gc,tlab`
/// debug `TLAB totals` line's per-VM stash and its two call sites.
#[cfg(test)]
mod gci_i28tl_tlab_totals_tests;

/// gci i30/ob: a line decorated at an earlier instant (`gcg-g3x` item 7) and
/// the replaceable process logger (`i28-dg-report.md` review item 4).
#[cfg(test)]
mod gci_i30_ob_tests;

/// gci i45/sl: `-Xlog:safepoint` (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016`):
/// HotSpot 25's line shape, the per-pause figures, and the open / seal /
/// close bookkeeping on a logger that is never installed (nothing prints).
#[cfg(test)]
mod gci_i45_sl_tests;

/// gci i46/lg: file outputs' options and rotation
/// (`gci-i45sl-xlog-file-output-appends-to-an-existing-log-FIXED-20261016`), the
/// `safepoint+stats` / `interpreter+safepoint` tag sets and the held
/// `safepoint` lines (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016`
/// residuals 5 and 3). The loggers here are parsed, never installed.
#[cfg(test)]
mod gci_i46_lg_tests;

/// gci i47w/xl47: the remark / cleanup split of a `safepoint` line (residual
/// 1 of `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`) and the
/// default stdout output's decorators for a line written outside a rule
/// (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 2's residual), on
/// loggers that are never installed (nothing prints).
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i47w_xl47_`.
#[cfg(test)]
mod gci_i47w_xl47_tests;
