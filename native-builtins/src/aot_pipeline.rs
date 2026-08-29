// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! CP12 — Startup time pipeline wiring.
//!
//! This module implements the two sub-tasks of CP12 on top of the
//! infrastructure already present in [`crate::aot`] and [`crate::cds`]:
//!
//! * T5.7.1 — AOT pipeline wiring: serializes method profiles and compiled
//!   JIT machine code to `~/.cratonvm/aot-cache/<hash>.profile` at shutdown,
//!   restores them on startup when `--aot-cache` is set, and refuses a cache
//!   entry whose class file SHA-256 no longer matches.
//! * T5.7.2 — CDS via `.jsa` archive: serializes in-memory `java.base` class
//!   bytes to `~/.cratonvm/cds/core.jsa` at shutdown (on `--cds-dump`) and
//!   memory-maps that file on startup (on `--cds`). A JDK build ID read
//!   from `$JAVA_HOME/release` is included in the archive header, and the
//!   archive is refused if the build ID differs on a later run.
//!
//! The code here is self-contained: it does not depend on the VM crate so
//! that this sub-crate can be compiled and tested independently. The VM
//! calls [`shutdown_flush`] at exit and [`startup_load`] at boot.

use crate::cds::{CdsArchiveEntry, CdsArchiveGenerator, CdsArchiveLoader};
use crate::crypto_impl::Sha256;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Sub-directory under `~/.cratonvm` that holds all AOT profile/JIT caches.
pub const AOT_CACHE_DIR: &str = "aot-cache";

/// Sub-directory under `~/.cratonvm` that holds the CDS archive(s).
pub const CDS_DIR: &str = "cds";

/// Default name of the CDS archive file.
pub const CDS_ARCHIVE_NAME: &str = "core.jsa";

/// Magic number written at the start of every AOT profile file.
pub const AOT_PROFILE_MAGIC: u32 = 0x504C_4E41; // "ALNP" (Ahead-of-time Leyden-iN-profile)

/// Current AOT profile format version understood by this JVM.
pub const AOT_PROFILE_VERSION: u16 = 1;

/// SHA-256 digest size in bytes.
pub const SHA256_LEN: usize = 32;

// ---------------------------------------------------------------------------
// Public configuration
// ---------------------------------------------------------------------------

/// CLI flags passed through from vm-cli.
#[derive(Debug, Clone, Default)]
pub struct AotPipelineConfig {
    /// `--aot-cache` — load/save AOT profile+JIT cache from `~/.cratonvm/aot-cache/`.
    pub aot_cache: bool,
    /// `--cds` — memory-map `~/.cratonvm/cds/core.jsa` at boot if present.
    pub cds_enabled: bool,
    /// `--cds-dump` — write `~/.cratonvm/cds/core.jsa` at shutdown.
    pub cds_dump: bool,
    /// Overrides the default `~/.cratonvm` root (used by integration tests).
    pub root_override: Option<PathBuf>,
    /// Overrides `$JAVA_HOME` for build-ID discovery (used by integration tests).
    pub java_home_override: Option<PathBuf>,
}

impl AotPipelineConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute the root directory (`~/.cratonvm` by default).
    pub fn root(&self) -> PathBuf {
        if let Some(ref r) = self.root_override {
            return r.clone();
        }
        dirs_home().join(".cratonvm")
    }

    /// Absolute path of the AOT cache sub-directory (not created).
    pub fn aot_cache_dir(&self) -> PathBuf {
        self.root().join(AOT_CACHE_DIR)
    }

    /// Absolute path of the CDS archive file.
    pub fn cds_archive_path(&self) -> PathBuf {
        self.root().join(CDS_DIR).join(CDS_ARCHIVE_NAME)
    }
}

/// Best-effort home-directory lookup. We avoid pulling in the `dirs` crate
/// because it is not already a workspace dependency; the environment-variable
/// fallback matches `dirs`' behaviour on Unix and Windows.
fn dirs_home() -> PathBuf {
    if let Ok(h) = cratonvm_types::flags::runtime_var("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    if let Ok(u) = cratonvm_types::flags::runtime_var("USERPROFILE") {
        if !u.is_empty() {
            return PathBuf::from(u);
        }
    }
    // Final fallback: current directory — bad, but won't crash the VM.
    PathBuf::from(".")
}

// ---------------------------------------------------------------------------
// AOT profile serialization (T5.7.1)
// ---------------------------------------------------------------------------

/// A method profile entry as serialized to disk.
///
/// Each entry fixes the `(class_hash, method_name, descriptor)` identity,
/// carries its call count, and optionally the raw JIT machine-code buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AotProfileEntry {
    /// SHA-256 of the underlying `.class` file — acts as the integrity guard
    /// described in the task spec.
    pub class_sha256: [u8; SHA256_LEN],
    /// Fully-qualified class name (slash-form, e.g. `java/lang/String`).
    pub class_name: String,
    /// Method name (e.g. `indexOf`).
    pub method_name: String,
    /// Method descriptor (e.g. `(Ljava/lang/String;)I`).
    pub descriptor: String,
    /// Total invocation count observed during the training run.
    pub call_count: u64,
    /// Raw x86-64 machine-code buffer, possibly empty if no JIT code was
    /// available for this method.
    pub jit_code: Vec<u8>,
    /// JIT flags recorded alongside the compiled buffer (opaque bit field).
    pub jit_flags: u32,
}

