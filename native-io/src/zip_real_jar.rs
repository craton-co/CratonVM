// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RA.7 — real-mode `java.util.jar.JarFile` + `java.util.zip.ZipFile`
//! natives.
//!
//! Approach: each `new JarFile(...)` opens the underlying zip via the
//! `zip` crate and stashes the owned `ZipArchive` in a global handle
//! table, keyed by a monotonically-increasing i64. The Java object
//! stores the handle in its `path` / native handle slot; subsequent
//! `getEntry(String)`, `entries()`, and `getInputStream(ZipEntry)`
//! calls look up the archive by handle.
//!
//! Real JDK `ZipEntry` objects are populated by field name. Synthetic-JDK
//! fallback objects retain the compact five-slot layout used by the older
//! native ZIP bridge.
//!
//! `getInputStream(ZipEntry)` returns a `java.io.ByteArrayInputStream`
//! populated with the inflated bytes — this sidesteps the need for
//! a streaming Java `Inflater` bridge for the typical JarFile use
//! case (reading MANIFEST.MF, META-INF/services/* , class bytes).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use zip::extra_fields::ExtraField;

use crate::io_flags;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

/// Owned jar state per open handle.
struct JarState {
    path: PathBuf,
    archive: zip::ZipArchive<File>,
    /// Lookup cache: name → entry index inside the archive. Rebuilt on
    /// first access to avoid paying the cost for jars that only read
    /// one manifest entry.
    name_index: Option<HashMap<String, usize>>,
    /// Per-index central-directory metadata, filled lazily. See
    /// [`entry_meta_at`] for why this exists and what it costs without it.
    meta: Vec<MetaSlot>,
}

/// The central-directory facts a `ZipEntry` is built from. Every field here
/// comes out of the record the archive parsed at open time; none of it needs
/// the entry's *content*, which is the whole point of caching it.
#[derive(Clone)]
struct ZipEntryMeta {
    name: String,
    method: i64,
    size: i64,
    csize: i64,
    crc: i64,
    extra: Option<Vec<u8>>,
    times: ZipEntryTimes,
}

/// One `meta` slot: not looked at yet, looked at and unreadable, or known.
///
/// `Unreadable` is a state rather than a re-derivable absence so a malformed
/// entry costs one failed read instead of one per lookup, and so `entries()`
/// (which skips it) and `getEntry` (which raises) keep the different answers
/// they gave before this cache existed.
#[derive(Clone)]
enum MetaSlot {
    Unread,
    Unreadable,
    Ready(ZipEntryMeta),
}

fn jar_table() -> &'static Mutex<HashMap<i64, JarState>> {
    static T: OnceLock<Mutex<HashMap<i64, JarState>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> i64 {
    static COUNTER: AtomicI64 = AtomicI64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Decompression-bomb guard
// ---------------------------------------------------------------------------

/// Default maximum *declared* uncompressed size, in bytes, of a single zip
/// entry that `getInputStream` / manifest reads will fully inflate into
/// memory. 512 MiB is comfortably larger than any legitimate class-bytes or
/// resource entry while still bounding a malicious archive's ability to OOM
/// the process. Overridable at startup via the `CRATONVM_ZIP_MAX_ENTRY_BYTES`
/// environment variable (consistent with the crate's other `CRATONVM_*`
/// env-driven knobs).
const DEFAULT_MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum allowed compression ratio (declared-uncompressed / compressed).
/// A genuine deflate stream of real content tops out well under ~100:1; a
/// ratio beyond this is the signature of a "zip bomb" entry (e.g. a 4 GiB
/// run of zeros compressed to a few KiB) and is rejected. A `compressed_size`
/// of 0 (a STORED empty entry, or a size the archive failed to report) skips
/// the ratio check — the absolute-size cap still applies.
const MAX_COMPRESSION_RATIO: u64 = 1000;

/// Returns the configured per-entry uncompressed-size cap. Reads
/// `CRATONVM_ZIP_MAX_ENTRY_BYTES` once on first call; falls back to
/// [`DEFAULT_MAX_ENTRY_BYTES`] when unset, empty, or unparseable.
fn max_entry_bytes() -> u64 {
    crate::io_flags()
        .zip_max_entry_bytes
        .unwrap_or(DEFAULT_MAX_ENTRY_BYTES)
}

/// Validate a zip entry's *declared* sizes against the decompression-bomb
/// guards before any large allocation is made. On success returns the
/// uncompressed size as a `usize` (already known to fit the cap, hence the
/// target's `usize`). On failure returns a descriptive error.
///
/// `declared_uncompressed` and `compressed` are the untrusted values taken
/// straight from the archive's central directory — they must NOT be used to
/// size an allocation until this check has passed.
fn guard_zip_entry_size(
    entry_name: &str,
    declared_uncompressed: u64,
    compressed: u64,
) -> Result<usize, MethodCallFailed> {
    let cap = max_entry_bytes();
    if declared_uncompressed > cap {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "zip entry '{entry_name}': declared uncompressed size \
                 {declared_uncompressed} exceeds the {cap}-byte cap \
                 (CRATONVM_ZIP_MAX_ENTRY_BYTES); refusing to inflate \
                 (possible decompression bomb)",
            ),
        }));
    }
    // Compression-ratio sanity check: a wildly-better-than-real ratio is the
    // hallmark of a crafted bomb. Skip when `compressed` is 0 (STORED empty
    // entry, or an unreported size) since the absolute cap above still holds.
    if compressed > 0 {
        let ratio = declared_uncompressed / compressed;
        if ratio > MAX_COMPRESSION_RATIO {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!(
                    "zip entry '{entry_name}': compression ratio {ratio}:1 \
                     (uncompressed {declared_uncompressed} / compressed \
                     {compressed}) exceeds the {MAX_COMPRESSION_RATIO}:1 \
                     limit; refusing to inflate (possible decompression bomb)",
                ),
            }));
        }
    }
    // Known to be <= cap, which is itself far below usize::MAX on any
    // supported target, so this conversion cannot fail in practice; map the
    // error anyway rather than unwrap.
    usize::try_from(declared_uncompressed).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "zip entry '{entry_name}': uncompressed size \
                 {declared_uncompressed} exceeds usize::MAX on this target",
            ),
        })
    })
}

/// Cap applied to `Vec::with_capacity` for a zip entry. We never pre-allocate
/// more than 16 MiB up front even when the (validated) declared size is
/// larger: `read_to_end` will grow the buffer as real bytes arrive, so an
/// entry that *lies* about its size cannot trick us into a huge eager
/// allocation. The declared size has already passed [`guard_zip_entry_size`].
fn prealloc_hint(declared_size: usize) -> usize {
    const PREALLOC_CAP: usize = 16 * 1024 * 1024;
    declared_size.min(PREALLOC_CAP)
}

// ---------------------------------------------------------------------------
// JarFile field helpers
// ---------------------------------------------------------------------------

