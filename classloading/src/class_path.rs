// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// T1.8.2 follow-up — production-code panic gate. The class-path
// loader is on the startup-critical path; a panic here would
// terminate the entire VM before user code even starts. We gate the
// lint on `not(test)` so the test module (which legitimately uses
// `.unwrap()`/`.expect()` for fixture construction) is unaffected.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used,))]

use crate::loader_flags;
use cratonvm_reader::SharedBytes;
use cratonvm_types::error::ClassFileError;
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::debug;
use zip::ZipArchive;

/// Percent-encode a filesystem path for embedding in a `file:`/`jar:file:` URL,
/// the way `File.toURI()` (`sun.net.www.ParseUtil.encodePath`) does.
///
/// Without this a directory literally named `custom#root` came back from
/// `ClassLoader.getResource` as `file:/.../custom#root/scanned/`, where the raw
/// `#` is a URL *fragment* delimiter — every consumer that re-parses the URL
/// silently drops everything after it (HotSpot keeps `custom%23root`; see
/// `core.io.support.PathMatchingResourcePatternResolverTests.encodedHashtagInPath`).
/// `%` is escaped first so the escapes added here are not double-encoded.
/// Mirrors `native-builtins::classloader::file_url_spec`, which already did
/// this for the `URLClassLoader.getURLs()` / manifest side only.
pub(crate) fn encode_path_for_url(p: &str) -> String {
    p.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
}

/// Immutable backing for an archive. On-disk archives are mapped once and all
/// `ZipArchive` cursors plus stored class entries share that mapping. Nested
/// archives and test fixtures retain an owned reference-counted buffer.
#[derive(Clone)]
enum ArchiveBacking {
    Owned(Arc<[u8]>),
    Mapped(Arc<memmap2::Mmap>),
    Shared(SharedBytes),
}

impl ArchiveBacking {
    fn from_vec(bytes: Vec<u8>) -> Self {
        Self::Owned(Arc::from(bytes))
    }
}

impl AsRef<[u8]> for ArchiveBacking {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

impl std::ops::Deref for ArchiveBacking {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

type SharedArchive = ZipArchive<Cursor<ArchiveBacking>>;

#[derive(Clone)]
struct ArchiveSlice {
    backing: ArchiveBacking,
    start: usize,
    end: usize,
}

impl AsRef<[u8]> for ArchiveSlice {
    fn as_ref(&self) -> &[u8] {
        &self.backing.as_ref()[self.start..self.end]
    }
}

/// Diagnostic-only (TLD/JAR-scan slowness investigation, 2026-07-21): time
/// every `find_resource`/`find_all_resource_urls` call and print a running
/// summary every 2000 calls when `CRATONVM_DBG_RESOURCE_TIMING=1`. Not a
/// permanent instrumentation point — remove before shipping the real fix.
fn diag_resource_call_wrapper<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| loader_flags().dbg_resource_timing);
    if !enabled {
        return f();
    }
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static NANOS: AtomicU64 = AtomicU64::new(0);
    let t0 = std::time::Instant::now();
    let result = f();
    let elapsed = t0.elapsed().as_nanos() as u64;
    let calls = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let total_nanos = NANOS.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
    if calls % 2000 == 0 || elapsed > 5_000_000 {
        eprintln!(
            "[RES-DIAG] {label} call#{calls} this_call={:?} total_calls={calls} total_time={:?}",
            std::time::Duration::from_nanos(elapsed),
            std::time::Duration::from_nanos(total_nanos),
        );
    }
    result
}

/// Read a classpath file into owned bytes without memory-mapping it.
///
/// Classpath entries are ordinary files controlled by launchers, build tools,
/// and sometimes deployment systems. A concurrent truncate of a memory-mapped
/// JAR can terminate the process on Unix when the mapping is touched. Keeping
/// the read on `File::read_to_end` avoids that signal-level failure mode; if
/// the file changes during the read, callers get an `InvalidData` error and the
/// classpath entry is skipped/fails closed just like a corrupt archive.
fn read_file_for_classpath(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let mut buf = Vec::with_capacity(safe_with_capacity(before.len()));
    file.read_to_end(&mut buf)?;
    let after = file.metadata()?;
    if classpath_file_metadata_changed(&before, &after) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "classpath file changed while being read: {}",
                path.display()
            ),
        ));
    }
    Ok(buf)
}

/// Map an on-disk archive once, falling back to owned bytes when mapping is
/// unavailable. Classpath archives are immutable for the lifetime of a class
/// loader by JVM convention. `CRATONVM_DISABLE_JAR_MMAP=1` retains the copied
/// path for deployment systems that replace/truncate archives in place.
fn read_archive_for_classpath(path: &Path) -> std::io::Result<ArchiveBacking> {
    if loader_flags().disable_jar_mmap {
        return read_file_for_classpath(path).map(ArchiveBacking::from_vec);
    }
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    if before.len() == 0 {
        return Ok(ArchiveBacking::from_vec(Vec::new()));
    }
    // SAFETY: the mapping is read-only, its owner is held by every archive
    // cursor/slice, and classpath archives are immutable after loader creation.
    // Deployments that cannot provide immutability use the opt-out above.
    let mapped = match unsafe { memmap2::MmapOptions::new().map(&file) } {
        Ok(mapped) => mapped,
        Err(_) => return read_file_for_classpath(path).map(ArchiveBacking::from_vec),
    };
    let after = file.metadata()?;
    if classpath_file_metadata_changed(&before, &after) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "classpath archive changed while being mapped: {}",
                path.display()
            ),
        ));
    }
    Ok(ArchiveBacking::Mapped(Arc::new(mapped)))
}

fn classpath_file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.len() != after.len() {
        return true;
    }
    match (before.modified(), after.modified()) {
        (Ok(before), Ok(after)) => before != after,
        _ => false,
    }
}

/// Round 7 audit fix (HIGH #4): bound the canonicalize cache so a
/// long-running JVM that probes many distinct paths (Spring-style
/// directory scans, fileless classloaders, agent-discovered classes)
/// can't grow it without limit. Picked to mirror the analogous round-3/4
/// cap on `class_bytes_cache`. Classpath roots stay constant; resolved-
/// file entries are usually well under this cap, so eviction only kicks
/// in for pathological probe sets.
const CANONICALIZE_CACHE_CAP: usize = 1024;

/// Cached verdict for the `CRATONVM_DBG_GETRESOURCES` env var.
///
/// `find_all_resource_urls` is called once per `ClassPath` instance
/// (bootstrap / extension / application) for every `getResources`
/// probe — a hot path on Spring-style classpath scans. The env var
/// can't change after process start, so `cratonvm_types::flags::runtime_var` (which locks
/// the libc environ and allocates a `String`) is read exactly once
/// here and the boolean verdict reused on every subsequent call.
static DBG_GETRESOURCES: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_DBG_GETRESOURCES` is set to a non-empty,
/// non-`"0"` value. Computed once and cached for the process lifetime.
fn dbg_getresources() -> bool {
    loader_flags().dbg_getresources
}

/// Cached verdict for the `CRATONVM_HARDEN_MANIFEST_CLASSPATH` env var.
///
/// VULN (low) — manifest `Class-Path` filesystem reach: per the JAR
/// spec, a JAR's `MANIFEST.MF` `Class-Path:` attribute can name
/// *arbitrary* paths (`../../etc`, `file:/etc`, absolute drive roots)
/// and those become additional classpath roots. HotSpot honours this
/// verbatim — the manifest is part of the trusted application bundle,
/// so a relative/absolute escape is the author's prerogative, and
/// matching that behaviour is the default here so legitimate launchers
/// (Surefire booter JARs, Spring Boot, fat JARs that point at a sibling
/// `lib/`) keep working.
///
/// When set to a non-empty, non-`"0"` value this flag opts into a
/// hardened policy: manifest `Class-Path` roots are restricted to
/// descendants of the JAR's own directory (`jar_dir`), so a hostile or
/// tampered manifest cannot pull in `file:/etc` or `..\..` outside the
/// app bundle. Resolution still happens; out-of-tree roots are dropped
/// (with a debug log) rather than silently honoured. Off by default to
/// preserve HotSpot parity. Read once at process start (the env can't
/// change mid-run), mirroring [`dbg_getresources`].
static HARDEN_MANIFEST_CLASSPATH: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set to a
/// non-empty, non-`"0"` value. Computed once and cached for the process
/// lifetime. See [`HARDEN_MANIFEST_CLASSPATH`].
fn harden_manifest_classpath() -> bool {
    loader_flags().harden_manifest_classpath
}

/// Bounded FIFO cache for [`fs::canonicalize`] results.
///
/// Round 7 audit fix (HIGH #4): the previous unbounded `HashMap` had
/// no eviction and never invalidated entries; a long-running process
/// that probes many distinct paths (Spring scanning,
/// agent-instrumented classes, hidden classes) could grow it without
/// bound. The FIFO tracker keeps `paths` and `order` in sync so that
/// when the map hits `CANONICALIZE_CACHE_CAP` entries, the
/// oldest-inserted path is evicted. Mirrors the same pattern used by
/// `class_bytes_cache_fifo` in `class_manager.rs`.
struct CanonicalizeCache {
    paths: HashMap<PathBuf, PathBuf>,
    /// Insertion-order tracker for FIFO eviction. The front is the
    /// oldest entry; the back is the most recent. Kept in sync with
    /// `paths`: every insert pushes to the back; every eviction pops
    /// from the front. Bounded by `CANONICALIZE_CACHE_CAP`.
    order: VecDeque<PathBuf>,
}

impl CanonicalizeCache {
    fn new() -> Self {
        Self {
            paths: HashMap::new(),
            order: VecDeque::with_capacity(CANONICALIZE_CACHE_CAP),
        }
    }

    /// Insert `(key, value)` honouring the FIFO cap. If `key` already
    /// existed the value is overwritten in place and the FIFO position
    /// is left unchanged (avoiding a linear `VecDeque` scan on the
    /// hot insert path — duplicate inserts are race-resolution writes
    /// that pick identical values, see callers).
    fn insert(&mut self, key: PathBuf, value: PathBuf) {
        if self.paths.contains_key(&key) {
            self.paths.insert(key, value);
            return;
        }
        if self.paths.len() >= CANONICALIZE_CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.paths.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.paths.insert(key, value);
    }

    /// Drop every memoized canonicalization. Used when a classpath root is
    /// retracted, since a cached hit may name a file the classpath can no
    /// longer serve.
    fn clear(&mut self) {
        self.paths.clear();
        self.order.clear();
    }
}

/// Does `entry` come from the filesystem root `root`?
///
/// Every variant carries the path it was built from: the archive/directory
/// itself for the flat forms, and the OUTER jar for the two nested forms — which
/// is the right answer for retraction, because one `add_path` of a fat JAR
/// appends one entry per nested archive inside it.
fn entry_derives_from(entry: &ClassPathEntry, root: &Path) -> bool {
    let source = match entry {
        ClassPathEntry::Directory(path)
        | ClassPathEntry::JarFile { path, .. }
        | ClassPathEntry::JmodFile { path, .. }
        | ClassPathEntry::JImageFile { path, .. } => path,
        ClassPathEntry::NestedDirectory { parent_jar, .. }
        | ClassPathEntry::NestedJar { parent_jar, .. } => parent_jar,
    };
    if source == root {
        return true;
    }
    // A spec and the recorded entry can spell the same file differently
    // (relative vs absolute, `/` vs `\`), so fall back to a canonical compare.
    match (source.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Represents the classpath used to find `.class` files.
///
/// Supports directory entries, flat JAR entries, and Spring Boot fat JAR
/// entries (nested JARs inside `BOOT-INF/lib/` and classes from
/// `BOOT-INF/classes/`).
pub struct ClassPath {
    entries: Vec<ClassPathEntry>,
    /// Round 5 audit fix (MED): per-`ClassPath` cache of
    /// [`fs::canonicalize`] results. Each `find_class` probe over a
    /// `Directory` entry previously canonicalized BOTH the classpath
    /// root and the resolved file on every call — that's two syscalls
    /// per probe even when neither path has changed since the last
    /// lookup. The cache is consulted before falling through to the
    /// `fs` call; populated lazily on first miss. Use `Mutex` (not
    /// `RwLock`) because the populate path only writes; reads of an
    /// already-cached entry are sub-ms even under contention.
    ///
    /// Round 7 audit fix (HIGH #4): capped at
    /// `CANONICALIZE_CACHE_CAP` with FIFO eviction (see
    /// [`CanonicalizeCache`]). Long-running processes that probe many
    /// distinct paths can no longer grow this map without bound.
    canonicalize_cache: Mutex<CanonicalizeCache>,
    /// Memoized `fs::canonicalize` for **classpath roots** only — see
    /// [`ClassPath::canonicalize_root`] for why these cannot share the
    /// bounded FIFO above. Unbounded by construction, because its key set is
    /// exactly the entry list: one insert per `Directory`/`JarFile` root the
    /// scan has touched, never a caller-supplied probe path.
    root_canonical_cache: Mutex<HashMap<PathBuf, PathBuf>>,
    /// Specs handed to [`ClassPath::add_path`], with a use count.
    ///
    /// `URLClassLoader.close()` must stop a closed loader serving new classes,
    /// which means retracting the roots that loader added — and until this
    /// existed there was no way to name them, so `close()` could only ever be a
    /// no-op. The count is what makes retraction safe when two loaders were
    /// handed the same JAR: the last one to close is the one that removes it.
    dynamic_specs: HashMap<String, u32>,
    /// `entries.len()` immediately before the FIRST [`ClassPath::add_path`].
    ///
    /// Retraction only ever considers entries at or past this index, so a JAR
    /// that is both a startup classpath root and a dynamically added one keeps
    /// its startup entry when the dynamic loader closes.
    static_len: Option<usize>,
    /// Relative paths present under each `Directory` entry, so a MISS on that
    /// entry costs a hash probe instead of a `File::exists` syscall.
    ///
    /// # Why a directory needed one when a JAR already had one
    ///
    /// A `JarFile` entry carries `entry_index` for exactly this reason — "hot
    /// class/resource lookup reject[s] misses without taking the ZipArchive
    /// lock". `Directory` had no equivalent, so [`ClassPath::find_class`] did
    /// a real `dir.join(rel).exists()` for every directory entry on every
    /// lookup. That is one syscall per directory per class, and a class is
    /// found in at most one of them, so the whole cost is misses.
    ///
    /// Measured on the netty suite's classpath (308 entries: 197 JARs, **111
    /// directories** — one `target/classes` and one `target/test-classes` per
    /// reactor module) loading netty's slf4j/logback first-touch, 1102 class
    /// definitions, Windows host:
    ///
    /// | classpath | first `DefaultThreadFactory.newThread` |
    /// |---|---|
    /// | all 308 entries | 2195 ms |
    /// | the 4 entries actually needed | 188 ms |
    ///
    /// 111 x 1102 = 122k stats at ~15-20 us each is the whole difference.
    /// Linux hides it — the same probe costs 483 ms there, faster than HotSpot
    /// — because a `stat` on a warm dentry cache is ~1 us. It is the same
    /// shape as the `JarFile` accessor defect written up in
    /// `a_stat_per_accessor_call_hid_behind_an_o1_cache`, one layer out.
    ///
    /// # Staleness
    ///
    /// The index is built once per directory, on first use, and never
    /// invalidated — so a class written into a classpath directory *after*
    /// that point is absent from it. That is why an index MISS is not the end
    /// of the lookup: `find_class` falls back to the real `exists()` probe for
    /// directory entries when no entry claimed the class, which is precisely
    /// the pre-index behaviour. Anything the old code could find, this finds;
    /// the index only removes syscalls from the path that succeeds.
    dir_index: Mutex<HashMap<PathBuf, Option<Arc<FxHashSet<Box<str>>>>>>,
}

/// Above this many files, a directory is left UNINDEXED and every lookup
/// stats it, as before. A classpath root with a million files under it is not
/// a normal `target/classes`, and building a set that size on the first class
/// load would trade a latency spike for the throughput win.
const DIR_INDEX_MAX_ENTRIES: usize = 200_000;

/// Companion bound to [`DIR_INDEX_MAX_ENTRIES`] on the other axis. The walk
/// follows symlinks, so a directory cycle is reachable and the file cap alone
/// would not stop it — a cycle of empty directories adds no files.
const DIR_INDEX_MAX_DIRS: usize = 50_000;

/// Per-archive memoized signing state for a signed JAR.
///
/// Security fix (V3, unsigned-entry attack — JAR spec §"Signature
/// Validation"): the archive-level signer *chain* alone is NOT a
/// licence to stamp every `.class` in the JAR with the signer's
/// certificates. The JAR spec only treats an entry as signed when the
/// **manifest commits to it** with a per-entry `<alg>-Digest` that
/// matches the entry's bytes. An entry that is present in the archive
/// but absent from the manifest (or present without a digest) is
/// **unsigned** — even inside a signed JAR — and an injected/swapped
/// `.class` must NOT inherit the signer's identity.
///
/// We therefore memoize, alongside the verified `chain`, the set of
/// entry names the manifest committed to (with the digest algorithm and
/// expected value the signer authenticated). `find_class_code_source_info`
/// consults `signed_entries` and re-hashes the specific class bytes
/// before attaching `chain`; a class that is not in this map, or whose
/// bytes don't match, is reported with an empty cert list (unsigned),
/// matching HotSpot's per-entry `CodeSigner` semantics.
struct JarSignerInfo {
    /// Verified archive-level signer cert chain (leaf first), the union
    /// of every signer block that passed full verification. Empty for an
    /// unsigned JAR or one that failed verification.
    chain: Vec<Vec<u8>>,
    /// Entry name (JAR-internal path, e.g. `com/example/Foo.class`) →
    /// the `(algorithm, expected-digest)` the manifest committed to for
    /// that entry, restricted to manifests bound to a verified signer.
    /// Only entries appearing here are eligible to inherit `chain`.
    signed_entries: HashMap<String, (crate::jar_signer::DigestAlg, Vec<u8>)>,
}

enum ClassPathEntry {
    Directory(PathBuf),
    /// A JAR file read into memory. The `Mutex` provides interior mutability
    /// since `ZipArchive::by_name` requires `&mut self`, and future-proofs
    /// for multi-threaded access (Phase 5).
    JarFile {
        path: PathBuf,
        archive: Mutex<SharedArchive>,
        backing: ArchiveBacking,
        /// Exact central-directory entry names in this archive. This lets hot
        /// class/resource lookup reject misses without taking the ZipArchive
        /// lock or asking zip::ZipArchive::by_name to hash/probe its index.
        entry_index: FxHashSet<String>,
        /// True if the JAR declares `Multi-Release: true` in its manifest (JEP 238).
        multi_release: bool,
        /// Audit-fix #7: cached set of `META-INF/versions/<N>/` directory
        /// numbers present in this archive. Built lazily on first
        /// multi-release lookup so subsequent lookups skip
        /// `by_name` probes for absent versions. `None` means the
        /// cache hasn't been built yet.
        ///
        /// PERF: wrapped in `Arc` so `ensure_versions_cache` can hand
        /// callers a cheap reference-count bump instead of deep-cloning
        /// the whole `BTreeSet` on every multi-release class lookup (the
        /// previous `cache.clone()` heap-allocated a fresh tree per
        /// lookup). The set is immutable once built, so sharing it is
        /// behavior-preserving.
        versions_cache: Mutex<Option<Arc<BTreeSet<u32>>>>,
        /// Report P1 (perf): memoized verified signer cert chain (leaf+chain
        /// DER) for this archive. `extract_jar_signer_blocks` is a pure
        /// function of the archive — the `CodeSource` certificates are
        /// identical for every class in the JAR — but it was re-run on every
        /// class lookup: full central-directory rescan, re-read of every
        /// `*.RSA`/`*.SF`, PKCS#7 parse, RSA/ECDSA/DSA verify, trust-chain
        /// walk, and (post V1 fix) a MANIFEST.MF + per-entry digest re-hash.
        /// Populated once on the first signed lookup and cloned thereafter;
        /// an empty chain (unsigned, or failed verification) is also cached so
        /// the rescan is skipped for unsigned JARs too. Security is unchanged
        /// — the full verification still runs, exactly once.
        ///
        /// V3: now also carries the manifest-committed `signed_entries` map so
        /// the signer chain is attached **per class** (only to entries the
        /// signer committed to), not blanket to every class in the archive.
        signer_cache: OnceLock<JarSignerInfo>,
    },
    /// A virtual directory inside a fat JAR (e.g. `BOOT-INF/classes/`).
    /// Entries are stored as a map from relative path to byte content.
    NestedDirectory {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Prefix inside the outer JAR (e.g. `BOOT-INF/classes/`).
        prefix: String,
        /// Cached entries: relative_path (without prefix) → bytes.
        entries_cache: HashMap<String, SharedBytes>,
    },
    /// A nested JAR extracted from inside a fat JAR (e.g. `BOOT-INF/lib/dep.jar`).
    NestedJar {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Path inside the outer JAR (e.g. `BOOT-INF/lib/spring-core-6.1.0.jar`).
        nested_path: String,
        /// The extracted nested archive.
        archive: Mutex<SharedArchive>,
        backing: ArchiveBacking,
        /// Exact central-directory entry names in this nested archive.
        entry_index: FxHashSet<String>,
        /// Report P1 (perf): memoized verified signer cert chain for this
        /// nested archive — see the matching field on `JarFile`. Populated
        /// once on the first signed lookup, cloned thereafter.
        ///
        /// V3: carries the manifest-committed `signed_entries` map for
        /// per-class cert attachment (see [`JarSignerInfo`]).
        signer_cache: OnceLock<JarSignerInfo>,
    },
    /// A JDK 9+ JMOD file (ZIP with 4-byte `JM\x01\x00` prefix).
    ///
    /// PERF (2026-07-26 boot-classpath-lazy): this variant used to carry a
    /// `classes_cache: HashMap<String, SharedBytes>` holding the **inflated
    /// bytes of every `classes/` entry**, built eagerly by `load_jmod`. On a
    /// stock JDK 25 `discover_boot_classpath` puts all 70 `.jmod` files on the
    /// boot classpath, so that cost 27,962 full deflate passes and ~136 MB of
    /// resident inflated bytes *before the first Java class loaded* — measured
    /// at 15-21 s even with optimised native zlib, and far worse in a debug
    /// build. See `arch-2026-07-26/boot-classpath-lazy.md`.
    ///
    /// The eager cache existed to stop existence probes paying a deflate each
    /// (`find_in_archive(..).is_some()` inflated the entry just to answer a
    /// yes/no). That is now solved the way the JAR path already solves it: a
    /// decompression-free name index built from the central directory
    /// (compare `JarFile::entry_index` / `build_archive_entry_index`), with
    /// bytes inflated only for entries actually requested. Existence checks
    /// are a hash-set probe; a real hit is one deflate of one entry.
    JmodFile {
        path: PathBuf,
        /// Names of every entry under `classes/`, with the `classes/` prefix
        /// stripped (e.g. `java/lang/Object.class`). Built once during
        /// `load_jmod` from the central directory — no entry is inflated to
        /// populate it. Membership here means "this JMOD can serve that
        /// name"; the bytes come from [`ClassPath::jmod_class_bytes`].
        class_entry_index: FxHashSet<String>,
        /// The full set of entry names in the JMOD (including non-class entries)
        /// kept for `list_jmod_classes` and resource lookups.
        all_entry_names: Vec<String>,
        /// The archive, used for every byte-serving lookup (classes via
        /// [`ClassPath::jmod_class_bytes`], other entries directly).
        archive: Mutex<SharedArchive>,
        backing: ArchiveBacking,
    },
    /// A JDK 9+ `lib/modules` jimage file (NEW-5): one binary blob holding
    /// every class and resource in the boot layer, accessed through the
    /// [`cratonvm_reader::JImageReader`] perfect-hash index.
    ///
    /// The lookup strategy:
    ///   1. `class_to_module`: pre-built map from internal class name
    ///      (e.g. `java/lang/String`) to its owning module (e.g. `java.base`).
    ///      Populated once during load by iterating the jimage entries.
    ///   2. `find_class` consults this map and then calls
    ///      `JImageReader::find_class(module, name)` to fetch bytes via the
    ///      perfect-hash path — O(1) amortized.
    ///   3. `find_resource` tries every known module for a matching resource,
    ///      stopping on the first hit (matches JDK `ModuleReader` semantics).
    JImageFile {
        path: PathBuf,
        reader: cratonvm_reader::JImageReader,
        /// Map from internal class name (no `.class` suffix) to its owning
        /// module name. Built once during load by walking every entry; used
        /// by `find_class` for O(1) module resolution.
        class_to_module: HashMap<String, String>,
        /// Map from resource path (with `.class` suffix for classes, or the
        /// raw resource path for non-class files) to the list of modules
        /// that contain it. Only non-class resources appear here — classes
        /// use `class_to_module` instead.
        resource_to_modules: HashMap<String, Vec<String>>,
        /// Every module name that contributes at least one entry. Used by
        /// `scan_module_infos` and by the module-layer builder.
        module_names: Vec<String>,
    },
}

impl std::fmt::Debug for ClassPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<String> = self
            .entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::Directory(p) => format!("dir:{}", p.display()),
                ClassPathEntry::JarFile { path, .. } => format!("jar:{}", path.display()),
                ClassPathEntry::NestedDirectory {
                    parent_jar, prefix, ..
                } => format!("nested-dir:{}!/{}", parent_jar.display(), prefix),
                ClassPathEntry::NestedJar {
                    parent_jar,
                    nested_path,
                    ..
                } => format!("nested-jar:{}!/{}", parent_jar.display(), nested_path),
                ClassPathEntry::JmodFile { path, .. } => format!("jmod:{}", path.display()),
                ClassPathEntry::JImageFile { path, .. } => {
                    format!("jimage:{}", path.display())
                }
            })
            .collect();
        f.debug_struct("ClassPath")
            .field("entries", &paths)
            .finish()
    }
}

impl std::fmt::Debug for ClassPathEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClassPathEntry::Directory(p) => write!(f, "Directory({})", p.display()),
            ClassPathEntry::JarFile { path, .. } => write!(f, "JarFile({})", path.display()),
            ClassPathEntry::NestedDirectory {
                parent_jar, prefix, ..
            } => write!(f, "NestedDirectory({}!/{})", parent_jar.display(), prefix),
            ClassPathEntry::NestedJar {
                parent_jar,
                nested_path,
                ..
            } => write!(f, "NestedJar({}!/{})", parent_jar.display(), nested_path),
            ClassPathEntry::JmodFile { path, .. } => write!(f, "JmodFile({})", path.display()),
            ClassPathEntry::JImageFile { path, .. } => {
                write!(f, "JImageFile({})", path.display())
            }
        }
    }
}

/// Do two entries read from the same place?
///
/// This is the identity HotSpot's `URLClassPath` dedupes on: it refuses to
/// push a URL already on its path, so `java -cp dir;dir` answers
/// `getResources` with ONE URL, not two (measured, JDK 25).
fn same_classpath_source(a: &ClassPathEntry, b: &ClassPathEntry) -> bool {
    match (a, b) {
        (ClassPathEntry::Directory(x), ClassPathEntry::Directory(y)) => x == y,
        (ClassPathEntry::JarFile { path: x, .. }, ClassPathEntry::JarFile { path: y, .. })
        | (ClassPathEntry::JmodFile { path: x, .. }, ClassPathEntry::JmodFile { path: y, .. })
        | (
            ClassPathEntry::JImageFile { path: x, .. },
            ClassPathEntry::JImageFile { path: y, .. },
        ) => x == y,
        (
            ClassPathEntry::NestedDirectory {
                parent_jar: xj,
                prefix: xp,
                ..
            },
            ClassPathEntry::NestedDirectory {
                parent_jar: yj,
                prefix: yp,
                ..
            },
        ) => xj == yj && xp == yp,
        (
            ClassPathEntry::NestedJar {
                parent_jar: xj,
                nested_path: xn,
                ..
            },
            ClassPathEntry::NestedJar {
                parent_jar: yj,
                nested_path: yn,
                ..
            },
        ) => xj == yj && xn == yn,
        _ => false,
    }
}

/// Append `entry` unless this `ClassPath` already reads from that source.
///
/// One physical source can reach a single `ClassPath` twice. The case that
/// mattered: `--jar <pathing-jar>` expands the launch jar's manifest
/// `Class-Path` in `vm-cli`, hands the result to `ClassPath::new`, and
/// `load_jar_data_at_depth` then expands that same manifest again when it
/// opens the launch jar. Jars survived it — that function returns early for an
/// archive already published — but directories were pushed unconditionally, so
/// every directory on a pathing jar's manifest was enumerated TWICE. Spring
/// Boot's `@WithPackageResources` copies each `getResources` hit into a fresh
/// temp root, so the second copy of the same directory failed with
/// `FileAlreadyExistsException` and took the whole SSL/PEM/JKS test cluster
/// with it.
fn push_classpath_entry(entries: &mut Vec<ClassPathEntry>, entry: ClassPathEntry) {
    if entries
        .iter()
        .any(|existing| same_classpath_source(existing, &entry))
    {
        return;
    }
    entries.push(entry);
}

/// Parsed contents of `META-INF/MANIFEST.MF` relevant to JAR loading.
#[derive(Debug, Default)]
pub struct ManifestInfo {
    /// `Main-Class` header (the launcher class).
    pub main_class: Option<String>,
    /// `Start-Class` header (Spring Boot: the actual application class).
    pub start_class: Option<String>,
    /// `Spring-Boot-Classes` header (default: `BOOT-INF/classes/`).
    pub boot_classes: Option<String>,
    /// `Spring-Boot-Lib` header (default: `BOOT-INF/lib/`).
    pub boot_lib: Option<String>,
    /// `Class-Path` header (space-separated relative paths to additional JARs).
    pub class_path: Option<String>,
    /// `Multi-Release: true` header (JEP 238, Java 9+).
    pub multi_release: bool,
    /// T19.H10: full attribute map from the manifest's main section.
    /// Keys are stored verbatim (case-preserving) so callers like
    /// `Class.getPackage().getImplementationVersion()` can pull arbitrary
    /// `Implementation-*` / `Specification-*` headers without losing
    /// whitespace or casing. Only the main section is captured —
    /// per-entry sections (rare; used by signed JARs) are not parsed.
    pub attributes: std::collections::HashMap<String, String>,
}

impl ManifestInfo {
    /// Fold the JAR manifest **main section** into logical lines per the JAR
    /// specification: any physical line beginning with a single leading space
    /// continues the previous line (the line break and continuation marker space
    /// are removed; the remainder is appended verbatim).
    ///
    /// This matters for long `Class-Path` headers (Surefire booter jars split
    /// across many 72-byte lines). The previous `replace("\\n ", "")` approach
    /// incorrectly deleted the continuation marker without appending the
    /// continuation text, corrupting classpath entries.
    fn fold_main_section_lines(text: &str) -> Vec<String> {
        let text = text.replace('\r', "");
        let mut folded: Vec<String> = Vec::new();
        let mut buf = String::new();
        for line in text.split('\n') {
            if line.is_empty() {
                if !buf.is_empty() {
                    folded.push(std::mem::take(&mut buf));
                }
                break;
            }
            if line.starts_with(' ') && !buf.is_empty() {
                buf.push_str(&line[1..]);
            } else {
                if !buf.is_empty() {
                    folded.push(std::mem::take(&mut buf));
                }
                buf.push_str(line);
            }
        }
        if !buf.is_empty() {
            folded.push(buf);
        }
        folded
    }

    /// Decode a single JAR-manifest `Class-Path:` token into a filesystem
    /// path.
    ///
    /// **SECURITY (VULN, low) — trusted-manifest filesystem reach.** This
    /// deliberately honours `file:` URLs, absolute paths, Windows
    /// drive-absolute paths, and `..` escapes, joining only genuinely
    /// relative tokens to `jar_dir`. That means a JAR's manifest can add
    /// *arbitrary* filesystem roots as classpath entries — e.g.
    /// `Class-Path: file:/etc` or `Class-Path: ../../secret` — exactly as
    /// HotSpot's `URLClassPath` does. The manifest is part of the trusted
    /// application bundle (it ships inside the JAR the user chose to run),
    /// so this is the spec-mandated behaviour and is REQUIRED by real
    /// launchers (Surefire booter JARs emit absolute `file:/C:/...` tokens;
    /// fat JARs point at sibling `lib/` dirs). We must NOT break it by
    /// default.
    ///
    /// Defence in depth: every path produced here is still subject to the
    /// downstream canonicalize/`is_safe_*` checks at *read* time, so a
    /// decoded `..` cannot escape the resolved root of whatever entry it
    /// becomes — this function only chooses the roots, it does not grant
    /// raw file access.
    ///
    /// For environments that want to forbid out-of-bundle reach, the
    /// caller [`ManifestInfo::resolve_class_path`] gates roots behind
    /// [`harden_manifest_classpath`] (`CRATONVM_HARDEN_MANIFEST_CLASSPATH`),
    /// restricting resolved roots to descendants of `jar_dir`. Decoding
    /// itself is left permissive so the caller can make that policy
    /// decision against the canonical form.
    fn decode_manifest_classpath_entry(entry: &str, jar_dir: &Path) -> PathBuf {
        let raw = entry.trim();
        // JAR manifests may carry file URLs (e.g. surefire booter jars emit
        // `file:/C:/...` entries). Treat those as absolute paths instead of
        // joining them to the launcher jar directory.
        // `file:` URI paths are percent-decoded by `decode_percent_path`
        // (a full RFC 3986 `%XX` decoder), replacing the prior no-op
        // `replace('%', "%")` + hardcoded `%20/%5B/%5D/%7B/%7D` subset.
        // Downstream reads are canonicalize-checked, so a decoded
        // `..`/separator cannot escape the intended root.
        let from_file_uri = |rest: &str| -> PathBuf {
            let mut out = decode_percent_path(rest);
            if let Some(stripped) = out.strip_prefix("///") {
                out = stripped.to_string();
            } else if let Some(stripped) = out.strip_prefix("//") {
                // file://<host>/... . For local drive paths this is typically
                // file:///C:/...; for malformed host forms preserve the slash.
                out = stripped.to_string();
            } else if let Some(stripped) = out.strip_prefix('/') {
                // file:/C:/... (single slash before drive letter) -> C:/...
                if stripped.as_bytes().get(1) == Some(&b':') {
                    out = stripped.to_string();
                }
            }
            PathBuf::from(out)
        };

        if let Some(rest) = raw.strip_prefix("file:") {
            return from_file_uri(rest);
        }

        let p = Path::new(raw);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
            // Windows drive-absolute path like C:/foo or C:\foo.
            return PathBuf::from(raw);
        }
        jar_dir.join(raw)
    }

    /// Parse a `MANIFEST.MF` file's bytes.
    pub fn parse(data: &[u8]) -> Self {
        let text = String::from_utf8_lossy(data);
        let mut info = ManifestInfo::default();
        // MANIFEST.MF continuation lines (leading space after a newline) are
        // folded into logical lines before key/value parsing — see
        // [`Self::fold_main_section_lines`].
        let joined_lines = Self::fold_main_section_lines(&text);
        // Per the JAR spec the file is split into sections by blank lines;
        // only the *main* section (the leading section before the first
        // blank line) carries the manifest-wide attributes that
        // `Package.getImplementationVersion()` etc. consult. Per-entry
        // sections after the first blank are skipped.
        for line in joined_lines.iter() {
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                // Cap most attributes to 8 KiB to keep a malicious manifest
                // from blowing out memory, but allow a larger window for
                // Class-Path: build tools (Surefire booter JARs in
                // particular) routinely emit 10s of KB when test classpaths
                // include many dependencies.
                let max_len = if key == "Class-Path" {
                    256 * 1024
                } else {
                    8 * 1024
                };
                if value.len() > max_len {
                    continue;
                }
                info.attributes.insert(key.to_string(), value.to_string());
                match key {
                    "Main-Class" => info.main_class = Some(value.to_string()),
                    "Start-Class" => info.start_class = Some(value.to_string()),
                    "Spring-Boot-Classes" => info.boot_classes = Some(value.to_string()),
                    "Spring-Boot-Lib" => info.boot_lib = Some(value.to_string()),
                    "Class-Path" => info.class_path = Some(value.to_string()),
                    "Multi-Release" => info.multi_release = value.eq_ignore_ascii_case("true"),
                    _ => {}
                }
            }
        }
        info
    }

    /// Resolve `Class-Path` manifest entries relative to a JAR file's parent directory.
    ///
    /// Per the JAR specification, `Class-Path` entries are space-separated URIs
    /// resolved relative to the JAR's location (not the current working directory).
    pub fn resolve_class_path(&self, jar_path: &Path) -> Vec<String> {
        let jar_dir = jar_path.parent().unwrap_or(Path::new("."));
        match &self.class_path {
            Some(cp) => cp
                .split_whitespace()
                .filter_map(|entry| {
                    let resolved = Self::decode_manifest_classpath_entry(entry, jar_dir);
                    // VULN (low) hardening opt-in: when
                    // `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set, drop any
                    // manifest `Class-Path` root that escapes the JAR's own
                    // directory (absolute roots, `file:/etc`, `..` escapes).
                    // Default-off preserves HotSpot's trusted-manifest
                    // behaviour; see `decode_manifest_classpath_entry`.
                    if harden_manifest_classpath() && !path_is_within(&resolved, jar_dir) {
                        debug!(
                            "manifest Class-Path entry {:?} resolves outside the \
                             JAR directory {:?}; dropped \
                             (CRATONVM_HARDEN_MANIFEST_CLASSPATH)",
                            entry, jar_dir
                        );
                        return None;
                    }
                    Some(resolved.to_string_lossy().into_owned())
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns true if this JAR appears to be a Spring Boot fat JAR.
    pub fn is_spring_boot(&self) -> bool {
        self.start_class.is_some() || self.boot_classes.is_some() || self.boot_lib.is_some()
    }
}

