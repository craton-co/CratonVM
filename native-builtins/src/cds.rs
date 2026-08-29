// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class Data Sharing (CDS / AppCDS) native method implementations.
//!
//! Provides archive infrastructure and the native stubs required by
//! `jdk/internal/misc/CDS` and `sun/management/ManagementFactoryHelper`.
//!
//! The JVM boots with CDS **disabled** by default (sharing = 0).  The archive
//! types below are present so that a future dump/load path can be wired in
//! without changing the public registration surface.
//!
//! F17-1 (2026-08-13): this header used to also claim `sun/misc/VM` and
//! `java/lang/ClassLoader`. It no longer does, because the registrations behind
//! that claim were fabrications — `javap` cannot find `sun.misc.VM` in the JDK
//! 25 image at all, and `java.lang.ClassLoader` has no `getCdsArchivePath`. See
//! the tombstone above the native handler functions for the transcripts.
//! `sun/management/CDSMetrics` went the same way, which is why "metrics
//! reporting" has left this sentence: the Rust `CdsMetrics` type stayed, the
//! Java class it was projected into never existed.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use crate::native_noop_with_this;

// ---------------------------------------------------------------------------
// Archive format constants
// ---------------------------------------------------------------------------

/// Magic number written at the start of every CDS archive file.
pub const CDS_MAGIC: u32 = 0xF00D_1234;

/// Current archive format version understood by this JVM.
pub const CDS_VERSION: u16 = 1;

/// Maximum number of classes that can be recorded in a single archive.
pub const CDS_MAX_CLASSES: usize = 65536;

// ---------------------------------------------------------------------------
// CdsArchive — on-disk / in-memory header
// ---------------------------------------------------------------------------

/// Header at byte offset 0 of every CDS archive file.
///
/// Layout (little-endian):
/// ```text
///  0..4   magic   (u32)  — must equal CDS_MAGIC
///  4..6   version (u16)  — must equal CDS_VERSION
///  6..10  count   (u32)  — number of CdsArchiveEntry records that follow
/// 10..18  checksum(u64)  — SHA-256 (truncated) over all entry bytes
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdsArchive {
    /// Must be `CDS_MAGIC`.
    pub magic: u32,
    /// Format version; must be `CDS_VERSION`.
    pub version: u16,
    /// Number of class entries stored.
    pub class_count: u32,
    /// Simple integrity checksum over the entry block.
    pub checksum: u64,
}

impl CdsArchive {
    /// Create a new, empty archive header.
    pub fn new() -> Self {
        CdsArchive {
            magic: CDS_MAGIC,
            version: CDS_VERSION,
            class_count: 0,
            checksum: 0,
        }
    }

    /// Return `true` if the magic and version fields are valid.
    pub fn is_valid(&self) -> bool {
        self.magic == CDS_MAGIC && self.version == CDS_VERSION
    }
}

impl Default for CdsArchive {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CdsArchiveEntry — per-class record inside the archive
// ---------------------------------------------------------------------------

/// A single class record stored inside a `CdsArchive`.
#[derive(Debug, Clone)]
pub struct CdsArchiveEntry {
    /// Internal class name (slash-separated), e.g. `java/lang/Object`.
    pub class_name: String,
    /// Byte offset of the raw class bytes within the archive data section.
    pub bytes_offset: u64,
    /// Number of raw class bytes (derived from `class_bytes.len()`).
    pub bytes_length: u32,
    /// Access flags copied from the class file.
    pub access_flags: u16,
    /// Internal name of the superclass, if any.
    pub superclass: Option<String>,
    /// Internal names of directly implemented interfaces.
    pub interfaces: Vec<String>,
    /// The raw `.class` file bytes for this entry.
    pub class_bytes: Vec<u8>,
}

impl CdsArchiveEntry {
    /// Construct a new entry with no superclass / interfaces.
    ///
    /// `bytes_length` is automatically computed from `class_bytes.len()`.
    pub fn new(class_name: impl Into<String>, bytes_offset: u64, class_bytes: Vec<u8>) -> Self {
        let bytes_length = class_bytes.len() as u32;
        CdsArchiveEntry {
            class_name: class_name.into(),
            bytes_offset,
            bytes_length,
            access_flags: 0x0021, // ACC_PUBLIC | ACC_SUPER
            superclass: None,
            interfaces: Vec::new(),
            class_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// CdsArchiveGenerator — collects classes, then writes the archive
// ---------------------------------------------------------------------------

/// Collects class metadata during a JVM dump run and writes a binary archive.
///
/// In a real dump-capable JVM this would be invoked via `-Xshare:dump`.  In
/// this stub implementation it is present so the type hierarchy is complete;
/// `write_archive` always succeeds but produces no output.
#[derive(Debug, Default)]
pub struct CdsArchiveGenerator {
    entries: Vec<CdsArchiveEntry>,
    output_path: String,
}

impl CdsArchiveGenerator {
    /// Create a generator that will write to `output_path`.
    pub fn new(output_path: impl Into<String>) -> Self {
        CdsArchiveGenerator {
            entries: Vec::new(),
            output_path: output_path.into(),
        }
    }

    /// Record a class entry to be included in the archive.
    pub fn add_entry(&mut self, entry: CdsArchiveEntry) {
        if self.entries.len() < CDS_MAX_CLASSES {
            self.entries.push(entry);
        }
    }

    /// Return the number of entries collected so far.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Return the configured output path.
    pub fn output_path(&self) -> &str {
        &self.output_path
    }

    /// Produce the archive header for the current entry set.
    pub fn build_header(&self) -> CdsArchive {
        CdsArchive {
            magic: CDS_MAGIC,
            version: CDS_VERSION,
            class_count: self.entries.len() as u32,
            checksum: self.compute_checksum(),
        }
    }

    /// Write the archive to the configured output file.
    ///
    /// Binary format:
    ///   [4 bytes] magic (0xF00D_1234)
    ///   [2 bytes] version (1)
    ///   [4 bytes] class_count
    ///   [8 bytes] checksum (FNV-1a)
    ///   For each entry:
    ///     [2 bytes] name_length
    ///     [N bytes] class_name (UTF-8)
    ///     [4 bytes] bytes_length
    ///     [N bytes] class_bytes (raw .class data)
    pub fn write_archive(&self) -> Result<(), String> {
        use std::io::Write;
        let header = self.build_header();
        let mut file = std::fs::File::create(&self.output_path)
            .map_err(|e| format!("CDS write failed: {}", e))?;

        // Write header
        file.write_all(&header.magic.to_be_bytes())
            .map_err(|e| e.to_string())?;
        file.write_all(&header.version.to_be_bytes())
            .map_err(|e| e.to_string())?;
        file.write_all(&header.class_count.to_be_bytes())
            .map_err(|e| e.to_string())?;
        file.write_all(&header.checksum.to_be_bytes())
            .map_err(|e| e.to_string())?;

        // Write entries
        for entry in &self.entries {
            let name_bytes = entry.class_name.as_bytes();
            file.write_all(&(name_bytes.len() as u16).to_be_bytes())
                .map_err(|e| e.to_string())?;
            file.write_all(name_bytes).map_err(|e| e.to_string())?;
            let bytes_length = entry.class_bytes.len() as u32;
            file.write_all(&bytes_length.to_be_bytes())
                .map_err(|e| e.to_string())?;
            // Write real class bytes from the entry
            file.write_all(&entry.class_bytes)
                .map_err(|e| e.to_string())?;
        }

        tracing::info!(
            "CDS archive written: {} entries to {}",
            self.entries.len(),
            self.output_path,
        );
        Ok(())
    }

    /// Compute a SHA-256 based integrity checksum over all entries.
    /// Uses SHA-256 for cryptographic integrity (truncated to u64).
    fn compute_checksum(&self) -> u64 {
        use crate::crypto_impl::Sha256;
        let mut hasher = Sha256::new();
        let mut sorted: Vec<&CdsArchiveEntry> = self.entries.iter().collect();
        sorted.sort_by(|a, b| a.class_name.cmp(&b.class_name));
        for entry in &sorted {
            hasher.update(entry.class_name.as_bytes());
            hasher.update(&entry.class_bytes);
        }
        let digest = hasher.finalize();
        // Truncate SHA-256 to u64 for the header format
        u64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ])
    }
}

// ---------------------------------------------------------------------------
// CdsArchiveLoader — maps/loads an archive at JVM startup
// ---------------------------------------------------------------------------

/// Loads a CDS archive on JVM startup and populates the class cache.
///
/// In a sharing-enabled JVM this would `mmap` the archive file.  Here it is
/// a stub that records the path and reports zero classes loaded.
#[derive(Debug)]
pub struct CdsArchiveLoader {
    archive_path: String,
    loaded: bool,
    classes_loaded: u32,
    load_time_ms: u64,
    /// Cached class bytes keyed by internal class name.
    /// Populated during try_load() for fast class lookup.
    class_cache: std::collections::HashMap<String, Vec<u8>>,
}

impl CdsArchiveLoader {
    /// Create a loader for the given archive path.
    pub fn new(path: impl Into<String>) -> Self {
        CdsArchiveLoader {
            archive_path: path.into(),
            loaded: false,
            classes_loaded: 0,
            load_time_ms: 0,
            class_cache: std::collections::HashMap::new(),
        }
    }