/// RA.7: `java.util.jar.JarFile` (and its parent `java.util.zip.ZipFile`)
/// declare a private long field `jzfile` that the real JDK uses as an
/// opaque handle into its C-side zlib state. We repurpose that slot to
/// hold our Rust-side `i64` handle so subsequent natives can find the
/// backing `JarState`. Dual-write also keeps a synthetic slot 1 for
/// backwards compat with any earlier code that hardcoded it.
fn set_jar_handle(ctx: &mut dyn NativeContext, this: ObjectRef, handle: i64) {
    ctx.set_field_by_name(this, "jzfile", Value::Long(handle));
    ctx.set_field(this, 1, Value::Long(handle));
}

/// Identity-hash → handle fallback for `get_jar_handle`.
///
/// Field-based handle storage works for `JarFile` (a CratonVM-synthetic class
/// whose object carries a usable Long slot) but NOT for a plain
/// `java.util.zip.ZipFile`: that object is allocated with no writable handle
/// slot, so `set_field`/`set_field_by_name` in `<init>` silently no-op (every
/// field reads back `Object(None)`), `get_jar_handle` returns 0, and
/// `entries()`/`getName()`/`size()` all fail — `ZipFile.entries()` famously
/// returning `null`, which NPEs ShrinkWrap's `URLPackageScanner`. We can't put
/// the handle on the object, so we key a side table on the object's stable
/// identity hash instead.
fn identity_handle_table() -> &'static Mutex<HashMap<i32, i64>> {
    static T: OnceLock<Mutex<HashMap<i32, i64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `System.identityHashCode(this)` — stable per object across GC.
fn identity_hash(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    match ctx.invoke(
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(this))],
    ) {
        Ok(Some(Value::Int(h))) => Some(h),
        _ => None,
    }
}

fn get_jar_handle(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    // Fast path: the on-object handle slot (works for JarFile).
    if let Value::Long(v) = ctx.get_field_by_name(this, "jzfile") {
        if v != 0 {
            return v;
        }
    }
    match ctx.get_field(this, 1) {
        Value::Long(v) if v != 0 => return v,
        Value::Int(v) if v != 0 => return v as i64,
        _ => {}
    }
    // Plain ZipFile: the object has no writable handle slot — recover via the
    // identity-keyed side table populated in `open_and_register`.
    if let Some(id) = identity_hash(ctx, this) {
        if let Some(h) = identity_handle_table().lock().get(&id).copied() {
            return h;
        }
    }
    0
}

fn read_file_abs_path(ctx: &mut dyn NativeContext, file_obj: ObjectRef) -> Option<String> {
    let path_val = ctx
        .invoke(
            "java/io/File",
            "getAbsolutePath",
            "()Ljava/lang/String;",
            &[Value::Object(Some(file_obj))],
        )
        .ok()??;
    match path_val {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn extract_string_arg(ctx: &dyn NativeContext, v: Value) -> Option<String> {
    match v {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Natives
// ---------------------------------------------------------------------------

/// `JarFile.<init>(File)` — open the jar and stash the handle.
fn native_jarfile_init_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let file_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "JarFile.<init>(File): file is null".to_string(),
            }));
        }
    };
    let path = read_file_abs_path(ctx, file_obj).ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "JarFile.<init>(File): file.getAbsolutePath returned null".to_string(),
        })
    })?;
    open_and_register(ctx, this, &path)
}

/// `JarFile.<init>(String)` — open the jar by path.
fn native_jarfile_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "JarFile.<init>(String): null or empty name".to_string(),
        }));
    }
    open_and_register(ctx, this, &name)
}

fn native_jarfile_init_string_verify(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The boolean `verify` flag is accepted but we don't currently
    // validate signatures. JBoss Modules' bootstrap jars aren't signed.
    native_jarfile_init_string(ctx, args)
}

fn native_jarfile_init_file_verify(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_jarfile_init_file(ctx, args)
}

/// Normalize a JAR/ZIP path the way `java.io.File` does for the one pattern
/// where the `(String)` and `(File)` `JarFile`/`ZipFile` constructors diverge
/// on Windows.
///
/// `java.util.zip.ZipFile(String name)` is specified as `this(new File(name))`,
/// so a leading-separator drive path like `/C:/foo` — exactly what
/// `URL{file:/C:/foo}.toURI().getSchemeSpecificPart()` yields on Windows —
/// normalizes to `C:\foo`. The `(File)` ctor already opens the normalized form
/// (it reads `file.getAbsolutePath()`), but the `(String)` ctor fed the raw
/// `/C:/foo` straight to the host `File::open`, which on Windows opens an
/// empty/wrong target, so `getEntry`/`getJarEntry`/`entries` saw zero entries.
/// Hibernate's archive scanner does precisely
/// `new JarFile(url.toURI().getSchemeSpecificPart())`, which is why locating
/// `META-INF/persistence.xml` inside a packaged `.par` failed (HIB-CV-17).
/// Strip the spurious leading separator before a `<letter>:` drive so both
/// constructors resolve the same file. Windows-only: on POSIX `/C:/foo` is a
/// legitimate path and must be left untouched.
fn normalize_drive_rooted_path(path: &str) -> &str {
    #[cfg(windows)]
    {
        let b = path.as_bytes();
        if b.len() >= 3
            && (b[0] == b'/' || b[0] == b'\\')
            && b[1].is_ascii_alphabetic()
            && b[2] == b':'
        {
            return &path[1..];
        }
    }
    path
}

fn open_and_register(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    path_str: &str,
) -> MethodCallResult {
    // HIB-CV-17: `ZipFile(String)`/`JarFile(String)` must resolve the same file
    // as the `(File)` ctor. Normalize a leading-separator Windows drive path
    // (`/C:/foo` → `C:/foo`) before opening, matching `new File(name)`.
    let path_str = normalize_drive_rooted_path(path_str);
    // SECURITY (HIGH): mirror the validation done by every other
    // guest-controlled file entry point in this crate. Under
    // `set_path_confine_to_cwd(true)` a guest must not be able to open
    // an arbitrary jar/zip outside the sandbox and read its contents —
    // and jars are routinely fed to class-loaders, so the read path is
    // privileged downstream. We surface a validation failure with the
    // same `Internal`-shaped error that the missing-file path uses
    // below, matching `JarFile`'s observed behavior for unreadable
    // archives.
    let validated_path = match crate::validate_path(path_str) {
        Ok(p) => p,
        Err(_) => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("JarFile: path rejected by sandbox: `{path_str}`"),
            }));
        }
    };
    let file = File::open(&validated_path).map_err(|e| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("JarFile: cannot open `{path_str}`: {e}"),
        })
    })?;
    let archive = zip::ZipArchive::new(file).map_err(|e| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("JarFile: `{path_str}` is not a valid zip: {e}"),
        })
    })?;
    let meta = vec![MetaSlot::Unread; archive.len()];
    let state = JarState {
        path: PathBuf::from(&validated_path),
        archive,
        name_index: None,
        meta,
    };
    let handle = next_handle();
    let table_len = {
        let mut t = jar_table().lock();
        t.insert(handle, state);
        t.len()
    };
    if io_flags().dbg_jar {
        eprintln!("[JAR] open handle={handle} table_len={table_len} path={validated_path:?}");
    }
    set_jar_handle(ctx, this, handle);
    // Recovery key for `get_jar_handle` when the object has no writable handle
    // slot (plain ZipFile): map its identity hash → handle.
    if let Some(id) = identity_hash(ctx, this) {
        identity_handle_table().lock().insert(id, handle);
    }
    // Also store the name on the parent ZipFile's `name` field if present.
    let name_str = ctx.create_string(path_str);
    ctx.set_field_by_name(this, "name", Value::Object(Some(name_str)));
    Ok(None)
}