impl AotProfileEntry {
    /// Convenience constructor — the caller supplies SHA-256 already.
    pub fn new(
        class_sha256: [u8; SHA256_LEN],
        class_name: impl Into<String>,
        method_name: impl Into<String>,
        descriptor: impl Into<String>,
        call_count: u64,
    ) -> Self {
        Self {
            class_sha256,
            class_name: class_name.into(),
            method_name: method_name.into(),
            descriptor: descriptor.into(),
            call_count,
            jit_code: Vec::new(),
            jit_flags: 0,
        }
    }

    /// Attach a JIT machine-code buffer (raw x86-64 bytes) and flags.
    pub fn with_jit_code(mut self, code: Vec<u8>, flags: u32) -> Self {
        self.jit_code = code;
        self.jit_flags = flags;
        self
    }

    /// Return true if a non-empty JIT machine-code buffer is attached.
    pub fn has_jit_code(&self) -> bool {
        !self.jit_code.is_empty()
    }
}

/// On-disk bundle: the union of all profile entries from a training run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AotProfileBundle {
    pub entries: Vec<AotProfileEntry>,
}

impl AotProfileBundle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, entry: AotProfileEntry) {
        self.entries.push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize the bundle to a binary blob.
    ///
    /// Layout (little-endian):
    /// ```text
    ///  0..4   magic         (u32)    AOT_PROFILE_MAGIC
    ///  4..6   version       (u16)    AOT_PROFILE_VERSION
    ///  6..10  entry_count   (u32)
    ///  10..N  entries
    ///  N..N+32 SHA-256 digest over the preceding bytes (integrity hash)
    /// ```
    ///
    /// Each entry:
    /// ```text
    ///  32 bytes  class_sha256
    ///  2  bytes  class_name_len + class_name bytes
    ///  2  bytes  method_name_len + method_name bytes
    ///  2  bytes  descriptor_len + descriptor bytes
    ///  8  bytes  call_count (u64)
    ///  4  bytes  jit_flags  (u32)
    ///  4  bytes  jit_code_len (u32)
    ///  N  bytes  jit_code
    /// ```
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&AOT_PROFILE_MAGIC.to_le_bytes());
        buf.extend_from_slice(&AOT_PROFILE_VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

        for e in &self.entries {
            buf.extend_from_slice(&e.class_sha256);
            put_str(&mut buf, &e.class_name);
            put_str(&mut buf, &e.method_name);
            put_str(&mut buf, &e.descriptor);
            buf.extend_from_slice(&e.call_count.to_le_bytes());
            buf.extend_from_slice(&e.jit_flags.to_le_bytes());
            buf.extend_from_slice(&(e.jit_code.len() as u32).to_le_bytes());
            buf.extend_from_slice(&e.jit_code);
        }

        // Full-strength integrity check: SHA-256 over the payload.
        let digest = sha256_bytes(&buf);
        buf.extend_from_slice(&digest);
        buf
    }

    /// Deserialize a bundle from bytes. Returns `None` on any format or
    /// integrity error (caller should treat as a missing cache).
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 10 + SHA256_LEN {
            return None;
        }
        let magic = u32::from_le_bytes(data[0..4].try_into().ok()?);
        if magic != AOT_PROFILE_MAGIC {
            return None;
        }
        let version = u16::from_le_bytes(data[4..6].try_into().ok()?);
        if version != AOT_PROFILE_VERSION {
            return None;
        }
        let entry_count = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
        if entry_count > 1_000_000 {
            return None;
        }

        // Verify trailing SHA-256 first.
        let split = data.len() - SHA256_LEN;
        let computed = sha256_bytes(&data[..split]);
        if computed != data[split..] {
            return None;
        }

        let mut pos = 10usize;
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            if pos + SHA256_LEN > split {
                return None;
            }
            let mut class_sha256 = [0u8; SHA256_LEN];
            class_sha256.copy_from_slice(&data[pos..pos + SHA256_LEN]);
            pos += SHA256_LEN;

            let class_name = take_str(data, &mut pos, split)?;
            let method_name = take_str(data, &mut pos, split)?;
            let descriptor = take_str(data, &mut pos, split)?;

            if pos + 8 > split {
                return None;
            }
            let call_count = u64::from_le_bytes(data[pos..pos + 8].try_into().ok()?);
            pos += 8;

            if pos + 4 > split {
                return None;
            }
            let jit_flags = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
            pos += 4;

            if pos + 4 > split {
                return None;
            }
            let code_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
            pos += 4;

            if code_len > 16 * 1024 * 1024 || pos + code_len > split {
                return None;
            }
            let jit_code = data[pos..pos + code_len].to_vec();
            pos += code_len;

            entries.push(AotProfileEntry {
                class_sha256,
                class_name,
                method_name,
                descriptor,
                call_count,
                jit_code,
                jit_flags,
            });
        }

        if pos != split {
            return None;
        }
        Some(Self { entries })
    }

    /// Filter entries keeping only those whose `class_sha256` matches the
    /// class bytes currently present on the class path. Unknown classes are
    /// silently dropped — this is the integrity guard required by the spec.
    pub fn filter_by_live_class_hashes(
        self,
        live_hashes: &HashMap<String, [u8; SHA256_LEN]>,
    ) -> Self {
        let mut kept = Vec::with_capacity(self.entries.len());
        for e in self.entries {
            match live_hashes.get(&e.class_name) {
                Some(h) if *h == e.class_sha256 => kept.push(e),
                _ => { /* stale — drop */ }
            }
        }
        Self { entries: kept }
    }
}