    /// Attempt to open and validate the archive.
    /// Returns true if the archive was successfully loaded.
    pub fn try_load(&mut self) -> bool {
        use std::io::Read;
        let start = std::time::Instant::now();

        let mut file = match std::fs::File::open(&self.archive_path) {
            Ok(f) => f,
            Err(_) => return false,
        };

        // Read header (18 bytes)
        let mut header_buf = [0u8; 18];
        if file.read_exact(&mut header_buf).is_err() {
            return false;
        }

        let magic =
            u32::from_be_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        let version = u16::from_be_bytes([header_buf[4], header_buf[5]]);
        let class_count =
            u32::from_be_bytes([header_buf[6], header_buf[7], header_buf[8], header_buf[9]]);
        let checksum = u64::from_be_bytes([
            header_buf[10],
            header_buf[11],
            header_buf[12],
            header_buf[13],
            header_buf[14],
            header_buf[15],
            header_buf[16],
            header_buf[17],
        ]);

        if magic != CDS_MAGIC || version != CDS_VERSION {
            tracing::warn!(
                "CDS archive invalid: magic={:#x} version={}",
                magic,
                version
            );
            return false;
        }

        // Read entries and their class bytes
        let mut entries_read = 0u32;
        for _ in 0..class_count {
            // Read name length (2 bytes)
            let mut len_buf = [0u8; 2];
            if file.read_exact(&mut len_buf).is_err() {
                break;
            }
            let name_len = u16::from_be_bytes(len_buf) as usize;

            // Read name bytes
            let mut name_buf = vec![0u8; name_len];
            if file.read_exact(&mut name_buf).is_err() {
                break;
            }

            // Read bytes_length (4 bytes)
            let mut blen_buf = [0u8; 4];
            if file.read_exact(&mut blen_buf).is_err() {
                break;
            }
            let bytes_len = u32::from_be_bytes(blen_buf) as usize;

            // Read real class bytes and cache them
            let mut class_bytes = vec![0u8; bytes_len];
            if file.read_exact(&mut class_bytes).is_err() {
                break;
            }

            // Store in cache for class loading
            if let Ok(class_name) = String::from_utf8(name_buf) {
                if !class_bytes.is_empty() {
                    self.class_cache.insert(class_name, class_bytes);
                }
            }
            entries_read += 1;
        }

        // Verify checksum integrity (must iterate in same order as generator)
        {
            use crate::crypto_impl::Sha256;
            let mut hasher = Sha256::new();
            let mut sorted_keys: Vec<&String> = self.class_cache.keys().collect();
            sorted_keys.sort();
            for name in &sorted_keys {
                let bytes = &self.class_cache[*name];
                hasher.update(name.as_bytes());
                hasher.update(bytes);
            }
            let digest = hasher.finalize();
            let computed = u64::from_be_bytes([
                digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6],
                digest[7],
            ]);
            if computed != checksum {
                tracing::warn!(
                    "CDS archive checksum mismatch: expected {:#x}, got {:#x}",
                    checksum,
                    computed
                );
                self.class_cache.clear();
                return false;
            }
        }

        self.loaded = entries_read > 0;
        self.classes_loaded = entries_read;
        self.load_time_ms = start.elapsed().as_millis() as u64;

        if self.loaded {
            tracing::info!(
                "CDS archive loaded: {} classes from {} ({}ms)",
                entries_read,
                self.archive_path,
                self.load_time_ms,
            );
        }
        self.loaded
    }

    /// Return whether the archive was successfully loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Number of classes read from the archive (0 until actually loaded).
    pub fn classes_loaded(&self) -> u32 {
        self.classes_loaded
    }

    /// Milliseconds taken to load the archive.
    pub fn load_time_ms(&self) -> u64 {
        self.load_time_ms
    }

    /// The configured archive path.
    pub fn archive_path(&self) -> &str {
        &self.archive_path
    }

    /// Look up cached class bytes by internal name (e.g., "java/lang/Object").
    /// Returns the raw .class file bytes if the class was in the archive.
    pub fn find_class_bytes(&self, name: &str) -> Option<&[u8]> {
        self.class_cache.get(name).map(|v| v.as_slice())
    }

    /// Return the number of classes cached from the archive.
    pub fn cached_class_count(&self) -> usize {
        self.class_cache.len()
    }