/// Ensure `state.name_index` is populated, then return it.
///
/// `file_names()` walks the already-parsed central directory: no I/O, no
/// per-entry record re-validation, no decompressor.
fn ensure_name_index(state: &mut JarState) -> &HashMap<String, usize> {
    if state.name_index.is_none() {
        // Perf fix (audit MED): avoid per-entry `by_index`, which re-reads
        // and re-validates the central-directory record (and would set up a
        // decompressor) for every entry just to grab the name. `file_names`
        // walks the already-parsed central directory in O(n) and yields
        // `&str` slices into the archive's metadata — no I/O, no inflate.
        let mut idx: HashMap<String, usize> = HashMap::with_capacity(state.archive.len());
        for (i, name) in state.archive.file_names().enumerate() {
            idx.insert(name.to_string(), i);
        }
        state.name_index = Some(idx);
    }
    state
        .name_index
        .as_ref()
        .expect("name_index was just populated")
}

/// The cached central-directory metadata for `idx`, reading it out of the
/// archive the first time and never again.
///
/// # Why this cache exists
///
/// An open archive's central directory does not change, so every field a
/// `ZipEntry` carries is a constant for the life of the handle — yet both
/// callers used to re-derive it from the archive on every call.
///
/// `getEntry` went through `ZipArchive::by_index`, which does two things
/// beyond reading the record: it seeks to and parses the entry's LOCAL file
/// header, and it builds the whole decompressor chain
/// (`BufReader` + `Decompressor` + `Crc32Reader`) — an inflate window and
/// trees, tens of KiB allocated and dropped — purely so the call could ask
/// for `name()` and `size()`. Nothing about that reader is used here.
///
/// `by_index_raw` is used for the one real read because it skips
/// `make_reader`; its `find_content` seek is memoised by the `zip` crate in a
/// `OnceCell`, and after this cache it happens at most once per entry anyway.
///
/// # What this is worth, honestly
///
/// No benchmark moved when this landed, and the reason is worth keeping:
/// `java.util.jar.JarFile` does **not** reach this file. Its natives are
/// re-registered later by `native-builtins`' `register_p59_jar`, which
/// `overwrote` these (visible as `"overwrote": "bridge"` in
/// `--dump-native-registry`), so for a `JarFile` receiver everything here is
/// dead. The jar-scan cost that prompted the look was in that other
/// registrar — a `std::fs::metadata` per accessor call, ~20-54 us on Windows;
/// see `jarfile-accessors-stat-the-file-on-every-call-FIXED-20260811.md`.
///
/// This is kept because it is strictly less work on the plain
/// `java.util.zip.ZipFile` path, which this file does still own, not because
/// a number moved.
fn entry_meta_at(state: &mut JarState, idx: usize) -> Option<ZipEntryMeta> {
    if idx >= state.meta.len() {
        // An archive whose length outran the slot vector (cannot happen for
        // the handles we build, but the index is caller-supplied): grow
        // rather than panic.
        state
            .meta
            .resize(state.archive.len().max(idx + 1), MetaSlot::Unread);
    }
    match &state.meta[idx] {
        MetaSlot::Ready(meta) => return Some(meta.clone()),
        MetaSlot::Unreadable => return None,
        MetaSlot::Unread => {}
    }
    let slot = match state.archive.by_index_raw(idx) {
        Ok(entry) => MetaSlot::Ready(ZipEntryMeta {
            name: entry.name().to_string(),
            // Round-9 HIGH: do NOT collapse non-Deflate methods to
            // DEFLATED(8). A previous shortcut returned 8 for every
            // non-stored method; later code paths that select an inflate
            // decompressor based on `method` then ran zlib on a BZIP2/LZMA
            // payload, producing corrupt bytes. Map to the standard ZIP
            // method codes so consumers see the real compression scheme.
            method: compression_method_code(&entry.compression()),
            size: entry.size() as i64,
            csize: entry.compressed_size() as i64,
            crc: entry.crc32() as i64 & 0xFFFF_FFFFi64,
            extra: entry
                .extra_data()
                .filter(|bytes| !bytes.is_empty())
                .map(ToOwned::to_owned),
            times: zip_entry_times(&entry),
        }),
        Err(_) => MetaSlot::Unreadable,
    };
    state.meta[idx] = slot;
    match &state.meta[idx] {
        MetaSlot::Ready(meta) => Some(meta.clone()),
        _ => None,
    }
}

/// `JarFile.getEntry(String)` / `ZipFile.getEntry(String)` → `ZipEntry`.
fn native_jarfile_get_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let handle = get_jar_handle(ctx, this);
    let mut table = jar_table().lock();
    let state = match table.get_mut(&handle) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };

    let idx = {
        let index = ensure_name_index(state);
        match index.get(&name) {
            Some(i) => *i,
            None => {
                // Try with trailing slash (directory semantics) before
                // giving up — matches JDK ZipFile behavior.
                let alt = format!("{name}/");
                match index.get(&alt) {
                    Some(i) => *i,
                    None => return Ok(Some(Value::Object(None))),
                }
            }
        }
    };

    // Materialize the entry metadata (cached per index — see `entry_meta_at`).
    let meta = entry_meta_at(state, idx).ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("ZipArchive::by_index_raw({idx}) failed"),
        })
    })?;
    drop(table);

    Ok(Some(Value::Object(Some(alloc_zip_entry(
        ctx,
        &meta.name,
        meta.method,
        meta.size,
        meta.csize,
        meta.crc,
        meta.extra,
        meta.times,
    )?))))
}

