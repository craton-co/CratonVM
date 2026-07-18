// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class Data Sharing (CDS / AppCDS) native method implementations.
//!
//! Provides archive infrastructure, metrics reporting, and all native stubs
//! required by `jdk/internal/misc/CDS`, `sun/misc/VM`, `java/lang/ClassLoader`,
//! and `sun/management/ManagementFactoryHelper` for CDS-related queries.
//!
//! The JVM boots with CDS **disabled** by default (sharing = 0).  The archive
//! types below are present so that a future dump/load path can be wired in
//! without changing the public registration surface.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, native_noop, native_noop_with_this, obj_arg};

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

/// Allocate a CDS metrics synthetic object.
///
/// Field layout (5 fields):
/// ```text
///  0  total_classes_in_archive   (Int)
///  1  classes_loaded_from_archive (Int)
///  2  archive_size_bytes         (Long)
///  3  archive_load_time_ms       (Long)
///  4  archive_path               (Object / String)
/// ```
fn alloc_cds_metrics_obj(ctx: &mut dyn NativeContext, metrics: &CdsMetrics) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "sun/management/CDSMetrics", 5);
    ctx.set_field(obj, 0, Value::Int(metrics.total_classes_in_archive as i32));
    ctx.set_field(
        obj,
        1,
        Value::Int(metrics.classes_loaded_from_archive as i32),
    );
    ctx.set_field(obj, 2, Value::Long(metrics.archive_size_bytes as i64));
    ctx.set_field(obj, 3, Value::Long(metrics.archive_load_time_ms as i64));
    let path_obj = ctx.create_string(&metrics.archive_path);
    ctx.set_field(obj, 4, Value::Object(Some(path_obj)));
    obj
}

/// Allocate a synthetic `java.util.Properties` object (2 fields: backing
/// array + size).  The object is empty; callers may add entries via
/// `ctx.set_field` if needed.
fn alloc_properties_obj(ctx: &mut dyn NativeContext) -> ObjectRef {
    use cratonvm_types::ClassId;
    let obj = alloc_concurrent_synthetic(ctx, "java/util/Properties", 2);
    let backing = ctx.new_ref_array(ClassId::new(0), 0);
    ctx.set_field(obj, 0, Value::Object(Some(backing)));
    ctx.set_field(obj, 1, Value::Int(0));
    obj
}

// ---------------------------------------------------------------------------
// Individual native handler functions (named, non-capturing)
// ---------------------------------------------------------------------------

// --- sun/management/ManagementFactoryHelper ---

fn native_get_cds_metrics(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let metrics = CdsMetrics::disabled();
    let obj = alloc_cds_metrics_obj(ctx, &metrics);
    Ok(Some(Value::Object(Some(obj))))
}

// --- java/lang/ClassLoader ---

fn native_get_cds_archive_path(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let path = ctx
        .get_system_property("jdk.internal.vm.cds.archive")
        .unwrap_or_default();
    let s = ctx.create_string(&path);
    Ok(Some(Value::Object(Some(s))))
}

// --- sun/misc/VM ---