/// Maximum Java version supported by this VM. Multi-release JARs check
/// `META-INF/versions/{N}/` entries descending from this version down to 9.
const MULTI_RELEASE_MAX_VERSION: u32 = 25;

/// Compile-time JVM feature version this build implements. `Runtime.version()
/// .feature()` returns this value to user code. Used to bound the
/// multi-release JAR shadow-search range so an attacker can't smuggle a
/// `META-INF/versions/<future-N>/java/lang/String.class` past the JVM by
/// targeting a version we haven't been compiled to honor.
const JVM_FEATURE_VERSION: u32 = MULTI_RELEASE_MAX_VERSION;

/// Audit-fix #2: zip-bomb cap. Maximum uncompressed bytes we will
/// `Vec::with_capacity` for a single ZIP entry. The declared `size` field
/// in the central directory is attacker-controlled and can be up to
/// `u64::MAX`; without this clamp a malicious JAR triggers a multi-GiB
/// allocation before any decompression even starts. 512 MiB is well
/// above any legitimate single class file or resource and matches the
/// upper bound JDK 21+'s `ZipInputStream` uses internally.
pub(crate) const MAX_UNCOMPRESSED_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// Directory prefix under which a JMOD stores its class files and the
/// resources that ship inside the module (`classes/java/lang/Object.class`,
/// `classes/META-INF/services/...`). A JMOD's other top-level directories
/// (`lib/`, `bin/`, `conf/`, `include/`, `legal/`, `man/`) are build-time
/// artefacts that the runtime reads from `$JAVA_HOME`, not from the module,
/// which is why only this subtree is indexed for class lookup.
const JMOD_CLASSES_PREFIX: &str = "classes/";

/// Clamp a ZIP entry's declared `size()` to [`MAX_UNCOMPRESSED_ENTRY_BYTES`]
/// for `Vec::with_capacity`. Returns the clamped capacity as `usize`.
#[inline]
fn safe_with_capacity(declared_size: u64) -> usize {
    declared_size.min(MAX_UNCOMPRESSED_ENTRY_BYTES) as usize
}

/// V2 (decompression-bomb DoS): read a ZIP entry's *streaming* inflate
/// output, bounding the **actual** inflated size — not just the declared
/// pre-allocation [`safe_with_capacity`] guards.
///
/// The `zip` reader streams deflate output up to the entry's declared
/// uncompressed size, which is attacker-controlled; a highly compressible
/// payload (~1000:1) lets a tiny compressed entry inflate to GBs and
/// `read_to_end` grows the buffer to that full size regardless of the
/// 512 MiB capacity clamp. Here we read with `Read::take(cap + 1)` and
/// reject (with an `io::Error`) the instant the running inflated total
/// exceeds [`MAX_UNCOMPRESSED_ENTRY_BYTES`], so the buffer can never grow
/// past the cap by more than one byte before the abort.
///
/// `declared_size` is the central-directory `size()` used only to seed the
/// initial `Vec` capacity (already clamped via [`safe_with_capacity`]); the
/// cap, not the declared size, is the authoritative bound on what we read.
fn read_entry_capped<R: Read>(reader: &mut R, declared_size: u64) -> std::io::Result<Vec<u8>> {
    let cap = MAX_UNCOMPRESSED_ENTRY_BYTES;
    let mut data = Vec::with_capacity(safe_with_capacity(declared_size));
    // Read at most `cap + 1` bytes: hitting `cap + 1` proves the real
    // inflated stream exceeds the cap (a lying-small `size()` bomb).
    let read = reader.take(cap + 1).read_to_end(&mut data)?;
    if read as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zip entry inflated size exceeds MAX_UNCOMPRESSED_ENTRY_BYTES (decompression bomb)",
        ));
    }
    Ok(data)
}

/// Audit-fix #3: reject zip-slip-style entry names so attacker-controlled
/// ZIP entries cannot poison in-memory resource caches. Validates against
/// path-escape, absolute paths, NUL bytes, Windows drive letters, and
/// the alternate Windows separator. Returns `false` for any name we
/// refuse to cache.
pub(crate) fn is_safe_entry_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('\0') {
        return false;
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    if name.contains(':') {
        return false; // Windows drive letters: `C:\...`
    }
    if name.contains('\\') {
        // ZIP spec mandates `/` as separator; backslash is suspicious
        // and on Windows would create an alternate path interpretation.
        return false;
    }
    // Reject any path component equal to `..` (path escape) or `.`
    // (no-op components that obscure traversal).
    for component in name.split('/') {
        if component == ".." || component == "." {
            return false;
        }
    }
    true
}

/// VULN (low) hardening helper: returns `true` when `candidate` resolves
/// to a location at or beneath `base`, using a purely *lexical*
/// normalization (no filesystem access — `resolve_class_path` is a pure
/// resolver and the paths may not exist yet).
///
/// Both paths are first lexically normalized: `.` components are dropped
/// and `..` components pop the previous normal component (a leading or
/// un-poppable `..` is preserved, which keeps the candidate firmly
/// *outside* any relative base). The candidate is then required to share
/// `base`'s full component prefix. A `base` that normalizes to empty
/// (e.g. `"."`) admits any relative candidate but still rejects absolute
/// ones and ones that climb above the current directory.
///
/// This is intentionally conservative: it only *adds* a restriction when
/// `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set, so a false negative
/// (dropping a borderline path) is preferable to admitting an escape.
fn path_is_within(candidate: &Path, base: &Path) -> bool {
    use std::path::Component;

    // Lexically normalize a path into a component vector. Returns `None`
    // if a `..` would climb above the root the path is anchored to, which
    // for our purposes means "treat as outside" (the caller drops it).
    fn normalize(p: &Path) -> Vec<Component<'_>> {
        let mut stack: Vec<Component<'_>> = Vec::new();
        for comp in p.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    match stack.last() {
                        // Pop a real directory component.
                        Some(Component::Normal(_)) => {
                            stack.pop();
                        }
                        // A `..` at/above an anchor (root/prefix) or after
                        // another preserved `..` is kept verbatim so the
                        // prefix check below can reject the escape.
                        _ => stack.push(comp),
                    }
                }
                other => stack.push(other),
            }
        }
        stack
    }

    let base_norm = normalize(base);
    let cand_norm = normalize(candidate);

    // An absolute candidate against a relative base (or vice-versa) can
    // never be "within": their root/prefix components won't match, so the
    // prefix check below handles it. The candidate must be at least as
    // long as the base and agree on every base component.
    if cand_norm.len() < base_norm.len() {
        return false;
    }
    // Special case: a base that normalizes to empty (e.g. `"."`, used when
    // the JAR has no parent dir) means "the JAR's own relative location".
    // A relative candidate is admissible, but an *absolute* one (a
    // root/prefix first component, e.g. manifest `file:/etc`) escapes it
    // and must be rejected.
    if base_norm.is_empty() {
        if let Some(first) = cand_norm.first() {
            if matches!(first, Component::Prefix(_) | Component::RootDir) {
                return false;
            }
        }
    }
    base_norm
        .iter()
        .zip(cand_norm.iter())
        .all(|(b, c)| b == c)
        // A normalized candidate that still contains a leading `..` escaped
        // above its anchor is never "within" a base lacking that same `..`.
        && !cand_norm
            .iter()
            .skip(base_norm.len())
            .any(|c| matches!(c, Component::ParentDir))
}

/// Percent-decode an RFC 3986 `file:` URI path.
///
/// Each `%XX` escape (two upper- or lower-case hex digits) is decoded to the
/// corresponding byte; the resulting byte sequence is then interpreted as
/// UTF-8 (lossily, so a malformed/non-UTF-8 manifest cannot panic us). Any
/// `%` not followed by two valid hex digits is preserved verbatim, so a
/// literal `%` in a path is left intact rather than dropped.
///
/// This replaces the earlier `replace('%', "%")` no-op plus a hardcoded
/// `%20/%5B/%5D/%7B/%7D` subset, which mangled any other escape (e.g.
/// `%2520`, accented characters, `%28`/`%29`). Decoding is intentionally
/// unconditional over the whole string; the surrounding `from_file_uri`
/// logic still strips the leading `file:` slashes afterward, and downstream
/// filesystem reads are canonicalize-vs-root checked, so a decoded separator
/// or `..` cannot be used to escape the intended classpath root.
pub(crate) fn decode_percent_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Need two hex digits following the '%'.
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reject resource names that could escape the classpath root or be
/// reinterpreted as a host path. This mirrors the input filter applied by
/// [`ClassPath::find_class`] so that `getResource`/`getResourceAsStream`
/// and the `find_all_resource_*` enumeration paths reject the same hostile
/// inputs: `..` traversal, NUL bytes, leading `/`, any Windows separator
/// `\\`, Windows drive letters (`:`), and `./` / `.\\`
/// current-dir references. The caller is expected to have already stripped
/// any leading `/` (HotSpot strips one leading slash from resource names);
/// a *remaining* leading `/` after that strip is still rejected here.
///
/// The downstream canonicalize-vs-root check in each resource path remains
/// the authoritative backstop; this is a cheap pre-filter that keeps the
/// resource and class entry points symmetric.
pub(crate) fn is_safe_resource_name(name: &str) -> bool {
    !(name.contains("..")
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':') // Windows drive letters (C:)
        || name.contains("./") // current-dir references
        || name.contains(".\\")) // Windows current-dir references
}

fn is_safe_class_name(name: &str) -> bool {
    !(name.contains("..")
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':') // Windows drive letters (C:)
        || name.contains("./")) // current-dir references
}

fn is_directory_resolvable_resource_name(name: &str) -> bool {
    // HotSpot normalizes "." and ".." when a URLClassLoader probes a directory
    // classpath root. Let directory lookups reach the canonical root check below,
    // but keep archive/JMOD/JRT lookups on the stricter literal resource filter.
    !(name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':'))
}

fn simple_resource_glob(name: &str) -> Option<(&str, &str)> {
    if !name.contains('*') && !name.contains('?') {
        return None;
    }
    let split = name.rfind('/').map(|idx| idx + 1).unwrap_or(0);
    let (prefix, pattern) = name.split_at(split);
    if prefix.contains('*')
        || prefix.contains('?')
        || (!prefix.is_empty() && !is_safe_resource_name(prefix))
        || pattern.is_empty()
    {
        return None;
    }
    Some((prefix, pattern))
}