    /// Drain all cached class bytes, returning ownership.
    ///
    /// After this call the internal cache is empty — the caller is expected to
    /// transfer the entries into the `ClassManager`'s CDS cache.
    pub fn drain_class_cache(&mut self) -> std::collections::HashMap<String, Vec<u8>> {
        std::mem::take(&mut self.class_cache)
    }
}

// ---------------------------------------------------------------------------
// CdsMetrics — statistics reported via sun/management/ManagementFactoryHelper
// ---------------------------------------------------------------------------

/// Runtime CDS statistics exposed to management callers.
#[derive(Debug, Clone, Default)]
pub struct CdsMetrics {
    /// Total classes present in the archive (may be 0 if no archive loaded).
    pub total_classes_in_archive: u32,
    /// Classes actually read from the archive this session.
    pub classes_loaded_from_archive: u32,
    /// Size of the archive file in bytes.
    pub archive_size_bytes: u64,
    /// Time taken to load the archive (milliseconds).
    pub archive_load_time_ms: u64,
    /// Filesystem path to the archive, or empty string if none.
    pub archive_path: String,
}

impl CdsMetrics {
    /// Create a default (all-zero, no archive) metrics instance.
    pub fn disabled() -> Self {
        CdsMetrics {
            total_classes_in_archive: 0,
            classes_loaded_from_archive: 0,
            archive_size_bytes: 0,
            archive_load_time_ms: 0,
            archive_path: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// AppCDS support types
// ---------------------------------------------------------------------------

/// Configuration for Application Class Data Sharing (AppCDS).
///
/// Controls which classes are eligible for archiving and where the output is
/// written.
#[derive(Debug, Default)]
pub struct AppCdsConfig {
    /// Glob patterns for class names to include (e.g. `"com/example/**"`).
    pub include_patterns: Vec<String>,
    /// Glob patterns for class names to exclude.
    pub exclude_patterns: Vec<String>,
    /// Output path for the generated dynamic archive.
    pub output_path: String,
}

impl AppCdsConfig {
    /// Create a config that archives all classes to `output_path`.
    pub fn new(output_path: impl Into<String>) -> Self {
        AppCdsConfig {
            include_patterns: vec!["**".to_string()],
            exclude_patterns: Vec::new(),
            output_path: output_path.into(),
        }
    }

    /// Return `true` if the class name matches include/exclude patterns.
    ///
    /// Simple prefix match: include wins over exclude, exclude wins by default.
    pub fn accepts(&self, class_name: &str) -> bool {
        // Any explicit exclude wins first
        for pat in &self.exclude_patterns {
            let prefix = pat.trim_end_matches('*').trim_end_matches('/');
            if class_name.starts_with(prefix) {
                return false;
            }
        }
        // Then check includes
        for pat in &self.include_patterns {
            if pat == "**" {
                return true;
            }
            let prefix = pat.trim_end_matches('*').trim_end_matches('/');
            if class_name.starts_with(prefix) {
                return true;
            }
        }
        false
    }
}

/// Dynamic CDS archive (JDK 13+).
///
/// Captures additional application classes at runtime on top of a base
/// shared archive.  This stub records entries but does no actual sharing.
#[derive(Debug, Default)]
pub struct DynamicArchive {
    base: CdsArchiveGenerator,
    config: AppCdsConfig,
    finalized: bool,
}

impl DynamicArchive {
    /// Create a new dynamic archive with the supplied configuration.
    pub fn new(config: AppCdsConfig) -> Self {
        let output_path = config.output_path.clone();
        DynamicArchive {
            base: CdsArchiveGenerator::new(output_path),
            config,
            finalized: false,
        }
    }

    /// Record a class if it matches the archive config's include/exclude rules.
    pub fn record_class(&mut self, entry: CdsArchiveEntry) {
        if self.config.accepts(&entry.class_name) {
            self.base.add_entry(entry);
        }
    }

    /// Return the number of classes recorded so far.
    pub fn recorded_count(&self) -> usize {
        self.base.entry_count()
    }

    /// Finalise and write the dynamic archive.  (Stub: no I/O.)
    pub fn finalize(&mut self) -> Result<(), String> {
        self.finalized = true;
        self.base.write_archive()
    }

    /// Return `true` after `finalize()` has been called.
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }
}

// ---------------------------------------------------------------------------
// Class-list file support
// ---------------------------------------------------------------------------

/// Parse a class list file produced by `-XX:DumpLoadedClassList`.
///
/// Each non-blank, non-comment line is treated as one class name (internal
/// slash-separated form or dot-separated; both are normalised to slashes).
pub fn parse_class_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.replace('.', "/"))
        .collect()
}

/// Render a list of class names to a class-list file format.
pub fn format_class_list(classes: &[String]) -> String {
    classes.join("\n")
}

// ---------------------------------------------------------------------------
// Synthetic object allocation helpers
// ---------------------------------------------------------------------------

// F17-1 (2026-08-13) — TWO ALLOCATION HELPERS USED TO LIVE HERE. Both are gone
// because the only natives that called them are gone; see the tombstone at the
// head of the handler section below for the `javap` evidence.
//
//   * `alloc_cds_metrics_obj` built a 5-slot `sun/management/CDSMetrics`. That
//     class is not in the JDK 25 runtime image.
//   * `alloc_properties_obj` built a 2-slot `java/util/Properties` for
//     `sun/misc/VM.savedProps()`. That class is not in the JDK 25 runtime image
//     either, and the 2-slot shape it minted is not `Properties`' real layout —
//     it was only ever safe because nothing but the deleted native read it.

// ---------------------------------------------------------------------------
// Individual native handler functions (named, non-capturing)
// ---------------------------------------------------------------------------

// ===========================================================================
// F17-1 TOMBSTONE (2026-08-13) — WHAT USED TO BE HERE AND WHY IT IS NOT.
//
// Eleven natives were deleted from this file, across three groups. All of them
// named a class or a member that the JDK 25 runtime image does not contain, so
// none of them could ever be dispatched to by real JDK bytecode; the fact that
// `sun.misc.VM` and `sun.management.CDSMetrics` were real *once* is exactly what
// made them plausible enough to survive this long. Measured on Microsoft
// 25.0.3+9-LTS (`java -version`: `OpenJDK Runtime Environment
// Microsoft-13877124 (build 25.0.3+9-LTS)`), not inferred:
//
//   1. `sun/management/CDSMetrics` — SIX registrations (`<init>` plus five
//      accessors), and its factory `sun/management/ManagementFactoryHelper
//      .getCDSMetrics()Lsun/management/CDSMetrics;`.
//
//          $ javap sun.management.CDSMetrics
//          Error: class not found: sun.management.CDSMetrics
//          $ javap -p sun.management.ManagementFactoryHelper | grep -c CDS
//          0
//
//      The second command is the one that matters: `ManagementFactoryHelper`
//      IS in the image and DOES load, so the failure mode was not "class
//      missing" but a real class carrying a method it does not declare. `javap
//      -p` shows all 22 of its methods and none mentions CDS. Baseline:
//      scripts/baselines/jdk25-sun.management.ManagementFactoryHelper.tsv.
//
//   2. `sun/misc/VM` — THREE registrations (`<init>`, `isBooted()Z`,
//      `savedProps()Ljava/util/Properties;`).
//
//          $ javap -p sun.misc.VM
//          Error: class not found: sun.misc.VM
//
//      The JDK-true home of this functionality is `jdk.internal.misc.VM`, and
//      NOTHING WAS ADDED HERE FOR IT — that class is already owned in full by
//      `lib.rs` (`isBooted()Z` at lib.rs:14894, alongside `initLevel`,
//      `getSavedProperty` and friends). `register()` is last-write-wins, so
//      minting a second body for those triples in this file would silently
//      decide which one runs based on registrar call order. Note also that the
//      shapes do not transfer: JDK 25 declares `getSavedProperties()Ljava/util/
//      Map;` and `getSavedProperty(Ljava/lang/String;)Ljava/lang/String;`, with
//      `savedProps` surviving only as a private *field*, so `savedProps()` had
//      no descriptor-compatible successor to be corrected into.
//
//   3. `java/lang/ClassLoader.getCdsArchivePath()Ljava/lang/String;` — ONE
//      registration on a class that very much exists.
//
//          $ javap -p java.lang.ClassLoader | grep -i -e archive -e cds
//            private void resetArchivedStates();
//
//      That single hit is the whole CDS-adjacent surface of `ClassLoader` in
//      JDK 25; there is no `getCdsArchivePath` at any access level. This is the
//      most dangerous of the three shapes, because a registration on a real,
//      always-loaded class reads as legitimate at a glance.
//
// DISPATCH CHECK, done before deleting rather than after (`call_native` panics
// on an unregistered triple, and a Rust panic kills the VM instead of raising
// something Java can catch): a tree-wide grep for `sun/misc/VM`, `CDSMetrics`
// and `getCdsArchivePath` finds no caller outside this file and its tests — the
// only surviving mention is a prose comment at lib.rs:14892. And this registrar
// is doubly out of reach of both shipping modes anyway: `register_cds_natives`
// is called only from `register_synthetic_overrides` (`#[cfg(feature =
// "synthetic-jdk")]`) and only under `#[cfg(feature = "experimental-aot")]`.
//
// `CdsMetrics` (the Rust struct) is deliberately still here. It is the archive
// generator's own bookkeeping type and has tests of its own; only the *Java*
// class it used to be projected into was fabricated.
// ===========================================================================

// --- jdk/internal/misc/CDS ---

fn native_cds_is_dumping_archive(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `jdk/internal/misc/CDS.isUsingArchive()Z`.
///
/// F17-1 (2026-08-13): RENAMED from `isSharingEnabled`, which JDK 25 does not
/// declare at any access level. `javap -p jdk.internal.misc.CDS` lists 23
/// members; the predicate for "is the VM using at least one CDS archive?" is
/// spelled `isUsingArchive`, and the JDK's own javadoc on it is that sentence
/// verbatim. Same descriptor, same meaning, different name — so this is a
/// spelling repair, not a deletion, and the body is unchanged.
///
/// A sibling fabrication, `isDumpingClassList()Z`, was deleted outright in the
/// same pass rather than renamed: there is no JDK 25 predicate it corresponds
/// to. `isDumpingArchive()Z` (already registered below, real spelling) and
/// `isDumpingStaticArchive()Z` are the two that exist, and neither means "is a
/// class list being written" — `dumpClassList(String)` is the action, and
/// HotSpot gates it on the same `configStatus` word rather than on a predicate
/// of its own.
fn native_cds_is_using_archive(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let enabled = ctx
        .get_system_property("jdk.internal.vm.cds.enabled")
        .map_or(false, |v| v == "true");
    Ok(Some(Value::Int(if enabled { 1 } else { 0 })))
}

/// `jdk/internal/misc/CDS.getCDSConfigStatus()I` — ADDED by F17-1 (2026-08-13).
///
/// This is a real `private static native` on JDK 25's `CDS`, and it was the
/// most load-bearing member of the class that this registrar did not cover:
///
///     $ javap -p jdk.internal.misc.CDS | grep getCDSConfigStatus
///       private static native int getCDSConfigStatus();
///
/// It is called from `CDS.<clinit>` — `private static final int configStatus =
/// getCDSConfigStatus();` (jdk25src java.base/jdk/internal/misc/CDS.java:55) —
/// which means every one of the class's five public predicates
/// (`isLoggingLambdaFormInvokers`, `isDumpingArchive`, `isUsingArchive`,
/// `isDumpingStaticArchive`, `isSingleThreadVM`) reads a field that only this
/// native can fill, and merely *initialising* `CDS` requires it.
///
/// WHY IT WAS INVISIBLE. `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`
/// does not list it. That is not staleness — `generate.py:175` keeps only rows
/// whose flags contain `public`, so a baseline structurally cannot see a
/// `private static native`, which is the exact access level most JDK natives
/// live at. Auditing a native registrar against a public-only surface therefore
/// produces false positives AND false negatives at once, and this class shows
/// both: it reported the real `logLambdaFormInvoker(String)V` as off-surface
/// (see its registration below) while staying silent about two genuinely
/// missing natives.
///
/// Zero is the whole-truth answer, not a stub: the bits are
/// `IS_DUMPING_ARCHIVE|IS_DUMPING_METHOD_HANDLES|IS_DUMPING_STATIC_ARCHIVE|
/// IS_LOGGING_LAMBDA_FORM_INVOKERS|IS_USING_ARCHIVE` (CDS.java:50-54), and
/// CratonVM is doing none of those five things.
fn native_cds_get_config_status(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `jdk/internal/misc/CDS.needsClassInitBarrier0(Ljava/lang/Class;)Z` — ADDED by
/// F17-1 (2026-08-13). The second real native the public-only baseline could not
/// see:
///
///     $ javap -p jdk.internal.misc.CDS | grep needsClassInitBarrier
///       public static boolean needsClassInitBarrier(java.lang.Class<?>);
///       private static native boolean needsClassInitBarrier0(java.lang.Class<?>);
///
/// Note the pair: the *public* half is on the baseline and the native half is
/// not, so a name-keyed audit sees `needsClassInitBarrier` as covered while the
/// method that actually needs a body is the one ending in `0`. Registering the
/// public half instead would have been wrong twice over — it has real bytecode,
/// so under `--jdk-only` §7 step 3 answers `Bytecode` for it and the native
/// would never run.
///
/// `false` is correct here for the same reason `getCDSConfigStatus` returns 0:
/// the barrier exists to serialise archived-heap class initialisation, and this
/// VM maps no archived heap.
fn native_cds_needs_class_init_barrier0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_cds_initialize_from_archive(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Nothing to restore: CratonVM's archive stores class BYTES only, with no
    // archived heap subgraph, so "the archived object was absent" is the
    // truthful outcome and every `java.base` caller is written for that case.
    //
    // The descriptor is `(Ljava/lang/Class;)V` — VOID. This used to return
    // `Ok(Some(Value::Int(0)))`, handing back an operand for a method whose
    // caller emits no pop. Void natives must return `Ok(None)`.
    Ok(None)
}

fn native_cds_get_random_seed_for_dumping(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // MUST be 0 when not dumping a static archive — and CratonVM never dumps
    // one. HotSpot returns 0 outside `-Xshare:dump`, and `ImmutableCollections`
    // depends on that: it seeds its SALT from this value and falls back to
    // `System.nanoTime()` only when the seed is 0.
    //
    // This used to return a fixed 12_345_678, which made SALT32L identical on
    // every run and removed the per-JVM iteration-order randomisation of
    // `Set.of` / `Map.of`. That is worse than a fidelity gap: the
    // randomisation exists so that code accidentally depending on
    // immutable-collection iteration order fails visibly rather than passing
    // here and breaking elsewhere. Pinning the seed hides precisely the class
    // of bug it was designed to expose.
    Ok(Some(Value::Long(0)))
}

fn native_cds_log_lambda_form_invoker(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_cds_define_archived_modules(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_cds_dump_class_list(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Would write the loaded-class list to the path in args[0]; stub no-op.
    Ok(None)
}

fn native_cds_dump_dynamic_archive(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Would write a dynamic AppCDS archive; stub no-op.
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all CDS / AppCDS native methods into `r`.
pub(crate) fn register_cds_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // -- sun/management/ManagementFactoryHelper --
    {
        let cls = "sun/management/ManagementFactoryHelper";
        // Pure static-holder: in the JDK this class is `final` with a private
        // constructor and only static factory/accessor members. There is no
        // instance state for a no-arg constructor to establish, so an empty
        // body is the real implementation, not a stub. KEEP.
        //
        // F17-1 (2026-08-13): the `getCDSMetrics()Lsun/management/CDSMetrics;`
        // that used to sit here is gone, and so is the entire
        // `sun/management/CDSMetrics` block that followed it. So are the
        // `java/lang/ClassLoader.getCdsArchivePath()` and `sun/misc/VM`
        // registrations. Do not re-add any of them — the `javap` transcripts are
        // in the tombstone above the handler functions.
        r.register(cls, "<init>", "()V", native_noop_with_this);
    }

    // -- jdk/internal/misc/CDS --
    {
        let cls = "jdk/internal/misc/CDS";
        // Pure static-holder: every `jdk.internal.misc.CDS` member — and every
        // native registered on it below — is static, and the class carries no
        // instance fields for a constructor to initialize. KEEP.
        r.register(cls, "<init>", "()V", native_noop_with_this);
        // F17-1 (2026-08-13): `isDumpingClassList()Z` USED TO BE THE SECOND
        // REGISTRATION HERE AND IS GONE. `javap -p jdk.internal.misc.CDS` on
        // Microsoft 25.0.3+9-LTS lists 23 members and none is named
        // `isDumpingClassList` at any access level. It has no differently-spelled
        // equivalent to be corrected into either — see
        // `native_cds_is_using_archive`'s doc comment for why
        // `isDumpingArchive`/`isDumpingStaticArchive` are not it.
        r.register(
            cls,
            "isDumpingArchive",
            "()Z",
            native_cds_is_dumping_archive,
        );
        // F17-1: was `isSharingEnabled`, a name JDK 25 does not declare.
        // `isUsingArchive()Z` is the JDK-true spelling of the same predicate.
        r.register(cls, "isUsingArchive", "()Z", native_cds_is_using_archive);
        // F17-1: both ADDED. Real `private static native` members of JDK 25's
        // `CDS` that no registrar covered, and that a public-only baseline
        // cannot report as missing. `getCDSConfigStatus` is the one `<clinit>`
        // calls. See their doc comments.
        r.register(
            cls,
            "getCDSConfigStatus",
            "()I",
            native_cds_get_config_status,
        );
        r.register(
            cls,
            "needsClassInitBarrier0",
            "(Ljava/lang/Class;)Z",
            native_cds_needs_class_init_barrier0,
        );
        r.register(
            cls,
            "initializeFromArchive",
            "(Ljava/lang/Class;)V",
            native_cds_initialize_from_archive,
        );
        r.register(
            cls,
            "getRandomSeedForDumping",
            "()J",
            native_cds_get_random_seed_for_dumping,
        );
        // F17-1: KEPT, against a report that called it off-surface. The
        // ONE-parameter `logLambdaFormInvoker` is the real native:
        //
        //     $ javap -p jdk.internal.misc.CDS | grep logLambdaFormInvoker
        //       private static native void logLambdaFormInvoker(java.lang.String);
        //       public static void logLambdaFormInvoker(String, String, String, String);
        //
        // JDK 25 declares BOTH. The four-String overload is ordinary Java
        // (CDS.java:142) whose whole body concatenates its arguments and calls
        // the one-String native (CDS.java:144), so registering a native for the
        // 4-arg form would shadow real bytecode with a body that drops three
        // arguments, and dropping the 1-arg form would take out the only one
        // that has no bytecode to fall back to. The audit that flagged this read
        // a public-only baseline, which by construction lists the public
        // overload and hides the private native — the descriptor difference
        // then looks like a divergence rather than an overload pair.
        r.register(
            cls,
            "logLambdaFormInvoker",
            "(Ljava/lang/String;)V",
            native_cds_log_lambda_form_invoker,
        );
        r.register(
            cls,
            "defineArchivedModules",
            "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
            native_cds_define_archived_modules,
        );
        r.register(
            cls,
            "dumpClassList",
            "(Ljava/lang/String;)V",
            native_cds_dump_class_list,
        );
        r.register(
            cls,
            "dumpDynamicArchive",
            "(Ljava/lang/String;)V",
            native_cds_dump_dynamic_archive,
        );
    }
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cds_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // -----------------------------------------------------------------------
    // Archive constant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cds_magic_constant() {
        assert_eq!(CDS_MAGIC, 0xF00D_1234);
    }

    #[test]
    fn test_cds_version_constant() {
        assert_eq!(CDS_VERSION, 1);
    }

    #[test]
    fn test_cds_max_classes_constant() {
        assert_eq!(CDS_MAX_CLASSES, 65536);
    }

    // -----------------------------------------------------------------------
    // CdsArchive header tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cds_archive_new_is_valid() {
        let archive = CdsArchive::new();
        assert!(archive.is_valid());
        assert_eq!(archive.magic, CDS_MAGIC);
        assert_eq!(archive.version, CDS_VERSION);
        assert_eq!(archive.class_count, 0);
    }

    #[test]
    fn test_cds_archive_default_is_valid() {
        let archive = CdsArchive::default();
        assert!(archive.is_valid());
    }

    #[test]
    fn test_cds_archive_bad_magic_is_invalid() {
        let archive = CdsArchive {
            magic: 0xDEAD_BEEF,
            version: CDS_VERSION,
            class_count: 0,
            checksum: 0,
        };
        assert!(!archive.is_valid());
    }

    #[test]
    fn test_cds_archive_bad_version_is_invalid() {
        let archive = CdsArchive {
            magic: CDS_MAGIC,
            version: 99,
            class_count: 0,
            checksum: 0,
        };
        assert!(!archive.is_valid());
    }

    // -----------------------------------------------------------------------
    // CdsArchiveEntry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_archive_entry_new() {
        let bytes = vec![0xCA; 512];
        let e = CdsArchiveEntry::new("java/lang/Object", 0, bytes);
        assert_eq!(e.class_name, "java/lang/Object");
        assert_eq!(e.bytes_offset, 0);
        assert_eq!(e.bytes_length, 512);
        assert_eq!(e.class_bytes.len(), 512);
        assert!(e.superclass.is_none());
        assert!(e.interfaces.is_empty());
    }

    #[test]
    fn test_archive_entry_with_superclass() {
        let bytes = vec![0xFE; 2048];
        let mut e = CdsArchiveEntry::new("java/lang/String", 512, bytes);
        e.superclass = Some("java/lang/Object".to_string());
        assert_eq!(e.superclass.as_deref(), Some("java/lang/Object"));
    }

    // -----------------------------------------------------------------------
    // CdsArchiveGenerator tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generator_starts_empty() {
        let gen = CdsArchiveGenerator::new("/tmp/classes.jsa");
        assert_eq!(gen.entry_count(), 0);
        assert_eq!(gen.output_path(), "/tmp/classes.jsa");
    }

    #[test]
    fn test_generator_add_entries() {
        let mut gen = CdsArchiveGenerator::new("/tmp/out.jsa");
        gen.add_entry(CdsArchiveEntry::new("java/lang/Object", 0, vec![0xCA; 100]));
        gen.add_entry(CdsArchiveEntry::new(
            "java/lang/String",
            100,
            vec![0xFE; 200],
        ));
        assert_eq!(gen.entry_count(), 2);
    }

    #[test]
    fn test_generator_header_class_count() {
        let mut gen = CdsArchiveGenerator::new("/tmp/out.jsa");
        gen.add_entry(CdsArchiveEntry::new("java/util/List", 0, vec![0xBA; 50]));
        let hdr = gen.build_header();
        assert!(hdr.is_valid());
        assert_eq!(hdr.class_count, 1);
    }

    #[test]
    fn test_generator_write_archive_stub_succeeds() {
        let gen = CdsArchiveGenerator::new("/tmp/out.jsa");
        assert!(gen.write_archive().is_ok());
    }

    #[test]
    fn test_generator_respects_max_classes() {
        let mut gen = CdsArchiveGenerator::new("/tmp/big.jsa");
        for i in 0..CDS_MAX_CLASSES + 10 {
            gen.add_entry(CdsArchiveEntry::new(
                format!("cls/{i}"),
                i as u64,
                vec![0x01],
            ));
        }
        assert_eq!(gen.entry_count(), CDS_MAX_CLASSES);
    }

    // -----------------------------------------------------------------------
    // CdsArchiveLoader tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_loader_new() {
        let loader = CdsArchiveLoader::new("/var/cache/jsa/app.jsa");
        assert_eq!(loader.archive_path(), "/var/cache/jsa/app.jsa");
        assert!(!loader.is_loaded());
        assert_eq!(loader.classes_loaded(), 0);
    }