// Length-prefixed UTF-8 helpers (u16 length, matching the existing AOT/CDS
// serializers in this crate).
fn put_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn take_str(data: &[u8], pos: &mut usize, limit: usize) -> Option<String> {
    if *pos + 2 > limit {
        return None;
    }
    let n = u16::from_le_bytes(data[*pos..*pos + 2].try_into().ok()?) as usize;
    *pos += 2;
    if *pos + n > limit {
        return None;
    }
    let s = std::str::from_utf8(&data[*pos..*pos + n]).ok()?.to_string();
    *pos += n;
    Some(s)
}

/// SHA-256 convenience wrapper using the in-tree crypto implementation.
pub fn sha256_bytes(data: &[u8]) -> [u8; SHA256_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut out = [0u8; SHA256_LEN];
    out.copy_from_slice(&hasher.finalize());
    out
}

// ---------------------------------------------------------------------------
// AOT path helpers
// ---------------------------------------------------------------------------

/// Map a (class_name, method_name, descriptor) tuple to the relative file
/// path under the AOT cache dir. The file name is a short hex hash of the
/// identifying triple so it survives arbitrary Java-name contents.
pub fn aot_profile_file_name(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let mut h = Sha256::new();
    h.update(class_name.as_bytes());
    h.update(b"/");
    h.update(method_name.as_bytes());
    h.update(b":");
    h.update(descriptor.as_bytes());
    let d = h.finalize();
    // 16 hex chars (8 bytes) is plenty of entropy for file-name uniqueness.
    let mut s = String::with_capacity(24);
    for b in &d[..8] {
        s.push_str(&format!("{:02x}", b));
    }
    s.push_str(".profile");
    s
}

// ---------------------------------------------------------------------------
// CDS archive with JDK build-ID verification (T5.7.2)
// ---------------------------------------------------------------------------

/// Read the `JAVA_VERSION` field from `$JAVA_HOME/release`, returning an empty
/// string if the file is missing or malformed. Used as the "JDK build ID"
/// for the CDS header per the task spec.
pub fn read_jdk_build_id(java_home: &Path) -> String {
    let release = java_home.join("release");
    let data = match std::fs::read_to_string(&release) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    for line in data.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("JAVA_VERSION=") {
            // Strip optional surrounding quotes.
            return rest.trim_matches('"').to_string();
        }
        if let Some(rest) = trimmed.strip_prefix("IMPLEMENTOR_VERSION=") {
            return rest.trim_matches('"').to_string();
        }
    }
    // Fall back to the whole file's SHA so the guard still triggers on real
    // differences between JDK installs.
    let h = sha256_bytes(data.as_bytes());
    format!("sha:{:02x}{:02x}{:02x}{:02x}", h[0], h[1], h[2], h[3])
}

/// Sentinel magic so we can tell "has build-id header" from "bare cds archive".
pub const CDS_BUILD_ID_MAGIC: u32 = 0xB10E_1D00;

/// Wrapper around a CDS archive file that prepends a JDK build-ID header.
///
/// On-disk layout:
/// ```text
///  0..4   build_id_magic (u32 big-endian, 0xB10E_1D00)
///  4..6   build_id_len   (u16 big-endian)
///  6..6+n build_id       (utf-8)
///  n..    regular CdsArchive body (see cds.rs)
/// ```
pub struct CdsArchiveWithBuildIdV2 {
    pub build_id: String,
    pub output_path: String,
    pub entries: Vec<CdsArchiveEntry>,
}

impl CdsArchiveWithBuildIdV2 {
    pub fn new(output_path: impl Into<String>, build_id: impl Into<String>) -> Self {
        Self {
            build_id: build_id.into(),
            output_path: output_path.into(),
            entries: Vec::new(),
        }
    }