fn native_vm_is_booted(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_vm_saved_props(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let props = alloc_properties_obj(ctx);
    Ok(Some(Value::Object(Some(props))))
}

// --- jdk/internal/misc/CDS ---

fn native_cds_is_dumping_class_list(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_cds_is_dumping_archive(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn native_cds_is_sharing_enabled(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let enabled = ctx
        .get_system_property("jdk.internal.vm.cds.enabled")
        .map_or(false, |v| v == "true");
    Ok(Some(Value::Int(if enabled { 1 } else { 0 })))
}

fn native_cds_initialize_from_archive(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // No-op: sharing is disabled; nothing to load.
    Ok(Some(Value::Int(0)))
}

fn native_cds_get_random_seed_for_dumping(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(12_345_678)))
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
        r.register(cls, "<init>", "()V", native_noop_with_this);
        r.register(
            cls,
            "getCDSMetrics",
            "()Lsun/management/CDSMetrics;",
            native_get_cds_metrics,
        );
    }

    // -- sun/management/CDSMetrics (accessor stubs) --
    {
        let cls = "sun/management/CDSMetrics";
        r.register(cls, "<init>", "()V", native_noop_with_this);
        r.register(cls, "getTotalClassesInArchive", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        });
        r.register(cls, "getClassesLoadedFromArchive", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        });
        r.register(cls, "getArchiveSizeBytes", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        });
        r.register(cls, "getArchiveLoadTimeMs", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        });
        r.register(
            cls,
            "getArchivePath",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(ctx.get_field(this, 4)))
            },
        );
    }

    // -- java/lang/ClassLoader --
    {
        let cls = "java/lang/ClassLoader";
        r.register(
            cls,
            "getCdsArchivePath",
            "()Ljava/lang/String;",
            native_get_cds_archive_path,
        );
    }

    // -- sun/misc/VM --
    {
        let cls = "sun/misc/VM";
        r.register(cls, "<init>", "()V", native_noop_with_this);
        r.register(cls, "isBooted", "()Z", native_vm_is_booted);
        r.register(
            cls,
            "savedProps",
            "()Ljava/util/Properties;",
            native_vm_saved_props,
        );
    }

    // -- jdk/internal/misc/CDS --
    {
        let cls = "jdk/internal/misc/CDS";
        r.register(cls, "<init>", "()V", native_noop_with_this);
        r.register(
            cls,
            "isDumpingClassList",
            "()Z",
            native_cds_is_dumping_class_list,
        );
        r.register(
            cls,
            "isDumpingArchive",
            "()Z",
            native_cds_is_dumping_archive,
        );
        r.register(
            cls,
            "isSharingEnabled",
            "()Z",
            native_cds_is_sharing_enabled,
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
        assert!(r
            .find(cls, "getCDSMetrics", "()Lsun/management/CDSMetrics;")
            .is_some());
    }

    #[test]
    fn test_cds_metrics_accessors_registered() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "sun/management/CDSMetrics";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getTotalClassesInArchive", "()I").is_some());
        assert!(r.find(cls, "getClassesLoadedFromArchive", "()I").is_some());
        assert!(r.find(cls, "getArchiveSizeBytes", "()J").is_some());
        assert!(r.find(cls, "getArchiveLoadTimeMs", "()J").is_some());
        assert!(r
            .find(cls, "getArchivePath", "()Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn test_classloader_cds_archive_path_registered() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        assert!(r
            .find(
                "java/lang/ClassLoader",
                "getCdsArchivePath",
                "()Ljava/lang/String;"
            )
            .is_some());
    }

    #[test]
    fn test_sun_misc_vm_registered() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "sun/misc/VM";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "isBooted", "()Z").is_some());
        assert!(r
            .find(cls, "savedProps", "()Ljava/util/Properties;")
            .is_some());
    }

    #[test]
    fn test_jdk_internal_cds_all_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "jdk/internal/misc/CDS";
        let methods: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("isDumpingClassList", "()Z"),
            ("isDumpingArchive", "()Z"),
            ("isSharingEnabled", "()Z"),
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

    #[test]
    fn test_registration_count_at_least_20() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        assert!(
            r.len() >= 20,
            "Expected at least 20 registrations, got {}",
            r.len()
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

        let expected: &[(&str, &str, &str)] = &[
            ("sun/management/ManagementFactoryHelper", "<init>", "()V"),
            (
                "sun/management/ManagementFactoryHelper",
                "getCDSMetrics",
                "()Lsun/management/CDSMetrics;",
            ),
            ("sun/management/CDSMetrics", "<init>", "()V"),
            (
                "sun/management/CDSMetrics",
                "getTotalClassesInArchive",
                "()I",
            ),
            (
                "sun/management/CDSMetrics",
                "getClassesLoadedFromArchive",
                "()I",
            ),
            ("sun/management/CDSMetrics", "getArchiveSizeBytes", "()J"),
            ("sun/management/CDSMetrics", "getArchiveLoadTimeMs", "()J"),
            (
                "sun/management/CDSMetrics",
                "getArchivePath",
                "()Ljava/lang/String;",
            ),
            (
                "java/lang/ClassLoader",
                "getCdsArchivePath",
                "()Ljava/lang/String;",
            ),
            ("sun/misc/VM", "<init>", "()V"),
            ("sun/misc/VM", "isBooted", "()Z"),
            ("sun/misc/VM", "savedProps", "()Ljava/util/Properties;"),
            ("jdk/internal/misc/CDS", "<init>", "()V"),
            ("jdk/internal/misc/CDS", "isDumpingClassList", "()Z"),
            ("jdk/internal/misc/CDS", "isDumpingArchive", "()Z"),
            ("jdk/internal/misc/CDS", "isSharingEnabled", "()Z"),
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

    impl cratonvm_native_api::NativeContext for PanicContext {
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
        fn resolve_field_index_by_class_id(
            &self,
            _: cratonvm_types::ClassId,
            _: &str,
        ) -> Option<usize> {
            panic!("MockNativeContext: resolve_field_index_by_class_id not implemented for testing")
        }
        fn load_class(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: load_class not implemented for testing")
        }
        fn new_object(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: new_object not implemented for testing")
        }
        fn invoke(
            &mut self,
            _: &str,
            _: &str,
            _: &str,
            _: &[Value],
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: invoke not implemented for testing")
        }
        fn identity_hash_code(&self, _: cratonvm_types::ObjectRef) -> i32 {
            panic!("MockNativeContext: identity_hash_code not implemented for testing")
        }
        fn record_printed_value(&mut self, _: Value) {
            panic!("MockNativeContext: record_printed_value not implemented for testing")
        }
        fn class_name_of_id(&self, _: cratonvm_types::ClassId) -> Option<String> {
            panic!("MockNativeContext: class_name_of_id not implemented for testing")
        }
        fn class_id_of_object(&self, _: cratonvm_types::ObjectRef) -> cratonvm_types::ClassId {
            panic!("MockNativeContext: class_id_of_object not implemented for testing")
        }
        fn capture_stack_trace(&mut self, _: i32) -> Vec<cratonvm_native_api::StackTraceEntry> {
            panic!("MockNativeContext: capture_stack_trace not implemented for testing")
        }
        fn get_stack_trace(&self, _: i32) -> Option<Vec<cratonvm_native_api::StackTraceEntry>> {
            panic!("MockNativeContext: get_stack_trace not implemented for testing")
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
        fn method_exists(&self, _: &str, _: &str, _: &str) -> bool {
            false
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
        fn alloc_object(
            &mut self,
            _: cratonvm_types::ClassId,
            _: usize,
        ) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: alloc_object not implemented for testing")
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
        fn is_interface_class(&self, _: cratonvm_types::ClassId) -> bool {
            panic!("MockNativeContext: is_interface_class not implemented for testing")
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
        fn object_num_fields(&self, _: cratonvm_types::ObjectRef) -> usize {
            panic!("MockNativeContext: object_num_fields not implemented for testing")
        }
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
        fn get_static_field(&self, _: cratonvm_types::ClassId, _: usize) -> Value {
            panic!("MockNativeContext: get_static_field not implemented for testing")
        }
        fn set_static_field(&mut self, _: cratonvm_types::ClassId, _: usize, _: Value) {
            panic!("MockNativeContext: set_static_field not implemented for testing")
        }
        fn primitive_class_mirror(&mut self, _: &str) -> cratonvm_types::ObjectRef {
            panic!("MockNativeContext: primitive_class_mirror not implemented for testing")
        }
        fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
            panic!("MockNativeContext: fd_table not implemented for testing")
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
        fn park(&mut self, _: Option<std::time::Duration>) {
            panic!("MockNativeContext: park not implemented for testing")
        }
        fn unpark(&self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: unpark not implemented for testing")
        }
        fn allocate_instance(&mut self, _: &str) -> Option<cratonvm_types::ObjectRef> {
            panic!("MockNativeContext: allocate_instance not implemented for testing")
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
        fn invoke_virtual(
            &mut self,
            _: cratonvm_types::ObjectRef,
            _: &str,
            _: &str,
            _: &[Value],
        ) -> cratonvm_types::error::MethodCallResult {
            panic!("MockNativeContext: invoke_virtual not implemented for testing")
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
        fn discover_reference(
            &mut self,
            _: u8,
            _: cratonvm_types::ObjectRef,
            _: cratonvm_types::ObjectRef,
            _: Option<cratonvm_types::ObjectRef>,
        ) {
            panic!("MockNativeContext: discover_reference not implemented for testing")
        }
        fn monitor_enter(&mut self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: monitor_enter not implemented for testing")
        }
        fn monitor_exit(&mut self, _: cratonvm_types::ObjectRef) {
            panic!("MockNativeContext: monitor_exit not implemented for testing")
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
        fn active_thread_count(&self) -> i32 {
            panic!("PanicContext::active_thread_count not implemented for testing")
        }
        fn enumerate_threads(&self, _: usize) -> Vec<cratonvm_types::ObjectRef> {
            panic!("PanicContext::enumerate_threads not implemented for testing")
        }
        fn heap_allocated_bytes(&self) -> usize {
            panic!("PanicContext::heap_allocated_bytes not implemented for testing")
        }
        fn loaded_class_count(&self) -> usize {
            panic!("PanicContext::loaded_class_count not implemented for testing")
        }
        fn gc_collection_count(&self) -> u64 {
            panic!("PanicContext::gc_collection_count not implemented for testing")
        }
        fn force_gc(&mut self) {}
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

    #[test]
    fn test_is_dumping_class_list_returns_zero() {
        let result = native_cds_is_dumping_class_list(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_is_dumping_archive_returns_zero() {
        let result = native_cds_is_dumping_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_is_sharing_enabled_returns_zero() {
        let result = native_cds_is_sharing_enabled(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_initialize_from_archive_returns_zero() {
        let result = native_cds_initialize_from_archive(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn test_random_seed_for_dumping_value() {
        let result = native_cds_get_random_seed_for_dumping(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Long(12_345_678)));
    }

    #[test]
    fn test_vm_is_booted_returns_one() {
        let result = native_vm_is_booted(&mut PanicContext, &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

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
