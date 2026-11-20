// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// ```text
/// $ javap -p jdk.internal.misc.CDS | grep getCDSConfigStatus
///   private static native int getCDSConfigStatus();
/// ```
///
/// It is called from `CDS.<clinit>` — `private static final int configStatus =
/// getCDSConfigStatus();` (jdk25src java.base/jdk/internal/misc/CDS.java:55) —
/// which means every one of the class's five public predicates
/// (`isLoggingLambdaFormInvokers`, `isDumpingArchive`, `isUsingArchive`,
/// `isDumpingStaticArchive`, `isSingleThreadVM`) reads a field that only this
/// native can fill, and merely *initialising* `CDS` requires it.
///
/// WHY IT WAS INVISIBLE. `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`
/// did not list it until F23-1 (2026-08-13). That was not staleness — the
/// generator's oracle (`JdkBaseline.java:221`, format version 1) emitted a
/// member only if ACC_PUBLIC or ACC_PROTECTED was set, so a baseline
/// structurally could not see a `private static native`, which is the exact
/// access level most JDK natives live at. It lists this method now, as
/// `private,static,native`. Auditing a native registrar against a public-only
/// surface therefore
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
/// ```text
/// $ javap -p jdk.internal.misc.CDS | grep needsClassInitBarrier
///   public static boolean needsClassInitBarrier(java.lang.Class<?>);
///   private static native boolean needsClassInitBarrier0(java.lang.Class<?>);
/// ```
///
/// Note the pair: the *public* half is on the baseline and the native half is
/// not, so a name-keyed audit sees `needsClassInitBarrier` as covered while the
/// method that actually needs a body is the one ending in `0`. Registering the
/// public half instead would have been wrong twice over — it has real bytecode,
/// so under `--real-jdk` §7 step 3 answers `Bytecode` for it and the native
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
pub fn register_cds_natives(r: &mut NativeMethodRegistry) {
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
        r.register(cls, "<init>", "()V", native_noop_with_this, cratonvm_native_api::NativeKind::Bridge);
    }

    // -- jdk/internal/misc/CDS --
    {
        let cls = "jdk/internal/misc/CDS";
        // Pure static-holder: every `jdk.internal.misc.CDS` member — and every
        // native registered on it below — is static, and the class carries no
        // instance fields for a constructor to initialize. KEEP.
        r.register(cls, "<init>", "()V", native_noop_with_this, cratonvm_native_api::NativeKind::Bridge);
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
            native_cds_is_dumping_archive, cratonvm_native_api::NativeKind::Bridge,
        );
        // F17-1: was `isSharingEnabled`, a name JDK 25 does not declare.
        // `isUsingArchive()Z` is the JDK-true spelling of the same predicate.
        r.register(cls, "isUsingArchive", "()Z", native_cds_is_using_archive, cratonvm_native_api::NativeKind::Bridge);
        // F17-1: both ADDED. Real `private static native` members of JDK 25's
        // `CDS` that no registrar covered, and that a public-only baseline
        // cannot report as missing. `getCDSConfigStatus` is the one `<clinit>`
        // calls. See their doc comments.
        r.register(
            cls,
            "getCDSConfigStatus",
            "()I",
            native_cds_get_config_status, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "needsClassInitBarrier0",
            "(Ljava/lang/Class;)Z",
            native_cds_needs_class_init_barrier0, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "initializeFromArchive",
            "(Ljava/lang/Class;)V",
            native_cds_initialize_from_archive, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getRandomSeedForDumping",
            "()J",
            native_cds_get_random_seed_for_dumping, cratonvm_native_api::NativeKind::Bridge,
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
            native_cds_log_lambda_form_invoker, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "defineArchivedModules",
            "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
            native_cds_define_archived_modules, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "dumpClassList",
            "(Ljava/lang/String;)V",
            native_cds_dump_class_list, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "dumpDynamicArchive",
            "(Ljava/lang/String;)V",
            native_cds_dump_dynamic_archive, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cds_tests;