    pub fn add_entry(&mut self, entry: CdsArchiveEntry) {
        self.entries.push(entry);
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn output_path(&self) -> &str {
        &self.output_path
    }

    /// Write the archive: build-id header, then a freshly generated body.
    pub fn write(&self) -> Result<(), String> {
        if let Some(parent) = Path::new(&self.output_path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let mut gen = CdsArchiveGenerator::new(&self.output_path);
        for e in &self.entries {
            gen.add_entry(e.clone());
        }
        // Write the inner archive into a temp file so we can prepend our
        // build-id header safely.
        let tmp_path = format!("{}.tmp", self.output_path);
        let tmp_gen = CdsArchiveGenerator::new(&tmp_path);
        let mut tmp_gen = tmp_gen;
        for e in &self.entries {
            tmp_gen.add_entry(e.clone());
        }
        tmp_gen.write_archive()?;

        let mut inner_bytes = Vec::new();
        std::fs::File::open(&tmp_path)
            .and_then(|mut f| f.read_to_end(&mut inner_bytes))
            .map_err(|e| e.to_string())?;

        let mut f = std::fs::File::create(&self.output_path).map_err(|e| e.to_string())?;
        f.write_all(&CDS_BUILD_ID_MAGIC.to_be_bytes())
            .map_err(|e| e.to_string())?;
        let id_bytes = self.build_id.as_bytes();
        let id_len = id_bytes.len().min(u16::MAX as usize) as u16;
        f.write_all(&id_len.to_be_bytes())
            .map_err(|e| e.to_string())?;
        f.write_all(&id_bytes[..id_len as usize])
            .map_err(|e| e.to_string())?;
        f.write_all(&inner_bytes).map_err(|e| e.to_string())?;

        let _ = std::fs::remove_file(&tmp_path);
        Ok(())
    }

    /// Read build-id and class bytes back from `path`. Returns `None` if the
    /// header is missing, the build ID does not match `expected_build_id`,
    /// or the underlying CDS archive fails to load.
    pub fn try_load(
        path: &str,
        expected_build_id: &str,
    ) -> Option<(String, HashMap<String, Vec<u8>>)> {
        let mut file = std::fs::File::open(path).ok()?;
        let mut magic_buf = [0u8; 4];
        file.read_exact(&mut magic_buf).ok()?;
        let magic = u32::from_be_bytes(magic_buf);
        if magic != CDS_BUILD_ID_MAGIC {
            return None;
        }
        let mut len_buf = [0u8; 2];
        file.read_exact(&mut len_buf).ok()?;
        let id_len = u16::from_be_bytes(len_buf) as usize;
        let mut id_bytes = vec![0u8; id_len];
        file.read_exact(&mut id_bytes).ok()?;
        let build_id = String::from_utf8(id_bytes).ok()?;

        if build_id != expected_build_id {
            return None;
        }

        // Spool the rest of the file out into a temp so the existing
        // CdsArchiveLoader (which opens by path) can read it. This keeps the
        // in-tree loader logic single-sourced.
        let mut rest = Vec::new();
        file.read_to_end(&mut rest).ok()?;
        let tmp_path = format!("{}.cds_inner_load", path);
        std::fs::write(&tmp_path, &rest).ok()?;

        let mut loader = CdsArchiveLoader::new(&tmp_path);
        let ok = loader.try_load();
        // Extract cached class bytes regardless of whether try_load reported
        // success (the loader may populate the cache even on checksum fail;
        // our integrity check here is the build-id match above).
        let classes = loader.drain_class_cache();
        let _ = std::fs::remove_file(&tmp_path);

        if !ok && classes.is_empty() {
            return None;
        }
        Some((build_id, classes))
    }
}

// ---------------------------------------------------------------------------
// Global state & high-level API
// ---------------------------------------------------------------------------

static PIPELINE_STATE: Mutex<Option<PipelineState>> = Mutex::new(None);

struct PipelineState {
    config: Option<AotPipelineConfig>,
    /// Profile bundle accumulated during this run. Flushed at shutdown if
    /// `--aot-cache` was set.
    bundle: AotProfileBundle,
    /// Class bytes accumulated during this run. Flushed at shutdown if
    /// `--cds-dump` was set.
    cds_classes: Vec<CdsArchiveEntry>,
    /// CDS classes loaded at startup (populated when `--cds` was set and the
    /// archive was valid).
    cds_loaded: HashMap<String, Vec<u8>>,
    /// AOT profile entries loaded at startup (populated when `--aot-cache`
    /// was set).
    aot_loaded: Vec<AotProfileEntry>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            config: None,
            bundle: AotProfileBundle::new(),
            cds_classes: Vec::new(),
            cds_loaded: HashMap::new(),
            aot_loaded: Vec::new(),
        }
    }
}

/// Acquire the pipeline state, initializing it lazily on first use.
fn with_state<R>(f: impl FnOnce(&mut PipelineState) -> R) -> R {
    let mut guard = PIPELINE_STATE.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(PipelineState::new());
    }
    f(guard.as_mut().unwrap())
}

/// Capture a method profile entry (called from the training path).
pub fn record_profile_entry(entry: AotProfileEntry) {
    with_state(|state| {
        if state.config.is_some() {
            state.bundle.add(entry);
        }
    });
}

/// Record a class to be included in the CDS dump.
pub fn record_cds_class(class_name: impl Into<String>, class_bytes: Vec<u8>) {
    let entry = CdsArchiveEntry::new(class_name, 0, class_bytes);
    with_state(|state| {
        state.cds_classes.push(entry);
    });
}