/// Map a `zip::CompressionMethod` to the standard ZIP method code (per
/// the APPNOTE.TXT specification: 0=STORED, 8=DEFLATED, 12=BZIP2,
/// 14=LZMA, etc).
///
/// Round-9 HIGH (native-misc): previously we collapsed everything that
/// wasn't `Stored` to 8 (DEFLATED). Java callers (including the JDK's
/// `ZipFile.getInputStream` infrastructure if it ever ran on this entry,
/// and user code inspecting `ZipEntry.getMethod()`) then assumed a zlib
/// payload — inflating BZIP2/LZMA bytes silently produced corrupted
/// output. We now publish the real method code so callers can detect
/// the unsupported scheme and throw `ZipException` instead of decoding
/// garbage. Variants we don't have feature-compiled at all are reached
/// via the `Unsupported(u16)` arm and we propagate the raw code as-is.
#[allow(deprecated)]
fn compression_method_code(m: &zip::CompressionMethod) -> i64 {
    // We avoid `serialize_to_u16` (pub(crate)) and `to_u16` (deprecated)
    // by pattern matching the variants present in the build. The deflate
    // feature is always on in our workspace `Cargo.toml`; other features
    // (bzip2, lzma, zstd, xz, deflate64, aes-crypto) are off by default,
    // so non-Stored / non-Deflated methods come through as the catch-all.
    match m {
        zip::CompressionMethod::Stored => 0,
        zip::CompressionMethod::Deflated => 8,
        // `Unsupported(v)` carries the raw 2-byte method code from the
        // ZIP central directory. Returning it verbatim is what the JDK
        // does on read: `ZipEntry.method` exposes the raw value and
        // `ZipFile.getInputStream` throws `ZipException("invalid CEN
        // header (bad compression method: X)")`. Until we wire that
        // path the value still warns Java code via `getMethod() != 8`.
        other => i64::from(other.to_u16()),
    }
}

fn alloc_zip_entry(
    ctx: &mut dyn NativeContext,
    name: &str,
    method: i64,
    size: i64,
    csize: i64,
    crc: i64,
    extra: Option<Vec<u8>>,
    times: ZipEntryTimes,
) -> Result<ObjectRef, MethodCallFailed> {
    // Materialize every allocation-prone value before allocating the ZipEntry.
    // This keeps the fresh entry out of a bare native local across a GC point.
    let mtime = times
        .modified
        .map(|time| zip_filetime(ctx, time))
        .transpose()?;
    let atime = times
        .access
        .map(|time| zip_filetime(ctx, time))
        .transpose()?;
    let ctime = times
        .creation
        .map(|time| zip_filetime(ctx, time))
        .transpose()?;
    let name_str = ctx.create_string(name);
    let extra = extra.map(|bytes| {
        let array = ctx.new_array(ArrayElementType::Byte, bytes.len());
        ctx.write_byte_array_from(array, 0, &bytes);
        array
    });
    let (entry, real_layout) = match ctx.ensure_class_initialized("java/util/zip/ZipEntry") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            (
                ctx.alloc_object(cid, real.max(6)),
                ctx.resolve_field_index_by_class_id(cid, "xdostime")
                    .is_some(),
            )
        }
        Err(_) => (ctx.alloc_object(ClassId::new(0), 6), false),
    };
    if real_layout {
        // Do not dual-write synthetic slots here: in the real JDK layout slot
        // 1 is `xdostime`, not `method`. The old write of `method` to slot 1
        // made every native ZipFile entry report a DOS date in 1979.
        ctx.set_field_by_name(entry, "name", Value::Object(Some(name_str)));
        ctx.set_field_by_name(entry, "method", Value::Int(method as i32));
        ctx.set_field_by_name(entry, "size", Value::Long(size));
        ctx.set_field_by_name(entry, "csize", Value::Long(csize));
        ctx.set_field_by_name(entry, "crc", Value::Long(crc));
        if let Some(extra) = extra {
            ctx.set_field_by_name(entry, "extra", Value::Object(Some(extra)));
        }
        if let Some(dos_time) = times.dos_time {
            ctx.set_field_by_name(entry, "xdostime", Value::Long(dos_time));
        }
        if let Some(time) = mtime {
            ctx.set_field_by_name(entry, "mtime", Value::Object(Some(time)));
        }
        if let Some(time) = atime {
            ctx.set_field_by_name(entry, "atime", Value::Object(Some(time)));
        }
        if let Some(time) = ctime {
            ctx.set_field_by_name(entry, "ctime", Value::Object(Some(time)));
        }
    } else {
        // Compact synthetic-JDK fallback: name, method, size, csize, crc.
        ctx.set_field(entry, 0, Value::Object(Some(name_str)));
        ctx.set_field(entry, 1, Value::Long(method));
        ctx.set_field(entry, 2, Value::Long(size));
        ctx.set_field(entry, 3, Value::Long(csize));
        ctx.set_field(entry, 4, Value::Long(crc));
    }
    Ok(entry)
}

/// The three `ZipEntry` time attributes, as the **central directory** carries
/// them.
///
/// The JDK's `ZipOutputStream` writes the 0x5455 extended-timestamp field
/// twice with different payloads: the local header gets every time it was
/// given (`5554 0d00 07 …`, flags 0x07, 13 bytes), while the central-directory
/// copy keeps the flags byte but carries the modified time alone
/// (`5554 0500 07 …`, 5 bytes). `ZipFile`/`JarFile` read the central
/// directory, so on HotSpot `getLastAccessTime()` and `getCreationTime()`
/// answer `null` for a JDK-written entry; only `ZipInputStream`, which walks
/// local headers, sees all three.
///
/// We used to read the local header too and merge it in, which made
/// `JarFile.entries()` answer real values where HotSpot answers `null`. Beyond
/// the parity break it cost a `File::open` plus two seeks and a read *per
/// entry, per call* — material on a jar with thousands of entries. Take the
/// central record and nothing else.
#[derive(Clone, Copy, Default)]
struct ZipEntryTimes {
    /// The central-directory DOS timestamp packed as `(date << 16) | time`.
    /// `ZipEntry.getTime()` uses this when no higher-precision `mtime` exists.
    dos_time: Option<i64>,
    modified: Option<i64>,
    access: Option<i64>,
    creation: Option<i64>,
}

/// Pull high-fidelity times out of ZIP extra fields. The DOS header is only
/// two-second resolution and, for entries written by the JDK with FileTime
/// metadata, may be the 1980 fallback while the real values live in 0x5455.
///
/// `extra_data_fields()` yields the **central-directory** extras — `by_index`
/// and `by_index_raw` both reach `central_header_to_zip_file`, which is the
/// only place the `zip` crate runs `parse_extra_field` for a seekable archive
/// (the other call site is `read_zipfile_from_stream`, a path we never take).
/// That is exactly the JDK's source for `ZipFile`/`JarFile`, so this function
/// must not be supplemented from anywhere else — see the note on
/// `ZipEntryTimes` for why reading the local header instead broke parity.
fn zip_entry_times(entry: &zip::read::ZipFile<'_>) -> ZipEntryTimes {
    let mut times = ZipEntryTimes::default();
    times.dos_time = entry
        .last_modified()
        .map(|time| (i64::from(time.datepart()) << 16) | i64::from(time.timepart()));
    for field in entry.extra_data_fields() {
        merge_zip_entry_times(&mut times, zip_extra_field_times(field));
    }
    times
}

