// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The inflater stream census, `CRATONVM_DBG=zip-stream-census`
//! (`CRATONVM_DBG_ZIP_STREAM_CENSUS`; interpreter round i6 wave 116, lane L5).
//!
//! # Why this exists
//!
//! `docs/internal/proposals/interpreter/i64-L5-proposal-serve-zipfile-entry-streams-from-the-vm-archive-20261028.md`
//! would serve `ZipFile$ZipFileInflaterInputStream.read` from the VM's own
//! archive, but only if that read is a few percent of a real start-up
//! OUTSIDE `Resource.getBytes` (a class read, which other switches already
//! cover). Its wave-65 note asks for the count first, taken where the
//! `Inflater.inflate*` native runs, attributed by the caller frame, and kept
//! out of the dispatch path. This is that count.
//!
//! # What it records
//!
//! Every `Inflater.inflate*` native call (all four overloads meet in
//! `zip_real::finish_inflate`), keyed by:
//! * the stream kind: `zipfile-entry` (a `ZipFile$ZipFileInflaterInputStream`
//!   frame is on the stack), `gzip` (`GZIPInputStream`), `inflater-stream`
//!   (another `InflaterInputStream`), or `direct` (the `Inflater` driven by
//!   application code);
//! * the consumer: `Resource.getBytes` when
//!   `jdk/internal/loader/Resource.getBytes` is on the stack, else the first
//!   frame, innermost first, outside `java/util/zip/`, `java/util/jar/`,
//!   `java/io/`, `sun/nio/cs/` and `sun/net/www/protocol/jar/`.
//!
//! with the call count and the output bytes; the `total` line also carries the
//! nanoseconds spent inside the four natives (since wave 117: the `inflate`
//! phase of `L5W117JarClassPhaseSplitBench`'s split, this census's own stack
//! capture taken out). The Java read layers around the native are not timed
//! here; the bench `L5W77StreamSplitBench` prices them.
//! Dumped at exit as `[zip-stream-census]` lines, biggest byte count first.
//!
//! Off (the default) it costs one flag read per inflate call. On, each call
//! captures the innermost 24 frames: a diagnostic, never a benchmark run.
//!
//! Process-wide, as `mh_finder_census`: a diagnostic dumped once at exit,
//! not compatibility state.

use cratonvm_native_api::NativeContext;

/// Is the census on (`CRATONVM_DBG_ZIP_STREAM_CENSUS`)?
#[inline]
pub(crate) fn enabled() -> bool {
    crate::nbflags().dbg_zip_stream_census
}

/// Frames looked at per call, innermost first.
const TAIL: usize = 24;
/// Bound on distinct (kind, consumer) rows.
const ROW_CAP: usize = 4096;
/// Rows printed at exit.
const DUMP_ROWS: usize = 60;

#[derive(Default, Clone, Copy)]
struct Row {
    calls: u64,
    bytes: u64,
}