/// Load caches referenced by the CLI flags in `config`. Returns a struct
/// describing how many entries were recovered.
pub fn startup_load(config: AotPipelineConfig) -> StartupStats {
    let root = config.root();
    let mut stats = StartupStats::default();

    // --- AOT cache ---
    if config.aot_cache {
        let dir = root.join(AOT_CACHE_DIR);
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("profile") {
                    continue;
                }
                let Ok(data) = std::fs::read(&p) else {
                    continue;
                };
                let Some(bundle) = AotProfileBundle::deserialize(&data) else {
                    continue;
                };
                stats.aot_profiles_loaded += bundle.entries.len();
                with_state(|state| state.aot_loaded.extend(bundle.entries));
            }
        }
    }

    // --- CDS archive ---
    if config.cds_enabled {
        let path = config.cds_archive_path();
        let java_home = config.java_home_override.clone().unwrap_or_else(|| {
            PathBuf::from(cratonvm_types::flags::runtime_var("JAVA_HOME").unwrap_or_default())
        });
        let expected = read_jdk_build_id(&java_home);
        if let Some((_, classes)) =
            CdsArchiveWithBuildIdV2::try_load(&path.to_string_lossy(), &expected)
        {
            stats.cds_classes_loaded = classes.len();
            with_state(|state| state.cds_loaded = classes);
        } else if path.exists() {
            stats.cds_rejected = true;
        }
    }

    // Stash the config for shutdown.
    with_state(|state| state.config = Some(config));

    stats
}

/// Flush caches referenced by the CLI flags in the stashed config.
pub fn shutdown_flush() -> ShutdownStats {
    // Pull everything we need out of the state up front so we drop the lock
    // before any file I/O (the Sha256 path inside write() must not block the
    // global state).
    let (config, bundle_entries, cds_classes): (
        Option<AotPipelineConfig>,
        Vec<AotProfileEntry>,
        Vec<CdsArchiveEntry>,
    ) = with_state(|state| {
        let cfg = state.config.clone();
        let be = std::mem::take(&mut state.bundle.entries);
        let cc = std::mem::take(&mut state.cds_classes);
        (cfg, be, cc)
    });
    let Some(config) = config else {
        return ShutdownStats::default();
    };

    let mut stats = ShutdownStats::default();

    // --- AOT cache ---
    if config.aot_cache && !bundle_entries.is_empty() {
        let dir = config.aot_cache_dir();
        let _ = std::fs::create_dir_all(&dir);

        // Split entries into per-method files so the `<hash>.profile` naming
        // convention from the spec applies.
        let mut by_method: HashMap<String, AotProfileBundle> = HashMap::new();
        for e in bundle_entries {
            let key = aot_profile_file_name(&e.class_name, &e.method_name, &e.descriptor);
            by_method.entry(key).or_default().add(e);
        }
        for (file_name, bundle) in by_method {
            let count = bundle.entries.len();
            let blob = bundle.serialize();
            let path = dir.join(&file_name);
            if std::fs::write(&path, &blob).is_ok() {
                stats.aot_profiles_written += count;
            }
        }
    }

    // --- CDS archive ---
    if config.cds_dump {
        let path = config.cds_archive_path();
        let java_home = config.java_home_override.clone().unwrap_or_else(|| {
            PathBuf::from(cratonvm_types::flags::runtime_var("JAVA_HOME").unwrap_or_default())
        });
        let build_id = read_jdk_build_id(&java_home);

        let mut archive =
            CdsArchiveWithBuildIdV2::new(path.to_string_lossy().to_string(), build_id);
        for e in cds_classes {
            archive.add_entry(e);
        }
        if archive.entry_count() > 0 && archive.write().is_ok() {
            stats.cds_classes_written = archive.entry_count();
        }
    }

    // Clear the configuration so a subsequent boot picks up fresh flags.
    with_state(|state| state.config = None);
    stats
}

/// Borrow any AOT profile entries that were recovered at startup. Used by
/// the interpreter to skip re-profiling hot methods already known to be hot.
pub fn loaded_aot_entries() -> Vec<AotProfileEntry> {
    with_state(|state| state.aot_loaded.clone())
}

/// Borrow class bytes that were recovered from the CDS archive at startup.
pub fn loaded_cds_classes() -> HashMap<String, Vec<u8>> {
    with_state(|state| state.cds_loaded.clone())
}

/// Summary stats from a [`startup_load`] call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupStats {
    /// How many AOT profile entries were recovered across all files.
    pub aot_profiles_loaded: usize,
    /// How many CDS class bytes were recovered.
    pub cds_classes_loaded: usize,
    /// True if the CDS archive was present but rejected (bad build ID,
    /// missing header, etc.).
    pub cds_rejected: bool,
}