fn zip_extra_field_times(field: &ExtraField) -> ZipEntryTimes {
    match field {
        ExtraField::ExtendedTimestamp(timestamp) => ZipEntryTimes {
            modified: timestamp.mod_time().map(|time| i64::from(time) * 1_000),
            access: timestamp.ac_time().map(|time| i64::from(time) * 1_000),
            creation: timestamp.cr_time().map(|time| i64::from(time) * 1_000),
            ..ZipEntryTimes::default()
        },
        ExtraField::Ntfs(timestamp) => ZipEntryTimes {
            modified: Some(windows_filetime_to_unix_millis(timestamp.mtime())),
            access: Some(windows_filetime_to_unix_millis(timestamp.atime())),
            creation: Some(windows_filetime_to_unix_millis(timestamp.ctime())),
            ..ZipEntryTimes::default()
        },
    }
}

fn windows_filetime_to_unix_millis(time: u64) -> i64 {
    (i128::from(time) / 10_000 - 11_644_473_600_000i128) as i64
}

fn merge_zip_entry_times(target: &mut ZipEntryTimes, source: ZipEntryTimes) {
    if source.modified.is_some() {
        target.modified = source.modified;
    }
    if source.access.is_some() {
        target.access = source.access;
    }
    if source.creation.is_some() {
        target.creation = source.creation;
    }
}

fn zip_filetime(ctx: &mut dyn NativeContext, millis: i64) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.invoke(
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(millis)],
    )? {
        Some(Value::Object(Some(time))) => Ok(time),
        _ => Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "FileTime.fromMillis returned no FileTime".to_string(),
        })),
    }
}

fn read_zip_entry_name(ctx: &dyn NativeContext, entry: ObjectRef) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(entry, "name") {
        if let Some(t) = ctx.read_string(s) {
            return Some(t);
        }
    }
    match ctx.get_field(entry, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// `JarFile.getInputStream(ZipEntry)` → `ByteArrayInputStream` with the
/// inflated entry bytes.
fn native_jarfile_get_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let entry = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = read_zip_entry_name(ctx, entry).unwrap_or_default();
    if name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    let (bytes, deflated) = {
        let handle = get_jar_handle(ctx, this);
        let mut table = jar_table().lock();
        let state = match table.get_mut(&handle) {
            Some(s) => s,
            None => return Ok(Some(Value::Object(None))),
        };
        let idx = match state.name_index.as_ref().and_then(|m| m.get(&name)) {
            Some(i) => *i,
            None => {
                // Lazy fill — same O(n) `file_names` path as `getEntry`.
                let mut idx: HashMap<String, usize> = HashMap::with_capacity(state.archive.len());
                for (i, n) in state.archive.file_names().enumerate() {
                    idx.insert(n.to_string(), i);
                }
                let got = idx.get(&name).copied();
                state.name_index = Some(idx);
                match got {
                    Some(i) => i,
                    None => return Ok(Some(Value::Object(None))),
                }
            }
        };
        let mut zf = state.archive.by_index(idx).map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("JarFile.getInputStream({name}): by_index({idx}) failed: {e}"),
            })
        })?;
        // Audit fix: ZIP entries can declare uncompressed sizes >=4 GiB
        // (ZIP64). On 32-bit targets `as usize` would silently truncate
        // the capacity hint to its low 32 bits, leaving us under-reserved
        // and — worse — masking a genuinely-too-big entry as a benign
        // small allocation.
        //
        // Decompression-bomb guard: the declared uncompressed size comes
        // from an untrusted archive. `guard_zip_entry_size` rejects entries
        // exceeding the configurable per-entry cap or an implausible
        // compression ratio, and `prealloc_hint` bounds the eager
        // `with_capacity` so a lying size cannot OOM us before any bytes
        // are read.
        let raw_size = zf.size();
        let size = guard_zip_entry_size(&name, raw_size, zf.compressed_size())?;
        // Which of the JDK's two entry-stream shapes this entry gets is
        // decided by its compression method — see `wrap_inflater_like`.
        let deflated = zf.compression() != zip::CompressionMethod::Stored;
        let mut buf: Vec<u8> = Vec::with_capacity(prealloc_hint(size));
        zf.read_to_end(&mut buf).map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("JarFile.getInputStream({name}): read failed: {e}"),
            })
        })?;
        (buf, deflated)
    };

    let bais = build_byte_array_input_stream(ctx, &bytes)?;
    if deflated {
        return Ok(Some(Value::Object(Some(wrap_inflater_like(ctx, bais)?))));
    }
    Ok(Some(Value::Object(Some(bais))))
}

/// Give a DEFLATED entry's stream the one `InflaterInputStream` behaviour a
/// bare `ByteArrayInputStream` gets wrong: a **zero-length read at EOF**.
///
/// The JDK hands `ZipFile.getInputStream` callers one of two streams, and they
/// disagree on exactly that call (MEASURED, Temurin 25.0.3, `ZeroLen2Probe`):
///
/// | entry method | JDK class | `read(b,0,0)` at EOF | `markSupported()` |
/// |---|---|---|---|
/// | DEFLATED | `ZipFile$ZipFileInflaterInputStream` | **0** (`InflaterInputStream`: `len == 0` returns 0 before anything else) | false |
/// | STORED | `ZipFile$ZipFileInputStream` | **-1** (`rem == 0` is checked first) | false |
///
/// `ByteArrayInputStream` answers -1 for both, because its own `read` checks
/// `pos >= count` before it clamps `len` — right for STORED, wrong for
/// DEFLATED. That one value is not academic: `java.io.InputStream`'s contract
/// says a zero-length read returns 0, so callers written against it treat -1
/// as end-of-file. H2's `FileUtils.readFully(FileChannel, ByteBuffer)` does
/// exactly that (`if (r < 0) throw new EOFException()`), and a read of the
/// zero remaining bytes at the end of a zip entry — which `FileZip.read`
/// forwards straight to this stream — therefore threw `EOFException` where
/// HotSpot completed the loop (`TestFileSystem.testZipFileSystem`, prefixes
/// `zip:` and `cache:zip:`).
///
/// `PushbackInputStream` is the wrapper that reproduces the DEFLATED row
/// without disturbing anything else: it returns 0 for `len == 0` unconditionally,
/// reports `markSupported()` as false (as both JDK zip streams do, and unlike
/// the bare `ByteArrayInputStream` returned until now), and leaves
/// `available()` exact — which the real `ZipFileInflaterInputStream` also is,
/// and a plain `InflaterInputStream` over the raw deflate bytes would NOT be
/// (it answers 1 until EOF), which is why this wraps the already-inflated
/// bytes rather than handing out a real inflater.
fn wrap_inflater_like(
    ctx: &mut dyn NativeContext,
    inner: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let cls = "java/io/PushbackInputStream";
    // `inner` must survive the class init and the allocation below, and the
    // wrapper must survive its own `<init>` — any of the three can trigger a
    // collection that moves them.
    let pin_inner = ctx.pin_native_root(inner);
    let cid = match ctx.ensure_class_initialized(cls) {
        Ok(cid) => cid,
        Err(_) => {
            ctx.unpin_native_roots(pin_inner);
            return Ok(inner);
        }
    };
    // The wrapper is only an improvement if the class it wraps in can actually
    // serve a bulk read. In real-JDK mode `read([BII)I` is the class's own
    // bytecode and this is the whole point; where the class exists only as the
    // five natives `phases_late::io_streams` registers for it (`<init>`,
    // `read()I`, `unread`, `available`, `close`), wrapping would REPLACE a
    // working bulk read with a missing method. Fall back to the unwrapped
    // stream there — the same behaviour as before this fix.
    if !ctx.method_exists(cls, "read", "([BII)I") {
        ctx.unpin_native_roots(pin_inner);
        return Ok(inner);
    }
    let obj = ctx.alloc_object(cid, ctx.class_num_total_fields(cid).max(4));
    let pin_obj = ctx.pin_native_root(obj);
    let inner = ctx.read_native_pin(pin_inner, inner);
    let init = ctx.invoke(
        cls,
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(obj)), Value::Object(Some(inner))],
    );
    let obj = ctx.read_native_pin(pin_obj, obj);
    let inner = ctx.read_native_pin(pin_inner, inner);
    ctx.unpin_native_roots(pin_inner);
    // A wrapper that could not be constructed is not worth failing the read
    // over: the bytes are already inflated and the unwrapped stream is what
    // this native returned before the fix.
    match init {
        Ok(_) => Ok(obj),
        Err(_) => Ok(inner),
    }
}