#[derive(Default)]
struct Tallies {
    rows: std::collections::HashMap<(&'static str, String), Row>,
    total: Row,
    dropped: u64,
    /// Nanoseconds inside the four `inflate*` natives, the census's own stack
    /// capture taken out (wave 117).
    native_ns: u64,
}

thread_local! {
    /// The stack-capture time of [`note_inflate_slow`] inside the native
    /// being timed, taken out of its time by [`note_inflate_time`].
    static CAPTURE_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A clock for one `inflate*` native, when the census is on (wave 117).
#[inline]
pub(crate) fn start() -> Option<std::time::Instant> {
    enabled().then(std::time::Instant::now)
}

/// Add one `inflate*` native's time since `clock` (minus this census's own
/// stack capture inside it) to the total. A no-op for `None`.
pub(crate) fn note_inflate_time(clock: Option<std::time::Instant>) {
    let Some(clock) = clock else { return };
    let ns = clock.elapsed().as_nanos() as u64;
    let capture = CAPTURE_NS.with(|c| c.replace(0));
    let mut guard = tallies().lock();
    guard.native_ns = guard.native_ns.saturating_add(ns.saturating_sub(capture));
}

static TALLIES: std::sync::OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Tallies>> =
    std::sync::OnceLock::new();

fn tallies() -> &'static cratonvm_types::lock_order::OrderedPlMutex<Tallies> {
    TALLIES.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            Default::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The layers a consumer is looked for below.
fn is_stream_layer(class: &str) -> bool {
    class.starts_with("java/util/zip/")
        || class.starts_with("java/util/jar/")
        || class.starts_with("java/io/")
        || class.starts_with("sun/nio/cs/")
        || class.starts_with("sun/net/www/protocol/jar/")
}

/// The stream kind and the consumer of the frames `innermost_first`
/// (`(class, method)` pairs, internal names).
fn classify<'a>(
    innermost_first: impl Iterator<Item = (&'a str, &'a str)> + Clone,
) -> (&'static str, String) {
    let mut kind = "direct";
    for (class, _) in innermost_first.clone() {
        match class {
            "java/util/zip/ZipFile$ZipFileInflaterInputStream" => {
                kind = "zipfile-entry";
                break;
            }
            "java/util/zip/GZIPInputStream" => {
                kind = "gzip";
                break;
            }
            "java/util/zip/InflaterInputStream" if kind == "direct" => kind = "inflater-stream",
            _ => {}
        }
    }
    let mut consumer: Option<String> = None;
    for (class, method) in innermost_first {
        if class == "jdk/internal/loader/Resource" && method == "getBytes" {
            return (kind, "Resource.getBytes".to_string());
        }
        if consumer.is_none() && !is_stream_layer(class) {
            consumer = Some(format!("{class}.{method}"));
        }
    }
    (kind, consumer.unwrap_or_else(|| "<no frame outside the stream layers>".to_string()))
}

/// Tally one inflate call that produced `bytes`. A no-op unless the census
/// is on.
#[inline]
pub(crate) fn note_inflate(ctx: &mut dyn NativeContext, bytes: u32) {
    if enabled() {
        note_inflate_slow(ctx, bytes);
    }
}

#[cold]
fn note_inflate_slow(ctx: &mut dyn NativeContext, bytes: u32) {
    let began = std::time::Instant::now();
    let (frames, _) = ctx.capture_stack_trace_tail(TAIL);
    // The capture is OUTERMOST-first.
    let key = classify(
        frames
            .iter()
            .rev()
            .map(|e| (&*e.class_name, &*e.method_name)),
    );
    let mut guard = tallies().lock();
    let add = |row: &mut Row| {
        row.calls += 1;
        row.bytes += u64::from(bytes);
    };
    add(&mut guard.total);
    if let Some(row) = guard.rows.get_mut(&key) {
        add(row);
    } else if guard.rows.len() < ROW_CAP {
        let mut row = Row::default();
        add(&mut row);
        guard.rows.insert(key, row);
    } else {
        guard.dropped += 1;
    }
    drop(guard);
    let spent = began.elapsed().as_nanos() as u64;
    CAPTURE_NS.with(|c| c.set(c.get().saturating_add(spent)));
}

/// Print the census (`[zip-stream-census]` lines on stderr). A no-op unless
/// the census is on; called once at exit by the launcher.
pub fn dump() {
    if !enabled() {
        return;
    }
    let guard = tallies().lock();
    let t = guard.total;
    eprintln!(
        "[zip-stream-census] total inflate-calls={} bytes={} native-ns={}",
        t.calls, t.bytes, guard.native_ns
    );
    let mut by_kind: std::collections::BTreeMap<(&str, bool), Row> = Default::default();
    for ((kind, consumer), row) in &guard.rows {
        let e = by_kind
            .entry((*kind, consumer == "Resource.getBytes"))
            .or_default();
        e.calls += row.calls;
        e.bytes += row.bytes;
    }
    for ((kind, get_bytes), row) in &by_kind {
        eprintln!(
            "[zip-stream-census] kind={kind} {} calls={} bytes={}",
            if *get_bytes { "via=Resource.getBytes" } else { "via=other" },
            row.calls,
            row.bytes
        );
    }
    let mut rows: Vec<_> = guard.rows.iter().collect();
    rows.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(b.1.calls.cmp(&a.1.calls)));
    for ((kind, consumer), row) in rows.into_iter().take(DUMP_ROWS) {
        eprintln!(
            "[zip-stream-census] row kind={kind} consumer={consumer} calls={} bytes={}",
            row.calls, row.bytes
        );
    }
    if guard.dropped > 0 {
        eprintln!("[zip-stream-census] rows-dropped={} (cap {ROW_CAP})", guard.dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn i116_a_class_read_and_a_resource_stream_are_told_apart() {
        let class_read = [
            ("java/util/zip/Inflater", "inflate"),
            ("java/util/zip/InflaterInputStream", "read"),
            ("java/util/zip/ZipFile$ZipFileInflaterInputStream", "read"),
            ("java/io/InputStream", "readNBytes"),
            ("jdk/internal/loader/Resource", "getBytes"),
            ("jdk/internal/loader/BuiltinClassLoader", "defineClass"),
        ];
        assert_eq!(
            classify(class_read.iter().copied()),
            ("zipfile-entry", "Resource.getBytes".to_string())
        );
        let properties = [
            ("java/util/zip/Inflater", "inflate"),
            ("java/util/zip/InflaterInputStream", "read"),
            ("java/util/zip/ZipFile$ZipFileInflaterInputStream", "read"),
            ("sun/nio/cs/StreamDecoder", "readBytes"),
            ("java/io/InputStreamReader", "read"),
            ("java/util/Properties$LineReader", "readLine"),
            ("java/util/Properties", "load0"),
        ];
        assert_eq!(
            classify(properties.iter().copied()),
            ("zipfile-entry", "java/util/Properties$LineReader.readLine".to_string())
        );
        let gzip = [
            ("java/util/zip/Inflater", "inflate"),
            ("java/util/zip/InflaterInputStream", "read"),
            ("java/util/zip/GZIPInputStream", "read"),
            ("App", "main"),
        ];
        assert_eq!(classify(gzip.iter().copied()), ("gzip", "App.main".to_string()));
        let direct = [("java/util/zip/Inflater", "inflate"), ("App", "main")];
        assert_eq!(classify(direct.iter().copied()), ("direct", "App.main".to_string()));
    }
}