fn glob_segment_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let (mut pi, mut ci) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_match = 0usize;

    while ci < candidate.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == candidate[ci]) {
            pi += 1;
            ci += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_match = ci;
        } else if let Some(star_idx) = star {
            pi = star_idx + 1;
            star_match += 1;
            ci = star_match;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match one candidate against an **already-parsed** glob.
///
/// Split out of [`resource_name_matches_simple_glob`] so a caller that tests
/// many candidates against one glob parses the glob once instead of once per
/// candidate. See [`ClassPath::matching_resource_entry_names`].
#[inline]
fn resource_name_matches_parsed_glob(prefix: &str, pattern: &str, candidate: &str) -> bool {
    let Some(tail) = candidate.strip_prefix(prefix) else {
        return false;
    };
    !tail.is_empty() && !tail.contains('/') && glob_segment_matches(pattern, tail)
}

fn resource_name_matches_simple_glob(glob: &str, candidate: &str) -> bool {
    let Some((prefix, pattern)) = simple_resource_glob(glob) else {
        return false;
    };
    resource_name_matches_parsed_glob(prefix, pattern, candidate)
}

/// Parse a `<jar-path>!/<prefix>/` specification of the form produced by
/// stripping `file:`/`jar:` off a `jar:file:/.../foo.jar!/some/dir/` URL.
/// Returns `Some((jar_path, prefix_with_trailing_slash))` if the input
/// matches; `None` otherwise. The prefix is guaranteed to end with `/`,
/// matching the convention used by [`ClassPathEntry::NestedDirectory`].
///
/// Examples:
///   "C:/dacapo.jar!/harness/" -> Some(("C:/dacapo.jar", "harness/"))
///   "/lib/foo.jar!/META-INF/"  -> Some(("/lib/foo.jar", "META-INF/"))
///   "/apps/app.war!/WEB-INF/classes/" -> Some(("/apps/app.war", "WEB-INF/classes/"))
///   "C:/x.jar"                 -> None  (no `!/` separator)
///   "C:/x.jar!/"               -> None  (empty prefix → equivalent to root)
fn parse_jar_subdir_spec(spec: &str) -> Option<(String, String)> {
    let idx = spec.find("!/")?;
    let jar_part = &spec[..idx];
    let prefix_part = &spec[idx + 2..];
    // Spring Boot's `JarUrl.create(file, "BOOT-INF/classes/")` always marks
    // the root of the nested location with a SECOND, trailing `!/` after the
    // prefix itself (`jar:nested:<jar>/!BOOT-INF/classes/!/`), which
    // `extract_url_path` passes through unchanged. Strip that trailing
    // marker so the prefix used to match zip entries is the real one
    // (`BOOT-INF/classes/`) rather than the literal, unmatchable
    // `BOOT-INF/classes/!/`.
    let prefix_part = prefix_part.strip_suffix("!/").unwrap_or(prefix_part);
    if jar_part.is_empty() || prefix_part.is_empty() {
        return None;
    }
    // The outer file's extension is deliberately not part of the grammar.
    // `URLClassLoader` treats a `jar:` URL as an archive independently of its
    // name, and application servers routinely use `.war`, `.ear`, and `.par`
    // files. The caller verifies that the named file exists and that it is a
    // readable ZIP before it contributes an entry, so accepting the syntactic
    // `!/` form here cannot turn an arbitrary resource miss into a classpath
    // entry.
    let prefix = if prefix_part.ends_with('/') {
        prefix_part.to_string()
    } else {
        format!("{prefix_part}/")
    };
    // Defence-in-depth: prefix must be a sane forward-slash-only path.
    if prefix.contains('\\') || prefix.contains('\0') || prefix.contains("..") {
        return None;
    }
    Some((jar_part.to_string(), prefix))
}

impl ClassPath {
    /// Round 5 audit fix (MED): cached [`fs::canonicalize`] wrapper.
    ///
    /// The classpath-traversal symlink check (audit-fix #5) canonicalizes
    /// both the directory root AND the resolved class file on every probe.
    /// On a Spring app with 15k classes × 3 directory entries that's
    /// ~90k canonicalize syscalls per cold start. The cache makes
    /// repeated lookups of the same `PathBuf` zero-syscall.
    ///
    /// Errors are NOT cached — a transient `ENOENT` should not be
    /// remembered as a fail-closed verdict for the rest of the process.
    /// Canonicalize a **classpath root** — the `dir` of a
    /// [`ClassPathEntry::Directory`] or the `path` of a
    /// [`ClassPathEntry::JarFile`] — through an unbounded memo.
    ///
    /// This exists because the bounded [`Self::canonicalize_cached`] FIFO
    /// cannot serve a scan whose working set IS the classpath. Every
    /// `getResources` probe walks every entry and canonicalizes each root it
    /// touches; once the classpath is larger than [`CANONICALIZE_CACHE_CAP`]
    /// that cyclic access pattern evicts each root exactly before it is next
    /// needed, so the hit rate collapses to zero and every probe re-runs
    /// `realpath` on every root. Measured on the Quarkus full-reactor harness
    /// (4230 entries, 1676 of them directories): 6.79M `readlink` calls for a
    /// single test class — half of a 110 s run — against 66.5K on HotSpot for
    /// the same work.
    ///
    /// The cap was added (Round 7 audit, HIGH #4) to stop an unbounded map
    /// growing on arbitrary *probe* paths, and that concern is untouched:
    /// probe paths still go through [`Self::canonicalize_cached`]. Roots are a
    /// different population — the key set here is bounded by `self.entries`,
    /// i.e. by the classpath the launcher was already given — so this map
    /// cannot grow past a size the process has already paid for. It is
    /// cleared alongside the bounded cache whenever a root is retracted.
    ///
    /// Errors are NOT cached, for the same reason as `canonicalize_cached`.
    fn canonicalize_root(&self, path: &Path) -> std::io::Result<PathBuf> {
        {
            let guard = self.root_canonical_cache.lock();
            if let Some(canon) = guard.get(path) {
                return Ok(canon.clone());
            }
        }
        let canon = fs::canonicalize(path)?;
        self.root_canonical_cache
            .lock()
            .insert(path.to_path_buf(), canon.clone());
        Ok(canon)
    }

    fn canonicalize_cached(&self, path: &Path) -> std::io::Result<PathBuf> {
        {
            let guard = self.canonicalize_cache.lock();
            if let Some(canon) = guard.paths.get(path) {
                return Ok(canon.clone());
            }
        }
        let canon = fs::canonicalize(path)?;
        // Insert under the lock; tolerate the race where another thread
        // inserted the same entry between our read and write — they
        // produce identical values so either wins. Round 7 audit fix
        // (HIGH #4): inserts now go through the bounded
        // [`CanonicalizeCache::insert`] which enforces the FIFO cap.
        self.canonicalize_cache
            .lock()
            .insert(path.to_path_buf(), canon.clone());
        Ok(canon)
    }

    fn matching_directory_resource_paths(dir: &Path, name: &str) -> Vec<PathBuf> {
        let Some((prefix, pattern)) = simple_resource_glob(name) else {
            return Vec::new();
        };
        let base = dir.join(prefix);
        let mut matches = Vec::new();
        let Ok(entries) = fs::read_dir(base) else {
            return matches;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if glob_segment_matches(pattern, file_name) {
                matches.push(entry.path());
            }
        }
        matches.sort();
        matches
    }

    /// Every entry name matching `glob`, sorted.
    ///
    /// PERF (2026-07-26 classpath-scan audit): the glob is parsed **once**.
    /// This used to call `resource_name_matches_simple_glob` per candidate,
    /// which re-ran `simple_resource_glob` — two `contains` scans, an `rfind`,
    /// and an `is_safe_resource_name` prefix check that is itself six more
    /// substring scans — for every name in the archive, on a loop-invariant
    /// string. The caller is `find_all_resource_urls` on the glob path, which
    /// visits *every* entry name of *every* archive on the classpath; the
    /// hottest reachable caller is `ClassLoader.getDefinedPackage`
    /// (`Class.getPackage()` → `"<pkg>/*.class"`), so this ran O(total entry
    /// names across all jars) times per `getPackage()` call.
    ///
    /// An unparseable glob previously made the per-candidate predicate return
    /// `false` for everything, yielding an empty vec; the early return here is
    /// the same answer without the walk.
    fn matching_resource_entry_names<'a, I>(names: I, glob: &str) -> Vec<String>
    where
        I: IntoIterator<Item = &'a String>,
    {
        let Some((prefix, pattern)) = simple_resource_glob(glob) else {
            return Vec::new();
        };
        let mut matches: Vec<String> = names
            .into_iter()
            .filter(|name| resource_name_matches_parsed_glob(prefix, pattern, name))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    fn checked_directory_resource_canonical(
        &self,
        dir: &Path,
        full_path: &Path,
        name: &str,
    ) -> Option<PathBuf> {
        let canon_dir = match self.canonicalize_root(dir) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    "Refusing to read resource {name}: cannot canonicalize classpath root {}: {e}",
                    dir.display()
                );
                return None;
            }
        };
        let canon_path = match self.canonicalize_cached(full_path) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    "Refusing to read resource {name}: cannot canonicalize resolved path {}: {e}",
                    full_path.display()
                );
                return None;
            }
        };
        if !canon_path.starts_with(&canon_dir) {
            debug!(
                "Resource path traversal blocked: {} escapes {}",
                canon_path.display(),
                canon_dir.display()
            );
            return None;
        }
        Some(canon_path)
    }

    fn directory_resource_url_from_canonical(canon_path: &Path) -> String {
        let p = canon_path.to_string_lossy().replace('\\', "/");
        let p = p.strip_prefix("//?/").unwrap_or(&p);
        let p = p.trim_start_matches('/');
        if canon_path.is_dir() && !p.ends_with('/') {
            format!("file:/{}/", encode_path_for_url(p))
        } else {
            format!("file:/{}", encode_path_for_url(p))
        }
    }

    /// Audit-fix #7: build (once) the set of `META-INF/versions/<N>/`
    /// numbers actually present in `archive`. Subsequent multi-release
    /// lookups iterate only over this set, skipping the per-class
    /// `by_name` probes the previous implementation paid for every
    /// absent version.
    fn ensure_versions_cache(
        archive: &Mutex<SharedArchive>,
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
    ) -> Arc<BTreeSet<u32>> {
        // PERF: hand back a shared `Arc` so the fast path (every
        // multi-release class lookup after the first) is an atomic
        // reference-count bump, not a heap-allocating deep `BTreeSet`
        // clone. The cached set is immutable after construction, so all
        // callers can safely share it; they only iterate over it.
        {
            let guard = versions_cache.lock();
            if let Some(cache) = guard.as_ref() {
                return Arc::clone(cache);
            }
        }
        // Build by scanning the central directory once.
        let mut set: BTreeSet<u32> = BTreeSet::new();
        {
            let mut archive_guard = archive.lock();
            for i in 0..archive_guard.len() {
                let name = match archive_guard.by_index_raw(i) {
                    Ok(e) => e.name().to_string(),
                    Err(_) => continue,
                };
                if let Some(rest) = name.strip_prefix("META-INF/versions/") {
                    if let Some(slash_idx) = rest.find('/') {
                        let ver_str = &rest[..slash_idx];
                        if let Ok(ver) = ver_str.parse::<u32>() {
                            set.insert(ver);
                        }
                    }
                }
            }
        }
        // PERF: store the set behind an `Arc` once; the build path no
        // longer pays an extra `set.clone()` — the `Arc` and its return
        // value share the single heap allocation.
        let shared = Arc::new(set);
        let mut guard = versions_cache.lock();
        // Tolerate the benign race where another thread built the cache
        // first; either set is identical (pure function of the archive),
        // so adopt whichever is already published to keep all callers on
        // one shared allocation.
        if let Some(existing) = guard.as_ref() {
            return Arc::clone(existing);
        }
        *guard = Some(Arc::clone(&shared));
        shared
    }

    /// Look up an entry in a multi-release JAR archive.
    ///
    /// Audit-fix #4 (multi-release shadow attack): the version-search
    /// upper bound is now [`JVM_FEATURE_VERSION`] (the actual feature
    /// version this VM implements), NOT a fixed-25 constant. A JAR
    /// that ships a `META-INF/versions/99/java/lang/String.class` can
    /// no longer shadow the base entry when the running JVM doesn't
    /// claim version 99.
    ///
    /// Audit-fix #7 (perf): we consult the cached per-archive
    /// versions set (built lazily on first lookup) and only probe
    /// versions actually present in the archive, eliminating the
    /// per-class 17 `by_name` round-trips.
    fn find_in_multi_release_archive(
        archive: &Mutex<SharedArchive>,
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: Option<&FxHashSet<String>>,
        name: &str,
    ) -> Option<Vec<u8>> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        // Search descending so the highest-supported version wins,
        // but only over versions that ACTUALLY exist in this archive
        // AND that are ≤ this runtime's feature version.
        let max = JVM_FEATURE_VERSION;
        for &ver in present.range(9..=max).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.is_some_and(|idx| !idx.contains(&versioned)) {
                continue;
            }
            if let Some(data) = Self::find_in_archive(archive, &versioned) {
                return Some(data);
            }
        }
        // Fall back to base entry.
        if entry_index.is_some_and(|idx| !idx.contains(name)) {
            return None;
        }
        Self::find_in_archive(archive, name)
    }

    fn find_shared_in_multi_release_archive(
        archive: &Mutex<SharedArchive>,
        backing: &ArchiveBacking,
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<SharedBytes> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        for &ver in present.range(9..=JVM_FEATURE_VERSION).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.contains(&versioned) {
                return Self::find_shared_in_archive(archive, backing, &versioned);
            }
        }
        if !entry_index.contains(name) {
            return None;
        }
        Self::find_shared_in_archive(archive, backing, name)
    }

    /// Return the physical entry selected for a multi-release lookup.
    ///
    /// Resource URLs must name this physical entry, not the logical base name:
    /// callers such as `URL.openStream()` re-open the URL and therefore cannot
    /// recover a version choice made only while probing the archive.  Returning
    /// `p/res.txt` for an effective `META-INF/versions/17/p/res.txt` made a
    /// multi-release class load use the right bytes while its corresponding
    /// resource URL exposed the base bytes.
    fn multi_release_entry_name(
        archive: &Mutex<SharedArchive>,
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<String> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        for &ver in present.range(9..=JVM_FEATURE_VERSION).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.contains(&versioned) {
                return Some(versioned);
            }
        }
        entry_index.contains(name).then(|| name.to_string())
    }

    fn build_archive_entry_index(archive: &mut SharedArchive) -> FxHashSet<String> {
        let mut index = FxHashSet::default();
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index_raw(i) {
                index.insert(entry.name().to_string());
            }
        }
        index
    }

    #[inline]
    fn find_in_indexed_archive(
        archive: &Mutex<SharedArchive>,
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<Vec<u8>> {
        if !entry_index.contains(name) {
            return None;
        }
        Self::find_in_archive(archive, name)
    }

    #[inline]
    fn find_shared_in_indexed_archive(
        archive: &Mutex<SharedArchive>,
        backing: &ArchiveBacking,
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<SharedBytes> {
        if !entry_index.contains(name) {
            return None;
        }
        Self::find_shared_in_archive(archive, backing, name)
    }

    /// Create a classpath from a list of path strings.
    ///
    /// Each entry can be a directory or an archive file. Non-existent paths and
    /// invalid archive files are silently skipped with a debug log message.
    ///
    /// JAR files are automatically scanned for Spring Boot fat JAR structure:
    /// if `BOOT-INF/classes/` or `BOOT-INF/lib/` are detected (via MANIFEST.MF
    /// or directory probing), nested entries are extracted and added to the
    /// classpath.
    ///
    /// **Wildcard expansion (Java CLI parity).** An entry ending in `/*` or
    /// `\*` (or that is exactly `*`) is expanded to every `*.jar` / `*.JAR`
    /// directly inside the parent directory. This matches HotSpot's
    /// `-cp lib/*` syntax used by Elasticsearch, Cassandra, and most
    /// hand-rolled launchers. Without expansion, the literal entry
    /// `lib/*` is treated as a non-existent path and silently dropped —
    /// producing an empty classpath and breaking `ServiceLoader`
    /// (`META-INF/services/...`) discovery for every dependency.
    pub fn new(paths: &[String]) -> Self {
        let __diag_start = loader_flags().dbg_classpath.then(std::time::Instant::now);
        let __diag_npaths = paths.len();
        let mut entries = Vec::new();
        for raw in paths {
            Self::process_classpath_token(raw, &mut entries, 0);
        }
        if let Some(t0) = __diag_start {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            eprintln!(
                "[CP] ClassPath::new call#{n} npaths={__diag_npaths} nentries={} elapsed={:?}",
                entries.len(),
                t0.elapsed()
            );
        }
        Self {
            entries,
            canonicalize_cache: Mutex::new(CanonicalizeCache::new()),
            root_canonical_cache: Mutex::new(HashMap::new()),
            dynamic_specs: HashMap::new(),
            static_len: None,
            dir_index: Mutex::new(HashMap::new()),
        }
    }

    /// Expand Java launcher classpath wildcards into their concrete JAR paths.
    ///
    /// This is also used when publishing `java.class.path`: HotSpot expands
    /// `lib/*` before exposing that property, and in-process javac consumes the
    /// property directly rather than consulting CratonVM's class loader.
    pub fn expand_classpath_entries(paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .flat_map(|raw| Self::expand_classpath_wildcard(raw))
            .collect()
    }

    /// Expand a single classpath token, honouring Java's `dir/*` wildcard.
    ///
    /// HotSpot's `java -cp lib/*` expands to every JAR (`*.jar` / `*.JAR`)
    /// directly inside `lib/` — non-recursive, ignoring sub-directories.
    /// Non-wildcard entries pass through unchanged. The expanded order is
    /// sorted so behaviour is deterministic across runs and platforms;
    /// `ServiceLoader` provider ordering is observable for SPIs that
    /// register the same interface in multiple jars, so we want
    /// byte-identical lists across runs.
    ///
    /// This is the entry point used by both [`ClassPath::new`] and
    /// [`ClassPath::add_path`] so dynamic loaders that synthesise
    /// `someLib/*` strings benefit equally.
    fn expand_classpath_wildcard(raw: &str) -> Vec<String> {
        // Accept `dir/*`, `dir\*`, and the bare token `*` (current dir).
        // Patterns with an embedded `*` mid-path are NOT supported, in
        // line with HotSpot — pass them through literally and let the
        // load fail with a "skipping non-existent classpath entry"
        // debug line.
        let (dir_part, matched) = if raw == "*" {
            (".".to_string(), true)
        } else if let Some(parent) = raw.strip_suffix("/*") {
            (parent.to_string(), true)
        } else if let Some(parent) = raw.strip_suffix("\\*") {
            (parent.to_string(), true)
        } else {
            (String::new(), false)
        };
        if !matched {
            return vec![raw.to_string()];
        }
        if dir_part.contains('*') {
            debug!("Classpath wildcard with embedded '*' not supported: {raw}");
            return vec![raw.to_string()];
        }

        let dir = PathBuf::from(if dir_part.is_empty() {
            "."
        } else {
            dir_part.as_str()
        });
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "Classpath wildcard {raw}: cannot read directory {}: {e}",
                    dir.display()
                );
                // Yield zero entries — matches `java -cp missing/*`,
                // which silently expands to the empty set rather than
                // erroring.
                return Vec::new();
            }
        };
        let mut jars: Vec<String> = Vec::new();
        for ent in read.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let is_jar = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("jar"));
            if !is_jar {
                continue;
            }
            jars.push(p.to_string_lossy().into_owned());
        }
        jars.sort();
        if jars.is_empty() {
            debug!(
                "Classpath wildcard {raw}: directory {} contains no JARs",
                dir.display()
            );
        } else {
            debug!(
                "Classpath wildcard {raw} -> {} JAR(s) under {}",
                jars.len(),
                dir.display()
            );
        }
        jars
    }

    /// Heuristic: does `path` look like a JDK 9+ jimage file?
    ///
    /// The standard location is `$JAVA_HOME/lib/modules` — a file named
    /// `modules` (no extension) under a `lib` directory. We accept any
    /// file named `modules` that exists and is larger than the jimage
    /// header; the real magic-number check happens inside
    /// [`JImageReader::open`] so callers still get a clean error if the
    /// file is not actually a jimage.
    fn is_likely_jimage(path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if path.extension().is_some() {
            return false; // anything with an extension is not `modules`
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some("modules") => {}
            _ => return false,
        }
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.len() >= 28, // header size
            Err(_) => false,
        }
    }

    /// Resolve a single classpath token (wildcard, jar-subdir spec,
    /// directory, jmod, jimage, or plain jar/file) into `entries`. Shared by
    /// [`ClassPath::new`]'s top-level path list (`depth=0`) and by a plain
    /// jar's own manifest `Class-Path:` expansion (`depth=parent_depth+1`,
    /// see [`ClassPath::load_jar_data_at_depth`]).
    fn process_classpath_token(raw: &str, entries: &mut Vec<ClassPathEntry>, depth: u32) {
        for p in Self::expand_classpath_wildcard(raw) {
            // A URLClassLoader rooted at an archive subdirectory hands us
            // `<archive>!/<prefix>/`. Handle it before interpreting the
            // token as a filesystem path so `.war`/`.ear` archives retain
            // their internal root for both class and resource lookup.
            if let Some((archive, prefix)) = parse_jar_subdir_spec(&p) {
                let path = PathBuf::from(archive);
                if path.exists() {
                    match read_archive_for_classpath(&path) {
                        Ok(data) => {
                            if let Some(entry) = Self::build_nested_jar_from_jar(
                                &path,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&path, data, &prefix))
                            {
                                push_classpath_entry(entries, entry);
                            }
                        }
                        Err(e) => {
                            debug!(
                                "Failed to read nested classpath archive {}: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    debug!(
                        "Skipping missing nested classpath archive: {}",
                        path.display()
                    );
                }
                continue;
            }
            let path = PathBuf::from(&p);
            if path.is_dir() {
                push_classpath_entry(entries, ClassPathEntry::Directory(path));
            } else if path.extension().is_some_and(|ext| ext == "jmod") && path.exists() {
                match Self::load_jmod(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read JMOD {}: {e}", path.display()),
                }
            } else if Self::is_likely_jimage(&path) {
                // NEW-5: the JDK 9+ runtime image at `$JAVA_HOME/lib/modules`
                // is a single jimage blob. Detection is by file name
                // (`modules` under any directory) plus a magic-number
                // verification inside `load_jimage`. The explicit file
                // name check lets users write `-cp /path/to/lib/modules`
                // without having to pass a special flag.
                match Self::load_jimage(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read jimage {}: {e}", path.display()),
                }
            } else if path.is_file() {
                // A URLClassLoader treats every existing file URL as an
                // archive candidate, regardless of its suffix. Hibernate's
                // packaged-bootstrap tests use `.par`/`.war`/`.ear` ZIPs;
                // accepting only `.jar` here made the per-loader resolver
                // silently lose their resources while `add_path` accepted
                // the same URLs. `load_jar_data` fails closed for ordinary
                // non-archive files, so the broader admission is safe.
                match read_archive_for_classpath(&path) {
                    Ok(data) => Self::load_jar_data_at_depth(&path, data, entries, depth),
                    Err(e) => debug!("Failed to read classpath archive {}: {e}", path.display()),
                }
            } else {
                debug!("Skipping non-existent classpath entry: {p}");
            }
        }
    }

    /// Load a JAR from raw bytes, auto-detecting fat JAR structure.
    fn load_jar_data(path: &Path, data: Vec<u8>, entries: &mut Vec<ClassPathEntry>) {
        Self::load_jar_data_at_depth(path, ArchiveBacking::from_vec(data), entries, 0);
    }

    /// Compatibility wrapper for legacy callers that still supply a manifest
    /// expansion set. `load_jar_data_at_depth` now owns recursive manifest
    /// processing and its depth guard, so the set is no longer needed here.
    fn load_jar_data_with_manifest_class_path(
        path: &Path,
        data: Vec<u8>,
        entries: &mut Vec<ClassPathEntry>,
        _expanded_manifest_jars: &mut FxHashSet<PathBuf>,
    ) {
        Self::load_jar_data_at_depth(path, ArchiveBacking::from_vec(data), entries, 0);
    }

    /// Manifest `Class-Path:` expansion recursion cap — guards against a
    /// cyclic chain (jar A's Class-Path names jar B, whose Class-Path names
    /// A again) recursing forever. Real classpaths are never nested this
    /// deep in practice.
    const MAX_CLASS_PATH_MANIFEST_DEPTH: u32 = 16;

    /// [`load_jar_data`], plus honouring a plain (non-fat) JAR's own
    /// manifest `Class-Path:` attribute.
    ///
    /// Per the JAR spec, `Class-Path:` is honoured for ANY jar a
    /// `URLClassLoader`/launcher opens, not just the process's initial
    /// classpath — but until this fix, `vm-cli`'s one-time `--jar <path>`
    /// bootstrap handling (`resolve_class_path`, `vm-cli/src/main.rs`) was
    /// the ONLY caller that expanded it. A "pathing JAR" (a jar containing
    /// no classes, only a manifest `Class-Path:` pointing at the real
    /// dependency jars/dirs — used by e.g. the spring-boot-suite-runner to
    /// dodge Windows' command-line length limit) loaded through this
    /// general-purpose `ClassPath::new` path — the one every ad hoc
    /// `URLClassLoader` (`ModifiedClassPathClassLoader`, custom test
    /// classloaders, etc.) is built from — therefore silently resolved to
    /// an unusable, effectively empty classpath: the pathing jar itself
    /// has no class entries, and its `Class-Path:` was never followed.
    fn load_jar_data_at_depth(
        path: &Path,
        data: ArchiveBacking,
        entries: &mut Vec<ClassPathEntry>,
        depth: u32,
    ) {
        // Manifest Class-Path chains can form cycles. Avoid mapping and
        // indexing an archive a second time once any entry from that archive
        // has already been published into this ClassPath.
        if entries.iter().any(|entry| match entry {
            ClassPathEntry::JarFile { path: loaded, .. } => loaded == path,
            ClassPathEntry::NestedDirectory {
                parent_jar: loaded, ..
            }
            | ClassPathEntry::NestedJar {
                parent_jar: loaded, ..
            } => loaded == path,
            _ => false,
        }) {
            return;
        }
        let backing = data.clone();
        let cursor = Cursor::new(data);
        match ZipArchive::new(cursor) {
            Ok(mut archive) => {
                // Check for MANIFEST.MF to detect Spring Boot fat JAR
                let manifest = Self::read_manifest(&mut archive);
                let manifest_class_path = manifest.resolve_class_path(path);
                let is_fat_jar =
                    manifest.is_spring_boot() || Self::probe_fat_jar_structure(&mut archive);

                if is_fat_jar {
                    debug!(
                        "Detected fat JAR: {} (Start-Class: {:?})",
                        path.display(),
                        manifest.start_class
                    );
                    Self::extract_fat_jar_entries(path, &mut archive, &backing, &manifest, entries);
                    // Also add the outer JAR itself (for classes at the root,
                    // e.g. the Spring Boot launcher classes in org/springframework/boot/loader/).
                    //
                    // T1.8.2 follow-up (MED #32): we previously `.unwrap()`'d
                    // the re-open. The first `ZipArchive::new` succeeded on
                    // the same byte buffer, so in practice this can only
                    // fail if the underlying `Vec<u8>` is somehow corrupted
                    // between the two opens — but a panic here would kill
                    // the VM before any user code runs. Fat-JAR entries
                    // we already pushed remain valid; we just skip the
                    // root-classes entry and log.
                    let mr = manifest.multi_release;
                    let data = archive.into_inner().into_inner();
                    let root_backing = data.clone();
                    match ZipArchive::new(Cursor::new(data)) {
                        Ok(mut reloaded) => {
                            let entry_index = Self::build_archive_entry_index(&mut reloaded);
                            entries.push(ClassPathEntry::JarFile {
                                path: path.to_path_buf(),
                                archive: Mutex::new(reloaded),
                                backing: root_backing,
                                entry_index,
                                multi_release: mr,
                                versions_cache: Mutex::new(None),
                                signer_cache: OnceLock::new(),
                            });
                        }
                        Err(e) => {
                            debug!(
                                "Failed to re-open fat JAR {} for root-class entry: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    let mr = manifest.multi_release;
                    debug!("Loaded JAR: {}", path.display());
                    let entry_index = Self::build_archive_entry_index(&mut archive);
                    entries.push(ClassPathEntry::JarFile {
                        path: path.to_path_buf(),
                        archive: Mutex::new(archive),
                        backing,
                        entry_index,
                        multi_release: mr,
                        versions_cache: Mutex::new(None),
                        signer_cache: OnceLock::new(),
                    });
                    if depth < Self::MAX_CLASS_PATH_MANIFEST_DEPTH {
                        for token in manifest.resolve_class_path(path) {
                            Self::process_classpath_token(&token, entries, depth + 1);
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to open JAR {}: {e}", path.display());
            }
        }
    }

    /// Add one classpath token, including wildcard expansion and any manifest
    /// dependencies discovered while opening archives.
    fn add_classpath_entry(
        raw: &str,
        entries: &mut Vec<ClassPathEntry>,
        expanded_manifest_jars: &mut FxHashSet<PathBuf>,
    ) {
        for p in Self::expand_classpath_wildcard(raw) {
            // A URLClassLoader rooted at an archive subdirectory hands us
            // `<archive>!/<prefix>/`. Handle it before interpreting the token
            // as a filesystem path so `.war`/`.ear` archives retain their
            // internal root for both class and resource lookup.
            if let Some((archive, prefix)) = parse_jar_subdir_spec(&p) {
                let path = PathBuf::from(archive);
                if path.exists() {
                    match read_archive_for_classpath(&path) {
                        Ok(data) => {
                            if let Some(entry) = Self::build_nested_jar_from_jar(
                                &path,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&path, data, &prefix))
                            {
                                push_classpath_entry(entries, entry);
                            }
                        }
                        Err(e) => {
                            debug!(
                                "Failed to read nested classpath archive {}: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    debug!(
                        "Skipping missing nested classpath archive: {}",
                        path.display()
                    );
                }
                continue;
            }

            let path = PathBuf::from(&p);
            if path.is_dir() {
                push_classpath_entry(entries, ClassPathEntry::Directory(path));
            } else if path.extension().is_some_and(|ext| ext == "jmod") && path.exists() {
                match Self::load_jmod(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read JMOD {}: {e}", path.display()),
                }
            } else if Self::is_likely_jimage(&path) {
                match Self::load_jimage(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read jimage {}: {e}", path.display()),
                }
            } else if path.is_file() {
                // URLClassLoader accepts every file URL as an archive
                // candidate. `load_jar_data_with_manifest_class_path` fails
                // closed for ordinary non-archive files.
                match read_archive_for_classpath(&path) {
                    Ok(data) => Self::load_jar_data_at_depth(&path, data, entries, 0),
                    Err(e) => debug!("Failed to read classpath archive {}: {e}", path.display()),
                }
            } else {
                debug!("Skipping non-existent classpath entry: {p}");
            }
        }
    }

    /// Read `META-INF/MANIFEST.MF` from an archive.
    fn read_manifest(archive: &mut SharedArchive) -> ManifestInfo {
        let result = archive
            .by_name("META-INF/MANIFEST.MF")
            .and_then(|mut entry| {
                // Audit-fix #2 (capacity) + V2 (streaming bound): clamp the
                // attacker-controlled declared size AND bound the actual
                // inflate via `read_entry_capped`. The `?` converts the
                // `io::Error` overflow into `ZipError` so the closure stays
                // in the `ZipResult` the `and_then` expects.
                let size = entry.size();
                Ok(read_entry_capped(&mut entry, size)?)
            });
        match result {
            Ok(data) => ManifestInfo::parse(&data),
            Err(_) => ManifestInfo::default(),
        }
    }

    /// Probe for fat JAR structure by checking if any entry starts with
    /// `BOOT-INF/` or `WEB-INF/classes/`.
    fn probe_fat_jar_structure(archive: &mut SharedArchive) -> bool {
        for i in 0..archive.len().min(100) {
            if let Ok(entry) = archive.by_index_raw(i) {
                let name = entry.name().to_string();
                if name.starts_with("BOOT-INF/") || name.starts_with("WEB-INF/classes/") {
                    return true;
                }
            }
        }
        false
    }

    /// Extract nested entries from a Spring Boot fat JAR.
    ///
    /// 1. All `.class` files under `BOOT-INF/classes/` (or custom path from
    ///    MANIFEST) become a `NestedDirectory` entry.
    /// 2. Each `.jar` file under `BOOT-INF/lib/` (or custom path) is extracted
    ///    into memory and added as a `NestedJar` entry.
    fn extract_fat_jar_entries(
        path: &Path,
        archive: &mut SharedArchive,
        backing: &ArchiveBacking,
        manifest: &ManifestInfo,
        entries: &mut Vec<ClassPathEntry>,
    ) {
        let classes_prefix = manifest
            .boot_classes
            .as_deref()
            .unwrap_or("BOOT-INF/classes/");
        let lib_prefix = manifest.boot_lib.as_deref().unwrap_or("BOOT-INF/lib/");

        // Ensure prefixes end with /
        let classes_prefix = if classes_prefix.ends_with('/') {
            classes_prefix.to_string()
        } else {
            format!("{classes_prefix}/")
        };
        let lib_prefix = if lib_prefix.ends_with('/') {
            lib_prefix.to_string()
        } else {
            format!("{lib_prefix}/")
        };

        // Also handle WEB-INF for WAR files
        let war_classes_prefix = "WEB-INF/classes/";

        // Phase 1: Collect BOOT-INF/classes/ entries into a NestedDirectory
        let mut classes_cache: HashMap<String, SharedBytes> = HashMap::new();
        let mut nested_jar_names: Vec<String> = Vec::new();

        for i in 0..archive.len() {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };

            // Audit-fix #3 (zip-slip): refuse to cache any entry whose
            // name escapes the JAR root, contains NUL bytes, uses
            // absolute paths, or smuggles a Windows drive letter.
            // In-memory caches alone do not write to disk, but a
            // poisoned cache namespace (`../etc/passwd`) lets the
            // attacker collide on lookup keys that legitimate code
            // might subsequently use.
            if !is_safe_entry_name(&name) {
                debug!(
                    "Fat JAR {}: rejecting unsafe entry name {name:?}",
                    path.display()
                );
                continue;
            }

            if name.starts_with(&classes_prefix) && name.len() > classes_prefix.len() {
                let relative = &name[classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') && is_safe_entry_name(relative)
                {
                    if let Some(data) = Self::find_shared_in_archive_locked(archive, backing, &name)
                    {
                        classes_cache.insert(relative.to_string(), data);
                    }
                }
            } else if name.starts_with(war_classes_prefix) && name.len() > war_classes_prefix.len()
            {
                let relative = &name[war_classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') && is_safe_entry_name(relative)
                {
                    if let Some(data) = Self::find_shared_in_archive_locked(archive, backing, &name)
                    {
                        classes_cache.insert(relative.to_string(), data);
                    }
                }
            } else if name.starts_with(&lib_prefix)
                && name.ends_with(".jar")
                && name.len() > lib_prefix.len()
            {
                nested_jar_names.push(name);
            }
        }

        if !classes_cache.is_empty() {
            debug!(
                "Fat JAR {}: extracted {} files from {}",
                path.display(),
                classes_cache.len(),
                classes_prefix
            );
            entries.push(ClassPathEntry::NestedDirectory {
                parent_jar: path.to_path_buf(),
                prefix: classes_prefix.to_string(),
                entries_cache: classes_cache,
            });
        }

        // Phase 2: Extract each nested JAR from BOOT-INF/lib/
        let mut nested_count = 0;
        for jar_name in &nested_jar_names {
            if let Some(jar_data) = Self::find_shared_in_archive_locked(archive, backing, jar_name)
            {
                let nested_backing = ArchiveBacking::Shared(jar_data);
                let cursor = Cursor::new(nested_backing.clone());
                match ZipArchive::new(cursor) {
                    Ok(mut nested_archive) => {
                        let entry_index = Self::build_archive_entry_index(&mut nested_archive);
                        entries.push(ClassPathEntry::NestedJar {
                            parent_jar: path.to_path_buf(),
                            nested_path: jar_name.clone(),
                            archive: Mutex::new(nested_archive),
                            backing: nested_backing,
                            entry_index,
                            signer_cache: OnceLock::new(),
                        });
                        nested_count += 1;
                    }
                    Err(e) => {
                        debug!(
                            "Fat JAR {}: failed to open nested JAR {}: {e}",
                            path.display(),
                            jar_name
                        );
                    }
                }
            }
        }

        if nested_count > 0 {
            debug!(
                "Fat JAR {}: loaded {} nested JARs from {}",
                path.display(),
                nested_count,
                lib_prefix
            );
        }
    }

    /// Returns true if this classpath has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of classpath entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Read the MANIFEST.MF from the first JAR on this classpath that has one.
    ///
    /// Useful for the CLI to determine `Start-Class` from a Spring Boot fat JAR.
    /// Read the manifest from a single JAR file without constructing a full ClassPath.
    ///
    /// Returns `None` if the JAR cannot be read or has no `MANIFEST.MF`.
    pub fn read_jar_manifest(jar_path: &Path) -> Option<ManifestInfo> {
        // Read into owned bytes and reject concurrent mutation before ZIP
        // parsing, so a changing JAR is handled as invalid input.
        let data = read_archive_for_classpath(jar_path).ok()?;
        let cursor = Cursor::new(data);
        let mut archive = ZipArchive::new(cursor).ok()?;
        let info = Self::read_manifest(&mut archive);
        Some(info)
    }

    pub fn read_first_manifest(&self) -> Option<ManifestInfo> {
        for entry in &self.entries {
            if let ClassPathEntry::JarFile { archive, .. } = entry {
                let mut guard = archive.lock();
                let info = Self::read_manifest(&mut guard);
                if info.main_class.is_some() || info.start_class.is_some() {
                    return Some(info);
                }
            }
        }
        None
    }

    /// Add a new path entry at runtime (for URLClassLoader and dynamic class loading).
    ///
    /// If `path` is a directory it is added as a `Directory` entry; if it ends
    /// with `.jar` (or `.zip`) and exists, it is opened as a `JarFile` entry.
    /// Fat JARs are auto-detected and their nested entries are extracted.
    /// Silently skips non-existent paths or unreadable JARs.
    ///
    /// Supports the same `dir/*` wildcard expansion as [`ClassPath::new`] so
    /// app servers / launchers that synthesise `lib/*` strings at runtime
    /// (e.g. JBoss module loader, Maven plugin loaders) get every JAR in
    /// the directory instead of silently dropping the entry.
    pub fn add_path(&mut self, path: &str) {
        if self.static_len.is_none() {
            self.static_len = Some(self.entries.len());
        }
        *self.dynamic_specs.entry(path.to_string()).or_insert(0) += 1;
        for expanded in Self::expand_classpath_wildcard(path) {
            // DaCapo-style URL handoff: a URL of the form
            // `jar:file:/<jar>!/<prefix>/` extracted by URLClassLoader will
            // arrive here as `<jar>!/<prefix>/` (the `file:` and `jar:`
            // prefixes are stripped by the URLClassLoader native before
            // `add_path` is called). Treat the outer JAR as the resource
            // root, but virtualised at the prefix — so a `findClass` for
            // `Foo` looks inside the JAR at `<prefix>Foo.class` instead of
            // `Foo.class`. This mirrors the JDK's `URLClassLoader` over a
            // `jar:` URL pointing at a subdirectory inside a JAR, which is
            // exactly what DaCapo 9.12-MR1's Harness does at startup
            // (`cl.getResource("harness/")` → URLClassLoader → loadClass
            // `org.dacapo.harness.TestHarness`).
            if let Some((jar_part, prefix)) = parse_jar_subdir_spec(&expanded) {
                let pb = std::path::PathBuf::from(jar_part);
                if pb.exists() {
                    match read_archive_for_classpath(&pb) {
                        Ok(data) => {
                            match Self::build_nested_jar_from_jar(
                                &pb,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&pb, data, &prefix))
                            {
                                Some(entry) => {
                                    debug!(
                                        "Dynamic classpath: adding nested-dir {}!/{}",
                                        pb.display(),
                                        prefix
                                    );
                                    push_classpath_entry(&mut self.entries, entry);
                                }
                                None => debug!(
                                    "Dynamic classpath: nested-dir {}!/{} \
                                     yielded no entries (skipping)",
                                    pb.display(),
                                    prefix
                                ),
                            }
                        }
                        Err(e) => debug!("Dynamic classpath: failed to read {}: {e}", pb.display()),
                    }
                    continue;
                }
                debug!(
                    "Dynamic classpath: nested-dir spec {} references missing JAR",
                    expanded
                );
                continue;
            }
            let pb = std::path::PathBuf::from(&expanded);
            if pb.is_dir() {
                debug!("Dynamic classpath: adding directory {expanded}");
                push_classpath_entry(&mut self.entries, ClassPathEntry::Directory(pb));
            } else if pb.extension().is_some_and(|e| e == "jmod") && pb.exists() {
                match Self::load_jmod(&pb) {
                    Ok(entry) => push_classpath_entry(&mut self.entries, entry),
                    Err(e) => debug!("Dynamic classpath: failed to read JMOD {expanded}: {e}"),
                }
            } else if pb.is_file() {
                // Any existing non-directory classpath entry is treated as a
                // JAR/ZIP archive regardless of file extension, matching the
                // real JDK's `URLClassPath` (a `file:` URL that does not end in
                // '/' becomes a `JarLoader`). Hibernate's packaged-archive tests
                // put `.par`/`.war`/`.ear` jars on a `URLClassLoader`; gating
                // archive loading on a `.jar`/`.zip` extension made every class
                // and resource inside them invisible (PackagedEntityManagerTest:
                // orm.xml/cfg.xml not found, entities not discovered). `load_jar_data`
                // fails closed (pushes no entry) when the bytes are not a valid
                // zip, so a stray non-archive file on the classpath is harmless.
                //
                // Use the same stable owned read as startup classpath JARs.
                match read_archive_for_classpath(&pb) {
                    Ok(data) => {
                        Self::load_jar_data_at_depth(&pb, data, &mut self.entries, 0);
                    }
                    Err(e) => {
                        debug!("Dynamic classpath: failed to read {expanded}: {e}");
                    }
                }
            } else {
                debug!("Dynamic classpath: skipping non-existent entry {expanded}");
            }
        }
    }

    /// Retract a spec previously handed to [`Self::add_path`].
    ///
    /// This is the other half of the `URLClassLoader.close()` contract: a closed
    /// loader must stop serving classes and resources it had not already
    /// loaded. Already-defined classes stay defined, exactly as on HotSpot —
    /// `close()` shuts the loader's `URLClassPath`, it does not unload anything.
    ///
    /// Returns the number of classpath entries actually removed. Zero is the
    /// normal answer for a spec that another live loader still holds (the use
    /// count from [`Self::add_path`] has not reached zero), for one that was
    /// never added, and for one whose files did not resolve to any entry.
    pub fn remove_path(&mut self, path: &str) -> usize {
        match self.dynamic_specs.get_mut(path) {
            Some(count) if *count > 1 => {
                *count -= 1;
                return 0;
            }
            Some(_) => {
                self.dynamic_specs.remove(path);
            }
            None => return 0,
        }
        let Some(static_len) = self.static_len else {
            return 0;
        };
        // Every filesystem root this spec could have contributed, in the same
        // two forms `add_path` accepts: a plain path (possibly wildcarded) and
        // the `<jar>!/<prefix>/` nested form, whose root is the outer JAR.
        let mut roots: Vec<PathBuf> = Vec::new();
        for expanded in Self::expand_classpath_wildcard(path) {
            if let Some((jar_part, _prefix)) = parse_jar_subdir_spec(&expanded) {
                roots.push(PathBuf::from(jar_part));
            } else {
                roots.push(PathBuf::from(&expanded));
            }
        }
        if roots.is_empty() {
            return 0;
        }
        let before = self.entries.len();
        let mut index = 0usize;
        self.entries.retain(|entry| {
            let position = index;
            index += 1;
            // Startup roots are never retractable — see `static_len`.
            if position < static_len {
                return true;
            }
            !roots.iter().any(|root| entry_derives_from(entry, root))
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            // A retracted root may have been the only source of a name that is
            // now absent again, and vice versa for anything memoized while it
            // was present.
            self.canonicalize_cache.lock().clear();
            self.root_canonical_cache.lock().clear();
            // The directory index is keyed on the root path, and a root that
            // comes back (a `URLClassLoader` closed and reopened over the same
            // directory) must not be served from the set we walked last time.
            // Clearing it costs one relazy walk per surviving root; keeping a
            // stale set would let a retracted-then-recreated directory answer
            // from contents that no longer exist.
            self.dir_index.lock().clear();
            debug!("Dynamic classpath: retracted {removed} entrie(s) for {path}");
        }
        removed
    }

    /// Build a [`ClassPathEntry::NestedJar`] from an archive entry that is
    /// itself a valid ZIP/JAR. This is the classpath form of Spring Boot's
    /// `jar:nested:<outer>/!<inner.jar>!/` URL: the trailing marker denotes
    /// the nested archive root, rather than a directory named `inner.jar`.
    ///
    /// Returning `None` for a non-archive entry lets the caller fall back to
    /// the ordinary nested-directory treatment for `<outer>!/<prefix>/`.
    fn build_nested_jar_from_jar(
        path: &Path,
        data: ArchiveBacking,
        nested_path: &str,
    ) -> Option<ClassPathEntry> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let backing = data.clone();
        let mut outer = ZipArchive::new(Cursor::new(data)).ok()?;
        let nested_backing = ArchiveBacking::Shared(Self::find_shared_in_archive_locked(
            &mut outer,
            &backing,
            nested_path,
        )?);
        let mut archive = ZipArchive::new(Cursor::new(nested_backing.clone())).ok()?;
        let entry_index = Self::build_archive_entry_index(&mut archive);
        Some(ClassPathEntry::NestedJar {
            parent_jar: path.to_path_buf(),
            nested_path: nested_path.to_string(),
            archive: Mutex::new(archive),
            backing: nested_backing,
            entry_index,
            signer_cache: OnceLock::new(),
        })
    }

    /// Build a [`ClassPathEntry::NestedDirectory`] by extracting every entry
    /// in `archive` whose name begins with `prefix` (which itself must end
    /// with `/`, mirroring how `BOOT-INF/classes/` is structured). Returns
    /// `None` if the archive cannot be opened or no entries match.
    ///
    /// Used by `add_path` for the DaCapo-style `<jar>!/<prefix>/` URL form.
    fn build_nested_directory_from_jar(
        path: &Path,
        data: ArchiveBacking,
        prefix: &str,
    ) -> Option<ClassPathEntry> {
        let backing = data.clone();
        let cursor = Cursor::new(data);
        let mut archive = match ZipArchive::new(cursor) {
            Ok(a) => a,
            Err(e) => {
                debug!("Failed to open JAR for nested-dir {}: {e}", path.display());
                return None;
            }
        };
        let mut entries_cache: HashMap<String, SharedBytes> = HashMap::new();
        let total = archive.len();
        for i in 0..total {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };
            if !is_safe_entry_name(&name) {
                continue;
            }
            if !name.starts_with(prefix) || name.len() <= prefix.len() {
                continue;
            }
            let relative = &name[prefix.len()..];
            if relative.is_empty() || relative.ends_with('/') {
                // Skip the directory marker itself and any subdirectories.
                continue;
            }
            if !is_safe_entry_name(relative) {
                continue;
            }
            if let Some(bytes) = Self::find_shared_in_archive_locked(&mut archive, &backing, &name)
            {
                entries_cache.insert(relative.to_string(), bytes);
            }
        }
        if entries_cache.is_empty() {
            return None;
        }
        Some(ClassPathEntry::NestedDirectory {
            parent_jar: path.to_path_buf(),
            prefix: prefix.to_string(),
            entries_cache,
        })
    }

    /// Find and read a class file by its binary name (e.g., `java/lang/Object`).
    ///
    /// Class names are validated to prevent path traversal attacks. Names containing
    /// `..` or starting with `/` are rejected.
    pub fn find_class(&self, class_name: &str) -> Result<SharedBytes, ClassFileError> {
        // Reject path traversal attempts, absolute paths, and suspicious patterns.
        // Class binary names use '/' as separator and must not escape the classpath root.
        if !is_safe_class_name(class_name) {
            return Err(ClassFileError::ClassNotFound {
                class_name: class_name.to_string(),
            });
        }

        // PASS 1 consults `dir_index` and never stats a directory that cannot
        // hold this class; PASS 2 is the historical stat-per-directory scan,
        // and runs only when pass 1 found nothing anywhere. See the
        // `dir_index` field doc for the measurement and for why the second
        // pass is what keeps this exactly as correct as the single pass it
        // replaces: a class created after a directory was indexed is invisible
        // to pass 1 and still found by pass 2.
        match self.find_class_pass(class_name, true) {
            Ok(bytes) => return Ok(bytes),
            Err(ClassFileError::ClassNotFound { .. }) => {}
            Err(other) => return Err(other),
        }
        self.find_class_pass(class_name, false)
    }

    /// One search of the entry list. `use_dir_index` selects pass 1 (skip a
    /// directory whose index says the class is absent) or pass 2 (probe every
    /// directory with a real `exists()`, as before the index existed).
    fn find_class_pass(
        &self,
        class_name: &str,
        use_dir_index: bool,
    ) -> Result<SharedBytes, ClassFileError> {
        let relative_path = format!("{}.class", class_name);
        // Executable WAR / Spring-Boot WAR support: when a class is requested
        // by its binary name, also probe the common archive-internal class
        // roots used by web/fat-jar containers. Jenkins's WAR is an
        // *executable* WAR with its Main-Class at the archive root
        // (`executable/Main.class`), not under `WEB-INF/classes/`; other
        // WARs put application classes under `WEB-INF/classes/`; Spring
        // Boot fat JARs use `BOOT-INF/classes/`. We try root first (the
        // canonical location for ordinary JARs) and only fall back to
        // the prefixed variants — this is a no-op for plain JARs because
        // those prefixed entries simply don't exist.
        let archive_candidates: [String; 3] = [
            relative_path.clone(),
            format!("WEB-INF/classes/{}", relative_path),
            format!("BOOT-INF/classes/{}", relative_path),
        ];

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    // The syscall this whole index exists to avoid. `Some(false)`
                    // is a definite absence at index-build time, and pass 2 is
                    // what covers the case where that has since changed.
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            // Pass 2 found what pass 1's index denied: the
                            // directory has grown. Drop the stale index so the
                            // next lookup rebuilds rather than falling through
                            // to pass 2 forever.
                            self.dir_index_invalidate(dir);
                        }
                        // Audit-fix #5: symlink-traversal check is now
                        // fail-CLOSED. If we cannot canonicalize either
                        // the classpath root or the resolved file we
                        // refuse to read — silently proceeding allowed
                        // a symlinked `Object.class -> /etc/passwd` to
                        // be served as classfile bytes.
                        //
                        // Round 5 audit fix (MED): routed through
                        // `canonicalize_cached` so repeat probes of the
                        // same paths skip the per-call syscall — the
                        // classpath root is identical across every
                        // probe and resolved-file paths repeat once a
                        // class is reloaded by JVMTI / instrumentation.
                        let canon_dir = self.canonicalize_root(dir).map_err(|e| {
                            debug!(
                                "Refusing to load {class_name}: cannot canonicalize \
                                 classpath root {}: {e}",
                                dir.display()
                            );
                            ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            }
                        })?;
                        let canon_path = self.canonicalize_cached(&full_path).map_err(|e| {
                            debug!(
                                "Refusing to load {class_name}: cannot canonicalize \
                                 resolved path {}: {e}",
                                full_path.display()
                            );
                            ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            }
                        })?;
                        if !canon_path.starts_with(&canon_dir) {
                            debug!(
                                "Path traversal blocked: {} escapes {}",
                                canon_path.display(),
                                canon_dir.display()
                            );
                            return Err(ClassFileError::ClassNotFound {
                                class_name: class_name.to_string(),
                            });
                        }
                        debug!("Found class {class_name} at {}", full_path.display());
                        // Use the stable classpath read helper so a class file
                        // that changes mid-read is rejected as invalid input.
                        return read_file_for_classpath(&full_path)
                            .map(SharedBytes::from)
                            .map_err(|e| ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            });
                    }
                }
                // Pass 2 exists only for the directory staleness window; every
                // archive kind answers from an in-memory index that pass 1
                // already consulted, so repeating them would be pure waste.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    backing,
                    ..
                } => {
                    for candidate in &archive_candidates {
                        let found = if *multi_release {
                            Self::find_shared_in_multi_release_archive(
                                archive,
                                backing,
                                versions_cache,
                                entry_index,
                                candidate,
                            )
                        } else {
                            Self::find_shared_in_indexed_archive(
                                archive,
                                backing,
                                entry_index,
                                candidate,
                            )
                        };
                        if let Some(data) = found {
                            debug!("Found class {class_name} in JAR (entry: {candidate})");
                            return Ok(data);
                        }
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get(&relative_path) {
                        debug!("Found class {class_name} in nested directory");
                        return Ok(data.clone());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    entry_index,
                    nested_path,
                    backing,
                    ..
                } => {
                    for candidate in &archive_candidates {
                        if let Some(data) = Self::find_shared_in_indexed_archive(
                            archive,
                            backing,
                            entry_index,
                            candidate,
                        ) {
                            debug!(
                                "Found class {class_name} in nested JAR {nested_path} \
                                 (entry: {candidate})"
                            );
                            return Ok(data);
                        }
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    // O(1) name probe against the central-directory index;
                    // only a hit pays a deflate, and only for this one entry.
                    if let Some(data) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, &relative_path)
                    {
                        debug!("Found class {class_name} in JMOD {}", path.display());
                        return Ok(data);
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    class_to_module,
                    ..
                } => {
                    // Two paths:
                    //   1. Fast path — the class_to_module index was built
                    //      at load time, so one HashMap lookup tells us
                    //      exactly which module owns the class.
                    //   2. Fallback — if the class isn't in the index
                    //      (edge case: entry added after load, or a
                    //      synthesized class), iterate known modules.
                    if let Some(module) = class_to_module.get(class_name) {
                        match reader.find_class(module, class_name) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found class {class_name} in jimage {} (module {module})",
                                    path.display()
                                );
                                return Ok(SharedBytes::from(bytes));
                            }
                            Ok(None) => {
                                // Index said module owns it, but the
                                // perfect-hash lookup disagrees — treat as
                                // a broken image and fall through so the
                                // caller sees a clean ClassNotFound.
                            }
                            Err(e) => {
                                return Err(ClassFileError::IoError {
                                    class_name: class_name.to_string(),
                                    source: std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("jimage: {e}"),
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        Err(ClassFileError::ClassNotFound {
            class_name: class_name.to_string(),
        })
    }

    /// Is `relative_path` present under the `Directory` entry `dir`, according
    /// to [`ClassPath::dir_index`]?
    ///
    /// `Some(false)` is the answer worth having: it lets `find_class` skip a
    /// `stat` for a directory that cannot hold this class. `Some(true)` means
    /// the file was there when the index was built and the caller should go on
    /// to the ordinary probe (which re-checks existence anyway). `None` means
    /// there is no usable index — an unreadable or oversized directory — and
    /// the caller must probe.
    ///
    /// Builds the index on first use, under the map lock. Two threads racing
    /// on the same cold directory can both walk it; the second insert wins and
    /// the sets are equal, so the race costs one redundant walk and nothing
    /// else. Holding the lock across the walk instead would serialise every
    /// other directory's first lookup behind it.
    fn dir_index_contains(&self, dir: &Path, relative_path: &str) -> Option<bool> {
        if let Some(index) = self.dir_index.lock().get(dir) {
            return index.as_ref().map(|set| set.contains(relative_path));
        }
        let built = Self::build_dir_index(dir);
        let answer = built.as_ref().map(|set| set.contains(relative_path));
        self.dir_index
            .lock()
            .insert(dir.to_path_buf(), built.clone());
        answer
    }

    /// Drop the cached index for `dir` so the next lookup rebuilds it.
    ///
    /// Called when the stat-based fallback finds a file the index said was
    /// absent, i.e. the directory has grown since the walk. One rebuild then
    /// serves every later lookup, instead of the fallback running forever.
    fn dir_index_invalidate(&self, dir: &Path) {
        self.dir_index.lock().remove(dir);
    }

    /// Walk `dir` and collect every file's path relative to it, in classpath
    /// form (`/` separators, no leading slash).
    ///
    /// Returns `None` when the directory cannot be read or holds more than
    /// [`DIR_INDEX_MAX_ENTRIES`] files — both mean "no index", and every
    /// lookup falls back to the syscall.
    ///
    /// Symlinks ARE followed, and that is deliberate rather than permissive.
    /// A symlinked entry left out of the index is a pass-1 miss that pass 2
    /// resolves — and pass 2's success invalidates the index, so the NEXT
    /// class under that symlink rebuilds the whole directory. Skipping
    /// symlinks would therefore trade one `stat` per lookup for one full
    /// directory walk per lookup, which is worse than the defect. Following
    /// them costs a real `metadata` call only on the entries that are
    /// symlinks; `file_type` answers the other 99.9% for free, out of the
    /// `FIND_DATA` on Windows and `d_type` on Linux.
    ///
    /// Following symlinks means a directory cycle is reachable, so the walk is
    /// bounded on BOTH axes: [`DIR_INDEX_MAX_ENTRIES`] files and
    /// `DIR_INDEX_MAX_DIRS` directories. Hitting either abandons the index
    /// (`None`), and every lookup on that entry falls back to the syscall —
    /// the pre-index behaviour, never a hang.
    ///
    /// Indexing a symlink target is not a security decision: the read path
    /// canonicalises and fail-closed refuses anything that escapes the root
    /// (see `find_class`), and that check is unchanged. An index entry only
    /// says "worth probing".
    fn build_dir_index(dir: &Path) -> Option<Arc<FxHashSet<Box<str>>>> {
        let mut set: FxHashSet<Box<str>> = FxHashSet::default();
        let mut stack = vec![(dir.to_path_buf(), String::new())];
        let mut dirs_seen = 0usize;
        while let Some((current, prefix)) = stack.pop() {
            dirs_seen += 1;
            if dirs_seen > DIR_INDEX_MAX_DIRS {
                return None;
            }
            let reader = std::fs::read_dir(&current).ok()?;
            for entry in reader.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let mut rel = String::with_capacity(prefix.len() + 1 + name.len());
                rel.push_str(&prefix);
                if !prefix.is_empty() {
                    rel.push('/');
                }
                rel.push_str(&name);
                let Ok(kind) = entry.file_type() else {
                    return None;
                };
                // Only a symlink needs the follow-through `metadata` stat.
                let kind = if kind.is_symlink() {
                    match entry.metadata() {
                        Ok(md) => md.file_type(),
                        // A dangling link is neither; skip it.
                        Err(_) => continue,
                    }
                } else {
                    kind
                };
                if kind.is_dir() {
                    stack.push((entry.path(), rel));
                } else if kind.is_file() {
                    if set.len() >= DIR_INDEX_MAX_ENTRIES {
                        return None;
                    }
                    set.insert(rel.into_boxed_str());
                }
            }
        }
        Some(Arc::new(set))
    }

    /// Find the filesystem path of the classpath entry that holds the given
    /// class.  Returns `Some(path)` where `path` is a `file:`-style path to
    /// the containing JAR or directory — the same thing HotSpot returns in
    /// `ProtectionDomain.getCodeSource().getLocation().getPath()`.
    ///
    /// For a bare directory, returns the directory's absolute path with a
    /// trailing slash.  For JARs (flat or nested), returns the JAR file's
    /// absolute path (not wrapped in `jar:!/`).  For JMOD/jimage modules,
    /// returns `None` — JDK internals aren't user-visible code.
    pub fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        // Audit-fix #6: match `find_class`'s full validation set (NUL
        // bytes, leading slash, backslash, drive letter, dot-dot,
        // relative-dir prefixes). The previous truncated check let
        // `..\\Object` or `C:\Foo` reach the JAR-name lookups below.
        if !is_safe_class_name(class_name) {
            return None;
        }
        // Two passes, for the reason `find_class` has two: this runs once per
        // class DEFINE (the origin census, and `defineClass`'s CodeSource), so
        // its own stat-per-directory scan was a second copy of the same
        // 111-directories x 1102-classes cost. Indexing `find_class` alone took
        // netty's first-touch 2195 ms -> 1299 ms and left 1332 ms of it here.
        self.find_class_source_path_pass(class_name, true)
            .or_else(|| self.find_class_source_path_pass(class_name, false))
    }

    fn find_class_source_path_pass(&self, class_name: &str, use_dir_index: bool) -> Option<String> {
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            self.dir_index_invalidate(dir);
                        }
                        return Some(
                            dir.to_string_lossy()
                                .trim_end_matches(['/', '\\'])
                                .to_string()
                                + "/",
                        );
                    }
                }
                // Pass 2 is only for the directory staleness window.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    path,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            &relative_path,
                        )
                        .is_some()
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, &relative_path)
                            .is_some()
                    };
                    if found {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar,
                    entries_cache,
                    ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        return Some(parent_jar.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedJar {
                    parent_jar,
                    archive,
                    entry_index,
                    nested_path,
                    ..
                } => {
                    if Self::find_in_indexed_archive(archive, entry_index, &relative_path).is_some()
                    {
                        // Return "$parent_jar!/$nested_path" style (JAR-in-JAR)
                        // so the caller can distinguish nested location
                        // layouts; Quarkus uses the outer jar path for its
                        // appRoot heuristic, so just return the outer JAR
                        // with the nested path appended after an exclamation
                        // (informational only — matches HotSpot's behaviour
                        // for fat JARs).
                        let outer = parent_jar.to_string_lossy();
                        return Some(format!("{outer}!/{nested_path}"));
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    ..
                } => {
                    // Existence only — never inflate for a code-source query.
                    if class_entry_index.contains(&relative_path) {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::JImageFile { .. } => {
                    // jimage contains modules — not user-visible code.
                }
            }
        }
        None
    }

    /// Find the code source for a given class: returns the containing JAR /
    /// directory URL and the DER-encoded X.509 certificates extracted from
    /// every **verified** signer in the JAR's `META-INF/*.RSA|.DSA|.EC`
    /// signature blocks (if any).
    ///
    /// The returned URL uses `file:` form for directories and JAR paths,
    /// matching HotSpot's `CodeSource.getLocation()`. The certificate
    /// vector holds the leaf X.509 cert DER for each PKCS#7 signer
    /// whose `messageDigest` authenticated attribute matched the
    /// corresponding `.SF` file's SHA-X digest — see
    /// [`crate::jar_signer::verify_signer_block`].  Signer blocks that
    /// fail verification contribute **no certificates** (so
    /// `Class.getCodeSource().getCertificates()` returns empty rather
    /// than opaque attacker bytes).
    ///
    /// Returns `None` if the class isn't on this classpath or lives in a
    /// JMOD/jimage module (JDK internals have no user-visible code source).
    ///
    /// # TRUST BOUNDARY — what a non-empty certificate vector means
    ///
    /// This is the API a Java caller ultimately sees, via
    /// `Class.getCodeSource().getCertificates()`, and the point at which
    /// application code is most likely to conclude "this class came from a
    /// trusted publisher". A non-empty vector means **all** of the
    /// following held (see `docs/security/signed-jar-trust.md`):
    ///
    ///   * a `META-INF/*.RSA|.DSA|.EC` signer block verified against its
    ///     `.SF` companion and chained to an anchor in the process trust
    ///     store — [`crate::jar_signer::verify_signer_block`];
    ///   * that `.SF` committed to this archive's exact `MANIFEST.MF`;
    ///   * every entry the manifest declares a digest for matched its bytes;
    ///   * **this specific class entry** is one the manifest committed to,
    ///     and the bytes about to be loaded still hash to the signed digest
    ///     (re-checked per class in [`Self::certs_for_signed_class`]).
    ///
    /// It does **not** mean the signer's certificate is unrevoked (no
    /// CRL/OCSP is consulted), nor that name-constraint or policy
    /// processing was performed. And an **empty** vector is not evidence of
    /// tampering: an unsigned JAR, a directory classpath entry, and a host
    /// with no trust anchors configured all produce the same empty result.
    /// Callers must fail closed on empty, never infer a reason from it.
    pub fn find_class_code_source_info(&self, class_name: &str) -> Option<(String, Vec<Vec<u8>>)> {
        // Audit-fix #6: match `find_class` / `find_class_source_path`'s full
        // validation set (NUL bytes, leading slash, backslash, drive letter,
        // dot-dot, relative-dir prefixes). The previous truncated check let
        // `..\\Object` or `C:\Foo` reach the JAR-name lookups below.
        if !is_safe_class_name(class_name) {
            return None;
        }
        // Two passes, same rule as `find_class` / `find_class_source_path`:
        // this is on `defineClass`'s CodeSource path, so its directory scan
        // was a third copy of the per-class stat sweep.
        self.find_class_code_source_info_pass(class_name, true)
            .or_else(|| self.find_class_code_source_info_pass(class_name, false))
    }

    fn find_class_code_source_info_pass(
        &self,
        class_name: &str,
        use_dir_index: bool,
    ) -> Option<(String, Vec<Vec<u8>>)> {
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            self.dir_index_invalidate(dir);
                        }
                        // Directories are never signed.
                        let abs = self
                            .canonicalize_root(dir)
                            .unwrap_or_else(|_| dir.to_path_buf());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').trim_end_matches('/').to_string();
                        return Some((format!("file:/{}/", encode_path_for_url(&p)), Vec::new()));
                    }
                }
                // Pass 2 is only for the directory staleness window.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    path,
                    signer_cache,
                    entry_index,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            &relative_path,
                        )
                        .is_some()
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, &relative_path)
                            .is_some()
                    };
                    if found {
                        let abs = self
                            .canonicalize_root(path)
                            .unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').to_string();
                        // Report P1 (perf): verify the signer blocks at most once
                        // per archive, then reuse the cached signing state.
                        let info =
                            signer_cache.get_or_init(|| Self::extract_jar_signer_blocks(archive));
                        // V3 (unsigned-entry attack): attach the signer chain
                        // ONLY if this exact class entry is committed-to by the
                        // verified manifest and its bytes still match. A class
                        // present in a signed JAR but absent from the manifest
                        // (or whose bytes were swapped post-sign) is unsigned.
                        // For multi-release JARs the *served* entry (possibly
                        // `META-INF/versions/<N>/<class>`) is the one that must
                        // be signed, so resolve it before checking.
                        let mr_cache = if *multi_release {
                            Some(versions_cache)
                        } else {
                            None
                        };
                        let certs =
                            Self::certs_for_signed_class(info, archive, &relative_path, mr_cache);
                        return Some((format!("file:/{}", encode_path_for_url(&p)), certs));
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar,
                    entries_cache,
                    ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        let p = parent_jar.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/').to_string();
                        return Some((format!("file:/{}", encode_path_for_url(&p)), Vec::new()));
                    }
                }
                ClassPathEntry::NestedJar {
                    parent_jar,
                    archive,
                    nested_path,
                    signer_cache,
                    entry_index,
                    ..
                } => {
                    if Self::find_in_indexed_archive(archive, entry_index, &relative_path).is_some()
                    {
                        let outer = parent_jar.to_string_lossy().replace('\\', "/");
                        let outer = outer.trim_start_matches('/').to_string();
                        // Report P1 (perf): verify the signer blocks at most once
                        // per nested archive, then reuse the cached signing state.
                        let info =
                            signer_cache.get_or_init(|| Self::extract_jar_signer_blocks(archive));
                        // V3 (unsigned-entry attack): per-class cert attachment —
                        // see the matching `JarFile` arm above. Nested JARs are
                        // not searched multi-release here, so pass `None`.
                        let certs =
                            Self::certs_for_signed_class(info, archive, &relative_path, None);
                        return Some((
                            format!("jar:file:/{}!/{nested_path}", encode_path_for_url(&outer)),
                            certs,
                        ));
                    }
                }
                ClassPathEntry::JmodFile { .. } | ClassPathEntry::JImageFile { .. } => {
                    // JDK internals — no user-visible code source.
                }
            }
        }
        None
    }

    /// Walk a JAR archive for every `META-INF/*.RSA`, `META-INF/*.DSA`,
    /// and `META-INF/*.EC` signature block, parse each as PKCS#7
    /// SignedData, and return the DER bytes of every **verified** leaf
    /// X.509 certificate.  Verification is performed by
    /// [`crate::jar_signer::verify_signer_block`] — it parses the
    /// SignedData container, extracts the embedded cert chain, and
    /// confirms that the signer's `messageDigest` authenticated
    /// attribute equals the SHA-X digest of the matching `*.SF` file.
    ///
    /// **Security note.**  Until this commit, the function returned the
    /// raw signer-block bytes verbatim and the call chain stored them as
    /// `CodeSource.certificates`.  PKCS#7 SignedData blobs are not
    /// X.509 certificates — `Class.getCodeSource().getCertificates()`
    /// was therefore handing attacker-controlled opaque bytes back to
    /// callers (e.g. policy `signedBy` filters) and treating them as
    /// trusted signer identities.  We now decode the structure and
    /// return only the real cert DER on successful integrity check.
    ///
    /// An empty result means either:
    ///   * the JAR is unsigned, or
    ///   * every signer block failed verification (parse error, missing
    ///     `.SF` companion, digest mismatch, ...). In that case
    ///     `Class.getCodeSource().getCertificates()` will be empty /
    ///     null — **never garbage** — which matches what HotSpot does
    ///     for a JAR that fails `jarsigner -verify`.
    ///
    /// # TRUST BOUNDARY
    ///
    /// Public-key verification of the SignerInfo signature and full
    /// certification-path construction to a trust anchor **are** performed
    /// (the older `TODO` here, which said neither was, is obsolete — see
    /// `jar_signer.rs` module docs and `docs/security/signed-jar-trust.md`).
    ///
    /// `chain` carries only the certificates on the *validated path*:
    /// [`crate::jar_signer::verify_signer_block`] narrows the
    /// attacker-supplied CMS `certificates` set to the certs it actually
    /// checked, so an extra certificate appended to a legitimately signed
    /// JAR cannot be surfaced here as one of the signer's.
    ///
    /// What remains unproven for a non-empty `chain`: **revocation** (no
    /// CRL/OCSP), name constraints, and certificate policies. And note the
    /// two-stage gate — a non-empty `chain` is archive-level; it is
    /// [`Self::certs_for_signed_class`] that decides whether any given
    /// class entry is entitled to it.
    fn extract_jar_signer_blocks(archive: &Mutex<SharedArchive>) -> JarSignerInfo {
        let mut guard = archive.lock();
        // Collect every safe META-INF entry name once; we need both the
        // `*.SF` companions (to feed into the integrity check) and the
        // `*.RSA|.DSA|.EC` signer blocks.
        let all_names: Vec<String> = (0..guard.len())
            .filter_map(|i| guard.by_index_raw(i).ok().map(|e| e.name().to_string()))
            .filter(|n| {
                // Audit-fix #3 (zip-slip): refuse to treat traversal /
                // absolute-path entry names as signer blocks. A
                // malicious JAR could otherwise smuggle a signature
                // block under `../META-INF/key.RSA` that gets attached
                // to the wrong CodeSource.
                is_safe_entry_name(n)
            })
            .collect();

        // Partition into signer blocks and a lookup map for `.SF` companions.
        let mut signer_block_names: Vec<String> = Vec::new();
        // Map from uppercase stem (e.g. "META-INF/FOO") to original
        // `.SF` entry name. This lets us pair `META-INF/foo.RSA` with
        // `META-INF/FOO.SF` regardless of letter case.
        let mut sf_by_stem: HashMap<String, String> = HashMap::new();
        for n in &all_names {
            let upper = n.to_ascii_uppercase();
            if !upper.starts_with("META-INF/") {
                continue;
            }
            if upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC") {
                signer_block_names.push(n.clone());
            } else if let Some(stem) = upper.strip_suffix(".SF") {
                sf_by_stem.insert(stem.to_string(), n.clone());
            }
        }

        let mut out: Vec<Vec<u8>> = Vec::new();
        // V3: union of every entry the bound manifest committed to (across
        // all verified signer blocks). Only classes appearing here are
        // eligible to inherit `out` (the signer chain).
        let mut signed_entries: HashMap<String, (crate::jar_signer::DigestAlg, Vec<u8>)> =
            HashMap::new();
        for name in signer_block_names {
            // Read signer block bytes (clamped against zip-bomb sizes,
            // streaming-bounded by `read_entry_capped` — V2).
            let block = match guard.by_name(&name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) if !d.is_empty() => d,
                _ => continue,
            };

            // Locate the matching `.SF`. The stem is everything before
            // the final `.` of the entry name; we look it up case-
            // insensitively to be compatible with archivers that
            // produce mixed-case filenames.
            let upper = name.to_ascii_uppercase();
            let stem_upper = match upper.rsplit_once('.') {
                Some((stem, _ext)) => stem.to_string(),
                None => continue,
            };
            let sf_name = match sf_by_stem.get(&stem_upper) {
                Some(n) => n.clone(),
                None => {
                    debug!(
                        "jar signer: signer block {} has no matching .SF — skipping",
                        name
                    );
                    continue;
                }
            };
            let sf_bytes = match guard.by_name(&sf_name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Verify self-consistency AND chain to trust-store anchor.
            // Task #40 wires the process-wide default trust store into
            // this gate — a JAR whose leaf doesn't chain to any anchor
            // (including every self-signed signer) ends up reported as
            // unsigned, matching the HotSpot "fails verify" behaviour.
            // On failure we silently drop the block.
            let trust_store = crate::jar_signer::default_trust_store();
            if let Some(vs) = crate::jar_signer::verify_signer_block(&block, &sf_bytes, trust_store)
            {
                // V1: the signature/`.SF` check above only binds the `.SF`
                // to the signer. The full jarsigner trust chain is
                // signature -> .SF -> MANIFEST.MF -> per-entry digest ->
                // bytes. Without the last two links, an attacker can swap a
                // signed JAR's class body (leaving MANIFEST/.SF/.RSA intact)
                // and still surface the original signer's certificate.
                // `verify_signed_entries` re-reads MANIFEST.MF and every
                // entry it commits to, computes the named digest, and
                // rejects the whole signer block on any mismatch — so the
                // certs are dropped (CodeSource reported as unsigned),
                // matching HotSpot's "fails verify" behaviour.
                if let Some(entries) = Self::verify_signed_entries(&mut guard, &sf_bytes) {
                    for cert in vs.chain {
                        out.push(cert);
                    }
                    // V3: record exactly which entries this verified signer
                    // committed to, so cert attachment can be gated per class.
                    for (entry_name, alg, expected) in entries {
                        signed_entries.entry(entry_name).or_insert((alg, expected));
                    }
                } else {
                    debug!(
                        "jar signer: signer block {} verified but a manifest \
                         entry digest did not match — dropping certs",
                        name
                    );
                }
            }
        }
        // V3: if no signer block fully verified, `out` is empty; the
        // `signed_entries` map is then irrelevant (no chain to attach) and is
        // returned empty too. An archive with a verified chain but a class
        // absent from `signed_entries` will be reported unsigned per class.
        JarSignerInfo {
            chain: out,
            signed_entries,
        }
    }

    /// V3 (unsigned-entry attack, JAR spec §"Signature Validation"):
    /// decide whether the archive-level signer `chain` may be attached to
    /// the specific class entry `relative_path`.
    ///
    /// The chain is returned **only** when `relative_path` is one of the
    /// entries the verified manifest committed to (recorded in
    /// `info.signed_entries`) AND the class's *current* bytes still hash to
    /// the digest the signer authenticated. Any other case — the entry is
    /// not named in the manifest, has no digest, or its bytes were swapped
    /// after signing — is treated as **unsigned** and yields an empty cert
    /// list, so `Class.getCodeSource().getCertificates()` is empty/null,
    /// matching HotSpot's per-entry `CodeSigner` behaviour.
    ///
    /// Re-hashing here (rather than trusting the one-time
    /// `verify_signed_entries` pass) is cheap — a single SHA over the class
    /// we are about to load anyway — and is robust against any in-memory
    /// cache staleness; it also means the per-archive `info` can be cached
    /// while the decision stays per class.
    ///
    /// `mr_versions` is `Some(versions_cache)` for a `Multi-Release: true`
    /// JAR. In that case the *served* entry may be a
    /// `META-INF/versions/<N>/<class>` override, and it is THAT entry name
    /// (not the base `relative_path`) the signer must have committed to —
    /// otherwise a versioned override could be smuggled in unsigned while
    /// inheriting the base entry's certs. We resolve the served name with
    /// the same descending search `find_in_multi_release_archive` uses.
    ///
    /// # TRUST BOUNDARY: this is the per-entry gate
    ///
    /// Archive-level verification is necessary but nowhere near sufficient.
    /// "The JAR is signed" and "this class is signed" are different claims,
    /// and only the second one licenses attaching `info.chain`. Every path
    /// out of this function that is not the final `info.chain.clone()`
    /// returns an **empty** vector — no chain, no signer, unsigned — and
    /// that is the fail-closed default for: an unverified archive, an entry
    /// the manifest never named, an entry the manifest named without a
    /// digest, an unreadable entry, a bytes-vs-digest mismatch, and (for a
    /// multi-release JAR) a versioned override the signer did not commit
    /// to. The last of those matters on its own: without resolving the
    /// *served* entry name first, an unsigned `META-INF/versions/<N>/`
    /// override would inherit the base entry's certificates.
    fn certs_for_signed_class(
        info: &JarSignerInfo,
        archive: &Mutex<SharedArchive>,
        relative_path: &str,
        mr_versions: Option<&Mutex<Option<Arc<BTreeSet<u32>>>>>,
    ) -> Vec<Vec<u8>> {
        // Unsigned JAR / failed verification: nothing to attach.
        if info.chain.is_empty() {
            return Vec::new();
        }

        // Resolve the entry name that actually serves this class. For a
        // multi-release JAR the highest present versioned override wins; the
        // base entry is the fallback. This mirrors
        // `find_in_multi_release_archive` so the entry we digest-check is the
        // one we hand to the classloader.
        let served_name: String = match mr_versions {
            Some(versions_cache) => {
                let present = Self::ensure_versions_cache(archive, versions_cache);
                let mut resolved: Option<String> = None;
                for &ver in present.range(9..=JVM_FEATURE_VERSION).rev() {
                    let versioned = format!("META-INF/versions/{ver}/{relative_path}");
                    // Existence only — the bytes are re-read below once the
                    // manifest has committed to the resolved name, so
                    // inflating here would decompress the entry twice.
                    if Self::archive_has_entry(archive, &versioned) {
                        resolved = Some(versioned);
                        break;
                    }
                }
                resolved.unwrap_or_else(|| relative_path.to_string())
            }
            None => relative_path.to_string(),
        };

        // The served entry must be one the signer explicitly committed to.
        let Some((alg, expected)) = info.signed_entries.get(&served_name) else {
            // Present in a signed JAR but NOT named (with a digest) in the
            // manifest → unsigned entry. Do not inherit the signer's certs.
            debug!(
                "jar signer: class entry {} is not committed-to by the signed \
                 manifest — reporting CodeSource as unsigned",
                served_name
            );
            return Vec::new();
        };
        // Re-read and re-hash the exact bytes we are about to load.
        let Some(bytes) = Self::find_in_archive(archive, &served_name) else {
            return Vec::new();
        };
        if crate::jar_signer::digest_matches(*alg, &bytes, expected) {
            info.chain.clone()
        } else {
            // Manifest names this entry but the on-disk bytes don't match the
            // signed digest (post-sign tamper) → unsigned.
            debug!(
                "jar signer: class entry {} digest mismatch vs signed manifest \
                 — reporting CodeSource as unsigned",
                served_name
            );
            Vec::new()
        }
    }

    /// V1: bind a verified signer to the actual entry bytes.
    ///
    /// Given the already signature-verified `.SF` bytes, this:
    ///   1. reads `META-INF/MANIFEST.MF` from the archive,
    ///   2. confirms the `.SF`'s `<alg>-Digest-Manifest` matches the
    ///      digest of that `MANIFEST.MF` (so the manifest is the one the
    ///      signer committed to), and
    ///   3. for every per-entry section in the manifest, re-reads the
    ///      named entry and confirms its bytes hash to the manifest's
    ///      `<alg>-Digest` value.
    ///
    /// Returns `Some(entries)` only when the manifest is bound by the `.SF`
    /// **and** every declared entry digest matches; each tuple is
    /// `(entry_name, algorithm, expected_digest)` for an entry the signer
    /// committed to. Any missing manifest, missing committed entry,
    /// unreadable entry, or digest mismatch returns `None` (fail-closed).
    /// Directory entries (`Name:` ending in `/`) carry no digest and are
    /// skipped by the parser.
    ///
    /// V3: the returned list is the authoritative set of *signed* entries
    /// for this signer. `find_class_code_source_info` uses it to attach the
    /// signer chain per class — entries absent from this list are unsigned
    /// even though they live inside the signed JAR (closing the JAR-spec
    /// unsigned-entry attack, where an injected `.class` not named in the
    /// manifest would otherwise inherit the signer's certificates).
    ///
    /// # TRUST BOUNDARY: INTEGRITY, conditional on the caller
    ///
    /// Every check here is a digest comparison — no key, no certificate.
    /// It is meaningful only because the caller has *already* obtained a
    /// `Some(_)` from [`crate::jar_signer::verify_signer_block`] for these
    /// exact `sf_bytes`; that is what turns "the manifest matches the `.SF`"
    /// into "the manifest matches what a verified signer committed to".
    /// Called with an unverified `.SF` this function would happily confirm
    /// an attacker's own manifest, so the ordering in
    /// [`Self::extract_jar_signer_blocks`] is load-bearing, not stylistic.
    ///
    /// The returned list is also **exhaustive by omission**: an archive
    /// entry with no manifest section simply does not appear, and callers
    /// must read "absent" as "unsigned".
    #[allow(clippy::type_complexity)]
    fn verify_signed_entries(
        archive: &mut SharedArchive,
        sf_bytes: &[u8],
    ) -> Option<Vec<(String, crate::jar_signer::DigestAlg, Vec<u8>)>> {
        // (1) Read MANIFEST.MF (streaming-bounded against zip-bombs).
        let manifest_bytes = match archive.by_name("META-INF/MANIFEST.MF").and_then(|mut e| {
            let size = e.size();
            Ok(read_entry_capped(&mut e, size)?)
        }) {
            Ok(d) => d,
            Err(_) => return None,
        };

        // (2) The `.SF` must commit to this exact MANIFEST.MF.
        if !crate::jar_signer::verify_sf_binds_manifest(sf_bytes, &manifest_bytes) {
            return None;
        }

        // (3) Every entry the manifest declares a digest for must match.
        let declared = crate::jar_signer::parse_manifest_entry_digests(&manifest_bytes);
        let mut verified: Vec<(String, crate::jar_signer::DigestAlg, Vec<u8>)> =
            Vec::with_capacity(declared.len());
        for entry in &declared {
            // Defence-in-depth: never let a manifest `Name:` smuggle a
            // traversal/drive-letter key into the lookup.
            if !is_safe_entry_name(&entry.name) {
                return None;
            }
            let bytes = match archive.by_name(&entry.name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) => d,
                // A manifest that signs an entry which is absent or
                // unreadable is a tampered/broken JAR — fail-closed.
                Err(_) => return None,
            };
            if !crate::jar_signer::digest_matches(entry.alg, &bytes, &entry.expected) {
                return None;
            }
            // V3: this entry is committed-to by the signer and its bytes
            // match — record it as a genuinely signed entry.
            verified.push((entry.name.clone(), entry.alg, entry.expected.clone()));
        }
        Some(verified)
    }

    /// Find a raw resource file by its classpath-relative name.
    ///
    /// The `resource_name` is a forward-slash-separated path (e.g., `scrabble.txt`
    /// or `org/renaissance/jdk/streams/data.txt`). Leading slashes are stripped.
    /// Searches all classpath entries in order; returns `Some(bytes)` on first match.
    pub fn find_resource(&self, resource_name: &str) -> Option<Vec<u8>> {
        diag_resource_call_wrapper("find_resource", || self.find_resource_impl(resource_name))
    }

    /// Test resource membership without reading or inflating its contents.
    ///
    /// VM bootstrap uses this to select optional native compatibility packs.
    /// Keeping the operation existence-only avoids turning a few classpath
    /// witnesses into archive decompression and allocation during startup.
    pub fn contains_resource(&self, resource_name: &str) -> bool {
        let name = resource_name.trim_start_matches('/');
        if !is_safe_resource_name(name) {
            return false;
        }
        self.entries.iter().any(|entry| match entry {
            ClassPathEntry::Directory(dir) => {
                let full_path = dir.join(Path::new(name));
                full_path.is_file()
                    && self
                        .checked_directory_resource_canonical(dir, &full_path, name)
                        .is_some()
            }
            ClassPathEntry::JarFile { entry_index, .. }
            | ClassPathEntry::NestedJar { entry_index, .. } => entry_index.contains(name),
            ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                entries_cache.contains_key(name)
            }
            ClassPathEntry::JmodFile {
                class_entry_index,
                all_entry_names,
                ..
            } => {
                let class_name = name.strip_suffix(".class");
                class_name.is_some_and(|class_name| class_entry_index.contains(class_name))
                    || all_entry_names.iter().any(|entry| {
                        entry == name
                            || entry
                                .strip_prefix("classes/")
                                .is_some_and(|entry| entry == name)
                    })
            }
            ClassPathEntry::JImageFile {
                class_to_module,
                resource_to_modules,
                ..
            } => {
                name.strip_suffix(".class")
                    .is_some_and(|class_name| class_to_module.contains_key(class_name))
                    || resource_to_modules.contains_key(name)
            }
        })
    }

    fn find_resource_impl(&self, resource_name: &str) -> Option<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        // Path safety: align with `find_class`'s input filter (rejects `..`,
        // NUL, leading slashes, `\\`, drive letters `:`, and `./` / `.\\`)
        // via the shared `is_safe_resource_name` helper. The canonicalize
        // check below remains the authoritative backstop.
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return None;
        }

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if !directory_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for full_path in Self::matching_directory_resource_paths(dir, name) {
                            if self
                                .checked_directory_resource_canonical(dir, &full_path, name)
                                .is_none()
                            {
                                continue;
                            }
                            if let Ok(data) = read_file_for_classpath(&full_path) {
                                debug!(
                                    "Found resource glob {name} in directory {} as {}",
                                    dir.display(),
                                    full_path.display()
                                );
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    let full_path = dir.join(Path::new(name));
                    if full_path.exists() {
                        // C35 audit fix (HIGH security): mirror
                        // `find_class`'s fail-CLOSED canonicalize
                        // contract. Previously the
                        // `if let (Ok(canon_dir), Ok(canon_path))` block
                        // *silently fell through* on canonicalize error
                        // and the un-canonicalized `full_path` was still
                        // read — an attacker who could plant a symlink
                        // that causes `fs::canonicalize` to fail (e.g.
                        // dangling symlink, permission-denied on a
                        // segment, or a Windows reparse point we can't
                        // resolve) could load arbitrary files via
                        // `getResourceAsStream`, even though the
                        // matching `find_class` path correctly rejected
                        // them. Now: on canonicalize failure we skip
                        // this entry (`continue`) so other classpath
                        // entries may still answer the probe, but this
                        // specific filesystem read does NOT proceed.
                        let canon_dir = match self.canonicalize_root(dir) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!(
                                    "Refusing to read resource {name}: cannot \
                                     canonicalize classpath root {}: {e}",
                                    dir.display()
                                );
                                continue;
                            }
                        };
                        let canon_path = match self.canonicalize_cached(&full_path) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!(
                                    "Refusing to read resource {name}: cannot \
                                     canonicalize resolved path {}: {e}",
                                    full_path.display()
                                );
                                continue;
                            }
                        };
                        if !canon_path.starts_with(&canon_dir) {
                            debug!(
                                "Resource path traversal blocked: {} escapes {}",
                                canon_path.display(),
                                canon_dir.display()
                            );
                            return None;
                        }
                        // Use the stable classpath read helper for resources
                        // too; a file that changes mid-read is skipped.
                        if let Ok(data) = read_file_for_classpath(&full_path) {
                            debug!("Found resource {name} in directory {}", dir.display());
                            return Some(data);
                        }
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates = Self::matching_resource_entry_names(
                            entry_index
                                .iter()
                                .filter(|entry| !entry.starts_with("META-INF/versions/")),
                            name,
                        );
                        for candidate in candidates {
                            let found = if *multi_release {
                                Self::find_in_multi_release_archive(
                                    archive,
                                    versions_cache,
                                    Some(entry_index),
                                    &candidate,
                                )
                            } else {
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            };
                            if let Some(data) = found {
                                debug!("Found resource glob {name} in JAR as {candidate}");
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            name,
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, name)
                    };
                    if let Some(data) = found {
                        debug!("Found resource {name} in JAR");
                        return Some(data);
                    }
                    // Slash-tolerant retry: HotSpot resolves a `cnf` request
                    // against a `cnf/` directory entry inside a JAR. Mirror
                    // that so `getResource(name)` is non-null for known
                    // archive subdirectories (DaCapo's bench loader does
                    // `getResource("cnf").getProtocol()` with no null check).
                    if !name.ends_with('/') {
                        let alt = format!("{name}/");
                        let alt_found = if *multi_release {
                            Self::find_in_multi_release_archive(
                                archive,
                                versions_cache,
                                Some(entry_index),
                                &alt,
                            )
                        } else {
                            Self::find_in_indexed_archive(archive, entry_index, &alt)
                        };
                        if let Some(data) = alt_found {
                            debug!("Found resource {alt} in JAR (slash-tolerant)");
                            return Some(data);
                        }
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(entries_cache.keys(), name);
                        if let Some(candidate) = candidates.first() {
                            if let Some(data) = entries_cache.get(candidate) {
                                debug!(
                                    "Found resource glob {name} in nested directory as {candidate}"
                                );
                                return Some(data.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(data) = entries_cache.get(name) {
                        debug!("Found resource {name} in nested directory");
                        return Some(data.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    nested_path,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(entry_index.iter(), name);
                        for candidate in candidates {
                            if let Some(data) =
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            {
                                debug!(
                                    "Found resource glob {name} in nested JAR {nested_path} as {candidate}"
                                );
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    if let Some(data) = Self::find_in_indexed_archive(archive, entry_index, name) {
                        debug!("Found resource {name} in nested JAR {nested_path}");
                        return Some(data);
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(class_entry_index.iter(), name);
                        if let Some(candidate) = candidates.first() {
                            if let Some(data) = Self::jmod_class_bytes(
                                archive,
                                backing,
                                class_entry_index,
                                candidate,
                            ) {
                                debug!(
                                    "Found resource glob {name} in JMOD {} as {candidate}",
                                    path.display()
                                );
                                return Some(data.to_vec());
                            }
                        }
                        continue;
                    }
                    // The `classes/` subtree covers both .class files and the
                    // resources that ship inside the module.
                    if let Some(data) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, name)
                    {
                        debug!("Found resource {name} in JMOD {}", path.display());
                        return Some(data.to_vec());
                    }
                    // Fall back to archive for non-class entries
                    let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                    if let Some(data) = Self::find_in_archive(archive, &jmod_name) {
                        debug!("Found resource {name} in JMOD {}", path.display());
                        return Some(data);
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    resource_to_modules,
                    class_to_module,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    // Classes live in `class_to_module`; non-class
                    // resources live in `resource_to_modules`. Try the
                    // appropriate map based on the file extension.
                    let (full_path_attempts, is_class): (Vec<String>, bool) =
                        if let Some(class_name) = name.strip_suffix(".class") {
                            if let Some(module) = class_to_module.get(class_name) {
                                (vec![format!("/{module}/{class_name}.class")], true)
                            } else {
                                (Vec::new(), true)
                            }
                        } else {
                            // Non-class resource: try every module that
                            // contains this path.
                            match resource_to_modules.get(name) {
                                Some(modules) => (
                                    modules.iter().map(|m| format!("/{m}/{name}")).collect(),
                                    false,
                                ),
                                None => (Vec::new(), false),
                            }
                        };
                    let _ = is_class;
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(resource_to_modules.keys(), name);
                        for candidate in candidates {
                            if let Some(modules) = resource_to_modules.get(&candidate) {
                                for module in modules {
                                    let attempt = format!("/{module}/{candidate}");
                                    match reader.find_resource(&attempt) {
                                        Ok(Some(bytes)) => {
                                            debug!(
                                                "Found resource glob {name} in jimage {} at {attempt}",
                                                path.display()
                                            );
                                            return Some(bytes);
                                        }
                                        Ok(None) => continue,
                                        Err(_) => continue,
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    for attempt in full_path_attempts {
                        match reader.find_resource(&attempt) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found resource {name} in jimage {} at {attempt}",
                                    path.display()
                                );
                                return Some(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }

        None
    }

    /// Find EVERY classpath entry that contains a resource with the given name
    /// and return a URL string for each match. This is the analogue of the
    /// JDK's `ClassLoader.getResources` enumeration — it walks every entry in
    /// order rather than stopping at the first hit.
    ///
    /// Returned URLs are strings of the form `file:/path/to/dir/<name>` for
    /// directory entries and `jar:file:/path/to/foo.jar!/<name>` for JAR /
    /// nested-JAR / JMOD / jimage entries. Callers can feed these into
    /// `java.net.URL` directly.
    /// Return the raw bytes of every classpath entry that contains a
    /// resource with the given name. Parallel to [`find_all_resource_urls`]
    /// but returns content rather than URL strings — used by Rust-side
    /// resource enumeration paths (e.g. `ServiceLoader` provider discovery)
    /// that want to bypass the JDK's `URL.openStream` / `BufferedReader`
    /// chain.
    pub fn find_all_resource_bytes(&self, resource_name: &str) -> Vec<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        // Same input filter as `find_class`/`find_resource` (see
        // `is_safe_resource_name`).
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return Vec::new();
        }
        let mut out: Vec<Vec<u8>> = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if !directory_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for full_path in Self::matching_directory_resource_paths(dir, name) {
                            if self
                                .checked_directory_resource_canonical(dir, &full_path, name)
                                .is_none()
                            {
                                continue;
                            }
                            if let Ok(bytes) = read_file_for_classpath(&full_path) {
                                out.push(bytes);
                            }
                        }
                        continue;
                    }
                    let full_path = dir.join(Path::new(name));
                    // C35 audit fix (HIGH security): fail-CLOSED on
                    // canonicalize error. The previous
                    // `if let (Ok, Ok)` silently fell through to the
                    // unchecked `read_file_for_classpath` call below,
                    // matching the `find_resource` fail-open hole. A
                    // symlink an attacker can plant such that one of
                    // these canonicalize calls fails is now skipped
                    // rather than read.
                    let canon_dir = match self.canonicalize_root(dir) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let canon_path = match self.canonicalize_cached(&full_path) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    if !canon_path.starts_with(&canon_dir) {
                        continue;
                    }
                    // Same stable read helper as the single-resource path.
                    if let Ok(bytes) = read_file_for_classpath(&full_path) {
                        out.push(bytes);
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates = Self::matching_resource_entry_names(
                            entry_index
                                .iter()
                                .filter(|entry| !entry.starts_with("META-INF/versions/")),
                            name,
                        );
                        for candidate in candidates {
                            let bytes = if *multi_release {
                                Self::find_in_multi_release_archive(
                                    archive,
                                    versions_cache,
                                    Some(entry_index),
                                    &candidate,
                                )
                            } else {
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            };
                            if let Some(b) = bytes {
                                out.push(b);
                            }
                        }
                        continue;
                    }
                    let bytes = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            name,
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, name)
                    };
                    if let Some(b) = bytes {
                        out.push(b);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(entries_cache.keys(), name)
                        {
                            if let Some(b) = entries_cache.get(&candidate) {
                                out.push(b.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(b) = entries_cache.get(name) {
                        out.push(b.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(entry_index.iter(), name)
                        {
                            if let Some(b) =
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            {
                                out.push(b);
                            }
                        }
                        continue;
                    }
                    if let Some(b) = Self::find_in_indexed_archive(archive, entry_index, name) {
                        out.push(b);
                    }
                }
                ClassPathEntry::JmodFile {
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(class_entry_index.iter(), name)
                        {
                            if let Some(b) = Self::jmod_class_bytes(
                                archive,
                                backing,
                                class_entry_index,
                                &candidate,
                            ) {
                                out.push(b.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(b) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, name)
                    {
                        out.push(b.to_vec());
                    } else {
                        let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                        if let Some(b) = Self::find_in_archive(archive, &jmod_name) {
                            out.push(b);
                        }
                    }
                }
                ClassPathEntry::JImageFile {
                    reader,
                    resource_to_modules,
                    class_to_module,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    let attempts: Vec<String> =
                        if let Some(class_name) = name.strip_suffix(".class") {
                            if let Some(module) = class_to_module.get(class_name) {
                                vec![format!("/{module}/{class_name}.class")]
                            } else {
                                Vec::new()
                            }
                        } else {
                            match resource_to_modules.get(name) {
                                Some(modules) => {
                                    modules.iter().map(|m| format!("/{m}/{name}")).collect()
                                }
                                None => Vec::new(),
                            }
                        };
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(resource_to_modules.keys(), name)
                        {
                            if let Some(modules) = resource_to_modules.get(&candidate) {
                                for module in modules {
                                    let attempt = format!("/{module}/{candidate}");
                                    if let Ok(Some(b)) = reader.find_resource(&attempt) {
                                        out.push(b);
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    for attempt in attempts {
                        if let Ok(Some(b)) = reader.find_resource(&attempt) {
                            out.push(b);
                        }
                    }
                }
            }
        }
        out
    }

    /// Build the `file:`-path segment of a reconstructed `jar:file:...!/...`
    /// resource URL for a `NestedDirectory`/`NestedJar` entry's `parent_jar`.
    ///
    /// `NestedDirectory`/`NestedJar` entries reached via `add_path`'s
    /// DaCapo-style `<jar>!/<prefix>/` handoff (see its doc comment) carry a
    /// `parent_jar` that is the UNTOUCHED substring of whatever raw `jar:`
    /// URL string the caller built — `extract_url_path` in
    /// `native-builtins/src/classloader.rs` strips only a spurious leading
    /// `/` before a Windows drive letter (for `PathBuf` resolvability) and
    /// otherwise never rewrites separators. Real Java's `URL`/`URLClassLoader`
    /// machinery is equally hands-off: `new URL(String)` never normalizes,
    /// so a caller-built `"jar:file:" + file.getAbsolutePath() + "!/..."`
    /// (Windows: backslash-separated, no leading `/`) round-trips through
    /// HotSpot byte-for-byte. Forcibly rewriting `parent_jar` to a canonical
    /// forward-slash `/C:/...` form here — as this used to do unconditionally
    /// — produced a URL that differed from HotSpot's for exactly that raw,
    /// backslash-spelled case (Spring Boot's
    /// `TomcatEmbeddedWebappClassLoaderTests`, which builds the expected URL
    /// via `File.getAbsolutePath()` directly, no `toURI()`).
    ///
    /// A backslash anywhere in `parent_jar` can only mean it came from that
    /// raw, unnormalized path (real on-disk jar discovery always resolves
    /// through `canonicalize_cached` into the `JarFile`/`Directory` variants,
    /// not this one) — preserve it verbatim. Otherwise fall back to the
    /// historical canonical-forward-slash form.
    fn nested_jar_url_path(parent_jar: &Path) -> std::borrow::Cow<'_, str> {
        let raw = parent_jar.to_string_lossy();
        if raw.contains('\\') {
            raw
        } else {
            let p = raw.trim_start_matches('/');
            std::borrow::Cow::Owned(format!("/{p}"))
        }
    }

    /// Every URL a SINGLE classpath entry serves for `name`.
    ///
    /// Split out of [`Self::find_all_resource_urls_impl`] so the whole-list
    /// walk and the incremental walk ([`Self::next_resource_url_from`]) run
    /// the same per-entry rules instead of two copies that can drift. The
    /// `continue`s that used to mean "next entry" are `return`s here; nothing
    /// else moved.
    fn resource_urls_for_entry(
        &self,
        entry: &ClassPathEntry,
        name: &str,
        archive_safe: bool,
        directory_safe: bool,
        dbg: bool,
    ) -> Vec<String> {
        let mut urls = Vec::new();
        match entry {
            ClassPathEntry::Directory(dir) => {
                if !directory_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    for full_path in Self::matching_directory_resource_paths(dir, name) {
                        if let Some(canon_path) =
                            self.checked_directory_resource_canonical(dir, &full_path, name)
                        {
                            urls.push(Self::directory_resource_url_from_canonical(&canon_path));
                        }
                    }
                    return urls;
                }
                // `getResources("")` names the classpath root ITSELF, and
                // the generic path below answers it the expensive way:
                // `dir.join("")` is `dir` with a trailing separator, so it
                // costs an `exists()` statx, a `canonicalize` of `dir`, a
                // SECOND `canonicalize` of the trailing-slash spelling
                // (a distinct cache key), and an `is_dir()` statx — per
                // directory entry, per call.
                //
                // That is not a hypothetical shape. SmallRye Config's
                // `AbstractLocationConfigSourceLoader.isInClassloader` is
                // exactly `classLoader.resources(uri.getPath()).anyMatch(..)`
                // with an empty path, and Quarkus's config bootstrap runs
                // it once per discovered config source per profile — 1388
                // times over a 4230-entry classpath for ONE test class,
                // which is where 4.43M `statx` calls came from against
                // HotSpot's 11.9K for the same run.
                //
                // A `Directory` entry is a directory that is on the
                // classpath; both facts were established when it was
                // admitted. Re-deriving them from the filesystem on every
                // probe buys nothing, so answer from the memoized root.
                if name.is_empty() {
                    if let Ok(canon_dir) = self.canonicalize_root(dir) {
                        let p = canon_dir.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/').trim_end_matches('/');
                        urls.push(format!("file:/{}/", encode_path_for_url(p)));
                    }
                    return urls;
                }
                let full_path = dir.join(Path::new(name));
                if full_path.exists() {
                    // C35 audit fix (HIGH security): fail-CLOSED on
                    // canonicalize error. Previously the
                    // `if let (Ok, Ok)` silently fell through to the
                    // `urls.push(file:/...)` emission below — an
                    // attacker who could plant a symlink causing
                    // canonicalize to fail could leak the existence
                    // of arbitrary files via `getResources()`
                    // enumeration (even though the matching
                    // `find_class` path correctly rejected the same
                    // symlink).
                    let canon_dir = match self.canonicalize_root(dir) {
                        Ok(p) => p,
                        Err(_) => return urls,
                    };
                    let canon_path = match self.canonicalize_cached(&full_path) {
                        Ok(p) => p,
                        Err(_) => return urls,
                    };
                    if !canon_path.starts_with(&canon_dir) {
                        return urls;
                    }
                    // `canon_path` is the already-canonicalized
                    // resolved path; reuse it directly instead of
                    // re-canonicalizing (and avoid the
                    // `unwrap_or(full_path)` fallback that, prior to
                    // this fix, would have emitted the
                    // un-canonicalized path on canonicalize error).
                    let p = canon_path.to_string_lossy().replace('\\', "/");
                    let p = p.strip_prefix("//?/").unwrap_or(&p);
                    let p = p.trim_start_matches('/');
                    // A directory resource (e.g. a package path queried via
                    // `ClassLoader.getResources("com/example/pkg/")`) must
                    // keep its trailing slash — real `URLClassLoader`
                    // preserves it, and Spring's
                    // `PathMatchingResourcePatternResolver` relies on it:
                    // its `rootDirCache` collapses sibling directory scans
                    // onto a shared parent `Resource` and reconstructs
                    // child paths via `createRelative`/
                    // `StringUtils.applyRelativePath`, which treats a
                    // no-trailing-slash URL as a FILE path and strips the
                    // last segment when appending a relative child —
                    // silently resolving to a sibling directory instead of
                    // a subdirectory. That broke any SECOND differently-
                    // pathed scan against the same resolver instance, e.g.
                    // a `@ComponentScan`-discovered `@Configuration` class
                    // whose OWN `@ComponentScan` scans a sibling package
                    // (ComponentScanAnnotationRecursionTests, 2+ levels of
                    // recursive `@ComponentScan`). Matches the established
                    // unconditional-slash pattern in
                    // `find_class_code_source_info` above, but here it
                    // must be conditional since this function also serves
                    // plain (non-directory) resource lookups.
                    if canon_path.is_dir() && !p.ends_with('/') {
                        urls.push(format!("file:/{}/", encode_path_for_url(&p)));
                    } else {
                        urls.push(format!("file:/{}", encode_path_for_url(&p)));
                    }
                }
            }
            ClassPathEntry::JarFile {
                archive,
                multi_release,
                versions_cache,
                entry_index,
                path,
                ..
            } => {
                if !archive_safe {
                    if dbg {
                        eprintln!(
                            "[GRES-DBG]   jar {} mr={} -> skipped unsafe name",
                            path.display(),
                            multi_release
                        );
                    }
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let candidates = Self::matching_resource_entry_names(
                        entry_index
                            .iter()
                            .filter(|entry| !entry.starts_with("META-INF/versions/")),
                        name,
                    );
                    if dbg {
                        eprintln!(
                            "[GRES-DBG]   jar {} mr={} -> {}",
                            path.display(),
                            multi_release,
                            if candidates.is_empty() { "miss" } else { "HIT" }
                        );
                    }
                    if !candidates.is_empty() {
                        let abs = self
                            .canonicalize_root(path)
                            .unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/');
                        for candidate in candidates {
                            urls.push(format!(
                                "jar:file:/{}!/{candidate}",
                                encode_path_for_url(&p)
                            ));
                        }
                    }
                    return urls;
                }
                let direct_entry = if *multi_release {
                    Self::multi_release_entry_name(archive, versions_cache, entry_index, name)
                } else {
                    entry_index.contains(name).then(|| name.to_string())
                };
                // HotSpot's URLClassLoader matches a request for `cnf` against
                // a `cnf/` directory entry inside a JAR. Without the slash-
                // tolerant retry, `getResource("cnf")` returned null even when
                // the JAR clearly contains the directory, breaking DaCapo's
                // `extractBenchmarkSet` (which dereferences the URL's
                // protocol without a null check).
                let slash_entry = if direct_entry.is_none() && !name.ends_with('/') {
                    let alt = format!("{name}/");
                    if *multi_release {
                        Self::multi_release_entry_name(archive, versions_cache, entry_index, &alt)
                    } else {
                        entry_index.contains(&alt).then_some(alt)
                    }
                } else {
                    None
                };
                let selected_entry = direct_entry.or(slash_entry);
                if dbg {
                    eprintln!(
                        "[GRES-DBG]   jar {} mr={} -> {}",
                        path.display(),
                        multi_release,
                        if selected_entry.is_some() {
                            "HIT"
                        } else {
                            "miss"
                        }
                    );
                }
                // Bind the entry name by pattern rather than testing a
                // separate `found` bool and then `expect()`ing the same
                // Option: the two can only ever agree, but the file denies
                // `clippy::expect_used` outside tests, so the pair broke
                // `cargo clippy` for this crate and every crate that
                // depends on it. `None` means no entry matched, which is
                // exactly "push no URL" — the arm the bool already took.
                if let Some(suffix) = selected_entry {
                    let abs = self
                        .canonicalize_root(path)
                        .unwrap_or_else(|_| path.clone());
                    let p = abs.to_string_lossy().replace('\\', "/");
                    // Strip UNC prefix \\?\ that canonicalize produces on Windows.
                    let p = p.strip_prefix("//?/").unwrap_or(&p);
                    let p = p.trim_start_matches('/');
                    urls.push(format!("jar:file:/{}!/{suffix}", encode_path_for_url(&p)));
                }
            }
            ClassPathEntry::NestedDirectory {
                parent_jar,
                prefix,
                entries_cache,
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    for candidate in Self::matching_resource_entry_names(entries_cache.keys(), name)
                    {
                        urls.push(format!("jar:file:{p}!/{prefix}{candidate}"));
                    }
                    return urls;
                }
                if entries_cache.contains_key(name) {
                    let p = Self::nested_jar_url_path(parent_jar);
                    urls.push(format!("jar:file:{p}!/{prefix}{name}"));
                }
            }
            ClassPathEntry::NestedJar {
                parent_jar,
                archive,
                nested_path,
                entry_index,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    for candidate in Self::matching_resource_entry_names(entry_index.iter(), name) {
                        urls.push(format!("jar:nested:{p}/!{nested_path}!/{candidate}"));
                    }
                    return urls;
                }
                if Self::find_in_indexed_archive(archive, entry_index, name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    urls.push(format!("jar:nested:{p}/!{nested_path}!/{name}"));
                }
            }
            ClassPathEntry::JmodFile {
                path,
                class_entry_index,
                archive,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = path.to_string_lossy().replace('\\', "/");
                    let p = p.trim_start_matches('/');
                    for candidate in
                        Self::matching_resource_entry_names(class_entry_index.iter(), name)
                    {
                        urls.push(format!(
                            "jar:file:/{}!/{candidate}",
                            encode_path_for_url(&p)
                        ));
                    }
                    return urls;
                }
                let found = class_entry_index.contains(name) || {
                    let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                    // URL emission only — never the bytes. Inflating the
                    // entry here just to discard it made every
                    // `getResource` hit on a JMOD pay a full deflate.
                    Self::archive_has_entry(archive, &jmod_name)
                };
                if found {
                    let p = path.to_string_lossy().replace('\\', "/");
                    let p = p.trim_start_matches('/');
                    urls.push(format!("jar:file:/{}!/{name}", encode_path_for_url(&p)));
                }
            }
            ClassPathEntry::JImageFile {
                reader,
                resource_to_modules,
                class_to_module,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                let attempts: Vec<String> = if let Some(class_name) = name.strip_suffix(".class") {
                    if let Some(module) = class_to_module.get(class_name) {
                        vec![format!("/{module}/{class_name}.class")]
                    } else {
                        Vec::new()
                    }
                } else {
                    match resource_to_modules.get(name) {
                        Some(modules) => modules.iter().map(|m| format!("/{m}/{name}")).collect(),
                        None => Vec::new(),
                    }
                };
                if simple_resource_glob(name).is_some() {
                    for candidate in
                        Self::matching_resource_entry_names(resource_to_modules.keys(), name)
                    {
                        if let Some(modules) = resource_to_modules.get(&candidate) {
                            for module in modules {
                                let attempt = format!("/{module}/{candidate}");
                                if matches!(reader.find_resource(&attempt), Ok(Some(_))) {
                                    urls.push(format!("jrt:{attempt}"));
                                }
                            }
                        }
                    }
                    return urls;
                }
                for attempt in attempts {
                    if matches!(reader.find_resource(&attempt), Ok(Some(_))) {
                        // JEP 220 jrt URL scheme: `jrt:/<module>/<resource>`.
                        // The TOC entry path is already `/<module>/<resource>`,
                        // so emit it verbatim with the `jrt:` scheme prefix.
                        // HotSpot's BuiltinClassLoader.findMiscResource builds
                        // the same URL via JNUFileSystemProvider.
                        urls.push(format!("jrt:{attempt}"));
                    }
                }
            }
        }
        urls
    }

    /// `true` when a resource name can be served one entry at a time.
    ///
    /// A glob name (`simple_resource_glob`) can match SEVERAL names inside a
    /// single entry, so "the first URL this entry serves" would silently drop
    /// the rest. Every other name yields at most one URL per entry, which is
    /// what makes the incremental walk equivalent to the whole-list one.
    pub fn name_supports_incremental_scan(resource_name: &str) -> bool {
        simple_resource_glob(resource_name.trim_start_matches('/')).is_none()
    }

    /// The next URL at or after entry `from`, and the entry index to resume at.
    ///
    /// The whole-list [`Self::find_all_resource_urls`] is the wrong shape for a
    /// caller that stops early, and `ClassLoader.resources(name).anyMatch(..)`
    /// — SmallRye Config's `isInClassloader`, and every `findFirst` over
    /// `resources()` — stops at the first match by construction. The JDK's own
    /// enumeration is lazy per element, which is why HotSpot answers such a
    /// call in 0.03 ms against 1.20 ms here for a 4230-entry classpath: it
    /// touches one entry, we touched all of them.
    ///
    /// Callers must gate on [`Self::name_supports_incremental_scan`].
    pub fn next_resource_url_from(
        &self,
        resource_name: &str,
        from: usize,
    ) -> Option<(String, usize)> {
        let name = resource_name.trim_start_matches('/');
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return None;
        }
        let dbg = dbg_getresources();
        for (index, entry) in self.entries.iter().enumerate().skip(from) {
            let urls = self.resource_urls_for_entry(entry, name, archive_safe, directory_safe, dbg);
            if let Some(url) = urls.into_iter().next() {
                return Some((url, index + 1));
            }
        }
        None
    }

    pub fn find_all_resource_urls(&self, resource_name: &str) -> Vec<String> {
        diag_resource_call_wrapper("find_all_resource_urls", || {
            self.find_all_resource_urls_impl(resource_name)
        })
    }

    fn find_all_resource_urls_impl(&self, resource_name: &str) -> Vec<String> {
        let name = resource_name.trim_start_matches('/');
        // Same input filter as `find_class`/`find_resource` (see
        // `is_safe_resource_name`).
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return Vec::new();
        }
        // ES2-DBG: env-gated tracing for the classpath resource walk.
        // `CRATONVM_DBG_GETRESOURCES=1` emits per-entry hit/miss for every
        // call. Each `ClassPath` instance (bootstrap / extension /
        // application) calls this independently, so a real probe prints
        // one block per loader — useful for spotting whether a specific
        // jar is missing from the application classpath entirely vs
        // simply lacking the resource.
        let dbg = dbg_getresources();
        if dbg {
            eprintln!(
                "[GRES-DBG] find_all_resource_urls({}) — scanning {} entries",
                name,
                self.entries.len()
            );
        }
        let mut urls = Vec::new();
        for entry in &self.entries {
            urls.extend(self.resource_urls_for_entry(
                entry,
                name,
                archive_safe,
                directory_safe,
                dbg,
            ));
        }
        if dbg {
            eprintln!(
                "[GRES-DBG] find_all_resource_urls({}) -> {} URLs",
                name,
                urls.len()
            );
        }
        urls
    }

    /// Scan every JAR/directory for `module-info.class` files and return
    /// their raw bytes.  A JAR may contain at most one `module-info.class`
    /// at its root; a directory may contain one too.
    ///
    /// Used by [`ClassManager`](super::ClassManager) during initialisation to
    /// pre-populate the module registry before any class is loaded.
    pub fn scan_module_infos(&self) -> Vec<Vec<u8>> {
        let mut results = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let path = dir.join("module-info.class");
                    // Keep directory module-info reads on the same stable
                    // classpath file helper.
                    if let Ok(data) = read_file_for_classpath(&path) {
                        debug!("Found module-info.class in directory {}", dir.display());
                        results.push(data);
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            "module-info.class",
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, "module-info.class")
                    };
                    if let Some(data) = found {
                        debug!("Found module-info.class in JAR");
                        results.push(data);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get("module-info.class") {
                        debug!("Found module-info.class in nested directory");
                        results.push(data.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    nested_path,
                    entry_index,
                    ..
                } => {
                    if let Some(data) =
                        Self::find_in_indexed_archive(archive, entry_index, "module-info.class")
                    {
                        debug!("Found module-info.class in nested JAR {nested_path}");
                        results.push(data);
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if let Some(data) = Self::jmod_class_bytes(
                        archive,
                        backing,
                        class_entry_index,
                        "module-info.class",
                    ) {
                        debug!("Found module-info.class in JMOD {}", path.display());
                        results.push(data.to_vec());
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    module_names,
                    ..
                } => {
                    // One module-info per module inside the jimage. Ask
                    // the reader for `/<module>/module-info.class` for
                    // every known module name.
                    for module in module_names {
                        let query = format!("/{module}/module-info.class");
                        match reader.find_resource(&query) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found module-info.class for {module} in jimage {}",
                                    path.display()
                                );
                                results.push(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
        results
    }

    /// List all class names available on this classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`) for every
    /// `.class` file found across all entries. `module-info` is excluded.
    pub fn list_class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    Self::walk_dir_classes(dir, dir, &mut names);
                }
                ClassPathEntry::JarFile { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    for key in entries_cache.keys() {
                        if let Some(cn) = Self::class_file_to_name(key) {
                            names.push(cn);
                        }
                    }
                }
                ClassPathEntry::NestedJar { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::JmodFile {
                    all_entry_names, ..
                } => {
                    for name in all_entry_names {
                        if let Some(class_path) = name.strip_prefix("classes/") {
                            if let Some(cn) = Self::class_file_to_name(class_path) {
                                names.push(cn);
                            }
                        }
                    }
                }
                ClassPathEntry::JImageFile {
                    class_to_module, ..
                } => {
                    // Every key in the class-to-module index is exactly
                    // a binary class name.
                    for name in class_to_module.keys() {
                        names.push(name.clone());
                    }
                }
            }
        }
        names
    }

    /// Return the list of JMOD module names on this classpath.
    ///
    /// Module names are extracted from the JMOD file name stem
    /// (e.g. `java.base.jmod` → `"java.base"`).
    /// Only `JmodFile` entries contribute; JARs and directories are skipped.
    pub fn list_jmod_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            if let ClassPathEntry::JmodFile { path, .. } = entry {
                if let Some(stem) = path.file_stem() {
                    modules.push(stem.to_string_lossy().into_owned());
                }
            }
        }
        modules
    }

    /// Return the total number of classes available in JMOD entries.
    pub fn jmod_class_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::JmodFile {
                    class_entry_index, ..
                } => class_entry_index.len(),
                _ => 0,
            })
            .sum()
    }

    /// Return the number of classes known to the jimage entries on this
    /// classpath. Like `jmod_class_count`, this is O(1) per entry since
    /// the class-to-module index was built at load time.
    pub fn jimage_class_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::JImageFile {
                    class_to_module, ..
                } => class_to_module.len(),
                _ => 0,
            })
            .sum()
    }

    /// List the module names contributed by jimage entries on this
    /// classpath. Deduplicated, sorted alphabetically.
    pub fn list_jimage_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            if let ClassPathEntry::JImageFile { module_names, .. } = entry {
                for m in module_names {
                    if !modules.contains(m) {
                        modules.push(m.clone());
                    }
                }
            }
        }
        modules.sort();
        modules
    }

    /// Load a JDK 9+ `lib/modules` jimage file onto this classpath.
    ///
    /// Opens the file via [`cratonvm_reader::JImageReader`], walks every
    /// entry once to build the class-to-module and resource-to-modules
    /// indexes, then inserts a [`ClassPathEntry::JImageFile`] onto the
    /// classpath. Lookups after this call return bytes directly from
    /// the jimage's perfect-hash index.
    ///
    /// Returns an error string if the file cannot be opened or is not
    /// a valid jimage. Callers that want a typed error should match on
    /// [`cratonvm_reader::JImageError`] via the direct reader API.
    pub fn add_jimage(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref().to_path_buf();
        let entry = Self::load_jimage(&path)?;
        push_classpath_entry(&mut self.entries, entry);
        Ok(())
    }

    /// Internal constructor for a [`ClassPathEntry::JImageFile`]. Reads
    /// the entire jimage, decodes every location record once, and
    /// builds the two lookup indexes. Returns a string error so the
    /// caller can surface it through the same error path as `load_jmod`.
    fn load_jimage(path: &Path) -> Result<ClassPathEntry, String> {
        let reader = cratonvm_reader::JImageReader::open(path)
            .map_err(|e| format!("open jimage {}: {e}", path.display()))?;

        // Walk every entry once to build the indexes.
        let entries = reader
            .iter_entries()
            .map_err(|e| format!("walk jimage {}: {e}", path.display()))?;

        let mut class_to_module: HashMap<String, String> = HashMap::new();
        let mut resource_to_modules: HashMap<String, Vec<String>> = HashMap::new();
        let mut module_set: std::collections::BTreeSet<String> = Default::default();

        for (full_path, _offset, _size) in entries {
            // Full paths look like "/module/a/b/C.class" or
            // "/module/some/resource.txt". Split off the leading
            // "/module/" prefix.
            let without_lead = match full_path.strip_prefix('/') {
                Some(s) => s,
                None => continue,
            };
            let (module, rest) = match without_lead.split_once('/') {
                Some((m, r)) => (m.to_string(), r.to_string()),
                None => continue,
            };
            module_set.insert(module.clone());

            if let Some(class_name) = rest.strip_suffix(".class") {
                // `module-info` is per-module, not a globally unique
                // class name. Skipping it from `class_to_module` avoids
                // one module's module-info silently overwriting another's
                // via the final insert. Callers that need module-info
                // bytes go through `scan_module_infos`, which walks the
                // module list directly and queries the reader for each.
                if class_name != "module-info" {
                    class_to_module.insert(class_name.to_string(), module);
                }
            } else {
                resource_to_modules.entry(rest).or_default().push(module);
            }
        }

        debug!(
            "Loaded jimage {}: {} classes, {} non-class resources, {} modules",
            path.display(),
            class_to_module.len(),
            resource_to_modules.len(),
            module_set.len()
        );

        Ok(ClassPathEntry::JImageFile {
            path: path.to_path_buf(),
            reader,
            class_to_module,
            resource_to_modules,
            module_names: module_set.into_iter().collect(),
        })
    }

    fn walk_dir_classes(base: &Path, dir: &Path, out: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    Self::walk_dir_classes(base, &path, out);
                } else if path.extension().is_some_and(|e| e == "class") {
                    if let Ok(rel) = path.strip_prefix(base) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        if let Some(cn) = Self::class_file_to_name(&s) {
                            out.push(cn);
                        }
                    }
                }
            }
        }
    }

    fn list_archive_classes(archive: &Mutex<SharedArchive>, out: &mut Vec<String>) {
        let mut guard = archive.lock();
        for i in 0..guard.len() {
            if let Ok(entry) = guard.by_index_raw(i) {
                let name = entry.name().to_string();
                if let Some(cn) = Self::class_file_to_name(&name) {
                    out.push(cn);
                }
            }
        }
    }

    fn class_file_to_name(path: &str) -> Option<String> {
        let path = path.strip_suffix(".class")?;
        if path == "module-info" || path.ends_with("/module-info") {
            return None;
        }
        Some(path.to_string())
    }

    /// Does `name` exist in the archive?
    ///
    /// PERF (2026-07-26 classpath-scan audit): the existence-only callers used
    /// to spell this `find_in_archive(..).is_some()`, which **inflates the
    /// whole entry** and throws the bytes away — a full deflate pass (and, in
    /// debug builds, an extremely slow one) to answer a yes/no. `by_name`
    /// seeks the central-directory record and builds the reader without
    /// reading any compressed data, so dropping the reader immediately costs
    /// only the seek. The mutex is held for the same short window either way.
    fn archive_has_entry(archive: &Mutex<SharedArchive>, name: &str) -> bool {
        archive.lock().by_name(name).is_ok()
    }

    /// Helper: try to read a named entry from a mutex-guarded ZipArchive.
    fn find_in_archive(archive: &Mutex<SharedArchive>, name: &str) -> Option<Vec<u8>> {
        let mut guard = archive.lock();
        let result = guard.by_name(name).and_then(|mut zip_entry| {
            // Audit-fix #2 / zip-bomb: the declared `size()` comes from the
            // JAR central directory and is attacker-controlled (up to
            // `u64::MAX`). `read_entry_capped` clamps the pre-allocation
            // AND bounds the actual streaming inflate (V2), matching the
            // sibling entry-read paths. The `?` converts the `io::Error`
            // overflow into the `ZipError` the `and_then` expects.
            let size = zip_entry.size();
            Ok(read_entry_capped(&mut zip_entry, size)?)
        });
        match result {
            Ok(data) => Some(data),
            Err(_) => None,
        }
    }

    /// Read an entry as shared class bytes. Stored entries become a direct
    /// range view over the archive mapping; compressed entries are inflated
    /// once into reference-counted owned storage.
    fn find_shared_in_archive(
        archive: &Mutex<SharedArchive>,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        let mut guard = archive.lock();
        Self::find_shared_in_archive_locked(&mut guard, backing, name)
    }

    fn find_shared_in_archive_locked(
        archive: &mut SharedArchive,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        let mut entry = archive.by_name(name).ok()?;
        let size = entry.size();
        if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        if entry.compression() == zip::CompressionMethod::Stored {
            let start = usize::try_from(entry.data_start()).ok()?;
            let len = usize::try_from(size).ok()?;
            let end = start.checked_add(len)?;
            if end > backing.as_ref().len() {
                return None;
            }
            return Some(SharedBytes::from_external(ArchiveSlice {
                backing: backing.clone(),
                start,
                end,
            }));
        }
        read_entry_capped(&mut entry, size)
            .ok()
            .map(SharedBytes::from)
    }

    /// Serve one `classes/` entry out of a JMOD, inflating it on demand.
    ///
    /// `relative` is the name with the `classes/` prefix already stripped
    /// (e.g. `java/lang/Object.class`). The index probe comes first so a
    /// miss costs a hash lookup and no ZIP work at all — that is the
    /// property the old eager `classes_cache` was bought with 136 MB of
    /// inflated bytes, and it is exactly what `JarFile::entry_index` buys
    /// the JAR path for free.
    ///
    /// Returns `None` when the name is not in this JMOD, when the entry
    /// exceeds [`MAX_UNCOMPRESSED_ENTRY_BYTES`], or when the entry is
    /// corrupt — the same three cases in which the eager cache simply had
    /// no key, so every caller's fall-through behaviour is unchanged.
    fn jmod_class_bytes(
        archive: &Mutex<SharedArchive>,
        backing: &ArchiveBacking,
        class_entry_index: &FxHashSet<String>,
        relative: &str,
    ) -> Option<SharedBytes> {
        if !class_entry_index.contains(relative) {
            return None;
        }
        let mut name = String::with_capacity(JMOD_CLASSES_PREFIX.len() + relative.len());
        name.push_str(JMOD_CLASSES_PREFIX);
        name.push_str(relative);
        Self::find_shared_in_archive(archive, backing, &name)
    }

    /// JMOD file header prefix: `JM` (0x4A 0x4D).
    const JMOD_MAGIC_PREFIX: [u8; 2] = [0x4A, 0x4D];

    /// Known JMOD major versions we support. JDK 9-25 all use major version 1.
    const JMOD_SUPPORTED_MAJOR: u8 = 0x01;

    /// Maximum JMOD minor version we accept (currently 0, future-proofed to 0xFF).
    const JMOD_MAX_MINOR: u8 = 0xFF;

    /// Load a JDK 9+ JMOD file. JMOD is a ZIP archive with a 4-byte header:
    ///   - Bytes 0-1: Magic `JM` (0x4A 0x4D)
    ///   - Byte 2: Major version (currently 0x01 for all JDK 9-25)
    ///   - Byte 3: Minor version (currently 0x00)
    ///
    /// Class files inside are stored under `classes/` (e.g. `classes/java/lang/Object.class`).
    ///
    /// Every entry name is read from the central directory into
    /// `class_entry_index`; **no entry is inflated here**. Class bytes are
    /// produced on demand by [`Self::jmod_class_bytes`].
    ///
    /// PERF (2026-07-26 boot-classpath-lazy): this used to pre-extract every
    /// `classes/` entry into a `HashMap<String, SharedBytes>`. That made
    /// existence probes free but paid for it up front — on a stock JDK 25 the
    /// 70 JMODs `discover_boot_classpath` puts on the boot classpath meant
    /// 27,962 deflate passes and ~136 MB resident before the first Java class
    /// loaded. The name index gives the same free existence probe (that was
    /// the actual point of the cache: the old `find_in_archive(..).is_some()`
    /// callers inflated an entry to answer a yes/no, which is where the
    /// "~30 s for 200 classes" debug-build figure came from) at ~1 % of the
    /// memory and none of the up-front CPU. It mirrors `build_archive_entry_index`
    /// on the JAR path.
    ///
    /// Repeat requests for the same class do not re-inflate in practice:
    /// `ClassManager::class_bytes_cache` already memoizes class bytes behind a
    /// 16 MiB FIFO cap, which is the right place for that bound to live.
    fn load_jmod(path: &Path) -> Result<ClassPathEntry, String> {
        // JMOD files are ordinary classpath archives here; read them through
        // the same owned, mutation-detecting helper as JARs.
        let data = read_archive_for_classpath(path).map_err(|e| format!("failed to read: {e}"))?;
        if data.len() < 4 {
            return Err("file too small to be a valid JMOD".to_string());
        }
        // Validate magic prefix
        if data[..2] != Self::JMOD_MAGIC_PREFIX {
            return Err(format!(
                "not a valid JMOD file (expected magic 0x4A4D, got 0x{:02X}{:02X})",
                data[0], data[1]
            ));
        }
        // Validate major version
        let major = data[2];
        let minor = data[3];
        if major != Self::JMOD_SUPPORTED_MAJOR {
            return Err(format!(
                "unsupported JMOD major version {major} (expected {}); \
                 this may be from a future JDK",
                Self::JMOD_SUPPORTED_MAJOR
            ));
        }
        if minor > Self::JMOD_MAX_MINOR {
            return Err(format!(
                "unsupported JMOD minor version {minor} (max {})",
                Self::JMOD_MAX_MINOR
            ));
        }
        debug!("JMOD {} version {}.{}", path.display(), major, minor);
        // Strip the 4-byte JMOD magic prefix to get standard ZIP data
        let zip_bytes = SharedBytes::from_external(ArchiveSlice {
            backing: data.clone(),
            start: 4,
            end: data.as_ref().len(),
        });
        let zip_data = ArchiveBacking::Shared(zip_bytes);
        let cursor = Cursor::new(zip_data.clone());
        let mut archive =
            ZipArchive::new(cursor).map_err(|e| format!("failed to parse ZIP inside JMOD: {e}"))?;

        let total_entries = archive.len();

        // Index every entry name from the central directory. `by_index_raw`
        // builds the entry reader without touching compressed data, so this
        // loop reads no payload bytes at all.
        let mut class_entry_index =
            FxHashSet::with_capacity_and_hasher(total_entries, Default::default());
        let mut all_entry_names = Vec::with_capacity(total_entries);

        for i in 0..total_entries {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };

            if let Some(relative) = name.strip_prefix(JMOD_CLASSES_PREFIX) {
                if !relative.is_empty() && !relative.ends_with('/') {
                    class_entry_index.insert(relative.to_string());
                }
            }
            all_entry_names.push(name);
        }

        debug!(
            "Loaded JMOD {} ({} entries, {} classes indexed, none inflated)",
            path.display(),
            total_entries,
            class_entry_index.len()
        );

        Ok(ClassPathEntry::JmodFile {
            path: path.to_path_buf(),
            class_entry_index,
            all_entry_names,
            archive: Mutex::new(archive),
            backing: zip_data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn jar_subdirectory_spec_accepts_war_archives() {
        assert_eq!(
            parse_jar_subdir_spec("/apps/test.war!/WEB-INF/classes/"),
            Some(("/apps/test.war".to_string(), "WEB-INF/classes/".to_string()))
        );
    }

    // -----------------------------------------------------------------------
    // Glob parse-once refactor (2026-07-26 classpath-scan audit)
    // -----------------------------------------------------------------------

    /// Hoisting the glob parse out of the per-candidate filter must not change
    /// which names match. Compare the batch API against the per-candidate
    /// predicate it replaced, over globs that exercise `*`, `?`, directory
    /// prefixes, the root, and the reject paths (no wildcard, wildcard in the
    /// prefix, empty pattern, unsafe prefix).
    #[test]
    fn matching_resource_entry_names_agrees_with_per_candidate_predicate() {
        let names: Vec<String> = [
            "java/lang/Object.class",
            "java/lang/String.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "Root.class",
            "java/lang/notes.txt",
            "java/lang/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        for glob in [
            "java/lang/*.class",
            "java/lang/*",
            "java/lang/?bject.class",
            "java/util/*.class",
            "*.class",
            "java/*/List.class",      // wildcard in the prefix → unparseable
            "java/lang/Object.class", // no wildcard → unparseable
            "java/lang/",             // empty pattern → unparseable
            "/java/lang/*",           // unsafe prefix → unparseable
        ] {
            let batch = ClassPath::matching_resource_entry_names(names.iter(), glob);
            let mut expected: Vec<String> = names
                .iter()
                .filter(|n| resource_name_matches_simple_glob(glob, n))
                .cloned()
                .collect();
            expected.sort();
            assert_eq!(batch, expected, "glob {glob:?} changed meaning");
        }
    }

    /// The parsed and unparsed forms must be the same predicate.
    #[test]
    fn parsed_glob_predicate_matches_unparsed_form() {
        let glob = "java/lang/*.class";
        let (prefix, pattern) = simple_resource_glob(glob).expect("glob must parse");
        for candidate in [
            "java/lang/Object.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "java/lang/",
            "",
        ] {
            assert_eq!(
                resource_name_matches_parsed_glob(prefix, pattern, candidate),
                resource_name_matches_simple_glob(glob, candidate),
                "candidate {candidate:?} disagreed"
            );
        }
    }

    /// `archive_has_entry` must answer exactly what `find_in_archive` answers,
    /// without paying for the inflate. Uses a Deflated entry so the two paths
    /// genuinely differ in work done.
    #[test]
    fn archive_has_entry_agrees_with_find_in_archive() {
        let dir = std::env::temp_dir().join("cratonvm_archive_has_entry");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("probe.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("pkg/present.txt", options).unwrap();
            zip.write_all(&b"payload".repeat(512)).unwrap();
            zip.finish().unwrap();
        }
        let backing = read_archive_for_classpath(&jar_path).unwrap();
        let archive: Mutex<SharedArchive> =
            Mutex::new(ZipArchive::new(Cursor::new(backing)).unwrap());

        for name in ["pkg/present.txt", "pkg/absent.txt", "", "pkg/"] {
            assert_eq!(
                ClassPath::archive_has_entry(&archive, name),
                ClassPath::find_in_archive(&archive, name).is_some(),
                "existence verdict for {name:?} diverged"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_finds_resource_in_war_subdirectory() {
        let dir = std::env::temp_dir().join("cratonvm_war_subdirectory_classpath");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let war_path = dir.join("test.war");
        {
            let file = fs::File::create(&war_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("WEB-INF/classes/test.txt", options).unwrap();
            zip.write_all(b"test resource").unwrap();
            zip.finish().unwrap();
        }

        let spec = format!("{}!/WEB-INF/classes/", war_path.display());
        let cp = ClassPath::new(&[spec]);
        assert_eq!(
            cp.find_resource("test.txt"),
            Some(b"test resource".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_expands_manifest_class_path() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dependency = dir.join("dependency.jar");
        let launcher = dir.join("launcher.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        {
            let file = fs::File::create(&dependency).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/services/example.Service", options)
                .unwrap();
            zip.write_all(b"example.ServiceImpl\n").unwrap();
            zip.finish().unwrap();
        }
        {
            let file = fs::File::create(&launcher).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\r\nClass-Path: dependency.jar\r\n\r\n")
                .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[launcher.to_string_lossy().into_owned()]);
        assert_eq!(cp.entry_count(), 2, "manifest dependency must be added");
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_class_path_cycle_is_not_reloaded() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path_cycle");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for (path, dependency) in [(&first, "second.jar"), (&second, "first.jar")] {
            let file = fs::File::create(path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(
                format!("Manifest-Version: 1.0\r\nClass-Path: {dependency}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[first.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            2,
            "manifest cycle must terminate without duplicates"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_classpath_finds_nothing() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object").is_err());
    }

    #[test]
    fn nonexistent_jar_skipped() {
        let cp = ClassPath::new(&["nonexistent.jar".to_string()]);
        assert!(cp.is_empty());
    }

    #[test]
    fn load_class_from_jar() {
        // Create a temporary JAR file with a fake .class entry
        let dir = std::env::temp_dir().join("cratonvm_test_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("test.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer
            .start_file("com/example/Hello.class", options)
            .unwrap();
        zip_writer
            .write_all(b"\xCA\xFE\xBA\xBE_fake_class_data")
            .unwrap();
        zip_writer.finish().unwrap();

        // Load from the JAR
        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        let data = cp.find_class("com/example/Hello").unwrap();
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.len() > 4);
        assert!(
            data.is_external(),
            "stored ZIP entries should retain the mapped archive backing"
        );

        // Not found in JAR
        assert!(cp.find_class("com/example/Missing").is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_class_uses_shared_owned_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_test_deflated_jar");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("test.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip_writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip_writer
                .start_file("com/example/Compressed.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_compressed_class_data")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let first = cp.find_class("com/example/Compressed").unwrap();
        let second = first.clone();
        assert_eq!(first.as_ref(), second.as_ref());
        assert!(
            !first.is_external() && !second.is_external(),
            "deflated entries should use reference-counted inflated storage"
        );
        assert_eq!(first.as_ref().as_ptr(), second.as_ref().as_ptr());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_format() {
        let cp = ClassPath::new(&[]);
        let debug = format!("{cp:?}");
        assert!(debug.contains("ClassPath"));
    }

    // -- Path traversal protection tests --

    #[test]
    fn path_traversal_double_dot_rejected() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn path_traversal_absolute_path_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
    }

    #[test]
    fn path_traversal_backslash_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("\\\\server\\share").is_err());
    }

    #[test]
    fn single_windows_separator_component_rejected_everywhere() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("pkg\\Foo").is_err());
        assert!(cp.find_class_source_path("pkg\\Foo").is_none());
        assert!(cp.find_class_code_source_info("pkg\\Foo").is_none());
        assert!(cp.find_resource("pkg\\config.properties").is_none());
        assert!(cp
            .find_all_resource_bytes("pkg\\config.properties")
            .is_empty());
        assert!(cp
            .find_all_resource_urls("pkg\\config.properties")
            .is_empty());
    }

    #[test]
    fn classpath_metadata_change_detects_length_change() {
        let dir = std::env::temp_dir().join("cratonvm_classpath_mutation_guard");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("guard.jar");

        fs::write(&path, b"before").unwrap();
        let before = fs::metadata(&path).unwrap();
        fs::write(&path, b"after mutation").unwrap();
        let after = fs::metadata(&path).unwrap();

        assert!(classpath_file_metadata_changed(&before, &after));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_null_byte_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn valid_class_name_format_accepted() {
        // Valid names should not be rejected (they'll fail with ClassNotFound
        // because there's no classpath entry, but NOT due to validation)
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/Object");
        assert!(
            matches!(result, Err(ClassFileError::ClassNotFound { .. })),
            "expected ClassNotFound, got {result:?}"
        );
    }

    #[test]
    fn find_resource_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir");
        let _ = fs::create_dir_all(&dir);
        let resource_path = dir.join("hello.txt");
        fs::write(&resource_path, b"hello world").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("hello.txt")
            .expect("resource should be found");
        assert_eq!(data, b"hello world");

        assert!(cp.find_resource("missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_simple_glob_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir_glob");
        let _ = fs::remove_dir_all(&dir);
        let web_inf = dir.join("org/springframework/web/context/WEB-INF");
        fs::create_dir_all(&web_inf).unwrap();
        fs::write(web_inf.join("myplaceholder.properties"), b"name=Rod").unwrap();
        fs::write(web_inf.join("myoverride.properties"), b"age=42").unwrap();
        fs::write(web_inf.join("other.properties"), b"ignored=true").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let pattern = "org/springframework/web/context/WEB-INF/myplace*.properties";
        let data = cp.find_resource(pattern).expect("globbed resource");
        assert_eq!(data, b"name=Rod");

        let bytes = cp.find_all_resource_bytes(pattern);
        assert_eq!(bytes, vec![b"name=Rod".to_vec()]);

        let urls = cp.find_all_resource_urls(pattern);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].ends_with("/org/springframework/web/context/WEB-INF/myplaceholder.properties"),
            "unexpected URL for globbed resource: {:?}",
            urls
        );

        assert!(
            cp.find_resource("org/springframework/web/*/WEB-INF/myplace*.properties")
                .is_none(),
            "wildcards in directory segments are intentionally unsupported"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_from_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("resources.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer.start_file("data/words.txt", options).unwrap();
        zip_writer.write_all(b"word1\nword2\nword3").unwrap();
        zip_writer.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_resource("data/words.txt").expect("resource in JAR");
        assert_eq!(data, b"word1\nword2\nword3");

        // Leading slash is stripped
        let data2 = cp
            .find_resource("/data/words.txt")
            .expect("leading slash stripped");
        assert_eq!(data2, b"word1\nword2\nword3");

        assert!(cp.find_resource("data/missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The short-circuiting walk must answer EXACTLY what the whole-list walk
    /// answers first — that equivalence is the licence for
    /// `ClassLoader.getResource` to stop at the first hit rather than build
    /// every URL and discard all but element 0.
    ///
    /// Asserted on a classpath where the name hits in MORE than one entry, and
    /// where the first hit is not the last entry: a fixture with a single hit
    /// cannot tell "returns the first" from "returns the only one", and one
    /// where the hit is last cannot tell "stopped early" from "walked
    /// everything".
    #[test]
    fn the_incremental_walk_returns_what_the_whole_list_walk_returns_first() {
        let dir = std::env::temp_dir().join(format!(
            "cratonvm_test_first_url_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        let empty = dir.join("empty");
        for d in [&first, &second, &empty] {
            fs::create_dir_all(d.join("pkg")).unwrap();
        }
        fs::write(first.join("pkg/R.txt"), b"from-first").unwrap();
        fs::write(second.join("pkg/R.txt"), b"from-second").unwrap();

        // `empty` last so a walk that does not stop early still has an entry
        // left to visit after the answer is known.
        let cp = ClassPath::new(&[
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
            empty.to_string_lossy().into_owned(),
        ]);

        let all = cp.find_all_resource_urls("pkg/R.txt");
        assert_eq!(all.len(), 2, "fixture must hit twice, got {all:?}");
        let (incremental, resume) = cp
            .next_resource_url_from("pkg/R.txt", 0)
            .expect("the incremental walk must find it too");
        assert_eq!(
            Some(&incremental),
            all.first(),
            "the two walks disagree on the FIRST url"
        );
        assert_eq!(resume, 1, "it must stop at the entry that answered");

        // A miss agrees too, and costs nothing to state.
        assert!(cp.find_all_resource_urls("pkg/Absent.txt").is_empty());
        assert!(cp.next_resource_url_from("pkg/Absent.txt", 0).is_none());

        // And the gate: a glob name can match several names inside ONE entry,
        // so it must NOT take the incremental path.
        assert!(!ClassPath::name_supports_incremental_scan("pkg/*.txt"));
        assert!(ClassPath::name_supports_incremental_scan("pkg/R.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_path_traversal_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../etc/passwd").is_none());
        assert!(cp.find_resource("foo\0bar").is_none());
    }

    // -- Spring Boot fat JAR tests --

    /// Helper: create a Spring Boot fat JAR with:
    /// - META-INF/MANIFEST.MF (with Start-Class)
    /// - BOOT-INF/classes/com/example/App.class
    /// - BOOT-INF/lib/dep.jar (containing org/dep/Util.class)
    /// - org/springframework/boot/loader/JarLauncher.class (launcher at root)
    fn create_fat_jar(jar_path: &Path) {
        let file = fs::File::create(jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(
            b"Manifest-Version: 1.0\r\n\
              Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
              Start-Class: com.example.App\r\n\
              Spring-Boot-Classes: BOOT-INF/classes/\r\n\
              Spring-Boot-Lib: BOOT-INF/lib/\r\n",
        )
        .unwrap();

        // Application class in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_app_class").unwrap();

        // Resource in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/application.properties", opts)
            .unwrap();
        zip.write_all(b"server.port=8080").unwrap();

        // Nested dependency JAR in BOOT-INF/lib/
        let mut dep_jar_buf = Vec::new();
        {
            let cursor = Cursor::new(&mut dep_jar_buf);
            let mut dep_zip = zip::ZipWriter::new(cursor);
            dep_zip.start_file("org/dep/Util.class", opts).unwrap();
            dep_zip.write_all(b"\xCA\xFE\xBA\xBE_util_class").unwrap();
            dep_zip
                .start_file("META-INF/services/org.dep.SPI", opts)
                .unwrap();
            dep_zip.write_all(b"org.dep.SpiImpl").unwrap();
            dep_zip.finish().unwrap();
        }
        zip.start_file("BOOT-INF/lib/dep-1.0.jar", opts).unwrap();
        zip.write_all(&dep_jar_buf).unwrap();

        // Launcher class at JAR root
        zip.start_file("org/springframework/boot/loader/JarLauncher.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_launcher").unwrap();

        zip.finish().unwrap();
    }

    #[test]
    fn fat_jar_detects_spring_boot_structure() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_detect");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);

        // Should have multiple entries: NestedDirectory + NestedJar(s) + outer JarFile
        assert!(
            cp.entry_count() >= 3,
            "expected >=3 entries, got {}",
            cp.entry_count()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_classes");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("com/example/App")
            .expect("should find App.class from BOOT-INF/classes/");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_app_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_nested");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/dep/Util")
            .expect("should find Util.class from BOOT-INF/lib/dep-1.0.jar");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_util_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_launcher_from_root() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_root");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("should find JarLauncher from JAR root");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_launcher"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_resource");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("application.properties")
            .expect("should find application.properties from BOOT-INF/classes/");
        assert_eq!(data, b"server.port=8080");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_spi");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("META-INF/services/org.dep.SPI")
            .expect("should find SPI file from nested JAR");
        assert_eq!(data, b"org.dep.SpiImpl");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_class_not_found_returns_error() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_missing");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(cp.find_class("com/example/Missing").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parsing() {
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
            Start-Class: com.example.DemoApplication\r\n\
            Spring-Boot-Version: 3.2.0\r\n\
            Spring-Boot-Classes: BOOT-INF/classes/\r\n\
            Spring-Boot-Lib: BOOT-INF/lib/\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
        assert_eq!(
            info.start_class.as_deref(),
            Some("com.example.DemoApplication")
        );
        assert!(info.is_spring_boot());
    }

    #[test]
    fn manifest_continuation_lines() {
        // MANIFEST.MF allows continuation lines starting with a single space
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLa\r\n \
            uncher\r\n\
            Start-Class: com.example.App\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
    }

    #[test]
    fn plain_jar_not_treated_as_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_plain_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("plain.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_foo").unwrap();
        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        // Plain JAR should have exactly 1 entry (JarFile)
        assert_eq!(cp.entry_count(), 1);

        let data = cp.find_class("com/example/Foo").unwrap();
        assert!(data.ends_with(b"_foo"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_add_path_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_dynamic_fatjar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let mut cp = ClassPath::new(&[]);
        assert!(cp.is_empty());

        cp.add_path(&jar_path.to_string_lossy());
        assert!(!cp.is_empty());

        // Should find classes from nested structure
        assert!(cp.find_class("com/example/App").is_ok());
        assert!(cp.find_class("org/dep/Util").is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // JMOD file tests
    // -----------------------------------------------------------------------

    #[test]
    fn jmod_magic_validation() {
        // A file with wrong magic should fail
        let dir = std::env::temp_dir().join("cratonvm_test_jmod");
        let _ = fs::create_dir_all(&dir);
        let bad_path = dir.join("bad.jmod");
        fs::write(&bad_path, b"NOT_JMOD_DATA").unwrap();
        assert!(ClassPath::load_jmod(&bad_path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_class_from_synthetic_jmod() {
        // Create a synthetic JMOD file: 4-byte magic + ZIP with classes/ prefix
        let dir = std::env::temp_dir().join("cratonvm_test_jmod2");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("test.jmod");

        // Build the ZIP portion in memory
        let mut zip_buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_buf);
            let mut zip_writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/java/lang/Object.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_object")
                .unwrap();
            zip_writer
                .start_file("classes/java/lang/String.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_string")
                .unwrap();
            zip_writer
                .start_file("classes/module-info.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_module_info")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        // Write JMOD = magic + ZIP data
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00); // minor version
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        // Load JMOD as classpath entry
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        // Find classes (the prefix "classes/" should be stripped automatically)
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(&obj_bytes[..4], b"\xCA\xFE\xBA\xBE");

        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Unknown class not found
        assert!(cp.find_class("java/lang/Missing").is_err());

        // list_class_names should find both (but not module-info)
        let names = cp.list_class_names();
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(!names.iter().any(|n| n.contains("module-info")));

        // module-info should be found by scan_module_infos
        let modules = cp.scan_module_infos();
        assert_eq!(modules.len(), 1);

        // Resource lookup should work via the classes/ prefix
        let resource = cp.find_resource("java/lang/Object.class");
        assert!(resource.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_real_jdk_jmod() {
        // Skip this test if no JDK is available
        let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
            .ok()
            .or_else(|| {
                let candidate = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
                if candidate.exists() {
                    Some(candidate.to_string_lossy().into_owned())
                } else {
                    None
                }
            });
        let Some(java_home) = java_home else {
            eprintln!("Skipping load_real_jdk_jmod: no JAVA_HOME set");
            return;
        };
        let jmod_path = PathBuf::from(&java_home)
            .join("jmods")
            .join("java.base.jmod");
        if !jmod_path.exists() {
            eprintln!(
                "Skipping load_real_jdk_jmod: {} not found",
                jmod_path.display()
            );
            return;
        }

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty(), "JMOD classpath should not be empty");

        // Must find java.lang.Object
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(
            &obj_bytes[..4],
            b"\xCA\xFE\xBA\xBE",
            "Object.class should have class file magic"
        );
        assert!(obj_bytes.len() > 100, "Object.class should be non-trivial");

        // Must find java.lang.String
        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");
        assert!(str_bytes.len() > 1000, "String.class should be substantial");

        // Must find java.util.HashMap
        let hm_bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&hm_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Class listing should have thousands of classes
        let names = cp.list_class_names();
        assert!(
            names.len() > 1000,
            "java.base should have >1000 classes, got {}",
            names.len()
        );
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"java/util/ArrayList".to_string()));
        assert!(names.contains(&"java/io/InputStream".to_string()));

        // module-info.class should be found
        let modules = cp.scan_module_infos();
        assert_eq!(
            modules.len(),
            1,
            "java.base.jmod should have exactly 1 module-info"
        );
    }

    // --- Phase 80.5: Path Traversal Fix Tests ---

    #[test]
    fn find_class_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn find_class_rejects_absolute_path() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
        assert!(cp.find_class("\\Windows\\System32\\cmd").is_err());
    }

    #[test]
    fn find_class_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn find_class_rejects_windows_drive() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("C:Windows/System32/cmd").is_err());
    }

    #[test]
    fn find_class_rejects_current_dir_reference() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("./java/lang/Object").is_err());
        assert!(cp.find_class(".\\java\\lang\\Object").is_err());
    }

    #[test]
    fn find_resource_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../../../etc/passwd").is_none());
    }

    #[test]
    fn find_resource_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("file\0.txt").is_none());
    }

    #[test]
    fn find_class_canonicalize_with_real_dir() {
        // Create a temp directory with a class file, verify canonicalization works
        let tmp = std::env::temp_dir().join("cratonvm_test_80_5");
        let _ = std::fs::create_dir_all(&tmp);
        let class_dir = tmp.join("com").join("test");
        let _ = std::fs::create_dir_all(&class_dir);
        // Write a minimal (invalid but loadable) class file
        let _ = std::fs::write(class_dir.join("Foo.class"), b"CAFEBABE_fake");
        let mut cp = ClassPath::new(&[]);
        cp.add_path(tmp.to_str().unwrap());
        // Normal lookup should work
        let result = cp.find_class("com/test/Foo");
        assert!(result.is_ok());
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── JMOD integration tests ────────────────────────────────────────
    // These require a real JDK 9+ installation. Run with:
    //   cargo test -p cratonvm-classloading -- --ignored

    /// Helper: find java.base.jmod on this machine.
    fn find_java_base_jmod() -> Option<PathBuf> {
        // Try JAVA_HOME first
        if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            let p = PathBuf::from(&val).join("jmods").join("java.base.jmod");
            if p.exists() {
                return Some(p);
            }
        }
        // Try common Windows paths
        for dir in &[
            r"C:\Program Files\Java",
            r"C:\Program Files\Eclipse Adoptium",
        ] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let jmod = entry.path().join("jmods").join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        // Try running java to find it
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let jmod = PathBuf::from(value.trim())
                        .join("jmods")
                        .join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        None
    }

    #[test]
    #[ignore] // requires JDK on host
    fn load_jmod_validates_magic_and_version() {
        let jmod_path = find_java_base_jmod();
        assert!(
            jmod_path.is_some(),
            "No JDK found — cannot test JMOD loading"
        );
        let jmod_path = jmod_path.unwrap();

        // Should load successfully
        let entry = ClassPath::load_jmod(&jmod_path);
        assert!(
            entry.is_ok(),
            "Failed to load {}: {:?}",
            jmod_path.display(),
            entry.err()
        );

        // Verify it's a JmodFile variant with a populated class_entry_index
        match entry.unwrap() {
            ClassPathEntry::JmodFile {
                class_entry_index,
                all_entry_names,
                ..
            } => {
                assert!(
                    class_entry_index.len() > 100,
                    "java.base should have >100 classes, got {}",
                    class_entry_index.len()
                );
                assert!(
                    all_entry_names.len() > class_entry_index.len(),
                    "all_entry_names should include non-class entries too"
                );
                // Should contain java.lang.Object
                assert!(
                    class_entry_index.contains("java/lang/Object.class"),
                    "java.base should contain java/lang/Object.class"
                );
                // Should contain java.lang.String
                assert!(
                    class_entry_index.contains("java/lang/String.class"),
                    "java.base should contain java/lang/String.class"
                );
                eprintln!(
                    "java.base.jmod: {} classes, {} total entries",
                    class_entry_index.len(),
                    all_entry_names.len()
                );
            }
            other => panic!("Expected JmodFile, got {other:?}"),
        }
    }

    #[test]
    #[ignore] // requires JDK on host
    fn find_class_from_jmod_returns_valid_classfile() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);

        // Find java.lang.Object
        let bytes = cp.find_class("java/lang/Object").unwrap();
        // Verify it starts with the class file magic
        assert_eq!(
            &bytes[..4],
            &[0xCA, 0xFE, 0xBA, 0xBE],
            "java.lang.Object should start with CAFEBABE"
        );
        // Check class file version is reasonable (major >= 45 for any JDK)
        let major = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert!(
            major >= 45 && major <= 80,
            "class file major version {major} out of expected range"
        );
        eprintln!(
            "java.lang.Object: {} bytes, class file version {major}",
            bytes.len()
        );

        // Find java.util.HashMap
        let bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE]);

        // Non-existent class should fail
        assert!(cp.find_class("java/lang/NonExistent").is_err());
    }

    #[test]
    #[ignore] // requires JDK on host
    fn list_jmod_modules_returns_module_names() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        // Load just java.base
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        let modules = cp.list_jmod_modules();
        assert_eq!(modules, vec!["java.base"]);

        // Load all jmods from the JDK
        let jmods_dir = jmod_path.parent().unwrap();
        let mut all_jmods: Vec<String> = fs::read_dir(jmods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jmod"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        all_jmods.sort();

        let cp = ClassPath::new(&all_jmods);
        let modules = cp.list_jmod_modules();
        assert!(modules.contains(&"java.base".to_string()));
        assert!(modules.contains(&"java.logging".to_string()));
        assert!(
            modules.len() >= 10,
            "Expected at least 10 modules, got {}",
            modules.len()
        );
        eprintln!(
            "Found {} modules: {:?}",
            modules.len(),
            &modules[..5.min(modules.len())]
        );
    }

    #[test]
    fn load_jmod_rejects_bad_magic() {
        let dir = std::env::temp_dir().join("cratonvm_test_bad_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bad.jmod");

        // Write a file with wrong magic
        fs::write(&path, b"BAAD\x00\x00\x00\x00").unwrap();
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("magic"), "Error should mention magic: {err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_unsupported_major_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_future_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("future.jmod");

        // Write JMOD header with major version 99 (unsupported)
        // followed by minimal ZIP data (will fail ZIP parse, but version check comes first)
        let mut data = vec![0x4A, 0x4D, 99, 0x00]; // JM + major=99 + minor=0
        data.extend_from_slice(&[0; 100]); // padding
        fs::write(&path, &data).unwrap();

        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("major version"),
            "Error should mention major version: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_too_small_file() {
        let dir = std::env::temp_dir().join("cratonvm_test_tiny_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("tiny.jmod");

        fs::write(&path, b"JM").unwrap(); // Only 2 bytes
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("too small"),
            "Error should mention file too small: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jmod_class_count_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert_eq!(cp.jmod_class_count(), 0);
    }

    #[test]
    fn list_jmod_modules_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert!(cp.list_jmod_modules().is_empty());
    }

    // -- ManifestInfo parsing tests --

    #[test]
    fn manifest_multi_release_true() {
        let data = b"Multi-Release: true\nMain-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
        assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
    }

    #[test]
    fn manifest_multi_release_false_by_default() {
        let data = b"Main-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(!info.multi_release);
    }

    #[test]
    fn manifest_multi_release_case_insensitive() {
        let data = b"Multi-Release: TRUE\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
    }

    #[test]
    fn manifest_classpath_not_dropped_when_large() {
        let mut entries = Vec::new();
        for i in 0..1200 {
            entries.push(format!("file:/C:/repo/lib/dep{i}.jar"));
        }
        let cp = entries.join(" ");
        let raw = format!("Manifest-Version: 1.0\nClass-Path: {cp}\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.class_path.is_some(),
            "large Class-Path must be preserved"
        );
    }

    #[test]
    fn manifest_classpath_file_uri_windows_drive() {
        let data = b"Class-Path: file:/C:/Users/dev/.m2/repository/x/y.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        let norm = cp[0].replace('\\', "/");
        assert_eq!(norm, "C:/Users/dev/.m2/repository/x/y.jar");
    }

    #[test]
    fn manifest_classpath_relative_entries_still_resolve_against_jar_dir() {
        let data = b"Class-Path: lib/a.jar ../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(cp.len(), 2);
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/lib/a.jar"));
        assert!(cp[1]
            .replace('\\', "/")
            .ends_with("/tmp/boot/../shared/b.jar"));
    }

    #[test]
    fn manifest_classpath_continuation_lines_preserve_entries() {
        // 72-column style split: first line ends with a space before the break;
        // continuation line begins with one marker space then the next path.
        let raw = concat!(
            "Manifest-Version: 1.0\n",
            "Class-Path: file:/C:/repo/lib/one.jar \n",
            " file:/C:/repo/lib/two.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 2, "expected two jars, got {:?}", cp);
        assert!(cp[0].replace('\\', "/").contains("/repo/lib/one.jar"));
        assert!(cp[1].replace('\\', "/").contains("/repo/lib/two.jar"));
    }

    #[test]
    fn manifest_classpath_mid_token_continuation() {
        let raw = concat!(
            "Class-Path: file:/C:/repo/lib/prefix-\n",
            " suffix.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        assert!(cp[0].replace('\\', "/").contains("prefix-suffix.jar"));
    }

    /// Optional fixture: copy a Surefire booter `META-INF/MANIFEST.MF` to
    /// `CratonVM/target/booter-extract/MANIFEST.MF` to validate folding on a
    /// real multi-kilobyte Class-Path header.
    #[test]
    fn manifest_parse_optional_surefire_booter_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/booter-extract/META-INF/MANIFEST.MF");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read fixture");
        let info = ManifestInfo::parse(&data);
        let cp = info.class_path.expect("Class-Path");
        assert!(
            cp.contains("junit-platform-launcher-1.10.1.jar"),
            "merged Class-Path should include junit launcher; len={}",
            cp.len()
        );
        assert!(
            !cp.contains(".././"),
            "malformed .././ segment after fold: {cp:?}"
        );
    }

    // -- Multi-release JAR tests --

    #[test]
    fn multi_release_jar_prefers_versioned_class() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF with Multi-Release
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Versioned class (Java 11)
        zip.start_file("META-INF/versions/11/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Should get the versioned class (v11), not the base.
        assert_eq!(&data[4..], b"_v11");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_highest_version_wins() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_highest");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr2.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Java 11 version
        zip.start_file("META-INF/versions/11/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        // Java 17 version
        zip.start_file("META-INF/versions/17/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Foo").unwrap();
        // Should get the highest available version (v17), since our VM targets Java 25.
        assert_eq!(&data[4..], b"_v17");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_multi_release_jar_ignores_versioned() {
        let dir = std::env::temp_dir().join("cratonvm_test_no_mr");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("no_mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // No Multi-Release header
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Main-Class: com.example.Main\n").unwrap();

        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        zip.start_file("META-INF/versions/17/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Without Multi-Release flag, should get the base version.
        assert_eq!(&data[4..], b"_base");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_falls_back_to_base() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_fallback");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_fallback.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Only base class (no versioned entry)
        zip.start_file("com/example/Base.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base_only").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Base").unwrap();
        assert_eq!(&data[4..], b"_base_only");

        let _ = fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // NEW-5 — jimage integration
    // =======================================================================

    /// Produce a temp-file path unique to this test run so concurrent
    /// tests don't collide.
    fn jimage_temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cratonvm_jimage_test");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!(
            "{tag}_{}.img",
            std::process::id().wrapping_mul(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            )
        ))
    }

    fn sample_jimage_resources() -> Vec<(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [u8],
    )> {
        vec![
            (
                "java.base",
                "java/lang",
                "String",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_string".as_ref(),
            ),
            (
                "java.base",
                "java/util",
                "HashMap",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_hashmap".as_ref(),
            ),
            (
                "java.desktop",
                "javax/swing",
                "JFrame",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_jframe".as_ref(),
            ),
            (
                "java.base",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_base_module_info".as_ref(),
            ),
            (
                "java.desktop",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_desktop_module_info".as_ref(),
            ),
        ]
    }

    /// Write a synthetic jimage to disk and load it through
    /// `ClassPath::add_jimage`. The resulting classpath must resolve
    /// every class via `find_class`.
    #[test]
    fn jimage_find_class_round_trip() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("find_class");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).expect("add_jimage");

        // `module-info` is scoped per-module and not resolvable via
        // the flat `find_class(name)` API (see `load_jimage`). All
        // other sample classes round-trip exactly.
        for (_module, parent, base, _ext, bytes) in sample_jimage_resources() {
            if parent.is_empty() && base == "module-info" {
                continue;
            }
            let internal = format!("{parent}/{base}");
            let got = cp.find_class(&internal).expect("class present");
            assert_eq!(got.as_ref(), bytes, "bytes for {internal} must round-trip");
        }

        let _ = fs::remove_file(&path);
    }

    /// The `lib/modules` auto-detection in `ClassPath::new` should pick
    /// up a file literally named `modules` without an extension.
    #[test]
    fn jimage_auto_detected_by_filename() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let dir = std::env::temp_dir().join(format!("cratonvm_jimage_auto_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("modules");
        fs::write(&path, &data).unwrap();

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        assert!(
            !cp.is_empty(),
            "ClassPath::new should auto-detect a file named `modules`"
        );
        let bytes = cp
            .find_class("java/lang/String")
            .expect("class present via auto-detected jimage");
        assert!(bytes.starts_with(b"\xCA\xFE\xBA\xBE"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `scan_module_infos` should return a module-info entry for every
    /// distinct module in the jimage.
    #[test]
    fn jimage_scan_module_infos_returns_every_module() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("scan_module_infos");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let infos = cp.scan_module_infos();
        assert_eq!(
            infos.len(),
            2,
            "exactly two modules in the sample jimage ({} found)",
            infos.len()
        );
        // Both should start with the class-file magic. The exact bytes
        // reflect what our sample resources wrote.
        for info in &infos {
            assert!(info.starts_with(b"\xCA\xFE\xBA\xBE"));
        }
        let _ = fs::remove_file(&path);
    }

    /// The class-to-module index exposed via `jimage_class_count` must
    /// reflect every `.class` entry in the image (module-infos count).
    #[test]
    fn jimage_class_count_matches_resources() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("class_count");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        // 3 regular classes (module-info entries are not counted
        // because they're scoped per-module, not global).
        assert_eq!(cp.jimage_class_count(), 3);
        let modules = cp.list_jimage_modules();
        assert_eq!(
            modules,
            vec!["java.base".to_string(), "java.desktop".to_string()]
        );

        let _ = fs::remove_file(&path);
    }

    /// `list_class_names` should include every class in the jimage.
    #[test]
    fn jimage_list_class_names_includes_every_entry() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("list_class_names");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let mut names = cp.list_class_names();
        names.sort();
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"javax/swing/JFrame".to_string()));
        // module-info is deliberately excluded from `class_to_module`
        // because it is scoped per-module rather than globally unique.
        assert!(
            !names.contains(&"module-info".to_string()),
            "module-info must not appear as a regular class name"
        );

        let _ = fs::remove_file(&path);
    }

    /// Missing-class lookups against a jimage classpath must return
    /// ClassNotFound and not leak a panic.
    #[test]
    fn jimage_missing_class_returns_clean_error() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("missing");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let err = cp.find_class("no/such/Class").unwrap_err();
        assert!(matches!(err, ClassFileError::ClassNotFound { .. }));

        let _ = fs::remove_file(&path);
    }

    /// `add_jimage` on a non-existent path must return an error, not
    /// a silent no-op.
    #[test]
    fn jimage_add_nonexistent_returns_error() {
        let mut cp = ClassPath::new(&[]);
        let err = cp
            .add_jimage("/nonexistent/cratonvm/jimage/file")
            .unwrap_err();
        assert!(
            err.contains("open jimage"),
            "error message should mention open failure: {err}"
        );
    }

    /// `add_jimage` on a bogus file (not a valid jimage) must return
    /// an error without corrupting the classpath.
    #[test]
    fn jimage_add_bogus_file_returns_error() {
        let path = jimage_temp_path("bogus");
        fs::write(&path, b"this is not a jimage file").unwrap();

        let mut cp = ClassPath::new(&[]);
        let err = cp.add_jimage(&path).unwrap_err();
        assert!(
            err.contains("open jimage") || err.contains("jimage"),
            "error should reference jimage: {err}"
        );
        assert!(cp.is_empty(), "classpath must be unchanged on failure");

        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // T19_H10: ManifestInfo.attributes — generic attribute capture so
    // `Class.getPackage().getImplementationVersion()` and friends can read
    // arbitrary `Implementation-*` / `Specification-*` headers without
    // expanding `ManifestInfo` for every new field.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h10_manifest_attributes_captures_implementation_fields() {
        // Use \n-only line separators (no continuation prefix) so the
        // parser captures every attribute as its own entry.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Title: keycloak-common\n\
Implementation-Version: 26.2.4\n\
Implementation-Vendor: Red Hat, Inc.\n\
Specification-Title: Keycloak Common\n\
Specification-Version: 26.2\n\
Specification-Vendor: Keycloak\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Title")
                .map(String::as_str),
            Some("keycloak-common")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("26.2.4")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Vendor")
                .map(String::as_str),
            Some("Red Hat, Inc.")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Title")
                .map(String::as_str),
            Some("Keycloak Common")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Version")
                .map(String::as_str),
            Some("26.2")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Vendor")
                .map(String::as_str),
            Some("Keycloak")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_returns_none_for_missing() {
        let manifest = b"Manifest-Version: 1.0\nMain-Class: foo.Bar\n";
        let info = ManifestInfo::parse(manifest);
        assert!(
            info.attributes.get("Implementation-Version").is_none(),
            "absent attribute must yield None"
        );
        assert_eq!(
            info.attributes.get("Manifest-Version").map(String::as_str),
            Some("1.0")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_skips_per_entry_sections() {
        // The `\n\n` separator ends the main section. Anything after must
        // not contaminate `info.attributes`.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Version: 1.0\n\
\n\
Name: foo/Bar.class\n\
Implementation-Version: 999.999\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("1.0"),
            "main-section attribute wins; per-entry section is ignored"
        );
        assert!(
            info.attributes.get("Name").is_none(),
            "per-entry `Name` header must not leak into main attributes"
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_caps_oversized_value() {
        // Build a manifest with one valid line + one 16 KiB-value line.
        // The big line must be silently dropped (returned as None) so a
        // hostile signed jar cannot blow our heap.
        let big = "X".repeat(16 * 1024);
        let raw = format!("Manifest-Version: 1.0\nFoo: {big}\nBar: ok\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.attributes.get("Foo").is_none(),
            "oversized value must be dropped"
        );
        assert_eq!(
            info.attributes.get("Bar").map(String::as_str),
            Some("ok"),
            "subsequent attributes must still be captured"
        );
    }

    // ── Classpath wildcard expansion (Elasticsearch / Cassandra parity) ──
    //
    // HotSpot's `java -cp lib/*` expands to every `*.jar` directly under
    // `lib/`. Real-world launchers (Elasticsearch, Cassandra, hand-rolled
    // `bin/foo` shell scripts) rely on this; without it `ServiceLoader`
    // sees zero classpath entries and returns an empty iterator.

    /// Build a temp dir with N JARs (valid zips, each containing a
    /// `META-INF/services/dummy.SPI` so `find_all_resource_urls` has
    /// something to enumerate). Returns the dir.
    fn make_jar_dir(name: &str, jar_names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for jar in jar_names {
            let p = dir.join(jar);
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/dummy.SPI", opts).unwrap();
            zip.write_all(b"com.example.Provider\n").unwrap();
            zip.finish().unwrap();
        }
        dir
    }

    /// `dir/*` must expand to every `.jar` in the directory.
    #[test]
    fn wildcard_expands_dir_star_to_all_jars() {
        let dir = make_jar_dir("cratonvm_wildcard_dir_star", &["a.jar", "b.jar", "c.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 3, "wildcard should expand to 3 jars");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The launcher-facing expansion API must publish concrete paths too.
    /// In-process javac reads `java.class.path` and cannot resolve a literal
    /// `lib/*` token on its own.
    #[test]
    fn wildcard_publication_expands_to_concrete_sorted_jars() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_property_publication",
            &["z.jar", "a.jar", "m.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let expanded = ClassPath::expand_classpath_entries(&[pattern]);
        let names: Vec<_> = expanded
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.jar", "m.jar", "z.jar"]);
        assert!(expanded.iter().all(|path| !path.contains('*')));
        let _ = fs::remove_dir_all(&dir);
    }

    /// All wildcard-expanded jars must be visible to
    /// `find_all_resource_urls` — this is the load-bearing claim for
    /// ServiceLoader. Before the fix this returned an empty Vec because
    /// the literal `lib/*` entry was silently dropped as a non-existent
    /// path.
    #[test]
    fn wildcard_makes_every_jar_searchable_for_services() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_services",
            &["alpha.jar", "beta.jar", "gamma.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            3,
            "every wildcard-expanded jar must contribute its META-INF/services entry"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Backslash form `dir\*` (Windows-style) must work identically.
    #[test]
    fn wildcard_accepts_backslash_form() {
        let dir = make_jar_dir("cratonvm_wildcard_backslash", &["x.jar", "y.jar"]);
        let pattern = format!("{}\\*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-jar files in the wildcard directory are ignored.
    #[test]
    fn wildcard_skips_non_jar_files() {
        let dir = make_jar_dir("cratonvm_wildcard_skips_non_jar", &["good.jar"]);
        fs::write(dir.join("README.txt"), b"hello").unwrap();
        fs::write(dir.join("native.dll"), [0u8; 8]).unwrap();
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1, "only `.jar` files must be picked up");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `*.JAR` (upper-case extension) is accepted — Windows is
    /// case-insensitive and real-world launchers occasionally ship
    /// `Some-Lib.JAR`.
    #[test]
    fn wildcard_accepts_uppercase_jar_extension() {
        let dir = make_jar_dir("cratonvm_wildcard_uppercase", &["LIB.JAR"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `add_path` (dynamic loading via URLClassLoader / module loader)
    /// honours the same wildcard so app servers that synthesise `lib/*`
    /// strings at runtime benefit.
    #[test]
    fn wildcard_works_via_add_path() {
        let dir = make_jar_dir("cratonvm_wildcard_add_path", &["one.jar", "two.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let mut cp = ClassPath::new(&[]);
        cp.add_path(&pattern);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-wildcard entries pass through unchanged — `foo.jar` must
    /// not be treated as a wildcard pattern (regression guard against
    /// false-positive expansion).
    #[test]
    fn wildcard_passthrough_for_plain_jar() {
        let dir = make_jar_dir("cratonvm_wildcard_passthrough", &["a.jar"]);
        let plain = dir.join("a.jar").to_string_lossy().into_owned();
        let cp = ClassPath::new(&[plain]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Explicit URLClassLoader entries are archive URLs, not Java launcher
    /// wildcard entries: a valid ZIP must remain searchable even when its
    /// extension is `.par` rather than `.jar`.
    #[test]
    fn explicit_non_jar_archive_is_searchable() {
        let dir = make_jar_dir("cratonvm_packaged_archive_suffix", &["payload.jar"]);
        let jar = dir.join("payload.jar");
        let par = dir.join("payload.par");
        fs::rename(&jar, &par).unwrap();

        let cp = ClassPath::new(&[par.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            1,
            "the .par ZIP must be admitted as an archive"
        );
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            1,
            "the resource inside the .par must be visible"
        );
        assert!(urls[0].contains("payload.par"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Wildcard pointing at a non-existent directory must silently
    /// expand to nothing (matches `java -cp missing/*` behaviour).
    #[test]
    fn wildcard_missing_dir_yields_empty_classpath() {
        let cp = ClassPath::new(&["definitely_does_not_exist_12345/*".to_string()]);
        assert!(cp.is_empty());
    }

    /// Provider ordering must be deterministic across runs. Two
    /// invocations on the same directory must produce the same URL
    /// order — `ServiceLoader` clients can depend on first-match
    /// semantics.
    #[test]
    fn wildcard_yields_deterministic_jar_order() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_order",
            &["zeta.jar", "alpha.jar", "mu.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp1 = ClassPath::new(&[pattern.clone()]);
        let cp2 = ClassPath::new(&[pattern]);
        let urls1 = cp1.find_all_resource_urls("META-INF/services/dummy.SPI");
        let urls2 = cp2.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(urls1, urls2, "wildcard expansion order must be stable");
        // Sanity: lexicographic — alpha < mu < zeta.
        assert!(urls1[0].contains("alpha.jar"));
        assert!(urls1[1].contains("mu.jar"));
        assert!(urls1[2].contains("zeta.jar"));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── ES2 multi-jar SPI enumeration (Elasticsearch parity) ───────────
    //
    // Elasticsearch's launcher script passes every jar EXPLICITLY (not
    // via `lib/*` wildcard). Only one of those 60+ jars holds the
    // `META-INF/services/org.elasticsearch.cli.CliToolProvider` SPI
    // descriptor. The other 59 must be walked without producing false
    // positives or short-circuiting the search after the first miss.
    //
    // Regression target: an earlier version of `find_all_resource_urls`
    // could short-circuit on `find_in_archive` returning Err (vs Ok(None)),
    // which would cause the walk to abort silently after the first jar
    // that lacked the entry. The current code returns Ok-or-None for
    // every entry and continues, but a unit test pins the contract.

    /// Build N empty jars + 1 jar that holds the SPI descriptor. All N+1
    /// must be walked by `find_all_resource_urls`, and exactly ONE URL
    /// must come back — the one from the lone jar that has the entry.
    /// Mirrors the Elasticsearch `-cp <60 explicit jars>` shape.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_provider() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 19 empty jars (just an empty MANIFEST.MF) + 1 jar with the SPI.
        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..19 {
            let p = dir.join(format!("empty-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        // The lone provider jar — modelled after server-cli-8.15.5.jar.
        let provider = dir.join("server-cli-x.y.z.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(
                "META-INF/services/org.elasticsearch.cli.CliToolProvider",
                opts,
            )
            .unwrap();
            zip.write_all(b"org.elasticsearch.server.cli.ServerCliProvider\n")
                .unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        assert_eq!(
            cp.entry_count(),
            20,
            "every explicitly-listed jar must register as a classpath entry"
        );
        let urls =
            cp.find_all_resource_urls("META-INF/services/org.elasticsearch.cli.CliToolProvider");
        assert_eq!(
            urls.len(),
            1,
            "exactly one jar holds the SPI descriptor; walk must not \
             short-circuit on the first miss. Got urls={urls:?}",
        );
        assert!(
            urls[0].contains("server-cli-x.y.z.jar"),
            "the lone hit must point at the provider jar, got {}",
            urls[0]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same multi-jar walk with `find_all_resource_bytes` — the Rust
    /// helper path used by service_loader.rs that bypasses URL stream
    /// handling. Must produce exactly one byte block matching the SPI
    /// content.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi_bytes");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..9 {
            let p = dir.join(format!("nothing-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("README", opts).unwrap();
            zip.write_all(b"nothing here\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        let provider = dir.join("provider.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/foo.Bar", opts).unwrap();
            zip.write_all(b"foo.BarImpl\n").unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        let bytes = cp.find_all_resource_bytes("META-INF/services/foo.Bar");
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], b"foo.BarImpl\n");
        let _ = fs::remove_dir_all(&dir);
    }

    // -- T1.8.2 follow-up (MED #32): fat-JAR re-open path no longer panics --

    /// Regression test: a malformed/truncated archive must NOT panic via the
    /// previous `.unwrap()` on the fat-JAR re-open path. Feeding raw garbage
    /// fails the very first `ZipArchive::new` and pushes no entries.
    #[test]
    fn load_jar_data_with_truncated_bytes_does_not_panic() {
        // Eight bytes that are not a valid zip central directory.
        let garbage: Vec<u8> = b"\x00\x01\x02\x03\x04\x05\x06\x07".to_vec();
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        let path = Path::new("truncated.jar");

        // The fix removed `.unwrap()` on re-open; the first open also
        // fails here, exercising the broader no-panic contract.
        ClassPath::load_jar_data(path, garbage, &mut entries);

        assert!(
            entries.is_empty(),
            "expected no entries from malformed archive, got {}",
            entries.len()
        );
    }

    /// Regression test: a fat JAR header that contains a Spring-Boot manifest
    /// but is truncated after the central-directory probe must not panic on
    /// the re-open path either. We construct a fat JAR, mangle the trailing
    /// bytes after the in-memory buffer has been built, and confirm load
    /// proceeds without panicking.
    #[test]
    fn fat_jar_corrupted_after_first_open_does_not_panic() {
        // Build a valid fat JAR in memory.
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(
                b"Manifest-Version: 1.0\r\n\
                  Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
                  Start-Class: com.example.App\r\n\
                  Spring-Boot-Classes: BOOT-INF/classes/\r\n\
                  Spring-Boot-Lib: BOOT-INF/lib/\r\n",
            )
            .unwrap();
            zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
                .unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE_app").unwrap();
            zip.finish().unwrap();
        }

        // Sanity: a pristine fat JAR loads fine and pushes >=1 entry.
        let mut ok_entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("ok.jar"), buf.clone(), &mut ok_entries);
        assert!(
            !ok_entries.is_empty(),
            "pristine fat JAR should push at least the BOOT-INF entries"
        );

        // Truncate the buffer mid-central-directory. The first open in
        // `load_jar_data` may still succeed for some inputs, but in either
        // case the re-open path (formerly `.unwrap()`) must not panic.
        let mut truncated = buf.clone();
        if truncated.len() > 32 {
            truncated.truncate(truncated.len() - 32);
        }
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("corrupt.jar"), truncated, &mut entries);
        // No assertion on entry count: the contract is "does not panic".
    }

    // -- VULN (low) manifest Class-Path hardening: `path_is_within` --
    //
    // These exercise the pure lexical containment helper that backs the
    // opt-in `CRATONVM_HARDEN_MANIFEST_CLASSPATH` policy. The env-gated
    // wiring in `resolve_class_path` is not toggled here (the flag latches
    // in a process-wide `OnceLock`), so we test the decision function
    // directly.

    #[test]
    fn path_is_within_accepts_descendants() {
        assert!(path_is_within(
            Path::new("C:/app/lib/dep.jar"),
            Path::new("C:/app")
        ));
        assert!(path_is_within(
            Path::new("C:/app/lib/sub/dep.jar"),
            Path::new("C:/app/lib")
        ));
        // Identical path is "within" itself.
        assert!(path_is_within(Path::new("C:/app"), Path::new("C:/app")));
    }

    #[test]
    fn path_is_within_rejects_dotdot_escape() {
        // `..` that climbs out of the base directory must be rejected.
        assert!(!path_is_within(
            Path::new("C:/app/../secret/x.jar"),
            Path::new("C:/app")
        ));
        assert!(!path_is_within(
            Path::new("C:/app/lib/../../secret"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_rejects_unrelated_absolute_root() {
        // A sibling/foreign absolute root (e.g. manifest `file:/etc`) is
        // not a descendant of the JAR directory.
        assert!(!path_is_within(Path::new("C:/etc"), Path::new("C:/app")));
        assert!(!path_is_within(
            Path::new("D:/other/x.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_dot_dot_normalizes_back_inside() {
        // `lib/../lib2` stays inside `C:/app`.
        assert!(path_is_within(
            Path::new("C:/app/lib/../lib2/dep.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn manifest_classpath_escape_preserved_by_default() {
        // Regression guard for the DEFAULT (HotSpot-parity) behaviour: with
        // the hardening flag OFF, a manifest Class-Path that escapes the
        // JAR directory MUST still resolve (matching
        // `manifest_classpath_relative_entries_still_resolve_against_jar_dir`).
        // This protects the trusted-manifest contract `decode_manifest_classpath_entry`
        // documents from accidental regression.
        let data = b"Class-Path: ../../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(
            cp.len(),
            1,
            "default policy must keep escaping entries: {cp:?}"
        );
        assert!(cp[0].replace('\\', "/").contains("../../shared/b.jar"));
    }

    // -----------------------------------------------------------------------
    // Lazy JMOD class serving (2026-07-26 boot-classpath-lazy)
    //
    // `load_jmod` used to inflate every `classes/` entry at load time. It now
    // indexes names only and inflates per lookup. These tests pin the three
    // things that change could plausibly break: the bytes, the fall-through to
    // other classpath entries, and the behaviour on a damaged archive.
    // -----------------------------------------------------------------------

    /// The 4-byte JMOD header that precedes the embedded ZIP.
    const TEST_JMOD_HEADER: [u8; 4] = [0x4A, 0x4D, 0x01, 0x00];

    /// Build a JMOD image in memory: `JM\x01\x00` followed by a ZIP holding
    /// `entries` verbatim (names are used exactly as given, so callers
    /// control whether an entry lands under `classes/`).
    ///
    /// Entries are **Deflated**, not Stored, so the lazy path is exercised
    /// against genuinely compressed data — a Stored entry would be served by
    /// the zero-copy `ArchiveSlice` branch and would never prove that
    /// on-demand inflation produces the right bytes.
    fn build_test_jmod_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let zip_bytes = zip.finish().unwrap().into_inner();

        let mut out = Vec::with_capacity(TEST_JMOD_HEADER.len() + zip_bytes.len());
        out.extend_from_slice(&TEST_JMOD_HEADER);
        out.extend_from_slice(&zip_bytes);
        out
    }

    fn jmod_temp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_lazy_jmod_{}_{}", std::process::id(), tag));
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("{tag}.jmod"))
    }

    /// A class body big enough that its deflate stream spans many bytes, so
    /// the corruption test below has payload to damage. Deterministic so a
    /// failure is reproducible.
    fn synthetic_class_bytes(seed: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        let mut x = seed as u32 | 1;
        while out.len() < len {
            // xorshift — pseudo-random so the entry does not compress to
            // nothing, but fully reproducible.
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push((x & 0xFF) as u8);
        }
        out
    }

    /// Equivalence with the eager implementation this replaced: for every
    /// `classes/` entry, `find_class` must return exactly the bytes that
    /// pre-extracting the whole archive would have cached.
    ///
    /// The reference side is computed here by replicating the old eager loop
    /// against the same archive, so this compares the two strategies rather
    /// than comparing the new one against a hand-written expectation.
    #[test]
    fn jmod_lazy_lookup_matches_eager_extraction_byte_for_byte() {
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "classes/java/lang/Object.class",
                synthetic_class_bytes(1, 3000),
            ),
            (
                "classes/java/lang/String.class",
                synthetic_class_bytes(2, 5000),
            ),
            (
                "classes/java/util/List.class",
                synthetic_class_bytes(3, 700),
            ),
            ("classes/module-info.class", synthetic_class_bytes(4, 400)),
            (
                "classes/META-INF/services/example.Service",
                b"example.ServiceImpl\n".to_vec(),
            ),
            // Not under `classes/` — a JMOD's build-time payload. Must not be
            // indexed as a class and must not be counted.
            ("legal/java.base/LICENSE", b"license text".to_vec()),
            ("lib/libjava.so", vec![0u8; 64]),
        ];
        let path = jmod_temp_path("equivalence");
        fs::write(&path, build_test_jmod_bytes(&entries)).unwrap();

        // --- reference: what the old eager pre-extraction would have cached
        let raw = fs::read(&path).unwrap();
        let mut reference: HashMap<String, Vec<u8>> = HashMap::new();
        {
            let mut archive = ZipArchive::new(Cursor::new(raw[4..].to_vec())).unwrap();
            for i in 0..archive.len() {
                let name = archive.by_index_raw(i).unwrap().name().to_string();
                if let Some(relative) = name.strip_prefix("classes/") {
                    if relative.is_empty() || relative.ends_with('/') {
                        continue;
                    }
                    let mut entry = archive.by_name(&name).unwrap();
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).unwrap();
                    reference.insert(relative.to_string(), buf);
                }
            }
        }
        assert_eq!(reference.len(), 5, "five entries live under classes/");

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);

        // The index must agree with the eager cache on membership...
        assert_eq!(
            cp.jmod_class_count(),
            reference.len(),
            "index must cover exactly the classes/ subtree"
        );

        // ...and on bytes, for every class the eager cache would have held.
        for name in ["java/lang/Object", "java/lang/String", "java/util/List"] {
            let got = cp.find_class(name).expect("class must resolve");
            let want = reference
                .get(&format!("{name}.class"))
                .expect("reference entry");
            assert_eq!(
                got.as_ref(),
                want.as_slice(),
                "lazy bytes for {name} must equal eagerly-extracted bytes"
            );
        }

        // Non-class resources under `classes/` come back too.
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );

        // A second lookup returns the same bytes — inflating on demand must
        // be idempotent, not stateful.
        let first = cp.find_class("java/lang/String").unwrap();
        let second = cp.find_class("java/lang/String").unwrap();
        assert_eq!(first.as_ref(), second.as_ref());

        // Entries outside `classes/` are not classes.
        assert!(cp.find_class("legal/java.base/LICENSE").is_err());
        assert!(cp.find_class("java/lang/Missing").is_err());

        // The code source still points at the JMOD, and answering that
        // question never needs the bytes.
        assert_eq!(
            cp.find_class_source_path("java/lang/Object"),
            Some(path.to_string_lossy().into_owned())
        );
        assert_eq!(cp.find_class_source_path("java/lang/Missing"), None);

        let _ = fs::remove_file(&path);
    }

    /// A JMOD and a jimage on the same classpath: a class only the JMOD has
    /// must still resolve. This is the case that makes the "just prefer
    /// `lib/modules`" alternative unsafe, and it is what the fall-through in
    /// `find_class` exists for.
    #[test]
    fn class_only_in_jmod_resolves_when_jimage_is_also_on_the_path() {
        let jimage_data =
            cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let jimage_path = jimage_temp_path("jmod_fallthrough");
        fs::write(&jimage_path, &jimage_data).unwrap();

        let jmod_only = synthetic_class_bytes(9, 2048);
        let jmod_path = jmod_temp_path("fallthrough");
        fs::write(
            &jmod_path,
            build_test_jmod_bytes(&[("classes/com/example/OnlyInJmod.class", jmod_only.clone())]),
        )
        .unwrap();

        // jimage first, JMOD second — the ordering a `lib/modules`-preferring
        // boot classpath would produce.
        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&jimage_path).expect("add_jimage");
        cp.add_path(&jmod_path.to_string_lossy());
        assert_eq!(cp.entry_count(), 2, "both entries must be on the classpath");

        // The jimage-only class resolves through the jimage reader...
        assert_eq!(
            cp.find_class("java/lang/String").unwrap().as_ref(),
            b"\xCA\xFE\xBA\xBE_jimage_string".as_ref()
        );
        // ...and the JMOD-only class resolves through the lazy JMOD path,
        // after the jimage arm has declined it.
        assert_eq!(
            cp.find_class("com/example/OnlyInJmod").unwrap().as_ref(),
            jmod_only.as_slice()
        );

        let _ = fs::remove_file(&jimage_path);
        let _ = fs::remove_file(&jmod_path);
    }

    /// Malformed and truncated JMODs must produce errors, never panics.
    /// `load_jmod` is reached from `ClassPath::new` on any host-supplied
    /// `JAVA_HOME`, so a damaged install must degrade to "class not found".
    #[test]
    fn malformed_jmod_errors_instead_of_panicking() {
        let good = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(7, 4096),
        )]);

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("shorter_than_header", vec![0x4A, 0x4D, 0x01]),
            ("bad_magic", {
                let mut v = good.clone();
                v[0] = 0x50;
                v[1] = 0x4B;
                v
            }),
            ("future_major_version", {
                let mut v = good.clone();
                v[2] = 0x02;
                v
            }),
            ("header_only", TEST_JMOD_HEADER.to_vec()),
            ("garbage_after_header", {
                let mut v = TEST_JMOD_HEADER.to_vec();
                v.extend_from_slice(&[0xFFu8; 512]);
                v
            }),
            // Valid header, ZIP truncated mid-way: the central directory is
            // gone, so the archive cannot be opened at all.
            ("truncated_zip", good[..good.len() / 2].to_vec()),
        ];

        for (tag, bytes) in cases {
            let path = jmod_temp_path(tag);
            fs::write(&path, &bytes).unwrap();

            // Direct: `load_jmod` reports an error string.
            assert!(
                ClassPath::load_jmod(&path).is_err(),
                "load_jmod({tag}) must return Err, not succeed or panic"
            );

            // Through the public entry point: the damaged file is skipped and
            // lookups fail cleanly.
            let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
            assert!(
                cp.find_class("java/lang/Object").is_err(),
                "find_class on damaged JMOD ({tag}) must return Err"
            );

            let _ = fs::remove_file(&path);
        }
    }

    /// Corruption that the *eager* loader would have absorbed at load time
    /// (the entry simply never entered the cache) now surfaces at lookup
    /// time. It must surface as `ClassNotFound`, not as a panic and not as
    /// silently truncated class bytes.
    ///
    /// The archive itself stays well-formed — only the last entry's
    /// compressed payload is damaged — so `load_jmod` succeeds and the name
    /// is in the index, which is precisely the new lazy-path-only case.
    #[test]
    fn jmod_with_corrupt_entry_payload_reports_class_not_found() {
        let mut bytes = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(11, 65536),
        )]);

        // Locate the central directory via the end-of-central-directory
        // record (no ZIP comment is written, so it is the final 22 bytes).
        let eocd = bytes.len() - 22;
        assert_eq!(
            &bytes[eocd..eocd + 4],
            &[0x50, 0x4B, 0x05, 0x06],
            "expected EOCD signature at end of synthesized JMOD"
        );
        let cd_offset = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]) as usize
            + TEST_JMOD_HEADER.len(); // offsets are relative to the ZIP, not the JMOD

        // Damage 256 bytes of the compressed stream, ending just before the
        // central directory. Well clear of the local file header.
        let end = cd_offset;
        let start = end - 256;
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }

        let path = jmod_temp_path("corrupt_payload");
        fs::write(&path, &bytes).unwrap();

        // The archive still parses: the index is built from the central
        // directory, which we did not touch.
        let entry = ClassPath::load_jmod(&path).expect("central directory is intact");
        match &entry {
            ClassPathEntry::JmodFile {
                class_entry_index, ..
            } => assert!(
                class_entry_index.contains("java/lang/Object.class"),
                "the damaged entry is still indexed"
            ),
            other => panic!("expected JmodFile, got {other:?}"),
        }

        // But serving it fails cleanly rather than panicking or returning
        // a partially-inflated class file.
        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        match cp.find_class("java/lang/Object") {
            Err(ClassFileError::ClassNotFound { .. }) => {}
            Err(other) => panic!("expected ClassNotFound, got {other:?}"),
            Ok(bytes) => panic!(
                "corrupt entry must not resolve; got {} bytes",
                bytes.as_ref().len()
            ),
        }

        let _ = fs::remove_file(&path);
    }
}