fn build_byte_array_input_stream(
    ctx: &mut dyn NativeContext,
    data: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let array = ctx.new_array(ArrayElementType::Byte, data.len());
    // AUDIT 2026-05-17: migrated to NativeContext::write_byte_array_from
    // bulk intrinsic (round-3 perf path). The VM override uses
    // `ptr::copy_nonoverlapping` against the array's raw payload, so
    // a 4 MiB class-bytes entry is one memcpy instead of 4M
    // `Value::Int` allocations + 4M dispatches.
    ctx.write_byte_array_from(array, 0, data);
    let bais_class = "java/io/ByteArrayInputStream";
    let cid = ctx.ensure_class_initialized(bais_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("{bais_class}: class initialization failed"),
        })
    })?;
    // Construct via <init>([B) — the standard BAIS ctor.
    let obj = ctx.alloc_object(cid, ctx.class_num_total_fields(cid).max(4));
    ctx.invoke(
        bais_class,
        "<init>",
        "([B)V",
        &[Value::Object(Some(obj)), Value::Object(Some(array))],
    )?;
    Ok(obj)
}

/// Build a `java.util.ArrayList` of synthetic `ZipEntry` objects for every
/// entry in `this`'s archive. Shared by `entries()` (wrapped in an
/// `Enumeration`) and `stream()` (wrapped in a `Stream`) — see the doc
/// comment on `native_jarfile_stream` for why `stream()` needs its own
/// native rather than falling through to real bytecode.
fn build_zip_entry_list(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let handle = get_jar_handle(ctx, this);
    let entries: Vec<ZipEntryMeta> = {
        let mut table = jar_table().lock();
        let state = match table.get_mut(&handle) {
            Some(s) => s,
            None => return Ok(Some(Value::Object(None))),
        };
        // Perf fix (audit MED, round-5/7 carryover): the old code called
        // `by_index(i)` for every entry. `by_index` does **two** expensive
        // things per entry: (1) `find_content` seeks to the local file
        // header and parses it, and (2) it builds a `make_reader` chain
        // (Inflate / Bzip / …) just so we can call `name()`/`size()` —
        // which both live in the central-directory record and never need
        // any of that machinery.
        //
        // `entry_meta_at` finishes that job: `by_index_raw` (no decompressor)
        // is paid at most ONCE per entry for the life of the handle, so a
        // second `entries()` on the same jar — which Jasper's TLD scan does
        // once per embedded-container start, 121 times in
        // `TomcatServletWebServerFactoryTests` — is pure cache reads.
        // An entry whose record will not read is skipped here exactly as the
        // old `if let Ok(f)` skipped it.
        let n = state.archive.len();
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            if let Some(meta) = entry_meta_at(state, i) {
                v.push(meta);
            }
        }
        v
    };

    // Build a java.util.ArrayList containing ZipEntry objects, then
    // return `list.elements()` — an Enumeration view.
    let al_class = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("{al_class}: not loaded"),
        })
    })?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al_class, "<init>", "()V", &[Value::Object(Some(list))])?;
    for meta in entries {
        let ze = alloc_zip_entry(
            ctx,
            &meta.name,
            meta.method,
            meta.size,
            meta.csize,
            meta.crc,
            meta.extra,
            meta.times,
        )?;
        ctx.invoke(
            al_class,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(ze))],
        )?;
    }
    Ok(Some(Value::Object(Some(list))))
}

/// `ZipFile.entries()` / `JarFile.entries()` → `Enumeration<ZipEntry>`.
/// Returns a synthetic `java.util.Enumeration` backed by a Rust Vec
/// snapshot of the archive's entries.
fn native_jarfile_entries(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list = match build_zip_entry_list(ctx, this)? {
        Some(Value::Object(Some(l))) => l,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Collections.enumeration(list)
    ctx.invoke(
        "java/util/Collections",
        "enumeration",
        "(Ljava/util/Collection;)Ljava/util/Enumeration;",
        &[Value::Object(Some(list))],
    )
}

/// `ZipFile.stream()` / `JarFile.stream()`.
///
/// BUG (found 2026-07-12): this method was NOT registered, so real JDK
/// bytecode ran instead — and real `ZipFile.stream()` calls the private
/// `ensureOpen()`, whose bytecode reads `this.res.zsrc` directly (not just
/// a null-check on `res`). Our synthetic `<init>` natives
/// (`native_jarfile_init_file`/`_string`/`_verify`) never run the real
/// constructor, so the real `res` (`ZipFile$CleanableResource`) field is
/// never populated and stays null — `getfield res.zsrc` on a null `res`
/// throws `NullPointerException: Cannot read field "zsrc" because "this.res"
/// is null`. Every OTHER public `ZipFile`/`JarFile` method that calls
/// `ensureOpen()` (`getEntry`, `getInputStream`, `entries`, `close`,
/// `getName`, `size`) is already registered here and so never reaches that
/// real bytecode — `stream()` (and `getComment()`, see below) were the gap.
/// Fix: register `stream()` too, reusing the same `ArrayList` this class
/// already builds for `entries()`, wrapped via `ArrayList.stream()` instead
/// of `Collections.enumeration(...)`.
fn native_jarfile_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list = match build_zip_entry_list(ctx, this)? {
        Some(Value::Object(Some(l))) => l,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke(
        "java/util/ArrayList",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(list))],
    )
}