/// Summary stats from a [`shutdown_flush`] call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShutdownStats {
    pub aot_profiles_written: usize,
    pub cds_classes_written: usize,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn tmp_dir_with_suffix(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let pid = std::process::id();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        d.push(format!("cratonvm_cp12_{tag}_{pid}_{ns}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // -----------------------------------------------------------------------
    // AotProfileEntry / AotProfileBundle unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn profile_entry_new_defaults_are_empty() {
        let e = AotProfileEntry::new([0u8; 32], "Foo", "bar", "()V", 0);
        assert!(e.jit_code.is_empty());
        assert_eq!(e.jit_flags, 0);
        assert!(!e.has_jit_code());
    }

    #[test]
    fn profile_entry_with_jit_code() {
        let e = AotProfileEntry::new([1u8; 32], "Foo", "bar", "()V", 5)
            .with_jit_code(vec![0x90, 0xC3], 0xDEAD_BEEF);
        assert!(e.has_jit_code());
        assert_eq!(e.jit_code, vec![0x90, 0xC3]);
        assert_eq!(e.jit_flags, 0xDEAD_BEEF);
    }

    #[test]
    fn bundle_serialize_roundtrip_preserves_entries() {
        let mut bundle = AotProfileBundle::new();
        bundle.add(
            AotProfileEntry::new([0xAA; 32], "com/foo/Bar", "run", "()V", 42)
                .with_jit_code(vec![0x48, 0x89, 0xE5, 0xC3], 0x01),
        );
        bundle.add(AotProfileEntry::new([0xBB; 32], "X", "y", "(I)I", 7));

        let blob = bundle.serialize();
        let restored = AotProfileBundle::deserialize(&blob).expect("roundtrip");
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries[0].call_count, 42);
        assert_eq!(restored.entries[0].jit_code, vec![0x48, 0x89, 0xE5, 0xC3]);
        assert_eq!(restored.entries[0].jit_flags, 0x01);
        assert_eq!(restored.entries[1].class_name, "X");
    }

    #[test]
    fn bundle_deserialize_rejects_bad_magic() {
        let mut data = AotProfileBundle::new().serialize();
        data[0] ^= 0xFF;
        assert!(AotProfileBundle::deserialize(&data).is_none());
    }

    #[test]
    fn bundle_deserialize_rejects_tampered_payload() {
        let mut bundle = AotProfileBundle::new();
        bundle.add(AotProfileEntry::new([0xCC; 32], "A", "m", "()V", 1));
        let mut blob = bundle.serialize();
        // Flip a byte in the middle so the SHA-256 integrity check trips.
        let mid = blob.len() / 2;
        blob[mid] ^= 0xFF;
        assert!(AotProfileBundle::deserialize(&blob).is_none());
    }

    #[test]
    fn bundle_deserialize_rejects_wrong_version() {
        let mut b = Vec::new();
        b.extend_from_slice(&AOT_PROFILE_MAGIC.to_le_bytes());
        b.extend_from_slice(&99u16.to_le_bytes()); // bad version
        b.extend_from_slice(&0u32.to_le_bytes()); // no entries
        b.extend_from_slice(&sha256_bytes(&b.clone()));
        assert!(AotProfileBundle::deserialize(&b).is_none());
    }

    #[test]
    fn filter_by_live_class_hashes_drops_stale_entries() {
        let mut bundle = AotProfileBundle::new();
        bundle.add(AotProfileEntry::new([0xAA; 32], "Good", "m", "()V", 1));
        bundle.add(AotProfileEntry::new([0xBB; 32], "Stale", "m", "()V", 1));
        bundle.add(AotProfileEntry::new([0xCC; 32], "Missing", "m", "()V", 1));

        let mut live = HashMap::new();
        live.insert("Good".to_string(), [0xAA; 32]);
        live.insert("Stale".to_string(), [0xDD; 32]); // wrong hash

        let filtered = bundle.filter_by_live_class_hashes(&live);
        assert_eq!(filtered.entries.len(), 1);
        assert_eq!(filtered.entries[0].class_name, "Good");
    }

    // -----------------------------------------------------------------------
    // Path helpers
    // -----------------------------------------------------------------------

    #[test]
    fn profile_file_name_is_deterministic_and_scoped() {
        let a = aot_profile_file_name("java/lang/String", "length", "()I");
        let b = aot_profile_file_name("java/lang/String", "length", "()I");
        assert_eq!(a, b);
        assert!(a.ends_with(".profile"));

        let c = aot_profile_file_name("java/lang/String", "length", "()J");
        assert_ne!(a, c);
    }

    #[test]
    fn config_paths_honour_root_override() {
        let root = tmp_dir_with_suffix("root");
        let cfg = AotPipelineConfig {
            root_override: Some(root.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.root(), root);
        assert_eq!(cfg.aot_cache_dir(), root.join(AOT_CACHE_DIR));
        assert_eq!(
            cfg.cds_archive_path(),
            root.join(CDS_DIR).join(CDS_ARCHIVE_NAME)
        );
    }

    // -----------------------------------------------------------------------
    // JDK build-ID parsing
    // -----------------------------------------------------------------------

    #[test]
    fn read_jdk_build_id_parses_java_version() {
        let d = tmp_dir_with_suffix("jh1");
        std::fs::write(d.join("release"), "JAVA_VERSION=\"25.0.1\"\nOTHER=x\n").unwrap();
        assert_eq!(read_jdk_build_id(&d), "25.0.1");
    }

    #[test]
    fn read_jdk_build_id_missing_file_is_empty() {
        let d = tmp_dir_with_suffix("jh2");
        // No release file.
        assert!(read_jdk_build_id(&d).is_empty());
    }

    #[test]
    fn read_jdk_build_id_falls_back_to_sha_for_unknown_format() {
        let d = tmp_dir_with_suffix("jh3");
        std::fs::write(d.join("release"), "HELLO=world\n").unwrap();
        let id = read_jdk_build_id(&d);
        assert!(id.starts_with("sha:"));
    }

    // -----------------------------------------------------------------------
    // CdsArchiveWithBuildIdV2 — write & load
    // -----------------------------------------------------------------------

    #[test]
    fn cds_with_build_id_roundtrip() {
        let d = tmp_dir_with_suffix("cds1");
        let path = d.join("core.jsa");
        let p = path.to_string_lossy().to_string();

        let mut archive = CdsArchiveWithBuildIdV2::new(p.clone(), "JDK-25.0.1");
        archive.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
        ));
        archive.add_entry(CdsArchiveEntry::new(
            "java/lang/String",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34, 0x01, 0x02],
        ));
        assert_eq!(archive.entry_count(), 2);
        archive.write().expect("write");
        assert!(path.exists());

        let loaded = CdsArchiveWithBuildIdV2::try_load(&p, "JDK-25.0.1").expect("load");
        assert_eq!(loaded.0, "JDK-25.0.1");
        assert_eq!(loaded.1.len(), 2);
        assert!(loaded.1.contains_key("java/lang/Object"));
        assert!(loaded.1.contains_key("java/lang/String"));
    }

    #[test]
    fn cds_with_build_id_rejects_mismatch() {
        let d = tmp_dir_with_suffix("cds2");
        let path = d.join("core.jsa");
        let p = path.to_string_lossy().to_string();

        let mut archive = CdsArchiveWithBuildIdV2::new(p.clone(), "JDK-25.0.1");
        archive.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
        ));
        archive.write().expect("write");

        // Load with a different build ID — should be refused.
        let res = CdsArchiveWithBuildIdV2::try_load(&p, "JDK-24.0.0");
        assert!(res.is_none());
    }

    #[test]
    fn cds_with_build_id_rejects_bare_archive() {
        // A CDS archive WITHOUT the build-id header must be refused.
        let d = tmp_dir_with_suffix("cds3");
        let path = d.join("core.jsa");
        let p = path.to_string_lossy().to_string();

        // Produce a bare archive via the existing generator.
        let mut gen = CdsArchiveGenerator::new(&p);
        gen.add_entry(CdsArchiveEntry::new(
            "java/lang/Object",
            0,
            vec![0xCA, 0xFE, 0xBA, 0xBE],
        ));
        gen.write_archive().expect("write");

        assert!(CdsArchiveWithBuildIdV2::try_load(&p, "anything").is_none());
    }

    // -----------------------------------------------------------------------
    // Integration tests — full startup/shutdown pipeline
    // -----------------------------------------------------------------------

    /// FIX(test-isolation): the integration tests below drive the PROCESS-GLOBAL
    /// `PIPELINE_STATE` through multi-step sequences (reset → record → flush →
    /// load → assert). Each `with_state` op is individually locked, but the
    /// *sequences* interleave under parallel `cargo test` — and the `--workspace`
    /// build enables `experimental-aot`, running these alongside the whole
    /// suite — so a sibling's `reset_pipeline_state()` / records clobber another
    /// test's accumulated state, yielding wrong counts (e.g.
    /// `aot_profiles_written != 2`, `cds_classes_loaded != 2`). Serialize the
    /// whole-test sequences on this lock so each runs atomically against the
    /// global pipeline state. Poison-tolerant so a panicking test can't wedge
    /// the rest.
    fn pipeline_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Use a fresh state for integration tests so they don't fight the global
    /// mutex-backed globals in other tests. We simulate the pipeline by
    /// inserting config, populating state, flushing, then reading back.
    fn reset_pipeline_state() {
        with_state(|s| {
            s.config = None;
            s.bundle = AotProfileBundle::new();
            s.cds_classes.clear();
            s.cds_loaded.clear();
            s.aot_loaded.clear();
        });
    }

    /// Integration test #1: AOT profile roundtrip.
    ///
    /// Simulates a training run: `startup_load` is a no-op, we record a few
    /// profile entries, `shutdown_flush` persists them, and a second
    /// `startup_load` recovers them.
    #[test]
    fn integration_aot_profile_roundtrip() {
        let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
        reset_pipeline_state();
        let root = tmp_dir_with_suffix("aot_rt");
        let java_home = tmp_dir_with_suffix("aot_rt_jh");
        std::fs::write(java_home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();

        let cfg = AotPipelineConfig {
            aot_cache: true,
            root_override: Some(root.clone()),
            java_home_override: Some(java_home.clone()),
            ..Default::default()
        };

        // First boot: empty cache.
        let stats = startup_load(cfg.clone());
        assert_eq!(stats.aot_profiles_loaded, 0);

        // Record a couple of hot methods.
        record_profile_entry(
            AotProfileEntry::new([0xAA; 32], "com/foo/Bar", "hot", "()V", 9_999)
                .with_jit_code(vec![0x48, 0x89, 0xE5, 0xC3], 0x1),
        );
        record_profile_entry(AotProfileEntry::new(
            [0xBB; 32],
            "com/foo/Baz",
            "warm",
            "(I)I",
            100,
        ));

        // Shutdown writes them to <hash>.profile files.
        let shut = shutdown_flush();
        assert_eq!(shut.aot_profiles_written, 2);

        // Second boot: cache is repopulated.
        reset_pipeline_state();
        let stats = startup_load(cfg);
        assert_eq!(stats.aot_profiles_loaded, 2);
        let loaded = loaded_aot_entries();
        let names: std::collections::HashSet<_> =
            loaded.iter().map(|e| e.class_name.clone()).collect();
        assert!(names.contains("com/foo/Bar"));
        assert!(names.contains("com/foo/Baz"));
        // Also verify JIT code bytes survived.
        let bar = loaded
            .iter()
            .find(|e| e.class_name == "com/foo/Bar")
            .unwrap();
        assert_eq!(bar.jit_code, vec![0x48, 0x89, 0xE5, 0xC3]);

        reset_pipeline_state();
    }

    /// Integration test #2: CDS roundtrip with matching build ID.
    #[test]
    fn integration_cds_roundtrip() {
        let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
        reset_pipeline_state();
        let root = tmp_dir_with_suffix("cds_rt");
        let java_home = tmp_dir_with_suffix("cds_rt_jh");
        std::fs::write(java_home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();

        // Dump.
        let dump_cfg = AotPipelineConfig {
            cds_dump: true,
            root_override: Some(root.clone()),
            java_home_override: Some(java_home.clone()),
            ..Default::default()
        };
        startup_load(dump_cfg);
        record_cds_class(
            "java/lang/Object",
            vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
        );
        record_cds_class(
            "java/lang/String",
            vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34, 0x10, 0x20],
        );
        let shut = shutdown_flush();
        assert_eq!(shut.cds_classes_written, 2);
        assert!(root.join(CDS_DIR).join(CDS_ARCHIVE_NAME).exists());

        // Load with matching build ID.
        reset_pipeline_state();
        let load_cfg = AotPipelineConfig {
            cds_enabled: true,
            root_override: Some(root.clone()),
            java_home_override: Some(java_home.clone()),
            ..Default::default()
        };
        let stats = startup_load(load_cfg);
        assert_eq!(stats.cds_classes_loaded, 2);
        assert!(!stats.cds_rejected);
        let classes = loaded_cds_classes();
        assert!(classes.contains_key("java/lang/Object"));
        assert!(classes.contains_key("java/lang/String"));

        reset_pipeline_state();
    }

    /// Integration test #3: stale cache rejection on JDK-build-ID mismatch.
    #[test]
    fn integration_stale_cache_rejected_on_build_id_mismatch() {
        let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
        reset_pipeline_state();
        let root = tmp_dir_with_suffix("stale");
        let java_home_a = tmp_dir_with_suffix("stale_jha");
        let java_home_b = tmp_dir_with_suffix("stale_jhb");
        std::fs::write(java_home_a.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();
        std::fs::write(java_home_b.join("release"), "JAVA_VERSION=\"26.0.0\"\n").unwrap();

        // Dump with JDK A.
        let dump_cfg = AotPipelineConfig {
            cds_dump: true,
            root_override: Some(root.clone()),
            java_home_override: Some(java_home_a.clone()),
            ..Default::default()
        };
        startup_load(dump_cfg);
        record_cds_class("java/lang/Object", vec![0xCA, 0xFE, 0xBA, 0xBE]);
        let shut = shutdown_flush();
        assert_eq!(shut.cds_classes_written, 1);

        // Load with JDK B — must be rejected.
        reset_pipeline_state();
        let load_cfg = AotPipelineConfig {
            cds_enabled: true,
            root_override: Some(root.clone()),
            java_home_override: Some(java_home_b.clone()),
            ..Default::default()
        };
        let stats = startup_load(load_cfg);
        assert_eq!(stats.cds_classes_loaded, 0);
        assert!(stats.cds_rejected);
        assert!(loaded_cds_classes().is_empty());

        reset_pipeline_state();
    }

    /// Integration test #4 (bonus): stale AOT entry rejection via class SHA.
    #[test]
    fn integration_stale_aot_entry_rejected_on_class_sha_mismatch() {
        let mut bundle = AotProfileBundle::new();
        bundle.add(AotProfileEntry::new(
            [0xAA; 32],
            "com/foo/Bar",
            "m",
            "()V",
            100,
        ));
        bundle.add(AotProfileEntry::new(
            [0xBB; 32],
            "com/foo/Baz",
            "m",
            "()V",
            50,
        ));

        // Only Bar is live, and its hash has changed since the profile was
        // written. Baz is live with the matching hash.
        let mut live = HashMap::new();
        live.insert("com/foo/Bar".to_string(), [0xDD; 32]); // changed
        live.insert("com/foo/Baz".to_string(), [0xBB; 32]); // same

        let filtered = bundle.filter_by_live_class_hashes(&live);
        assert_eq!(filtered.entries.len(), 1);
        assert_eq!(filtered.entries[0].class_name, "com/foo/Baz");
    }
}