    #[test]
    fn test_loader_try_load_returns_false() {
        let mut loader = CdsArchiveLoader::new("/non/existent.jsa");
        assert!(!loader.try_load());
        assert!(!loader.is_loaded());
    }

    // -----------------------------------------------------------------------
    // CdsMetrics tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cds_metrics_disabled() {
        let m = CdsMetrics::disabled();
        assert_eq!(m.total_classes_in_archive, 0);
        assert_eq!(m.classes_loaded_from_archive, 0);
        assert_eq!(m.archive_size_bytes, 0);
        assert_eq!(m.archive_load_time_ms, 0);
        assert!(m.archive_path.is_empty());
    }

    // -----------------------------------------------------------------------
    // AppCdsConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_appcds_config_new_accepts_all() {
        let cfg = AppCdsConfig::new("/tmp/dynamic.jsa");
        assert!(cfg.accepts("com/example/Foo"));
        assert!(cfg.accepts("java/lang/Object"));
    }

    #[test]
    fn test_appcds_config_exclude_wins() {
        let mut cfg = AppCdsConfig::new("/tmp/dyn.jsa");
        cfg.exclude_patterns
            .push("com/example/internal/**".to_string());
        cfg.include_patterns = vec!["com/example/**".to_string()];
        assert!(!cfg.accepts("com/example/internal/Secret"));
        assert!(cfg.accepts("com/example/Public"));
    }

    #[test]
    fn test_appcds_config_include_only() {
        let cfg = AppCdsConfig {
            include_patterns: vec!["com/myapp/**".to_string()],
            exclude_patterns: vec![],
            output_path: "/tmp/out.jsa".to_string(),
        };
        assert!(cfg.accepts("com/myapp/Main"));
        assert!(!cfg.accepts("java/lang/Object"));
    }

    // -----------------------------------------------------------------------
    // DynamicArchive tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_dynamic_archive_record_and_finalize() {
        let cfg = AppCdsConfig::new("/tmp/dyn.jsa");
        let mut dyn_arch = DynamicArchive::new(cfg);
        dyn_arch.record_class(CdsArchiveEntry::new("com/foo/Bar", 0, vec![0xBE; 128]));
        assert_eq!(dyn_arch.recorded_count(), 1);
        assert!(!dyn_arch.is_finalized());
        assert!(dyn_arch.finalize().is_ok());
        assert!(dyn_arch.is_finalized());
    }

    #[test]
    fn test_dynamic_archive_exclude_filters() {
        let mut cfg = AppCdsConfig::new("/tmp/dyn.jsa");
        cfg.include_patterns = vec!["com/keep/**".to_string()];
        let mut dyn_arch = DynamicArchive::new(cfg);
        dyn_arch.record_class(CdsArchiveEntry::new("com/keep/Good", 0, vec![0xAA; 10]));
        dyn_arch.record_class(CdsArchiveEntry::new("com/drop/Bad", 0, vec![0xBB; 10]));
        assert_eq!(dyn_arch.recorded_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Class-list parse/format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_class_list_basic() {
        let text = "java/lang/Object\njava/lang/String\n";
        let list = parse_class_list(text);
        assert_eq!(list, vec!["java/lang/Object", "java/lang/String"]);
    }

    #[test]
    fn test_parse_class_list_skips_comments() {
        let text = "# header\njava/util/List\n# more\njava/util/Map\n";
        let list = parse_class_list(text);
        assert_eq!(list, vec!["java/util/List", "java/util/Map"]);
    }

    #[test]
    fn test_parse_class_list_normalises_dots() {
        let text = "java.lang.Object\n";
        let list = parse_class_list(text);
        assert_eq!(list, vec!["java/lang/Object"]);
    }

    #[test]
    fn test_parse_class_list_blank_lines() {
        let text = "\n  \njava/util/Set\n\n";
        let list = parse_class_list(text);
        assert_eq!(list, vec!["java/util/Set"]);
    }

    #[test]
    fn test_format_class_list() {
        let classes = vec!["java/lang/Object".to_string(), "java/util/List".to_string()];
        let text = format_class_list(&classes);
        assert_eq!(text, "java/lang/Object\njava/util/List");
    }

    // -----------------------------------------------------------------------
    // Registration tests — verify all methods are findable
    // -----------------------------------------------------------------------

    #[test]
    fn test_management_factory_helper_registration() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "sun/management/ManagementFactoryHelper";
        assert!(r.find(cls, "<init>", "()V").is_some());
    }

    /// F17-1 (2026-08-13) — REPLACES four tests that asserted the presence of
    /// registrations for classes and members JDK 25 does not have
    /// (`test_cds_metrics_accessors_registered`,
    /// `test_classloader_cds_archive_path_registered`,
    /// `test_sun_misc_vm_registered`, and the `getCDSMetrics` half of
    /// `test_management_factory_helper_registration`).
    ///
    /// Those tests were not neutral about the fabrication — they PINNED it.
    /// Each one asserted `is_some()` on a triple whose defect was that it
    /// existed at all, so the only way to fail them was to fix the bug. Flipping
    /// them to `is_none()` is what turns roughly a hundred lines of deletion
    /// into something a reader can check without re-running `javap`.
    #[test]
    fn f17_1_registrar_mints_nothing_absent_from_the_jdk25_image() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        // `javap` on Microsoft 25.0.3+9-LTS answers "class not found" for each of
        // these three classes, so every triple on them was unreachable.
        let absent_classes = ["sun/management/CDSMetrics", "sun/misc/VM"];
        let registered: Vec<(&str, &str, &str)> = r
            .dump_registrations()
            .into_iter()
            .map(|(c, m, d, _)| (c, m, d))
            .collect();
        for cls in absent_classes {
            let minted: Vec<_> = registered
                .iter()
                .filter(|(c, _, _)| *c == cls)
                .map(|(_, m, d)| format!("{m}{d}"))
                .collect();
            assert!(
                minted.is_empty(),
                "{cls} is not in the JDK 25 runtime image, but the CDS registrar \
                 still mints natives for it: {minted:?}"
            );
        }
        // These two classes DO exist; the members did not.
        let absent_members: &[(&str, &str, &str)] = &[
            (
                "sun/management/ManagementFactoryHelper",
                "getCDSMetrics",
                "()Lsun/management/CDSMetrics;",
            ),
            (
                "java/lang/ClassLoader",
                "getCdsArchivePath",
                "()Ljava/lang/String;",
            ),
            ("jdk/internal/misc/CDS", "isDumpingClassList", "()Z"),
            ("jdk/internal/misc/CDS", "isSharingEnabled", "()Z"),
        ];
        for (cls, name, desc) in absent_members {
            assert!(
                r.find(cls, name, desc).is_none(),
                "{cls}.{name}{desc} is not declared by JDK 25 — re-registering it \
                 re-introduces the fabrication F17-1 removed"
            );
        }
    }

    #[test]
    fn test_jdk_internal_cds_all_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "jdk/internal/misc/CDS";
        // F17-1 (2026-08-13): every entry below is checked against `javap -p
        // jdk.internal.misc.CDS`, i.e. ALL access levels — not against
        // `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`, which
        // `generate.py:175` filters to `public` and which therefore lists none
        // of the natives this registrar exists to supply. `isDumpingClassList`
        // and `isSharingEnabled` were removed from this list; `isUsingArchive`,
        // `getCDSConfigStatus` and `needsClassInitBarrier0` were added.
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("isDumpingArchive", "()Z"),
            ("isUsingArchive", "()Z"),
            ("getCDSConfigStatus", "()I"),
            ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
            ("initializeFromArchive", "(Ljava/lang/Class;)V"),
            ("getRandomSeedForDumping", "()J"),
            ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
            (
                "defineArchivedModules",
                "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
            ),
            ("dumpClassList", "(Ljava/lang/String;)V"),
            ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
        ];
        for (name, desc) in methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing jdk/internal/misc/CDS.{name}{desc}"
            );
        }
    }

    /// F17-1 (2026-08-13): was `test_registration_count_at_least_20`. The
    /// registrar now mints 12, so a floor of 20 would be red — but the floor was
    /// never the point, and raising or lowering a `>=` is not either. What
    /// matters is that the count is EXACT and moves only when someone means it
    /// to: a one-sided `>=` cannot notice a fabricated class being added back,
    /// which is the regression this file has already had once.
    #[test]
    fn f17_1_registration_count_is_exact() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        assert_eq!(
            r.len(),
            12,
            "CDS registrar count changed. Expected 12: \
             ManagementFactoryHelper.<init> (1) + jdk/internal/misc/CDS (11). \
             If you added a registration, check it against `javap -p` on the \
             real JDK 25 image FIRST — `javap` with no flag, and the frozen \
             baselines, are public-only and cannot see a `private static \
             native`."
        );
    }

    // -----------------------------------------------------------------------
    // Return-value correctness tests (via registry dispatch)
    //
    // For functions that return pure constants and require no heap access we
    // can invoke the retrieved function pointer with a no-op stub context.
    // To keep the tests self-contained we use a minimal inline stub rather
    // than pulling in the full VM, consistent with the pattern used in
    // jmx_tests and crypto_tests which test only `r.find(…).is_some()`.
    // -----------------------------------------------------------------------

    /// Verify every registered native can be located in the registry.
    #[test]
    fn test_all_registered_methods_findable() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);

        // F17-1 (2026-08-13): eleven rows removed here — the whole
        // `sun/management/CDSMetrics` and `sun/misc/VM` blocks,
        // `ManagementFactoryHelper.getCDSMetrics`,
        // `ClassLoader.getCdsArchivePath`, and `CDS.{isDumpingClassList,
        // isSharingEnabled}` — and three added. The removals are classes and
        // members `javap` cannot find on Microsoft 25.0.3+9-LTS; the transcripts
        // are in the tombstone above the handler functions. This list is now the
        // registrar's full contents, which is what
        // `f17_1_registration_count_is_exact` cross-checks it against.
        let expected: &[(&str, &str, &str)] = &[
            ("sun/management/ManagementFactoryHelper", "<init>", "()V"),
            ("jdk/internal/misc/CDS", "<init>", "()V"),
            ("jdk/internal/misc/CDS", "isDumpingArchive", "()Z"),
            ("jdk/internal/misc/CDS", "isUsingArchive", "()Z"),
            ("jdk/internal/misc/CDS", "getCDSConfigStatus", "()I"),
            (
                "jdk/internal/misc/CDS",
                "needsClassInitBarrier0",
                "(Ljava/lang/Class;)Z",
            ),
            (
                "jdk/internal/misc/CDS",
                "initializeFromArchive",
                "(Ljava/lang/Class;)V",
            ),
            ("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J"),
            (
                "jdk/internal/misc/CDS",
                "logLambdaFormInvoker",
                "(Ljava/lang/String;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "defineArchivedModules",
                "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "dumpClassList",
                "(Ljava/lang/String;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "dumpDynamicArchive",
                "(Ljava/lang/String;)V",
            ),
        ];

        for (cls, name, desc) in expected {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing registration for {cls}.{name}{desc}"
            );
        }
        // F17-1: make the check two-sided. `is_some()` over a hand-written list
        // only ever proves the list is a SUBSET of what is registered — it is
        // structurally incapable of noticing an extra registration, which is
        // precisely how the eleven fabricated triples removed above sat here
        // being asserted-present rather than being questioned.
        assert_eq!(
            expected.len(),
            r.len(),
            "the registrar holds registrations this list does not name: {:?}",
            r.dump_registrations()
                .into_iter()
                .map(|(c, m, d, _)| (c, m, d))
                .filter(|t| !expected.contains(t))
                .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // CDS real class bytes round-trip test
    // -----------------------------------------------------------------------

    #[test]
    fn test_cds_write_and_read_real_class_bytes() {
        // Write an archive with real (simulated) class bytes and read it back
        let class_bytes_a = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34, 0xFF, 0xAB];
        let class_bytes_b = vec![
            0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x3D, 0x01, 0x02, 0x03,
        ];

        let tmp_dir = std::env::temp_dir();
        let archive_path = tmp_dir.join("test_cds_real_bytes.jsa");
        let path_str = archive_path.to_str().unwrap().to_string();

        // Write archive
        let mut gen = CdsArchiveGenerator::new(&path_str);
        gen.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            class_bytes_a.clone(),
        ));
        gen.add_entry(CdsArchiveEntry::new(
            "java/lang/String",
            class_bytes_a.len() as u64,
            class_bytes_b.clone(),
        ));
        let header = gen.build_header();
        assert!(header.is_valid());
        assert_eq!(header.class_count, 2);
        assert!(gen.write_archive().is_ok());

        // Read it back via CdsArchiveLoader
        let mut loader = CdsArchiveLoader::new(&path_str);
        assert!(loader.try_load());
        assert!(loader.is_loaded());
        assert_eq!(loader.classes_loaded(), 2);

        // Manually read the file to verify bytes are preserved
        use std::io::Read;
        let mut file = std::fs::File::open(&path_str).unwrap();
        // Skip header (4 + 2 + 4 + 8 = 18 bytes)
        let mut header_buf = [0u8; 18];
        file.read_exact(&mut header_buf).unwrap();

        // Read first entry
        let mut len_buf = [0u8; 2];
        file.read_exact(&mut len_buf).unwrap();
        let name_len = u16::from_be_bytes(len_buf) as usize;
        let mut name_buf = vec![0u8; name_len];
        file.read_exact(&mut name_buf).unwrap();
        assert_eq!(String::from_utf8(name_buf).unwrap(), "java/lang/Object");

        let mut blen_buf = [0u8; 4];
        file.read_exact(&mut blen_buf).unwrap();
        let bytes_len = u32::from_be_bytes(blen_buf) as usize;
        let mut read_bytes = vec![0u8; bytes_len];
        file.read_exact(&mut read_bytes).unwrap();
        assert_eq!(
            read_bytes, class_bytes_a,
            "First entry class bytes must be preserved"
        );

        // Read second entry
        file.read_exact(&mut len_buf).unwrap();
        let name_len2 = u16::from_be_bytes(len_buf) as usize;
        let mut name_buf2 = vec![0u8; name_len2];
        file.read_exact(&mut name_buf2).unwrap();
        assert_eq!(String::from_utf8(name_buf2).unwrap(), "java/lang/String");

        file.read_exact(&mut blen_buf).unwrap();
        let bytes_len2 = u32::from_be_bytes(blen_buf) as usize;
        let mut read_bytes2 = vec![0u8; bytes_len2];
        file.read_exact(&mut read_bytes2).unwrap();
        assert_eq!(
            read_bytes2, class_bytes_b,
            "Second entry class bytes must be preserved"
        );

        // Cleanup
        let _ = std::fs::remove_file(&path_str);
    }

    // -----------------------------------------------------------------------
    // Direct function invocation tests for zero-context handlers
    //
    // The handlers below only return a constant `Value`; they never call
    // methods on the `NativeContext` trait, so we pass a dummy reference
    // through a thin stub that satisfies the borrow checker.
    // -----------------------------------------------------------------------

    // =========================================================================
    // TEST-ONLY MOCK: PanicContext
    //
    // This is a no-op NativeContext stub used solely for unit tests that call
    // native handlers which never interact with the context. If a test
    // unexpectedly invokes a context method, the panic message identifies
    // exactly which method was called to aid debugging.
    //
    // This struct lives inside #[cfg(test)] and must NEVER be used in
    // production code.
    // =========================================================================
    struct PanicContext;

    impl cratonvm_native_api::NativeClassAccess for PanicContext {
        fn is_package_exported_unqualified(&self, _: &str, _: &str) -> bool {
            panic!("MockNativeContext: is_package_exported_unqualified not implemented for testing")
        }
        fn is_package_exported_to(&self, _: &str, _: &str, _: &str) -> bool {
            panic!("MockNativeContext: is_package_exported_to not implemented for testing")
        }
        fn is_package_open_unqualified(&self, _: &str, _: &str) -> bool {
            panic!("MockNativeContext: is_package_open_unqualified not implemented for testing")
        }
        fn is_package_open_to(&self, _: &str, _: &str, _: &str) -> bool {
            panic!("MockNativeContext: is_package_open_to not implemented for testing")
        }
        fn check_deep_reflection_access(
            &self,
            _: cratonvm_types::ClassId,
            _: cratonvm_types::ClassId,
        ) -> Result<(), String> {
            panic!("MockNativeContext: check_deep_reflection_access not implemented for testing")
        }
        fn load_class(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: load_class not implemented for testing")
        }
        fn class_name_of_id(&self, _: cratonvm_types::ClassId) -> Option<String> {
            panic!("MockNativeContext: class_name_of_id not implemented for testing")
        }
        fn class_id_of_object(&self, _: cratonvm_types::ObjectRef) -> cratonvm_types::ClassId {
            panic!("MockNativeContext: class_id_of_object not implemented for testing")
        }
        fn method_exists(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        fn ensure_class_initialized(
            &mut self,
            _: &str,
        ) -> Result<cratonvm_types::ClassId, cratonvm_types::error::MethodCallFailed> {
            panic!("MockNativeContext: ensure_class_initialized not implemented for testing")
        }
        fn is_subclass(&self, _: cratonvm_types::ClassId, _: cratonvm_types::ClassId) -> bool {
            panic!("MockNativeContext: is_subclass not implemented for testing")
        }
        fn superclass_of(&self, _: cratonvm_types::ClassId) -> Option<cratonvm_types::ClassId> {
            panic!("MockNativeContext: superclass_of not implemented for testing")
        }
        fn class_id_by_name(&self, _: &str) -> Option<cratonvm_types::ClassId> {
            panic!("MockNativeContext: class_id_by_name not implemented for testing")
        }
        fn loader_id_of_class(&self, _: cratonvm_types::ClassId) -> i32 {
            2
        }
        fn is_record_class(&self, _: cratonvm_types::ClassId) -> bool {
            panic!("MockNativeContext: is_record_class not implemented for testing")
        }
        fn record_components(&self, _: cratonvm_types::ClassId) -> Vec<(String, String)> {
            panic!("MockNativeContext: record_components not implemented for testing")
        }
        fn is_sealed_class(&self, _: cratonvm_types::ClassId) -> bool {
            panic!("MockNativeContext: is_sealed_class not implemented for testing")
        }
        fn permitted_subclasses(&self, _: cratonvm_types::ClassId) -> Vec<String> {
            panic!("MockNativeContext: permitted_subclasses not implemented for testing")
        }
        fn declared_fields(
            &self,
            _: cratonvm_types::ClassId,
        ) -> Vec<cratonvm_native_api::FieldMetadata> {
            panic!("MockNativeContext: declared_fields not implemented for testing")
        }
        fn declared_methods(
            &self,
            _: cratonvm_types::ClassId,
        ) -> Vec<cratonvm_native_api::MethodMetadata> {
            panic!("MockNativeContext: declared_methods not implemented for testing")
        }
        fn class_interfaces(&self, _: cratonvm_types::ClassId) -> Vec<cratonvm_types::ClassId> {
            panic!("MockNativeContext: class_interfaces not implemented for testing")
        }
        fn class_access_flags(&self, _: cratonvm_types::ClassId) -> u16 {
            panic!("MockNativeContext: class_access_flags not implemented for testing")
        }
        fn primitive_class_mirror(&mut self, _: &str) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: primitive_class_mirror not implemented for testing")
        }
        fn class_annotations(
            &self,
            _: cratonvm_types::ClassId,
        ) -> Vec<cratonvm_native_api::AnnotationData> {
            panic!("MockNativeContext: class_annotations not implemented for testing")
        }
        fn method_annotations(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
            _: &str,
        ) -> Vec<cratonvm_native_api::AnnotationData> {
            panic!("MockNativeContext: method_annotations not implemented for testing")
        }
        fn field_annotations(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
        ) -> Vec<cratonvm_native_api::AnnotationData> {
            panic!("MockNativeContext: field_annotations not implemented for testing")
        }
        fn module_name_of_class(&self, _: cratonvm_types::ClassId) -> Option<String> {
            panic!("MockNativeContext: module_name_of_class not implemented for testing")
        }
        fn find_resource(&self, _: &str) -> Option<Vec<u8>> {
            panic!("MockNativeContext: find_resource not implemented for testing")
        }
        fn list_application_class_names(&self) -> Vec<String> {
            panic!("MockNativeContext: list_application_class_names not implemented for testing")
        }
        fn register_dynamic_classpath(&mut self, _: &[String]) {
            panic!("MockNativeContext: register_dynamic_classpath not implemented for testing")
        }
        fn define_class_from_bytes(
            &mut self,
            _: &str,
            _: &[u8],
        ) -> Option<cratonvm_types::ClassId> {
            panic!("MockNativeContext: define_class_from_bytes not implemented for testing")
        }
        fn define_class_with_loader(
            &mut self,
            _: &str,
            _: &[u8],
            _: u32,
        ) -> Option<cratonvm_types::ClassId> {
            panic!("MockNativeContext: define_class_with_loader not implemented for testing")
        }
        fn class_id_by_name_and_loader(&self, _: &str, _: u32) -> Option<cratonvm_types::ClassId> {
            panic!("MockNativeContext: class_id_by_name_and_loader not implemented for testing")
        }
        fn allocate_loader_id(&mut self) -> u32 {
            panic!("MockNativeContext: allocate_loader_id not implemented for testing")
        }
        fn method_parameter_annotations(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
            _: &str,
        ) -> Vec<Vec<cratonvm_native_api::AnnotationData>> {
            panic!("PanicContext::method_parameter_annotations not implemented for testing")
        }
        fn method_annotation_default(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
            _: &str,
        ) -> Option<cratonvm_native_api::AnnotationElementValue> {
            panic!("PanicContext::method_annotation_default not implemented for testing")
        }
        fn class_signature(&self, _: cratonvm_types::ClassId) -> Option<String> {
            None
        }
        fn method_signature(&self, _: cratonvm_types::ClassId, _: &str, _: &str) -> Option<String> {
            None
        }
        fn field_signature(&self, _: cratonvm_types::ClassId, _: &str) -> Option<String> {
            None
        }
    }

    impl cratonvm_native_api::NativeInvokeAccess for PanicContext {
        fn invoke(
            &mut self,
            _: &str,
            _: &str,
            _: &str,
            _: &[Value],
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: invoke not implemented for testing")
        }
        fn invoke_virtual(
            &mut self,
            _: cratonvm_types::ObjectRef,
            _: &str,
            _: &str,
            _: &[Value],
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: invoke_virtual not implemented for testing")
        }
    }

    impl cratonvm_native_api::NativeHeapAccess for PanicContext {
        fn resolve_field_index_by_class_id(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
        ) -> Option<usize> {
            panic!("MockNativeContext: resolve_field_index_by_class_id not implemented for testing")
        }
        fn new_object(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: new_object not implemented for testing")
        }
        fn identity_hash_code(&self, _: cratonvm_types::ObjectRef) -> i32 {
            panic!("MockNativeContext: identity_hash_code not implemented for testing")
        }
        fn get_field(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
            panic!("MockNativeContext: get_field not implemented for testing")
        }
        fn set_field(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
            panic!("MockNativeContext: set_field not implemented for testing")
        }
        fn get_field_by_name(&self, _: cratonvm_types::ObjectRef, _: &str) -> Value {
            panic!("MockNativeContext: get_field_by_name not implemented for testing")
        }
        fn set_field_by_name(&self, _: cratonvm_types::ObjectRef, _: &str, _: Value) {
            panic!("MockNativeContext: set_field_by_name not implemented for testing")
        }
        fn resolve_field_index(&self, _: &str, _: &str) -> Option<usize> {
            panic!("MockNativeContext: resolve_field_index not implemented for testing")
        }
        fn new_array(
            &mut self,
            _: cratonvm_types::ArrayElementType,
            _: usize,
        ) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: new_array not implemented for testing")
        }
        fn new_ref_array(
            &mut self,
            _: cratonvm_types::ClassId,
            _: usize,
        ) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: new_ref_array not implemented for testing")
        }
        fn array_length(&self, _: cratonvm_types::ObjectRef) -> usize {
            panic!("MockNativeContext: array_length not implemented for testing")
        }
        fn get_array_element(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
            panic!("MockNativeContext: get_array_element not implemented for testing")
        }
        fn set_array_element(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
            panic!("MockNativeContext: set_array_element not implemented for testing")
        }
        fn heap_kind_of(&self, _: cratonvm_types::ObjectRef) -> cratonvm_types::ObjectKind {
            panic!("MockNativeContext: heap_kind_of not implemented for testing")
        }
        fn heap_element_type_of(
            &self,
            _: cratonvm_types::ObjectRef,
        ) -> cratonvm_types::ArrayElementType {
            panic!("MockNativeContext: heap_element_type_of not implemented for testing")
        }
        fn create_string(&mut self, _: &str) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: create_string not implemented for testing")
        }
        fn read_string(&self, _: cratonvm_types::ObjectRef) -> Option<String> {
            panic!("MockNativeContext: read_string not implemented for testing")
        }
        fn get_class_mirror(&mut self, _: cratonvm_types::ClassId) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: get_class_mirror not implemented for testing")
        }
        fn alloc_object(
            &mut self,
            _: cratonvm_types::ClassId,
            _: usize,
        ) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: alloc_object not implemented for testing")
        }
        fn object_num_fields(&self, _: cratonvm_types::ObjectRef) -> usize {
            panic!("MockNativeContext: object_num_fields not implemented for testing")
        }
        fn get_field_volatile(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
            panic!("MockNativeContext: get_field_volatile not implemented for testing")
        }
        fn set_field_volatile(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
            panic!("MockNativeContext: set_field_volatile not implemented for testing")
        }
        fn compare_and_swap_field(
            &mut self,
            _: cratonvm_types::ObjectRef,
            _: usize,
            _: Value,
            _: Value,
        ) -> bool {
            panic!("MockNativeContext: compare_and_swap_field not implemented for testing")
        }
        fn allocate_instance(&mut self, _: &str) -> Option<cratonvm_types::ObjectRef> {
            panic!("MockNativeContext: allocate_instance not implemented for testing")
        }
        fn discover_reference(
            &mut self,
            _: u8,
            _: cratonvm_types::ObjectRef,
            _: cratonvm_types::ObjectRef,
            _: Option<cratonvm_types::ObjectRef>,
        ) {
            panic!("MockNativeContext: discover_reference not implemented for testing")
        }
        fn heap_allocated_bytes(&self) -> usize {
            panic!("PanicContext::heap_allocated_bytes not implemented for testing")
        }
    }

    impl cratonvm_native_api::NativeThreadAccess for PanicContext {
        fn thread_id(&self) -> u64 {
            panic!("MockNativeContext: thread_id not implemented for testing")
        }
        fn monitor_wait(
            &mut self,
            _: cratonvm_types::ObjectRef,
            _: Option<u64>,
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: monitor_wait not implemented for testing")
        }
        fn monitor_notify(
            &mut self,
            _: cratonvm_types::ObjectRef,
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: monitor_notify not implemented for testing")
        }
        fn monitor_notify_all(
            &mut self,
            _: cratonvm_types::ObjectRef,
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: monitor_notify_all not implemented for testing")
        }
        fn thread_start(
            &mut self,
            _: cratonvm_types::ObjectRef,
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: thread_start not implemented for testing")
        }
        fn thread_join(
            &mut self,
            _: cratonvm_types::ObjectRef,
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: thread_join not implemented for testing")
        }
        fn thread_is_alive(&self, _: cratonvm_types::ObjectRef) -> bool {
            panic!("MockNativeContext: thread_is_alive not implemented for testing")
        }
        fn current_thread_object(&mut self) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: current_thread_object not implemented for testing")
        }
        fn thread_interrupt(&mut self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: thread_interrupt not implemented for testing")
        }
        fn is_interrupted(&self, _: bool) -> bool {
            panic!("MockNativeContext: is_interrupted not implemented for testing")
        }
        fn park(&mut self, _: Option<std::time::Duration>) {
            panic!("MockNativeContext: park not implemented for testing")
        }
        fn unpark(&self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: unpark not implemented for testing")
        }
        fn get_scoped_value(&self, _: u64) -> Option<Value> {
            panic!("MockNativeContext: get_scoped_value not implemented for testing")
        }
        fn push_scoped_value(&mut self, _: u64, _: Value) {
            panic!("MockNativeContext: push_scoped_value not implemented for testing")
        }
        fn pop_scoped_value(&mut self) {
            panic!("MockNativeContext: pop_scoped_value not implemented for testing")
        }
        fn scoped_value_depth(&self) -> usize {
            panic!("MockNativeContext: scoped_value_depth not implemented for testing")
        }
        fn monitor_enter(&mut self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: monitor_enter not implemented for testing")
        }
        fn monitor_exit(&mut self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: monitor_exit not implemented for testing")
        }
        fn active_thread_count(&self) -> i32 {
            panic!("PanicContext::active_thread_count not implemented for testing")
        }
        fn enumerate_threads(&self, _: usize) -> Vec<cratonvm_types::ObjectRef> {
            panic!("PanicContext::enumerate_threads not implemented for testing")
        }
    }

    impl cratonvm_native_api::NativeExceptionAccess for PanicContext {
        fn capture_stack_trace(&mut self, _: i32) -> Vec<cratonvm_native_api::StackTraceEntry> {
            panic!("MockNativeContext: capture_stack_trace not implemented for testing")
        }
        fn get_stack_trace(&self, _: i32) -> Option<Vec<cratonvm_native_api::StackTraceEntry>> {
            panic!("MockNativeContext: get_stack_trace not implemented for testing")
        }
    }

    impl cratonvm_native_api::NativeGpuAccess for PanicContext {}

    impl cratonvm_native_api::NativeSystemAccess for PanicContext {
        fn record_printed_value(&mut self, _: Value) {
            panic!("MockNativeContext: record_printed_value not implemented for testing")
        }
        fn record_printed_line(&mut self, _: String) {
            panic!("MockNativeContext: record_printed_line not implemented for testing")
        }
        fn get_system_stream(&self, _: &str) -> Option<cratonvm_types::ObjectRef> {
            panic!("MockNativeContext: get_system_stream not implemented for testing")
        }
        fn get_system_property(&self, _: &str) -> Option<String> {
            None
        }
        fn set_system_property(&mut self, _: &str, _: &str) -> Option<String> {
            panic!("MockNativeContext: set_system_property not implemented for testing")
        }
        fn is_interface_class(&self, _: cratonvm_types::ClassId) -> bool {
            panic!("MockNativeContext: is_interface_class not implemented for testing")
        }
        fn get_static_field(&self, _: cratonvm_types::ClassId, _: usize) -> Value {
            panic!("MockNativeContext: get_static_field not implemented for testing")
        }
        fn set_static_field(&mut self, _: cratonvm_types::ClassId, _: usize, _: Value) {
            panic!("MockNativeContext: set_static_field not implemented for testing")
        }
        fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
            panic!("MockNativeContext: fd_table not implemented for testing")
        }
        fn allocate_native_memory(&mut self, _: usize, _: usize) -> Option<(i64, *mut u8)> {
            panic!("MockNativeContext: allocate_native_memory not implemented for testing")
        }
        fn free_native_memory(&mut self, _: i64) {
            panic!("MockNativeContext: free_native_memory not implemented for testing")
        }
        fn load_native_library(
            &mut self,
            _: &str,
        ) -> Result<i64, cratonvm_types::error::MethodCallFailed> {
            panic!("MockNativeContext: load_native_library not implemented for testing")
        }
        fn find_native_symbol(&self, _: i64, _: &str) -> Option<usize> {
            panic!("MockNativeContext: find_native_symbol not implemented for testing")
        }
        fn register_upcall(&mut self, _: cratonvm_native_api::ffi::UpcallEntry) -> usize {
            panic!("MockNativeContext: register_upcall not implemented for testing")
        }
        fn get_upcall_info(&self, _: usize) -> Option<(cratonvm_types::ObjectRef, Vec<i32>, i32)> {
            panic!("MockNativeContext: get_upcall_info not implemented for testing")
        }
        fn loaded_class_count(&self) -> usize {
            panic!("PanicContext::loaded_class_count not implemented for testing")
        }
        fn gc_collection_count(&self) -> u64 {
            panic!("PanicContext::gc_collection_count not implemented for testing")
        }
        fn force_gc(&mut self) {}
    }

    // F17-1 (2026-08-13): `test_is_dumping_class_list_returns_zero` was deleted
    // with the native it exercised. It is worth naming what that test was
    // actually measuring: `isDumpingClassList` is not a member of JDK 25's
    // `jdk.internal.misc.CDS` at any access level, so a green assertion here
    // reported only that a Rust function returned the constant it was written to
    // return — the invented Java method it was reachable through never appeared
    // in the test at all. A unit test on a native's BODY cannot see that the
    // native's TRIPLE is fabricated; only the registration list can.

    #[test]
    fn test_is_dumping_archive_returns_zero() {
        let result = native_cds_is_dumping_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    /// F17-1: was `test_is_sharing_enabled_returns_zero`. Same body, JDK-true
    /// name — `isUsingArchive` is what JDK 25 calls this predicate.
    #[test]
    fn test_is_using_archive_returns_zero() {
        let result = native_cds_is_using_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    /// F17-1: `getCDSConfigStatus()I` must answer 0 — every bit in the word
    /// (`IS_DUMPING_ARCHIVE`, `IS_DUMPING_METHOD_HANDLES`,
    /// `IS_DUMPING_STATIC_ARCHIVE`, `IS_LOGGING_LAMBDA_FORM_INVOKERS`,
    /// `IS_USING_ARCHIVE`) describes something CratonVM does not do.
    ///
    /// This one is not cosmetic: JDK 25's `CDS.<clinit>` is
    /// `configStatus = getCDSConfigStatus()`, so any non-zero answer here would
    /// silently switch on a code path — e.g. `isLoggingLambdaFormInvokers()`
    /// going true routes `logSpeciesType` and the 4-arg `logLambdaFormInvoker`
    /// into a native that discards its input.
    #[test]
    fn f17_1_cds_config_status_is_zero() {
        let result = native_cds_get_config_status(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    /// F17-1: `needsClassInitBarrier0(Class)Z` must answer false — the barrier
    /// orders initialisation of classes reached through an archived heap
    /// subgraph, and CratonVM maps none.
    #[test]
    fn f17_1_needs_class_init_barrier_is_false() {
        let result = native_cds_needs_class_init_barrier0(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_initialize_from_archive_returns_void() {
        // The descriptor is `(Ljava/lang/Class;)V`. Handing back an operand for
        // a void method leaves it on the stack — the caller emits no pop.
        let result = native_cds_initialize_from_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_random_seed_for_dumping_is_zero_outside_dump() {
        // Must be 0 when not dumping a static archive, which CratonVM never
        // does. `ImmutableCollections` seeds SALT from this value and falls
        // back to `System.nanoTime()` only on 0; a fixed non-zero seed made
        // `Set.of` / `Map.of` iteration order identical on every run, hiding
        // exactly the order-dependence bugs the randomisation exists to expose.
        let result = native_cds_get_random_seed_for_dumping(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Long(0)));
    }

    // F17-1 (2026-08-13): `test_vm_is_booted_returns_one` was deleted with
    // `native_vm_is_booted`. `sun.misc.VM` is not in the JDK 25 image
    // (`javap -p sun.misc.VM` → class not found). The equivalent that DOES exist,
    // `jdk/internal/misc/VM.isBooted()Z`, is registered and tested by `lib.rs`
    // (registration at lib.rs:14894) — a second copy here would have been a
    // last-write-wins coin flip decided by registrar call order.

    #[test]
    fn test_log_lambda_form_invoker_is_noop() {
        let result = native_cds_log_lambda_form_invoker(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_define_archived_modules_is_noop() {
        let result = native_cds_define_archived_modules(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_dump_class_list_is_noop() {
        let result = native_cds_dump_class_list(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_dump_dynamic_archive_is_noop() {
        let result = native_cds_dump_dynamic_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_checksum_is_sha256_based() {
        let mut gen1 = CdsArchiveGenerator::new("/tmp/test1.jsa");
        gen1.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE],
        ));
        let h1 = gen1.build_header();

        let mut gen2 = CdsArchiveGenerator::new("/tmp/test2.jsa");
        gen2.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE],
        ));
        let h2 = gen2.build_header();

        // Same entries should produce same checksum
        assert_eq!(h1.checksum, h2.checksum);
        assert_ne!(h1.checksum, 0);
    }

    #[test]
    fn test_checksum_changes_with_different_data() {
        let mut gen1 = CdsArchiveGenerator::new("/tmp/test1.jsa");
        gen1.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE],
        ));
        let h1 = gen1.build_header();

        let mut gen2 = CdsArchiveGenerator::new("/tmp/test2.jsa");
        gen2.add_entry(CdsArchiveEntry::new(
            "java/lang/String",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE],
        ));
        let h2 = gen2.build_header();

        assert_ne!(h1.checksum, h2.checksum);
    }
}