/// `JarFile.getManifest()` → `java.util.jar.Manifest` loaded from
/// `META-INF/MANIFEST.MF`, or null if absent.
fn native_jarfile_get_manifest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handle = get_jar_handle(ctx, this);
    let mf_bytes = {
        let mut table = jar_table().lock();
        let state = match table.get_mut(&handle) {
            Some(s) => s,
            None => return Ok(Some(Value::Object(None))),
        };
        // Perf fix: reuse the lazily-built `name_index` instead of an O(n)
        // `file_names()` scan on every call. Build the index once (same as
        // `getEntry`/`getInputStream`), then do a hashed lookup.
        if state.name_index.is_none() {
            let mut idx: HashMap<String, usize> = HashMap::with_capacity(state.archive.len());
            for (i, n) in state.archive.file_names().enumerate() {
                idx.insert(n.to_string(), i);
            }
            state.name_index = Some(idx);
        }
        // `name_index` is keyed by exact entry name; the manifest is stored
        // under the canonical "META-INF/MANIFEST.MF". Some archives use a
        // lower/mixed-case path, so retain the case-insensitive fallback
        // (still hashed for the common case) to match prior behavior.
        let idx_opt = state.name_index.as_ref().and_then(|m| {
            m.get("META-INF/MANIFEST.MF").copied().or_else(|| {
                m.iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("META-INF/MANIFEST.MF"))
                    .map(|(_, i)| *i)
            })
        });
        let Some(idx) = idx_opt else {
            return Ok(Some(Value::Object(None)));
        };
        let mut zf = state.archive.by_index(idx).map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("manifest read by_index({idx}): {e}"),
            })
        })?;
        // Same ZIP64 truncation + decompression-bomb guard as
        // `getInputStream`: reject an oversized or bomb-ratio manifest
        // entry, and bound the eager pre-allocation.
        let raw_size = zf.size();
        let size = guard_zip_entry_size("META-INF/MANIFEST.MF", raw_size, zf.compressed_size())?;
        let mut buf = Vec::with_capacity(prealloc_hint(size));
        zf.read_to_end(&mut buf).map_err(|e| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("manifest read body: {e}"),
            })
        })?;
        buf
    };

    // Feed the bytes to a fresh Manifest via new Manifest(InputStream).
    let bais = build_byte_array_input_stream(ctx, &mf_bytes)?;
    let mf_class = "java/util/jar/Manifest";
    let mf_cid = ctx.ensure_class_initialized(mf_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("{mf_class}: not loaded"),
        })
    })?;
    let mf_obj = ctx.alloc_object(mf_cid, ctx.class_num_total_fields(mf_cid).max(2));
    ctx.invoke(
        mf_class,
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(mf_obj)), Value::Object(Some(bais))],
    )?;
    Ok(Some(Value::Object(Some(mf_obj))))
}

/// `JarFile.close()` — drop the archive and remove the handle.
fn native_jarfile_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let handle = get_jar_handle(ctx, this);
    let table_len = {
        let mut t = jar_table().lock();
        t.remove(&handle);
        t.len()
    };
    if io_flags().dbg_jar {
        eprintln!("[JAR] close handle={handle} table_len={table_len}");
    }
    // Drop the identity→handle recovery entry if it still points at us.
    if let Some(id) = identity_hash(ctx, this) {
        let mut t = identity_handle_table().lock();
        if t.get(&id).copied() == Some(handle) {
            t.remove(&id);
        }
    }
    set_jar_handle(ctx, this, 0);
    Ok(None)
}

/// `ZipFile.getName()` — returns the absolute path we opened under.
fn native_jarfile_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handle = get_jar_handle(ctx, this);
    let path = {
        let table = jar_table().lock();
        match table.get(&handle) {
            Some(s) => s.path.to_string_lossy().into_owned(),
            None => String::new(),
        }
    };
    if path.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let s = ctx.create_string(&path);
    Ok(Some(Value::Object(Some(s))))
}

/// `ZipFile.size()` — number of entries.
fn native_jarfile_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let handle = get_jar_handle(ctx, this);
    let n = {
        let table = jar_table().lock();
        match table.get(&handle) {
            Some(s) => s.archive.len() as i32,
            None => 0,
        }
    };
    Ok(Some(Value::Int(n)))
}

/// `ZipFile.getComment()` — the zip's central-directory comment, or `null`
/// if none. Registered for the same reason `stream()` is (see
/// `native_jarfile_stream`'s doc comment): real bytecode calls
/// `ensureOpen()`, which NPEs on our never-populated `res` field.
fn native_jarfile_get_comment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handle = get_jar_handle(ctx, this);
    let comment = {
        let table = jar_table().lock();
        match table.get(&handle) {
            Some(s) => s.archive.comment().to_vec(),
            None => return Ok(Some(Value::Object(None))),
        }
    };
    if comment.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let s = ctx.create_string(&String::from_utf8_lossy(&comment));
    Ok(Some(Value::Object(Some(s))))
}

// Hold the unused-value-warning silencer for extract_string_arg.
#[allow(dead_code)]
fn _extract_string_arg_unused(_ctx: &dyn NativeContext, _v: Value) -> Option<String> {
    extract_string_arg(_ctx, _v)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_jar_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let jf = "java/util/jar/JarFile";
    let zf = "java/util/zip/ZipFile";

    for cls in [jf, zf] {
        r.register(cls, "<init>", "(Ljava/io/File;)V", native_jarfile_init_file);
        r.register(
            cls,
            "<init>",
            "(Ljava/io/File;Z)V",
            native_jarfile_init_file_verify,
        );
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/String;)V",
            native_jarfile_init_string,
        );
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/String;Z)V",
            native_jarfile_init_string_verify,
        );
        r.register(
            cls,
            "getEntry",
            "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
            native_jarfile_get_entry,
        );
        r.register(
            cls,
            "getInputStream",
            "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
            native_jarfile_get_input_stream,
        );
        r.register(
            cls,
            "entries",
            "()Ljava/util/Enumeration;",
            native_jarfile_entries,
        );
        r.register(
            cls,
            "stream",
            "()Ljava/util/stream/Stream;",
            native_jarfile_stream,
        );
        r.register(
            cls,
            "getComment",
            "()Ljava/lang/String;",
            native_jarfile_get_comment,
        );
        r.register(cls, "close", "()V", native_jarfile_close);
        r.register(
            cls,
            "getName",
            "()Ljava/lang/String;",
            native_jarfile_get_name,
        );
        r.register(cls, "size", "()I", native_jarfile_size);
    }

    // JarFile-specific: getManifest
    r.register(
        jf,
        "getManifest",
        "()Ljava/util/jar/Manifest;",
        native_jarfile_get_manifest,
    );
    r.register(
        jf,
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        native_jarfile_get_entry,
    );
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::io::Write;
    use tempfile::NamedTempFile;
    use zip::write::SimpleFileOptions;

    fn make_test_jar() -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zw.write_all(b"Manifest-Version: 1.0\r\n\r\n").unwrap();
        zw.start_file("hello.txt", opts).unwrap();
        zw.write_all(b"hello jar world").unwrap();
        zw.finish().unwrap();
        tmp
    }

    #[test]
    #[ignore]
    fn diag_bench_open_all_module_jars() {
        // Diagnostic-only, not a real regression test: point
        // CRATONVM_DIAG_JAR_LIST at a newline/semicolon-separated classpath
        // file (e.g. spring-boot-jetty's cratonvm-test-cp.txt) and time how
        // long raw `zip::ZipArchive::new` open + central-directory parse
        // takes across every real jar, to isolate whether that's the TLD-scan
        // slowdown bottleneck independent of the VM/interpreter.
        let list_path = crate::io_flags()
            .diag_jar_list
            .clone()
            .expect("set CRATONVM_DIAG_JAR_LIST to a classpath file path");
        let contents = std::fs::read_to_string(&list_path).unwrap();
        let paths: Vec<&str> = contents
            .split(|c| c == ';' || c == '\n' || c == '\r')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && s.to_ascii_lowercase().ends_with(".jar"))
            .collect();
        eprintln!("diag: {} jar paths", paths.len());

        let start = std::time::Instant::now();
        let mut opened = 0usize;
        let mut failed = 0usize;
        let mut per_jar: Vec<(std::time::Duration, &str)> = Vec::new();
        for p in &paths {
            let t0 = std::time::Instant::now();
            match File::open(p) {
                Ok(f) => match zip::ZipArchive::new(f) {
                    Ok(_archive) => {
                        opened += 1;
                        per_jar.push((t0.elapsed(), p));
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("diag: zip parse failed for {p}: {e}");
                    }
                },
                Err(e) => {
                    failed += 1;
                    eprintln!("diag: open failed for {p}: {e}");
                }
            }
        }
        let total = start.elapsed();
        per_jar.sort_by(|a, b| b.0.cmp(&a.0));
        eprintln!(
            "diag: opened={opened} failed={failed} total={total:?} avg={:?}",
            total.checked_div(opened.max(1) as u32).unwrap_or_default()
        );
        eprintln!("diag: top 15 slowest opens:");
        for (dur, p) in per_jar.iter().take(15) {
            eprintln!("diag:   {dur:?}  {p}");
        }

        // Second pass: re-open the SAME jars again (simulating a second
        // TldScanner pass / second server start in the same process) to see
        // whether repeat opens are cheaper (OS file-cache warm) or the same
        // cost every time.
        let start2 = std::time::Instant::now();
        for p in &paths {
            if let Ok(f) = File::open(p) {
                let _ = zip::ZipArchive::new(f);
            }
        }
        eprintln!(
            "diag: second pass (warm cache) total={:?}",
            start2.elapsed()
        );
    }

    #[test]
    fn open_jar_and_list_entries() {
        let tmp = make_test_jar();
        let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
        // 2 entries expected (manifest + hello.txt)
        assert_eq!(archive.len(), 2);
    }

    #[test]
    fn read_manifest_content_from_zip() {
        let tmp = make_test_jar();
        let mut archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
        let mut mf = archive.by_name("META-INF/MANIFEST.MF").unwrap();
        let mut s = String::new();
        mf.read_to_string(&mut s).unwrap();
        assert!(s.starts_with("Manifest-Version: 1.0"));
    }

    #[test]
    fn handle_allocator_is_monotonic() {
        let h1 = next_handle();
        let h2 = next_handle();
        assert!(h2 > h1);
    }

    #[test]
    fn cursor_readback_preserves_bytes() {
        let data = b"test-bytes";
        let mut c = Cursor::new(data.to_vec());
        let mut out = Vec::new();
        c.read_to_end(&mut out).unwrap();
        assert_eq!(&out, data);
    }

    /// A JDK-written entry carries two *different* 0x5455 payloads: 13 bytes
    /// with all three times in the local header, 5 bytes with the modified
    /// time alone in the central directory — and both stamp the same flags
    /// byte 0x07. `zip_entry_times` reads central records, so it must answer
    /// modified-only, matching what HotSpot's `ZipFile` reports. Feeding it
    /// the local payload instead is what used to make `getLastAccessTime()`
    /// and `getCreationTime()` non-`null` where HotSpot answers `null`.
    ///
    /// Byte layout below is verbatim from a jar written by Temurin 25.0.3+9's
    /// `JarOutputStream` (`5554 0d00 07 …` local, `5554 0500 07 …` central).
    #[test]
    fn central_extended_timestamp_carries_modified_time_only() {
        fn extended_timestamp(payload: &[u8]) -> ZipEntryTimes {
            let mut cursor = std::io::Cursor::new(payload);
            let stamp = zip::extra_fields::ExtendedTimestamp::try_from_reader(
                &mut cursor,
                payload.len() as u16,
            )
            .expect("well-formed 0x5455 payload");
            zip_extra_field_times(&ExtraField::ExtendedTimestamp(stamp))
        }

        // Central directory: flags 0x07, but only the modified time follows.
        let mut central = vec![0x07];
        central.extend_from_slice(&1_700_000_001u32.to_le_bytes());
        let times = extended_timestamp(&central);
        assert_eq!(times.modified, Some(1_700_000_001_000));
        assert_eq!(times.access, None, "central record carries no access time");
        assert_eq!(
            times.creation, None,
            "central record carries no creation time"
        );

        // The same parser still reads all three from a full 13-byte payload,
        // which is what a central record looks like when a writer does emit
        // every time there. Nothing in our read path feeds it local bytes.
        let mut full = vec![0x07];
        for seconds in [1_700_000_001u32, 1_700_000_002, 1_700_000_003] {
            full.extend_from_slice(&seconds.to_le_bytes());
        }
        let times = extended_timestamp(&full);
        assert_eq!(times.modified, Some(1_700_000_001_000));
        assert_eq!(times.access, Some(1_700_000_002_000));
        assert_eq!(times.creation, Some(1_700_000_003_000));
    }

    /// Regression coverage for `native_jarfile_get_comment`'s data source.
    /// The zip's central-directory comment must round-trip through
    /// `ZipArchive::comment()` exactly as written — this is the extraction
    /// step `native_jarfile_get_comment` relies on before wrapping it as a
    /// Java `String`.
    #[test]
    fn zip_comment_round_trips() {
        let tmp = NamedTempFile::new().unwrap();
        let file = tmp.reopen().unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.set_comment("hello from a zip comment");
        let opts = SimpleFileOptions::default();
        zw.start_file("a.txt", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();

        let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
        assert_eq!(archive.comment(), b"hello from a zip comment");
    }

    #[test]
    fn zip_with_no_comment_has_empty_comment() {
        let tmp = make_test_jar();
        let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
        assert!(archive.comment().is_empty());
    }
}
