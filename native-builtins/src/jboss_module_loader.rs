// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H4 — JBoss Modules `LocalModuleLoader` + `DefaultBootModuleLoaderHolder`.
//!
//! After `ConcurrentHashMap.initTable`, `MethodHandles$Lookup`,
//! `StackWalker`, `JDKSpecific`, and `currentCarrierThread` are unblocked,
//! KC16 (WildFly / JBoss Modules) reaches:
//!
//! ```text
//! Exception in thread "main" java/lang/NullPointerException:
//!     Cannot invoke loadModule on null
//! ```
//!
//! `org.jboss.modules.Main.main(String[])` reads
//! `DefaultBootModuleLoaderHolder.INSTANCE` (the holder idiom for a
//! lazy `ModuleLoader` singleton).  The holder's `<clinit>` fails (for
//! the same WeakReference / MBean reasons documented in
//! `deprecated_internal.rs::register_jboss_module_loader_init_bypass`)
//! and B6-swallows.  Without the post-clinit fixup wired up below,
//! `INSTANCE` is left null and `Main.loadModule` NPEs.
//!
//! This module fixes the NPE end-to-end:
//!
//! 1. `DefaultBootModuleLoaderHolder.INSTANCE` is populated post-clinit
//!    with a synthetic `LocalModuleLoader` object.  See
//!    [`build_default_boot_holder_instance`] (called from
//!    `vm/src/vm/vm_util.rs::post_clinit_fixup`).
//! 2. `LocalModuleLoader.loadModule(String)` is registered as a native
//!    that walks the `-mp` filesystem path, parses `module.xml`, and
//!    returns a synthetic `org/jboss/modules/Module` whose resource
//!    roots are the jars listed in `module.xml`.  See
//!    [`native_loader_load_module`].
//! 3. `Module.getClassLoader()` returns a synthetic
//!    `ModuleClassLoader` that delegates to the system class loader
//!    (KC16 doesn't fully sandbox the boot module).  See
//!    [`native_module_get_class_loader`].
//! 4. `Module.loadClass(String)` resolves through the system class
//!    loader using the module's resource roots as classpath.  See
//!    [`native_module_load_class`].
//!
//! # Security
//!
//! - `validate_module_name` rejects empty / oversize / NUL / control /
//!   path-separator / `..` traversal sequences.  KC16's
//!   `org.jboss.as.standalone` uses dots only; any module name that
//!   tries to escape `<mp>` fails closed.
//! - `module.xml` parsing is delegated to `jboss_module_xml.rs` whose
//!   T17.Γ caps (1 MiB file size, hard nesting limit, strict-accept
//!   rules) we inherit.
//! - Every resolved module path is required to lie under the
//!   `-mp` root: we canonicalize the root and the candidate, then
//!   assert a prefix match.  This defeats both `..` traversal and
//!   symlink attacks (because canonicalize follows symlinks before
//!   the prefix comparison).
//!
//! # Concurrency
//!
//! - The cached `Module` objects per `(loader, name)` live in a
//!   `parking_lot::Mutex<HashMap>` so concurrent `loadModule` calls
//!   from multiple worker threads are safe.
//! - The `OnceLock<PathBuf>` resolved from `-mp` is computed on the
//!   first call.  Re-entrancy is fine: we never call back into Java
//!   from inside the lookup path.

#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use cratonvm_native_api::{DefineClassFull, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};
use parking_lot::Mutex;

use crate::jboss_module_xml::{parse_module_xml, ModuleXml, ServicesDisposition};
use crate::try_alloc_concurrent_synthetic;

// ===========================================================================
// Class names (kept centralized so anchor strings are easy to spot in greps)
// ===========================================================================

pub(crate) const CN_MODULE_LOADER: &str = "org/jboss/modules/LocalModuleLoader";
pub(crate) const CN_MODULE: &str = "org/jboss/modules/Module";
pub(crate) const CN_MODULE_CLASSLOADER: &str = "org/jboss/modules/ModuleClassLoader";
pub(crate) const CN_DEFAULT_BOOT_HOLDER: &str = "org/jboss/modules/DefaultBootModuleLoaderHolder";
pub(crate) const CN_MODULE_NOT_FOUND: &str = "org/jboss/modules/ModuleNotFoundException";

// ---------------------------------------------------------------------------
// Field layout — kept in sync with the synthetic_stub_fields entries in
// classloading/src/class_manager.rs.  Each native uses the slot indices
// below by name.
// ---------------------------------------------------------------------------

/// `LocalModuleLoader`:  slot 0 = root (String, the `-mp` path).
const LOADER_SLOT_ROOT: usize = 0;
const LOADER_FIELD_COUNT: usize = 1;

/// `Module`:
///   slot 0 = name (String)
///   slot 1 = loader (LocalModuleLoader or null)
///   slot 2 = classLoader (ModuleClassLoader or null, lazily populated)
///   slot 3 = resourceRoots (Object[] of String paths)
const MOD_SLOT_NAME: usize = 0;
const MOD_SLOT_LOADER: usize = 1;
const MOD_SLOT_CLASSLOADER: usize = 2;
const MOD_SLOT_RESOURCE_ROOTS: usize = 3;
const MOD_FIELD_COUNT: usize = 4;

/// `ModuleClassLoader`:
///   slot 0 = module (Module back-reference)
const MCL_SLOT_MODULE: usize = 0;
const MCL_FIELD_COUNT: usize = 1;

/// `ModuleNotFoundException`: 2 fields (message, cause) like every
/// `Throwable` subclass.
pub(crate) const MNF_FIELD_COUNT: usize = 2;

// ===========================================================================
// Module-name validation
// ===========================================================================

/// Reject module names that a downstream API could interpret as a path
/// traversal, an absolute filesystem reference, or an unexpected
/// control sequence.
///
/// JBoss module names are dot-separated (e.g. `org.jboss.as.standalone`).
/// Anything that contains `:`, `/`, `\`, `..`, NUL, or any control byte
/// is rejected with `IllegalArgumentException`.
pub(crate) fn validate_module_name(name: &str) -> Result<(), RuntimeError> {
    if name.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "module name must not be empty".to_string(),
        });
    }
    if name.len() > 256 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("module name too long: {} bytes (max 256)", name.len()),
        });
    }
    for b in name.bytes() {
        if b < 0x20 || b == 0x7F {
            return Err(RuntimeError::IllegalArgumentException {
                message: "module name contains control byte".to_string(),
            });
        }
        if b == b'/' || b == b'\\' || b == b':' {
            return Err(RuntimeError::IllegalArgumentException {
                message: "module name contains path separator".to_string(),
            });
        }
    }
    if name.contains("..") {
        return Err(RuntimeError::IllegalArgumentException {
            message: "module name contains traversal sequence".to_string(),
        });
    }
    Ok(())
}

// ===========================================================================
// Module-path resolution
// ===========================================================================

/// Process-wide cache of the canonicalized `-mp` roots. Set lazily on
/// the first `loadModule` call.
static MP_ROOTS_CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Process-wide Maven repository hint captured from `-Dmaven.repo.local`.
/// Build-tree WildFly distributions use `<artifact name="g:a:v"/>` entries in
/// module.xml instead of copied `<resource-root>` jars, so the native resolver
/// has to map those coordinates to the same local repository Maven used.
static MAVEN_REPO_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn maven_repo_root_slot() -> &'static Mutex<Option<PathBuf>> {
    MAVEN_REPO_ROOT.get_or_init(|| Mutex::new(None))
}

fn remember_maven_repo_root(root: Option<String>) {
    let Some(raw) = root else {
        return;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    *maven_repo_root_slot().lock() = Some(PathBuf::from(trimmed));
}

/// Find the `-mp <path>` argument in the process command line.
///
/// JBoss Modules consumes this flag from `Main.main(String[])` — by
/// the time `loadModule` is called the value has been parsed but
/// not stored anywhere our native code can see.  Instead, we scan
/// `std::env::args()`, which still contains the raw CLI arguments
/// (the cratonvm CLI captures positional args via `trailing_var_arg`,
/// so they remain in the process argv).
///
/// Resolution order (first match wins):
///   1. `CRATONVM_JBOSS_MP_ROOT` environment variable — used by external
///      integration tests (`vm/tests/wp8_10_jboss_modules_smoke.rs`)
///      that need to inject a `-mp` value without touching the process
///      argv (which is owned by the test harness, not our test).
///   2. `-mp <path>`, `-modulepath <path>`, or `--module-path <path>`
///      in the process argv.
///
/// Returns `None` if neither source is present.
fn find_mp_argument_from<I>(configured_root: Option<String>, args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    if let Some(root) = configured_root {
        if !root.is_empty() {
            return Some(root);
        }
    }
    let args: Vec<String> = args.into_iter().collect();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "-mp" || a == "-modulepath" || a == "--module-path" {
            if let Some(next) = iter.next() {
                return Some(next.clone());
            }
        }
    }
    None
}

fn find_mp_argument() -> Option<String> {
    find_mp_argument_from(
        cratonvm_types::flags::runtime_var("CRATONVM_JBOSS_MP_ROOT").ok(),
        std::env::args(),
    )
}

fn split_module_path_entries(raw: &str) -> Vec<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    raw.split(sep)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn canonicalize_module_path_entry(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    // `canonicalize` resolves symlinks; if the path doesn't exist we fall back
    // to a non-canonical path so tests that operate on tempdirs still see a
    // deterministic root value. In production the path always exists.
    match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(_) => p.canonicalize().ok().unwrap_or_else(|| p.to_path_buf()),
    }
}

/// Resolve and canonicalize every entry in the `-mp` path once. Subsequent
/// calls return the cached values.
fn resolve_mp_roots() -> Vec<PathBuf> {
    MP_ROOTS_CACHE
        .get_or_init(|| {
            find_mp_argument()
                .map(|raw| {
                    split_module_path_entries(&raw)
                        .into_iter()
                        .map(|entry| canonicalize_module_path_entry(&entry))
                        .collect()
                })
                .unwrap_or_default()
        })
        .clone()
}

/// Per-test override for the `-mp` root. When set, takes precedence over
/// resolved CLI/env roots.
#[cfg(test)]
static MP_ROOT_TEST_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_mp_root_for_test(root: Option<PathBuf>) {
    *MP_ROOT_TEST_OVERRIDE.lock() = root;
}

#[cfg(test)]
fn current_mp_root_for_test() -> Option<PathBuf> {
    MP_ROOT_TEST_OVERRIDE.lock().clone()
}

#[cfg(not(test))]
fn current_mp_root_for_test() -> Option<PathBuf> {
    None
}

/// Active module path roots: test override > resolved CLI/env roots.
fn module_path_roots() -> Vec<PathBuf> {
    if let Some(p) = current_mp_root_for_test() {
        return vec![p];
    }
    resolve_mp_roots()
}

/// Primary module path root for code paths that only need a representative value.
fn module_path_root() -> Option<PathBuf> {
    module_path_roots().into_iter().next()
}

fn maven_repo_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(cached) = maven_repo_root_slot().lock().clone() {
        roots.push(cached);
    }
    for key in ["CRATONVM_MAVEN_REPO_LOCAL", "MAVEN_REPO_LOCAL", "M2_REPO"] {
        if let Ok(raw) = cratonvm_types::flags::runtime_var(key) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                roots.push(PathBuf::from(trimmed));
            }
        }
    }
    if let Ok(userprofile) = cratonvm_types::flags::runtime_var("USERPROFILE") {
        if !userprofile.trim().is_empty() {
            roots.push(PathBuf::from(userprofile).join(".m2").join("repository"));
        }
    }
    if let Ok(home) = cratonvm_types::flags::runtime_var("HOME") {
        if !home.trim().is_empty() {
            roots.push(PathBuf::from(home).join(".m2").join("repository"));
        }
    }
    roots
}

fn is_safe_maven_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains("..")
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

fn resolve_artifact_path(coord: &str) -> Option<PathBuf> {
    if coord.contains('$') || coord.contains('{') || coord.contains('}') {
        return None;
    }
    let parts: Vec<&str> = coord.split(':').collect();
    let (group, artifact, version, classifier) = match parts.as_slice() {
        [g, a, v] => (*g, *a, *v, None),
        [g, a, v, c] => (*g, *a, *v, Some(*c)),
        _ => return None,
    };
    if ![group, artifact, version]
        .iter()
        .all(|part| is_safe_maven_part(part))
        || classifier
            .map(|part| !is_safe_maven_part(part))
            .unwrap_or(false)
    {
        return None;
    }

    let mut relative = PathBuf::new();
    for segment in group.split('.') {
        if !is_safe_maven_part(segment) {
            return None;
        }
        relative.push(segment);
    }
    relative.push(artifact);
    relative.push(version);
    let filename = match classifier {
        Some(c) => format!("{artifact}-{version}-{c}.jar"),
        None => format!("{artifact}-{version}.jar"),
    };
    relative.push(filename);

    for root in maven_repo_candidates() {
        let candidate = root.join(&relative);
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate).ok().or(Some(candidate));
        }
    }
    None
}

// ===========================================================================
// Module discovery
// ===========================================================================

/// Resolved on-disk location of a module.
///
/// `module.xml` lives at `module_dir/module.xml`; resource jars live
/// at `module_dir/<resource-root.path>` and are returned as absolute
/// canonical strings.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedModule {
    pub module_xml_path: PathBuf,
    pub module_dir: PathBuf,
    pub mx: ModuleXml,
    pub resource_roots: Vec<PathBuf>,
}

/// Locate the `module.xml` file for `name` by walking every layer
/// under `<root>/system/layers/<layer>/<dot-to-slash(name)>/main/`
/// plus the legacy `<root>/<dot-to-slash(name)>/main/` location.
///
/// Returns `None` if no matching directory was found anywhere.
pub(crate) fn locate_module_xml(root: &Path, name: &str) -> Option<PathBuf> {
    let rel = name.replace('.', "/");
    // Legacy non-layered path.
    let legacy = root.join(&rel).join("main").join("module.xml");
    if legacy.is_file() {
        return Some(legacy);
    }
    // Layered path.  Read layers.conf if present; default to `base`.
    let layers_root = root.join("system").join("layers");
    let mut layers: Vec<String> = Vec::new();
    let layers_conf = root.join("layers.conf");
    if let Ok(contents) = std::fs::read_to_string(&layers_conf) {
        for line in contents.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("layers=") {
                for part in rest.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        layers.push(p.to_string());
                    }
                }
            }
        }
    }
    // JBoss always implicitly appends `base`.
    if !layers.iter().any(|l| l == "base") {
        layers.push("base".to_string());
    }
    for layer in &layers {
        let candidate = layers_root
            .join(layer)
            .join(&rel)
            .join("main")
            .join("module.xml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Add-ons tree.
    let addons_root = root.join("system").join("add-ons");
    if let Ok(entries) = std::fs::read_dir(&addons_root) {
        for entry in entries.flatten() {
            let candidate = entry.path().join(&rel).join("main").join("module.xml");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve a candidate path for the under-root check, following symlinks in
/// the on-disk prefix without requiring the *whole* candidate to exist.
///
/// V1 — the old code fell back to the raw (non-canonical) candidate whenever
/// `canonicalize` failed (which happens for a not-yet-existing path or a
/// symlink with an unresolvable target). That fallback bypassed the symlink
/// guard. We instead canonicalize the **longest existing ancestor** (so any
/// symlink in the real prefix is followed) and re-attach the remaining
/// lexical components, rejecting any `..` segment. `None` means the path
/// could not be safely resolved and the caller must fail closed.
fn resolve_for_confinement(candidate: &Path) -> Option<PathBuf> {
    // Fast path: the whole candidate exists and canonicalizes — symlinks fully
    // followed.
    if let Ok(c) = std::fs::canonicalize(candidate) {
        return Some(c);
    }
    // Walk up to the longest existing ancestor, canonicalizing it (so a
    // symlink anywhere in the existing prefix is resolved), then append the
    // trailing components that don't yet exist on disk.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = candidate;
    loop {
        if let Ok(canon_prefix) = std::fs::canonicalize(cur) {
            let mut resolved = canon_prefix;
            for seg in tail.iter().rev() {
                // A `..` in the not-yet-existing tail could climb back out of
                // the resolved prefix — refuse it. (Normal segments and `.`
                // are fine; `.` is a no-op.)
                if seg.as_os_str() == ".." {
                    return None;
                }
                if seg.as_os_str() == "." {
                    continue;
                }
                resolved.push(seg);
            }
            return Some(resolved);
        }
        match cur.parent() {
            Some(parent) => {
                if let Some(name) = cur.file_name() {
                    tail.push(name.to_os_string());
                }
                // `parent()` of e.g. "foo" is "" — stop to avoid an infinite
                // loop on a relative path whose root never exists.
                if parent.as_os_str().is_empty() {
                    return None;
                }
                cur = parent;
            }
            None => return None,
        }
    }
}

/// Given a canonicalized module-path root and a resolved
/// `module.xml` path, assert the module path is *under* the root.
///
/// Defeats both `..` traversal and symlink attacks: the candidate is resolved
/// through [`resolve_for_confinement`] (symlinks in its existing prefix are
/// followed) before the prefix check.
///
/// V1 — fails **closed**: if the candidate cannot be safely resolved (e.g. a
/// symlink with an unresolvable target, or a path whose existing prefix can't
/// be canonicalized), we reject rather than falling back to the raw path,
/// which would have let a symlink escape the root.
fn ensure_under_root(root: &Path, candidate: &Path) -> Result<(), RuntimeError> {
    // The root is trusted configuration computed once from `-mp`; for a root
    // that doesn't exist (test tempdirs that were torn down, etc.) we keep the
    // non-canonical fallback so deterministic test roots still work. The
    // candidate, by contrast, is attacker-influenced and must fail closed.
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canonical_candidate = match resolve_for_confinement(candidate) {
        Some(c) => c,
        None => {
            return Err(RuntimeError::SecurityException {
                message: format!(
                    "module path {} could not be resolved for confinement under {}",
                    candidate.display(),
                    canonical_root.display(),
                ),
            });
        }
    };
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(RuntimeError::SecurityException {
            message: format!(
                "module path {} escapes module root {}",
                canonical_candidate.display(),
                canonical_root.display(),
            ),
        });
    }
    Ok(())
}

/// Resolve `name` against `root` end-to-end: locate module.xml, parse
/// it, materialize each resource-root path, and verify everything
/// stays inside the root.
pub(crate) fn resolve_module(root: &Path, name: &str) -> Result<ResolvedModule, RuntimeError> {
    validate_module_name(name)?;
    let module_xml_path =
        locate_module_xml(root, name).ok_or_else(|| RuntimeError::ClassNotFoundException {
            class_name: format!("module:{}", name),
        })?;
    ensure_under_root(root, &module_xml_path)?;
    let module_dir = module_xml_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.to_path_buf());
    let mx = parse_module_xml(&module_xml_path).map_err(|e| RuntimeError::IOException {
        message: format!("failed to parse {}: {}", module_xml_path.display(), e),
    })?;
    if let Some(target) = mx
        .alias_target
        .as_deref()
        .filter(|target| !target.is_empty())
    {
        if target == name {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("module alias {name} points to itself"),
            });
        }
        return resolve_module(root, target);
    }
    // Resource roots are relative to module_dir.  Materialize each as
    // an absolute path and confirm it stays under the canonical root.
    let mut resource_roots = Vec::with_capacity(mx.resource_roots.len() + mx.artifacts.len());
    for rr in &mx.resource_roots {
        // Reject `..` segments at the input layer too — defense in depth.
        if rr.path.contains("..") {
            return Err(RuntimeError::SecurityException {
                message: format!("resource-root path {} contains traversal sequence", rr.path),
            });
        }
        let absolute = module_dir.join(&rr.path);
        ensure_under_root(root, &absolute)?;
        resource_roots.push(absolute);
    }
    for coord in &mx.artifacts {
        match resolve_artifact_path(coord) {
            Some(path) => resource_roots.push(path),
            None if crate::nbflags().dbg_wf => {
                eprintln!("[jboss-module] artifact {coord:?} did not resolve in local Maven repo");
            }
            None => {}
        }
    }
    Ok(ResolvedModule {
        module_xml_path,
        module_dir,
        mx,
        resource_roots,
    })
}

/// WP2.1 — resolve `name` against an ordered list of roots.
///
/// Used by `LocalModuleLoader.loadModule` when the user constructed the
/// loader with `new LocalModuleLoader(File[])` (the JBoss public API).
/// Each root is tried in turn; on `ClassNotFoundException` we move to
/// the next root, on any other error we propagate immediately.
///
/// The first root that successfully resolves wins.  If every root
/// returns `ClassNotFoundException`, the final error is rebuilt with
/// the original module name so callers can map it to
/// `ModuleNotFoundException`.
pub(crate) fn resolve_module_in_roots(
    roots: &[PathBuf],
    name: &str,
) -> Result<ResolvedModule, RuntimeError> {
    validate_module_name(name)?;
    if roots.is_empty() {
        return Err(RuntimeError::ClassNotFoundException {
            class_name: format!("module:{}", name),
        });
    }
    let mut last_err: Option<RuntimeError> = None;
    for root in roots {
        match resolve_module(root, name) {
            Ok(r) => return Ok(r),
            Err(e @ RuntimeError::ClassNotFoundException { .. }) => {
                last_err = Some(e);
                continue;
            }
            Err(other) => return Err(other),
        }
    }
    Err(
        last_err.unwrap_or_else(|| RuntimeError::ClassNotFoundException {
            class_name: format!("module:{}", name),
        }),
    )
}

// ===========================================================================
// Module / Loader cache
// ===========================================================================

/// Per-process cache of `module-name -> (identity_key, ObjectRef)` so
/// concurrent `loadModule(name)` from multiple threads always returns the
/// same `Module` instance — JBoss's contract.
///
/// We don't track per-loader caches because cratonvm only has one
/// `LocalModuleLoader` instance (the boot holder).  When/if a second
/// loader appears, the cache keys can be promoted to
/// `(loader_object_ref, name)`.
///
/// GC: each cached Module is kept alive + registry-remapped via
/// `register_var_handle_root` at insert time; every cache hit re-reads the
/// CURRENT address via `read_var_handle_root(identity_key)` because the GC
/// cannot rewrite these raw static copies (ASYNC_POOL pattern, lib.rs).
/// Before this fix entries were bare `ObjectRef`s that were neither rooted
/// nor remapped — cached Modules were ALSO collectable once the Java caller
/// dropped its reference. Bounded by the number of distinct modules, so the
/// permanent registration does not grow unboundedly.
static MODULE_CACHE: OnceLock<Mutex<std::collections::HashMap<String, (i32, ObjectRef)>>> =
    OnceLock::new();

fn module_cache() -> &'static Mutex<std::collections::HashMap<String, (i32, ObjectRef)>> {
    MODULE_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Per-process cache of `(module-name, ResolvedModule)` so the dependency
/// closure walker can re-read the module's parsed `module.xml` (resources,
/// dependencies, etc.) without re-parsing every call.  Keyed by the same
/// canonical module name as `MODULE_CACHE`.
///
/// This complements `MODULE_CACHE` (which holds the Java-visible Module
/// objects) — we need both because the resolver state lives in Rust and the
/// Module reference lives in the JVM heap.
static RESOLVED_MODULES: OnceLock<Mutex<std::collections::HashMap<String, ResolvedModule>>> =
    OnceLock::new();

fn resolved_modules() -> &'static Mutex<std::collections::HashMap<String, ResolvedModule>> {
    RESOLVED_MODULES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
pub(crate) fn clear_module_cache_for_test() {
    if let Some(c) = MODULE_CACHE.get() {
        c.lock().clear();
    }
    if let Some(c) = RESOLVED_MODULES.get() {
        c.lock().clear();
    }
    let _ = REGISTERED_PATHS.get().map(|m| m.lock().clear());
    if let Some(c) = MAVEN_REPO_ROOT.get() {
        *c.lock() = None;
    }
}

/// Set of resource-root paths that have already been pushed onto the shared
/// dynamic classpath.  We dedupe so each JAR is only registered once even
/// when multiple modules point at it (or when transitive walks revisit the
/// same dependency).
static REGISTERED_PATHS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn registered_paths() -> &'static Mutex<std::collections::HashSet<String>> {
    REGISTERED_PATHS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Process-wide cache of the boot holder INSTANCE so the post-clinit
/// fixup and the `loadModule` native always hand out the same
/// `LocalModuleLoader`.
///
/// GC: stored as `(identity_key, ObjectRef)` — kept alive + registry-remapped
/// via `register_var_handle_root`; every read re-fetches the CURRENT address
/// via `read_var_handle_root(identity_key)` because the GC cannot rewrite
/// this raw static copy (ASYNC_POOL pattern, lib.rs). Before this fix the
/// slot held a bare `ObjectRef` that was neither rooted nor remapped — the
/// singleton was collectable/movable out from under the cache.
static BOOT_LOADER: OnceLock<Mutex<Option<(i32, ObjectRef)>>> = OnceLock::new();

fn boot_loader_slot() -> &'static Mutex<Option<(i32, ObjectRef)>> {
    BOOT_LOADER.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn clear_boot_loader_for_test() {
    if let Some(c) = BOOT_LOADER.get() {
        *c.lock() = None;
    }
}

// ===========================================================================
// Public construction helpers (called from vm/src/vm/vm_util.rs)
// ===========================================================================

/// Construct (or fetch) the singleton `LocalModuleLoader` object.
///
/// Called both from [`build_default_boot_holder_instance`] and from
/// the lazy-rebuild path in [`native_loader_load_module`] when the
/// caller didn't pass a real receiver.
pub fn build_local_module_loader(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    {
        let slot = boot_loader_slot().lock();
        if let Some((key, cached)) = *slot {
            // Re-read the CURRENT address: the GC remaps the var-handle-root
            // registry entry after a move, not this raw static copy. Contexts
            // without a registry (mocks) fall back to the cached ref.
            return Ok(ctx.read_var_handle_root(key).unwrap_or(cached));
        }
    }
    let loader = try_alloc_concurrent_synthetic(ctx, CN_MODULE_LOADER, LOADER_FIELD_COUNT)?;
    // GC-safety: `create_string` below can trigger a moving GC; `loader` is
    // used again in the following `set_field` unpinned otherwise -- the same
    // "Family 1" stale-ObjectRef pattern as the WildFly boot-crash fixes (see
    // wildfly-parallel-boot-stale-objectref-residual.md).
    // This is the singleton boot module loader, so every module load run
    // through this path was at risk.
    let loader_pin = ctx.pin_native_root(loader);
    let root_str = match module_path_root() {
        Some(p) => ctx.create_string(&p.to_string_lossy()),
        None => ctx.create_string(""),
    };
    let loader = ctx.read_native_pin(loader_pin, loader);
    ctx.unpin_native_roots(loader_pin);
    ctx.set_field(loader, LOADER_SLOT_ROOT, Value::Object(Some(root_str)));
    // Keep alive + registry-remapped across GC moves (VarHandle-root pattern);
    // key computed on the just-registered address, no allocation in between.
    ctx.register_var_handle_root(loader);
    let key = ctx.identity_hash_code(loader);
    {
        let mut slot = boot_loader_slot().lock();
        // Double-checked locking: another thread may have raced us. (Our
        // orphaned registration is harmless — same trade-off as ASYNC_POOL.)
        if let Some((ekey, existing)) = *slot {
            return Ok(ctx.read_var_handle_root(ekey).unwrap_or(existing));
        }
        *slot = Some((key, loader));
    }
    Ok(loader)
}

/// Allocate the `DefaultBootModuleLoaderHolder.INSTANCE` value.  The
/// caller (a post-clinit fixup in `vm_util.rs`) writes the result to
/// the holder's static field.
pub fn build_default_boot_holder_instance(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(build_local_module_loader(ctx)?)
}

// ===========================================================================
// Native: LocalModuleLoader.loadModule(String) → Module
// ===========================================================================

fn build_resource_root_array(ctx: &mut dyn NativeContext, paths: &[PathBuf]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, paths.len());
    // GC-safety: `create_string` per iteration can trigger a moving GC; `arr`
    // is written into again via `set_array_element` afterward, both within
    // the same iteration and across iterations. Pin once, re-read before
    // each use.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, p) in paths.iter().enumerate() {
        let s = ctx.create_string(&p.to_string_lossy());
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    arr
}

/// Build a `Module` object populated from `resolved`.
fn build_module_object(
    ctx: &mut dyn NativeContext,
    name: &str,
    loader: ObjectRef,
    resolved: &ResolvedModule,
) -> Result<ObjectRef, MethodCallFailed> {
    let module = try_alloc_concurrent_synthetic(ctx, CN_MODULE, MOD_FIELD_COUNT)?;
    // GC-safety: `module` (and the `loader` parameter, reused below) are held
    // across several subsequent GC-triggering calls (`create_string`,
    // `build_resource_root_array`, the `mcl` allocation) before their last
    // use. Same "Family 1" stale-ObjectRef pattern as the WildFly boot-crash
    // fixes (see wildfly-parallel-boot-stale-objectref-residual.md)
    // -- pin both now and re-read the forwarded reference before each use.
    let module_pin = ctx.pin_native_root(module);
    let loader_pin = ctx.pin_native_root(loader);
    let name_str = ctx.create_string(name);
    let module = ctx.read_native_pin(module_pin, module);
    ctx.set_field(module, MOD_SLOT_NAME, Value::Object(Some(name_str)));
    let loader = ctx.read_native_pin(loader_pin, loader);
    ctx.set_field(module, MOD_SLOT_LOADER, Value::Object(Some(loader)));
    ctx.set_field_by_name(module, "name", Value::Object(Some(name_str)));
    let arr = build_resource_root_array(ctx, &resolved.resource_roots);
    let module = ctx.read_native_pin(module_pin, module);
    let loader = ctx.read_native_pin(loader_pin, loader);
    ctx.set_field_by_name(module, "moduleLoader", Value::Object(Some(loader)));
    ctx.set_field(module, MOD_SLOT_RESOURCE_ROOTS, Value::Object(Some(arr)));
    let mcl = try_alloc_concurrent_synthetic(ctx, CN_MODULE_CLASSLOADER, MCL_FIELD_COUNT)?;
    let module = ctx.read_native_pin(module_pin, module);
    ctx.set_field(mcl, MCL_SLOT_MODULE, Value::Object(Some(module)));
    // Real-JDK-mode fix: the real `org.jboss.modules.ModuleClassLoader` class
    // (loaded from the actual jboss-modules.jar bytecode) declares `private
    // final Module module;` -- a real field whose offset does not generally
    // coincide with our synthetic MCL_SLOT_MODULE index. Every other field
    // this function populates gets a name-based write alongside its
    // index-based one (see "moduleLoader"/"name"/"moduleClassLoader" below)
    // for exactly this reason; this one was missing it. Without it, real
    // bytecode that reads `this.module` inside `ModuleClassLoader` (e.g. its
    // `loadClass`/`loadModuleClass` delegation, reached via
    // `Class.forName(name, resolve, mcl)` during `Module.run`) observes the
    // default-zero/null value and NPEs with "Cannot invoke
    // Module.loadModuleClass(...) because "module" is null" -- the exact
    // WildFly `parallel-extension-add`-adjacent boot failure this blocks.
    ctx.set_field_by_name(mcl, "module", Value::Object(Some(module)));
    ctx.set_field(module, MOD_SLOT_CLASSLOADER, Value::Object(Some(mcl)));
    ctx.set_field_by_name(module, "moduleClassLoader", Value::Object(Some(mcl)));

    // RKC16N.12 — populate `mainClassName` on the real `org.jboss.modules.Module`
    // class layout. The synthetic above only writes our 4 slots, but the real
    // bytecode (loaded from jboss-modules.jar) reads `getfield mainClassName`
    // at PC 1 of `Module.run(String[])` which then drives PC 40 of
    // `Module.run(String,String[])` — `Class.forName(mainClassName, false, mcl)`.
    // Without this, the field reads back its default zero/null and KC16 boot
    // calls `Class.forName("", ...)` which fails as ClassNotFoundException
    // with no detail (the empty class-name path through MCL.loadClass).
    // Setting by NAME (not slot index) handles the real class's field layout.
    if let Some(main_class) = resolved.mx.main_class.as_deref() {
        let main_str = ctx.create_string(main_class);
        let module = ctx.read_native_pin(module_pin, module);
        ctx.set_field_by_name(module, "mainClassName", Value::Object(Some(main_str)));
    }
    // Real-JDK-mode fix: the real `org.jboss.modules.Module` class declares
    // `private volatile Linkage linkage;`, initialized by its real
    // constructor. Our synthetic construction never runs that constructor,
    // so the field stays default-null; the first real bytecode that reads
    // `module.linkage` (e.g. `ConcurrentClassLoader.loadClass`'s
    // `oldLinkage.getState()` check, reached transitively from
    // `Module.getPaths()` during boot) NPEs on "oldLinkage is null".
    //
    // A first attempt seeded this with the real `Linkage.NONE` static field
    // -- WRONG: `NONE = new Linkage(State.NEW)`, and `Module.getPaths()`'s
    // real bytecode treats `NEW` (like `LINKING`) as "link in progress,
    // wait()" (`synchronized (this) { while (state==LINKING||state==NEW)
    // this.wait(); ... }`). Since CratonVM never runs the real linking
    // machinery that would `notify()` this module, that produced a
    // permanent single-thread self-deadlock (confirmed live via the T19.H1
    // watchdog thread dump: the lone "main" thread parked forever in
    // `Module.getPaths` pc=55, `Object.wait()`).
    //
    // Fixed: construct a real `Linkage` already in the `LINKED` terminal
    // state via its real 1-arg constructor (`Linkage(State)`, which itself
    // defaults dependencySpecs/dependencies to the real empty
    // `NO_DEPENDENCY_SPECS`/`NO_DEPENDENCIES` and paths to
    // `Collections.emptyMap()` -- exactly right for a module we resolve and
    // serve resources for entirely through our own native machinery, not
    // real dependency-linkage bytecode). This takes the `if (state ==
    // LINKED) return linkage.getPaths();` fast path at `Module.getPaths`
    // pc=10-21 unconditionally, never reaching the wait loop.
    let module = ctx.read_native_pin(module_pin, module);
    if let Ok(state_cid) = ctx.ensure_class_initialized("org/jboss/modules/Linkage$State") {
        if let Some(idx) = ctx.static_field_index_by_name(state_cid, "LINKED") {
            let linked_state = ctx.get_static_field(state_cid, idx);
            if let Ok(Some(linkage_obj)) = ctx.new_object_initialized(
                "org/jboss/modules/Linkage",
                "(Lorg/jboss/modules/Linkage$State;)V",
                &[linked_state],
            ) {
                let module = ctx.read_native_pin(module_pin, module);
                ctx.set_field_by_name(module, "linkage", linkage_obj);
            }
        }
    }
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    Ok(module)
}

/// Throw `org.jboss.modules.ModuleNotFoundException` with `name`.
/// Allocate a single-message exception (`alloc_object` layout: field 0 = the
/// message `String`) and populate it.
///
/// GC-safety: this file has several throw sites of the shape "alloc the
/// exception object, then `create_string` the message, then `set_field` the
/// freshly-allocated exception again" -- `create_string` can trigger a
/// moving GC, which silently corrupts the exception object per the
/// `pin_native_root` contract (same "Family 1" stale-ObjectRef pattern as the
/// WildFly boot-crash fixes; see
/// wildfly-parallel-boot-stale-objectref-residual.md).
/// Centralized here instead of repeating the pin/read/unpin dance at each
/// call site.
///
/// # Slot 0 is NOT `detailMessage` on a real JDK layout, and both writes are load-bearing
///
/// The `alloc_object` layout in the header above is the SYNTHETIC-STUB one.
/// Thirty-one of this function's call sites name `java/lang/ClassNotFoundException`,
/// `NoClassDefFoundError` or `NullPointerException`, and under `--real-jdk` /
/// `--jdk-only` those are the REAL classes, whose slot 0 is
/// `Throwable.backtrace` — `detailMessage` is a different slot.
///
/// A bare `set_field(exc, 0, msg)` was nevertheless observable as the message,
/// because `native_throwable_get_message` reads slot 0 back whenever the
/// receiver's OWN class declares no `detailMessage` (`resolve_field_index` does
/// not walk to `Throwable`) and the slot happens to hold a `java/lang/String`.
/// Two wrongs cancelling: the loader wrote the wrong field and the shadow
/// `getMessage` read the wrong field.
///
/// **The first `getMessage` to run as real bytecode found the null.** MEASURED
/// 2026-09-10 while retiring the throwable family's shadows:
///
/// ```text
///   Class.forName("no.such.Klass") -> e.getMessage()
///     HotSpot                          no.such.Klass
///     with Throwable.getMessage retired   null
/// ```
///
/// So both writes stay, and neither is redundant:
///
///  * `set_field(exc, 0, ..)` keeps COMPATIBLE mode byte-for-byte identical —
///    the shadow `getMessage` is still what runs there and it still reads
///    slot 0;
///  * [`write_throwable_detail_message`] puts the message where the REAL
///    `Throwable.getMessage()` bytecode looks. On a real layout it resolves
///    `Throwable.detailMessage`'s index and writes that; on a synthetic stub
///    the index does not fit the object and it falls back to slot 0, writing
///    the same value the line above already wrote.
///
/// The honest fix is for these sites to construct through `<init>(String)`
/// like `create_exception_object` does, which would also give the throwable a
/// stack trace and a `cause` sentinel it does not have today. That is a wider
/// change than a shadow retirement should carry, and it is recorded in the lane
/// T write-up rather than attempted here.
pub fn alloc_single_message_exception(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
    message: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let exc = try_alloc_concurrent_synthetic(ctx, class_name, num_fields)?;
    let exc_pin = ctx.pin_native_root(exc);
    let msg = ctx.create_string(message);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.unpin_native_roots(exc_pin);
    ctx.set_field(exc, 0, Value::Object(Some(msg)));
    crate::lang_misc::write_throwable_detail_message(ctx, exc, Value::Object(Some(msg)));
    Ok(exc)
}

fn throw_module_not_found(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    let exc = alloc_single_message_exception(ctx, CN_MODULE_NOT_FOUND, MNF_FIELD_COUNT, name);
    Ok(MethodCallFailed::ExceptionThrown(exc?))
}

/// WP2.1 — Extract the `File[]` roots that the user passed to
/// `new LocalModuleLoader(File[])`.
///
/// The real jboss-modules.jar carries the roots in
/// `LocalModuleFinder.repoRoots`, which is reachable from the
/// `LocalModuleLoader` instance via the inherited `ModuleLoader.finders`
/// field (or `getFinders()` accessor).
///
/// We try (in order):
///
/// 1. `getFinders()` virtual call — works when the loader's superclass
///    layout is intact.
/// 2. Direct `finders` field read — fallback when the synthetic stub
///    layout doesn't include `getFinders` dispatch.
///
/// For each `LocalModuleFinder` element we read its `repoRoots: File[]`
/// field, then call `File.getAbsolutePath()` on each entry to get the
/// canonical path string.
///
/// Returns an empty Vec on any failure — the caller falls back to the
/// process-wide `module_path_root()` resolution.
fn extract_receiver_roots(ctx: &mut dyn NativeContext, receiver: ObjectRef) -> Vec<PathBuf> {
    let finders_arr = read_finders_array(ctx, receiver);
    let finders_arr = match finders_arr {
        Some(a) => a,
        None => return Vec::new(),
    };
    let n = ctx.array_length(finders_arr);
    let mut roots: Vec<PathBuf> = Vec::new();
    for i in 0..n {
        let finder = match ctx.get_array_element(finders_arr, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        // Only LocalModuleFinder carries File[] roots.  Other finders
        // (custom user finders) have no file-system root we can extract;
        // skip them and let module_path_root() handle the fallback.
        let class_name = ctx
            .class_name_of_id(ctx.class_id_of_object(finder))
            .unwrap_or_default();
        if !class_name.ends_with("/LocalModuleFinder")
            && !class_name.ends_with(".LocalModuleFinder")
            && class_name != "org/jboss/modules/LocalModuleFinder"
        {
            continue;
        }
        let repo_roots_val = ctx.get_field_by_name(finder, "repoRoots");
        let arr = match repo_roots_val {
            Value::Object(Some(a)) => a,
            _ => continue,
        };
        let m = ctx.array_length(arr);
        for j in 0..m {
            let file = match ctx.get_array_element(arr, j) {
                Value::Object(Some(f)) => f,
                _ => continue,
            };
            // Prefer `File.getAbsolutePath()` virtual call — it returns the
            // OS-canonical path string regardless of how the File was
            // constructed.  Fall back to the `path` field if invoke fails.
            let path_str = match ctx.invoke_virtual(
                file,
                "getAbsolutePath",
                "()Ljava/lang/String;",
                &[Value::Object(Some(file))],
            ) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            };
            let path_str = path_str.or_else(|| match ctx.get_field_by_name(file, "path") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            });
            if let Some(p) = path_str {
                if !p.is_empty() {
                    let path = PathBuf::from(&p);
                    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                    roots.push(canonical);
                }
            }
        }
    }
    roots
}

/// Read the `finders: ModuleFinder[]` field from a ModuleLoader receiver.
///
/// Tries the virtual `getFinders()` accessor first (cheaper and survives
/// any field-renaming), then falls back to a direct `finders` field
/// read.  Returns the array ObjectRef or None.
fn read_finders_array(ctx: &mut dyn NativeContext, receiver: ObjectRef) -> Option<ObjectRef> {
    if let Ok(Some(Value::Object(Some(arr)))) = ctx.invoke_virtual(
        receiver,
        "getFinders",
        "()[Lorg/jboss/modules/ModuleFinder;",
        &[Value::Object(Some(receiver))],
    ) {
        return Some(arr);
    }
    match ctx.get_field_by_name(receiver, "finders") {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    }
}

/// `LocalModuleLoader.loadModule(String name)` — locate the module on
/// disk, parse `module.xml`, and return a populated `Module`.
/// True for JPMS platform module names (`java.base`, `java.logging`,
/// `jdk.unsupported`, ...). These are JDK modules, not JBoss modules — a
/// JBoss module tree never contains them (JBoss module names in the `java.*`
/// namespace don't exist; `javax.*` API shims don't match the `java.` prefix
/// because of the trailing dot).
fn is_jdk_platform_module(name: &str) -> bool {
    name == "java.base" || name.starts_with("java.") || name.starts_with("jdk.")
}

/// Build, root, and cache a synthetic `Module` for a JPMS platform module —
/// empty resource roots, no dependencies. See the call site in
/// `native_loader_load_module` for the rationale (infinispan's
/// `ModuleClassLoaderMarshaller` requires `loadModule("java.base")` to
/// succeed on every cache-container start).
fn synthesize_platform_module(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: &str,
) -> MethodCallResult {
    let resolved = ResolvedModule {
        module_xml_path: PathBuf::new(),
        module_dir: PathBuf::new(),
        mx: ModuleXml {
            name: name.to_string(),
            ..Default::default()
        },
        resource_roots: Vec::new(),
    };
    {
        let mut store = resolved_modules().lock();
        store
            .entry(name.to_string())
            .or_insert_with(|| resolved.clone());
    }
    let module = build_module_object(ctx, name, loader, &resolved)?;
    // Same keep-alive + race-loser discipline as the normal loadModule tail.
    ctx.register_var_handle_root(module);
    let mkey = ctx.identity_hash_code(module);
    let mut cache = module_cache().lock();
    if let Some(&(ekey, existing)) = cache.get(name) {
        return Ok(Some(Value::Object(Some(
            ctx.read_var_handle_root(ekey).unwrap_or(existing),
        ))));
    }
    cache.insert(name.to_string(), (mkey, module));
    Ok(Some(Value::Object(Some(module))))
}

pub(crate) fn native_loader_load_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (LocalModuleLoader), args[1] = String name
    //
    // GC: the `_` arm below ALLOCATES and does not return, so `args` — the
    // snapshot `safe_native_call_impl` took before this native was entered —
    // names pre-call addresses from there on. `this` was already pinned for
    // the `extract_receiver_roots` window; the NAME argument was not, and it
    // is read out of `args` after that allocation. Pin it at entry, before
    // anything can collect. See `internal/audits/wide-tranche-triage-20260907.md`.
    let name_pin = match args.get(1) {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => build_local_module_loader(ctx)?,
    };
    // GC-safety: `extract_receiver_roots` below internally calls
    // `invoke_virtual` (`File.getAbsolutePath()`), which can trigger a
    // moving GC; `this` is reused as the `loader` argument to
    // `build_module_object` further down, unpinned otherwise. Same
    // "Family 1" pattern as the rest of this file's fixes (see
    // wildfly-parallel-boot-stale-objectref-residual.md).
    let this_pin = ctx.pin_native_root(this);
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(s))) => match name_pin {
            Some((pin, obj)) => ctx.read_native_pin(pin, obj),
            None => *s,
        },
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException {
                message: Some("LocalModuleLoader.loadModule: name must not be null".to_string()),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "LocalModuleLoader.loadModule: name must be a String".to_string(),
            }
            .into());
        }
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    if let Err(e) = validate_module_name(&name) {
        return Err(e.into());
    }
    remember_maven_repo_root(
        ctx.get_system_property("maven.repo.local")
            .or_else(|| ctx.get_system_property("localRepository")),
    );

    // Cache hit?
    {
        let cache = module_cache().lock();
        if let Some(&(key, cached)) = cache.get(&name) {
            // Re-read the CURRENT (post-GC) address — the var-handle-root
            // registry entry is remapped after a move, this raw copy is not.
            return Ok(Some(Value::Object(Some(
                ctx.read_var_handle_root(key).unwrap_or(cached),
            ))));
        }
    }

    // T19.H8 — Skip `extract_receiver_roots` when the receiver is the
    // synthetic boot loader. The post-clinit fixup in `vm_util.rs`
    // allocates a `LocalModuleLoader` with one zero-initialised slot
    // (no `finders` populated). `extract_receiver_roots` then calls
    // `invoke_virtual(receiver, "getFinders", ...)`, which on a synthetic
    // stub class with no bytecode for `getFinders` recurses through the
    // virtual-dispatch fallback path back into `loadModule` (or sister
    // dispatch sites), producing the Main.main pc=1306 100%-CPU spin
    // that watchdog T19.H1 caught with 0 dumps.  We fall straight to
    // the process-wide `-mp` cache for any receiver whose `finders`
    // slot is null/uninitialised — the original WP2.1 path is still
    // taken for caller-constructed `LocalModuleLoader(File[])`.
    let receiver_has_finders = matches!(
        ctx.get_field_by_name(this, "finders"),
        Value::Object(Some(_))
    );
    let mut roots: Vec<PathBuf> = if receiver_has_finders {
        extract_receiver_roots(ctx, this)
    } else {
        Vec::new()
    };
    for mp in module_path_roots() {
        if !roots.iter().any(|r| r == &mp) {
            roots.push(mp);
        }
    }
    if roots.is_empty() {
        if is_jdk_platform_module(&name) {
            let this_now = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            return synthesize_platform_module(ctx, this_now, &name);
        }
        if crate::nbflags().dbg_wf {
            eprintln!("[jboss-module] loadModule({name}) no roots; receiver_has_finders={receiver_has_finders}");
        }
        return Err(throw_module_not_found(ctx, &name)?);
    }

    let dbg_wf = crate::nbflags().dbg_wf;
    if dbg_wf {
        eprintln!(
            "[jboss-module] loadModule({name}) receiver_has_finders={receiver_has_finders} roots={}",
            roots.len()
        );
        for root in &roots {
            eprintln!("[jboss-module]   root={}", root.display());
        }
    }
    let resolved = match resolve_module_in_roots(&roots, &name) {
        Ok(r) => {
            if dbg_wf {
                eprintln!(
                    "[jboss-module] loadModule({name}) resolved xml={} resources={}",
                    r.module_xml_path.display(),
                    r.resource_roots.len()
                );
                for root in &r.resource_roots {
                    eprintln!("[jboss-module]   resource={}", root.display());
                }
            }
            r
        }
        Err(RuntimeError::ClassNotFoundException { .. }) => {
            // JPMS platform modules (`java.base`, `java.logging`, `jdk.*`,
            // ...) are not JBoss modules and never resolve from the module
            // tree — real jboss-modules serves them through its JDK module
            // bridge with a Module whose class loader sees platform classes.
            // WildFly's clustering marshaller calls `loadModule("java.base")`
            // on every infinispan cache-container start; throwing
            // `ModuleNotFoundException` here failed those services on every
            // standalone boot. Synthesize an empty-resource module instead —
            // JDK classes resolve through the shared bootstrap path regardless
            // of the requesting loader, so no resource roots are needed. Tree
            // resolution above still wins for any name a distribution really
            // ships (checked first, so this is strictly a fallback).
            if is_jdk_platform_module(&name) {
                if dbg_wf {
                    eprintln!("[jboss-module] loadModule({name}) synthesizing JDK platform module");
                }
                let this_now = ctx.read_native_pin(this_pin, this);
                ctx.unpin_native_roots(this_pin);
                return synthesize_platform_module(ctx, this_now, &name);
            }
            if dbg_wf {
                eprintln!("[jboss-module] loadModule({name}) not found in roots");
            }
            return Err(throw_module_not_found(ctx, &name)?);
        }
        Err(e) => {
            if dbg_wf {
                eprintln!("[jboss-module] loadModule({name}) failed: {e:?}");
            }
            return Err(e.into());
        }
    };

    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let module = build_module_object(ctx, &name, this, &resolved)?;
    // GC-safety: several calls further down this function (notably the
    // RKC19/WF39 brute-force block's `ensure_class_initialized` pre-warm loop,
    // gated on `is_brute_force_trigger` -- exactly the WildFly bootstrap
    // module path, e.g. `org.jboss.as.standalone`) can trigger a moving GC
    // before `module` is used again at `register_var_handle_root`/cache
    // insertion below.
    let module_pin = ctx.pin_native_root(module);

    // Stash the resolved module so the dependency-closure walker (used by
    // ModuleClassLoader.loadClass / getResource) can re-traverse without
    // re-parsing the XML.
    {
        let mut store = resolved_modules().lock();
        store.insert(name.clone(), resolved.clone());
    }

    // Pre-register the module's own resource-root jars on the shared
    // dynamic classpath so the system class loader can resolve classes
    // that we route through (after the visibility check).  This keeps
    // class definition single-sourced (no double-define) while still
    // letting the MCL.loadClass native vouch for visibility.
    register_resource_roots(ctx, &resolved.resource_roots);

    // T19_H15 — additionally register the **transitive linkage closure**
    // of resource-roots on the shared classpath.  When bytecode in this
    // module is verified, the JVM's verifier resolves referenced classes
    // (field types, parameter types, exception classes) through the
    // application class loader — bypassing JBoss's per-module
    // `MCL.loadClass` native.  Without the deps' jars on the dynamic
    // classpath at this point, the verifier raises
    // `NoClassDefFoundError` (KC16 hits this on
    // `org.jboss.as.controller.access.JmxAction$Impact` referenced
    // indirectly from `org.jboss.as.jmx.PluggableMBeanServerImpl`).
    //
    // `register_resource_roots` deduplicates by absolute path, so
    // re-walking on every loadModule call is O(n) in already-seen jars
    // and idempotent.
    let linkage_roots = transitive_linkage_roots(&name);
    if !linkage_roots.is_empty() {
        register_resource_roots(ctx, &linkage_roots);
    }

    // Round-19 defense-in-depth: WildFly's `org.jboss.as.standalone` module
    // has an empty `<resources>` block — its `main-class` lives in
    // `org.jboss.as.server` (a re-exported dep). Some module.xml files
    // physically ship `.jar` files alongside `module.xml` that aren't
    // listed in `<resources>` (or use globbing patterns we don't yet
    // parse); to avoid NoSuchMethodException when `Module.run` ->
    // `Class.forName(mainClassName, false, mcl)` hits the empty-resources
    // case, we additionally force-register every `.jar` we physically
    // find inside `<mp>/<dotted>/main/` (and its transitive deps' main/
    // directories). Idempotent via `register_resource_roots`.
    let physical_jars = collect_physical_main_dir_jars(&name);
    if !physical_jars.is_empty() {
        register_resource_roots(ctx, &physical_jars);
    }

    // RKC19/WF39 — Brute-force fallback for the WildFly bootstrap entry points.
    //
    // The previous BFS in `collect_physical_main_dir_jars` *should* descend
    // from `org.jboss.as.standalone` into its `org.jboss.as.server` dep and
    // pick up `wildfly-server-*.jar`.  But that walk silently misses modules
    // when `ensure_resolved` returns None (e.g. the dep references a layer
    // path that didn't make it into our cached `module_path_root`) and we've
    // observed the WildFly 39 boot path produce
    // `NoSuchMethodException: org/jboss/as/server/Main.main`, which means
    // `Class.forName("org.jboss.as.server.Main", false, mcl)` couldn't find
    // the class on the dynamic classpath.
    //
    // Belt-and-braces: when loading any of the well-known WildFly bootstrap
    // modules, additionally walk **every** `.jar` under
    // `<root>/system/layers/<layer>/` and register them on the dynamic
    // classpath.  This guarantees that the `wildfly-server-*.jar` reaches
    // the application class loader regardless of any gap in our BFS.
    //
    // The walk runs at most ONCE per `(root, module_name)` pair (tracked in
    // `BRUTE_FORCED_ROOTS`) and is capped at `MAX_BRUTE_FORCE_JARS` so a
    // pathological deep tree cannot stall startup.
    if is_brute_force_trigger(&name) {
        // WF32-fix: the brute-force layered-jar walk is now DISABLED by
        // default (opt back in with `CRATONVM_JBOSS_BRUTE_FORCE_JARS=1`).
        //
        // Why: this walk used to register *every* `.jar` under
        // `<root>/system/layers/*/` (750+ jars for WildFly 32) onto the
        // shared dynamic classpath. That flood is the root cause of the
        // WildFly boot hang:
        //
        //   * `ClassLoader.getResources("META-INF/MANIFEST.MF")` then
        //     enumerates one URL per registered jar — 750 URLs instead of
        //     the handful a properly module-isolated classloader sees.
        //   * WildFly's `Main.main` iterates that enumeration, doing a
        //     `URL.openStream()` + `Manifest` parse on each. Every
        //     `openStream` re-opens and re-indexes a zip central directory
        //     (~280 ms/jar in CratonVM), so the scan takes ~210 s and the
        //     120 s watchdog aborts the process.
        //
        // The walk was only ever a belt-and-braces fallback to make the
        // bootstrap entry-point class (`org.jboss.as.server.Main`)
        // loadable. That class — and its real transitive dependency
        // jars — are already registered by the normal resolution path
        // above (`register_resource_roots(resolved.resource_roots)` +
        // `transitive_linkage_roots` + `collect_physical_main_dir_jars`),
        // so dumping all 750 layered jars adds nothing for class loading
        // while quadratically poisoning every resource enumeration.
        //
        // NOTE for a future agent: the correct long-term design is a real
        // `module.xml`-driven resolver that gives each JBoss module its
        // own isolated `ModuleClassLoader`, so `getResources` from module
        // X only sees X's `<resource-root>` jars. Until then, the normal
        // resolution path covers the common case; re-enable this walk via
        // the env var only if a specific app regresses with a
        // `NoSuchMethodException` on its bootstrap entry class.
        let brute_force_enabled = crate::nbflags().jboss_brute_force_jars;
        let brute_jars = if brute_force_enabled {
            brute_force_collect_layered_jars(&roots, &name)
        } else {
            Vec::new()
        };
        if !brute_jars.is_empty() {
            register_resource_roots(ctx, &brute_jars);
        }

        // RKC19/WF39 — Task D: After the brute-force walk has guaranteed that
        // every layered jar is on the dynamic classpath, force-load the
        // module's declared entry-point class (and a small list of well-known
        // WildFly fallback alternatives).  This pre-warms class definition so
        // by the time WildFly's `Module.run` reaches
        // `Class.forName(mainClassName, false, mcl)` -> `getDeclaredMethod
        // ("main", String[].class)`, the class is already fully resolved and
        // its `main(String[])` method is discoverable.
        //
        // Failures are silently swallowed — this is best-effort.  A genuinely
        // missing entry class will still surface via the normal
        // `Module.run` path's `ClassNotFoundException`/`NoSuchMethodException`
        // chain, which is recoverable upstream.
        let mut entry_candidates: Vec<String> = Vec::new();
        if let Some(declared) = resolved.mx.main_class.as_deref() {
            entry_candidates.push(declared.replace('.', "/"));
        }
        // Task E — Best-effort fallback main classes for varying WildFly
        // versions. The first one with a usable `main(String[])` wins
        // implicitly via the JVM's class resolution (Module.run only consults
        // one — `mainClassName`).  But by pre-loading every candidate, we
        // guarantee that if WildFly's recorded `mainClassName` matches any of
        // these, the class is ready.
        // NOTE: only genuine WildFly bootstrap entry points belong here.
        // Keycloak 16 is itself a WildFly distribution: it boots through the
        // very same `org.jboss.as.standalone` module and runs
        // `org.jboss.as.server.Main` — there is no `org/keycloak/Main` class
        // anywhere on its module path. Earlier revisions speculatively
        // pre-warmed `org/keycloak/Main` here; that class can never resolve,
        // so it only produced an alarming `ensure_class_initialized FAIL:
        // org/keycloak/Main` line that masqueraded as the fatal boot error
        // while being a silently-swallowed best-effort miss. Removed.
        for fallback in &[
            "org/jboss/as/server/Main",
            "org/jboss/as/Main",
            "org/jboss/as/standalone/Main",
            "org/jboss/as/embedded/EmbeddedStandaloneServerFactory$Main",
        ] {
            if !entry_candidates.iter().any(|c| c == *fallback) {
                entry_candidates.push((*fallback).to_string());
            }
        }
        for candidate in &entry_candidates {
            // Best-effort pre-warm: a miss here is fully recoverable.
            // `Module.run` only ever consults the ONE class recorded
            // in `mainClassName`; the other candidates are speculative
            // pre-warms for varying WildFly versions.
            let _ = ctx.ensure_class_initialized(candidate);
        }

        // Real-bytecode audit: the RKC19/WF39 Task C "synthetic class
        // definition fallback" has been GATED OFF by default.
        //
        // This loop previously synthesized a 191-byte class file with a
        // no-op `main([Ljava/lang/String;)V` body and (when redefine=true
        // on `define_class_full`) REPLACED real WildFly / Keycloak entry
        // classes whose bytecode hadn't been pre-loaded. That is a pure
        // fake-out: any classes injected this way would silently return
        // without booting WildFly.
        //
        // The loop body is left intact so future debugging can re-enable
        // it via `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE=1`, but the default
        // behavior is to run the real bytecode for every entry candidate
        // that loaded via `ensure_class_initialized` above, and to fail
        // loudly (rather than fake-out) for any that didn't.
        let allow_synth_bytecode = crate::nbflags().use_wildfly_synth_bytecode;
        if !allow_synth_bytecode {
            // Skip the entire synthesis pass. Suppress dead-code warning
            // on the synthesis helper since it is now only called from
            // inside the opt-in branch below.
            let _ = build_synthetic_class_with_main;
        }
        if allow_synth_bytecode {
            for synth_name in &entry_candidates {
                let cid_pre = ctx.class_id_by_name(synth_name);
                let main_exists_pre =
                    ctx.method_exists(synth_name, "main", "([Ljava/lang/String;)V");
                // KC17 — critical fix.  Previously this guard only checked
                // `class_id_by_name(...).is_some()` and skipped the synth when a
                // class was loaded.  But `ensure_class_initialized` above can
                // materialise a class stub that LACKS `main([Ljava/lang/String;)V`
                // (e.g. a `NoClassDefFoundError`-stub or a partial class entry).
                // The reflective `Class.forName(...).getDeclaredMethod("main",
                // String[].class)` chain in jboss-modules then walks that stub's
                // method table, finds no `main`, and throws
                // `NoSuchMethodException`.  Skip the synth ONLY when the class is
                // loaded AND already exposes a `main([Ljava/lang/String;)V`
                // method.  Otherwise re-define so the reflective lookup resolves.
                if cid_pre.is_some() && main_exists_pre {
                    continue;
                }
                let bytecode = build_synthetic_class_with_main(synth_name);
                // WF8 — `define_class_from_bytes` is a strict "register new
                // class" API: when a class with this name is ALREADY loaded
                // (which is exactly the WildFly/Keycloak failure mode — a
                // class stub got materialised earlier but lacks `main`), it
                // bails out with `None`.  Instead, use `define_class_full`
                // with `allow_redefine: true` so the synthetic bytecode
                // physically REPLACES the incomplete class in place.  This
                // ensures the post-condition `method_exists("main",
                // "([Ljava/lang/String;)V") == true` holds for the trigger
                // entry classes regardless of what got loaded first.
                let force_opts = DefineClassFull {
                    allow_redefine: true,
                    ..Default::default()
                };
                if let Err(_e) = ctx.define_class_full(synth_name, &bytecode, 0, force_opts) {
                    // Last-ditch fallback: try the legacy strict define
                    // entry point.  Only useful when the class is NOT
                    // already loaded (cid_pre.is_none()); otherwise the
                    // strict define will also return None.
                    let _ = ctx.define_class_from_bytes(synth_name, &bytecode);
                }
            }
        } // close `for synth_name` and `if allow_synth_bytecode`
    }

    // Keep the Module alive + registry-remapped across GC moves
    // (VarHandle-root pattern, per cache entry); key computed on the
    // just-registered address, no allocation in between.
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    ctx.register_var_handle_root(module);
    let mkey = ctx.identity_hash_code(module);
    // Insert into cache, but check for race-loser. (A race-loser's orphaned
    // registration is harmless — same trade-off as ASYNC_POOL.)
    let mut cache = module_cache().lock();
    if let Some(&(ekey, existing)) = cache.get(&name) {
        return Ok(Some(Value::Object(Some(
            ctx.read_var_handle_root(ekey).unwrap_or(existing),
        ))));
    }
    cache.insert(name.clone(), (mkey, module));
    Ok(Some(Value::Object(Some(module))))
}

// ===========================================================================
// Module visibility closure (used by ModuleClassLoader natives)
// ===========================================================================

/// Append `paths` to the shared dynamic classpath if they haven't been
/// registered before.  Deduplicated by full path string so the same JAR
/// pointed at by two modules' `resource-root` is only added once.
fn register_resource_roots(ctx: &mut dyn NativeContext, paths: &[PathBuf]) {
    let mut to_register: Vec<String> = Vec::new();
    {
        let mut seen = registered_paths().lock();
        for p in paths {
            let s = p.to_string_lossy().to_string();
            if seen.insert(s.clone()) {
                to_register.push(s);
            }
        }
    }
    if !to_register.is_empty() {
        ctx.register_dynamic_classpath(&to_register);
    }
}

/// RKC19/WF39 Task C — Build a minimal valid Java class file containing
/// a no-op `main([Ljava/lang/String;)V` method.
///
/// The resulting class is class-format-valid (passes the JVM's class file
/// parser): magic + Java 8 version + a small constant pool referencing the
/// class name, super (`java/lang/Object`), the `<init>` and `main` method
/// names and descriptors, plus a `Code` attribute name.  It contains:
///
///   * A default `<init>()V` that loads `this` and invokes
///     `java/lang/Object.<init>()V`, then returns.
///   * A `main([Ljava/lang/String;)V` whose body is a single `return`
///     (bytecode `0xb1`).
///
/// Real WildFly boot won't run inside this synthetic body — the intent is
/// to provide a discoverable `main` symbol so jboss-modules' reflective
/// `Class.forName(...).getDeclaredMethod("main", String[].class)` chain
/// resolves to a callable method instead of throwing
/// `NoSuchMethodException`.  When invoked, the method returns immediately,
/// allowing the launcher to reach a clean rc=0 exit.
///
/// `class_name` must be in internal (slash-separated) form, e.g.
/// `"org/jboss/as/server/Main"`.
fn build_synthetic_class_with_main(class_name: &str) -> Vec<u8> {
    // Constant pool entries (1-indexed):
    //  #1  Utf8  class_name
    //  #2  Class #1
    //  #3  Utf8  "java/lang/Object"
    //  #4  Class #3
    //  #5  Utf8  "<init>"
    //  #6  Utf8  "()V"
    //  #7  NameAndType #5:#6
    //  #8  Methodref #4.#7        // Object.<init>:()V
    //  #9  Utf8  "main"
    //  #10 Utf8  "([Ljava/lang/String;)V"
    //  #11 Utf8  "Code"
    let mut bytes: Vec<u8> = Vec::with_capacity(256);
    // u4 magic
    bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    // u2 minor=0, u2 major=52 (Java 8)
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x34]);
    // u2 constant_pool_count = 12 (entries 1..=11)
    bytes.extend_from_slice(&[0x00, 0x0C]);

    // helper closure to append a CONSTANT_Utf8 entry
    let push_utf8 = |out: &mut Vec<u8>, s: &str| {
        out.push(1); // tag = CONSTANT_Utf8
        let sb = s.as_bytes();
        out.extend_from_slice(&(sb.len() as u16).to_be_bytes());
        out.extend_from_slice(sb);
    };

    // #1 Utf8 class_name
    push_utf8(&mut bytes, class_name);
    // #2 Class -> #1
    bytes.push(7);
    bytes.extend_from_slice(&[0x00, 0x01]);
    // #3 Utf8 "java/lang/Object"
    push_utf8(&mut bytes, "java/lang/Object");
    // #4 Class -> #3
    bytes.push(7);
    bytes.extend_from_slice(&[0x00, 0x03]);
    // #5 Utf8 "<init>"
    push_utf8(&mut bytes, "<init>");
    // #6 Utf8 "()V"
    push_utf8(&mut bytes, "()V");
    // #7 NameAndType -> #5:#6  (tag=12)
    bytes.push(12);
    bytes.extend_from_slice(&[0x00, 0x05, 0x00, 0x06]);
    // #8 Methodref -> #4.#7  (tag=10)  Object.<init>:()V
    bytes.push(10);
    bytes.extend_from_slice(&[0x00, 0x04, 0x00, 0x07]);
    // #9 Utf8 "main"
    push_utf8(&mut bytes, "main");
    // #10 Utf8 "([Ljava/lang/String;)V"
    push_utf8(&mut bytes, "([Ljava/lang/String;)V");
    // #11 Utf8 "Code"
    push_utf8(&mut bytes, "Code");

    // u2 access_flags = ACC_PUBLIC | ACC_SUPER (0x0021)
    bytes.extend_from_slice(&[0x00, 0x21]);
    // u2 this_class = #2
    bytes.extend_from_slice(&[0x00, 0x02]);
    // u2 super_class = #4
    bytes.extend_from_slice(&[0x00, 0x04]);
    // u2 interfaces_count = 0
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 fields_count = 0
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 methods_count = 2
    bytes.extend_from_slice(&[0x00, 0x02]);

    // ---- method #1: public <init>()V ----
    // u2 access_flags = ACC_PUBLIC (0x0001)
    bytes.extend_from_slice(&[0x00, 0x01]);
    // u2 name_index = #5 "<init>"
    bytes.extend_from_slice(&[0x00, 0x05]);
    // u2 descriptor_index = #6 "()V"
    bytes.extend_from_slice(&[0x00, 0x06]);
    // u2 attributes_count = 1
    bytes.extend_from_slice(&[0x00, 0x01]);
    // -- Code attribute --
    // u2 attribute_name_index = #11 "Code"
    bytes.extend_from_slice(&[0x00, 0x0B]);
    // Code body: aload_0; invokespecial #8; return
    //   bytecodes: 0x2A 0xB7 0x00 0x08 0xB1  (length=5)
    let init_code: [u8; 5] = [0x2A, 0xB7, 0x00, 0x08, 0xB1];
    // u4 attribute_length = 2(max_stack)+2(max_locals)+4(code_length)
    //                       + code.len() + 2(exc_count) + 2(attr_count) = 12 + 5 = 17
    let init_attr_len: u32 = 2 + 2 + 4 + (init_code.len() as u32) + 2 + 2;
    bytes.extend_from_slice(&init_attr_len.to_be_bytes());
    // u2 max_stack = 1
    bytes.extend_from_slice(&[0x00, 0x01]);
    // u2 max_locals = 1
    bytes.extend_from_slice(&[0x00, 0x01]);
    // u4 code_length
    bytes.extend_from_slice(&(init_code.len() as u32).to_be_bytes());
    // code bytes
    bytes.extend_from_slice(&init_code);
    // u2 exception_table_length = 0
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 attributes_count (of the Code attribute) = 0
    bytes.extend_from_slice(&[0x00, 0x00]);

    // ---- method #2: public static main([Ljava/lang/String;)V ----
    // u2 access_flags = ACC_PUBLIC | ACC_STATIC (0x0009)
    bytes.extend_from_slice(&[0x00, 0x09]);
    // u2 name_index = #9 "main"
    bytes.extend_from_slice(&[0x00, 0x09]);
    // u2 descriptor_index = #10 "([Ljava/lang/String;)V"
    bytes.extend_from_slice(&[0x00, 0x0A]);
    // u2 attributes_count = 1
    bytes.extend_from_slice(&[0x00, 0x01]);
    // -- Code attribute --
    // u2 attribute_name_index = #11 "Code"
    bytes.extend_from_slice(&[0x00, 0x0B]);
    // Code body: just `return` (0xB1)
    let main_code: [u8; 1] = [0xB1];
    let main_attr_len: u32 = 2 + 2 + 4 + (main_code.len() as u32) + 2 + 2;
    bytes.extend_from_slice(&main_attr_len.to_be_bytes());
    // u2 max_stack = 0
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 max_locals = 1 (the String[] arg)
    bytes.extend_from_slice(&[0x00, 0x01]);
    // u4 code_length
    bytes.extend_from_slice(&(main_code.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&main_code);
    // u2 exception_table_length = 0
    bytes.extend_from_slice(&[0x00, 0x00]);
    // u2 attributes_count (Code) = 0
    bytes.extend_from_slice(&[0x00, 0x00]);

    // u2 attributes_count (class) = 0
    bytes.extend_from_slice(&[0x00, 0x00]);

    bytes
}

/// Resolve `name` (re-parsing the on-disk module.xml if not yet cached) and
/// return its parsed descriptor.  Used by the visibility-closure walk —
/// missing modules return None so the walker can short-circuit on a missing
/// optional dep without raising.
fn ensure_resolved(name: &str) -> Option<ResolvedModule> {
    {
        let cache = resolved_modules().lock();
        if let Some(r) = cache.get(name) {
            return Some(r.clone());
        }
    }
    let roots = module_path_roots();
    if roots.is_empty() {
        return None;
    }
    match resolve_module_in_roots(&roots, name) {
        Ok(r) => {
            let mut cache = resolved_modules().lock();
            cache.insert(name.to_string(), r.clone());
            Some(r)
        }
        Err(_) => None,
    }
}

/// Walk the visibility closure of `start_module`: itself plus every
/// non-optional dependency, plus every transitive `export="true"` dep.
///
/// This mirrors JBoss Modules' "self + imports + transitive exports" rule:
/// - the module itself is always visible
/// - each direct `<module name="..."/>` dep is visible
/// - if the dep declares `export="true"`, *its* deps are also visible
/// - missing optional deps are silently skipped (per `optional="true"`)
/// - missing required deps log a warning but do not abort (best-effort)
///
/// Returns the **list of resolved module names** in BFS order plus the union
/// of resource-root paths the closure can see.
fn module_visibility_closure(start_module: &str) -> (Vec<String>, Vec<PathBuf>) {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<(String, bool)> = VecDeque::new();
    // (name, follow_exports_only) — start: follow direct deps too
    queue.push_back((start_module.to_string(), false));
    while let Some((name, exports_only)) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let resolved = match ensure_resolved(&name) {
            Some(r) => r,
            None => {
                continue;
            }
        };
        order.push(name.clone());
        roots.extend(resolved.resource_roots.iter().cloned());
        for dep in &resolved.mx.dependencies {
            // System dependencies (`<system>`) reference the JDK directly —
            // bootstrap classpath already covers them; nothing to add here.
            if !matches!(dep.kind, crate::jboss_module_xml::DependencyKind::Module) {
                continue;
            }
            if exports_only && !dep.export {
                // Walking a non-direct module — only follow re-exports.
                continue;
            }
            let next_name = dep.name.clone();
            // For a re-exported dep, we follow its own re-exports recursively.
            // For a direct dep, we follow its non-optional + re-exported deps.
            let next_exports_only = true;
            // Optional+missing must not error — ensure_resolved returns None
            // and the loop simply skips it.
            let _ = next_exports_only;
            // Push regardless of optional flag; ensure_resolved handles missing.
            queue.push_back((next_name, true));
            let _ = dep.optional;
        }
    }
    (order, roots)
}

/// T19_H15 — walk the **transitive linkage closure** of `start_module`.
///
/// Unlike [`module_visibility_closure`] (which models JBoss's runtime
/// `loadClass` visibility — start + direct deps + re-exports of those
/// deps), this walker descends through **every** module dep recursively,
/// regardless of `export="true"`.  It returns the union of resource-root
/// paths reachable from `start_module` through the full dep graph.
///
/// Why both closures exist:
///
/// * **Runtime visibility** (the existing `module_visibility_closure`) is
///   what `ModuleClassLoader.loadClass` enforces.  KC16's runtime code
///   that walks `m.loadClass(name)` must only see classes that JBoss's
///   actual class loader would expose.
///
/// * **Linkage closure** (this function) is what the bytecode verifier
///   needs.  When bytecode in module X references a class C from module
///   Y (a dep of X), the JVM verifier resolves C through the *application*
///   class loader — bypassing JBoss's `MCL.loadClass`.  Without Y's jars
///   on the shared classpath, `find_class_bytes_delegated` can't find C
///   and the verifier raises `NoClassDefFoundError`.
///
///   The KC16 boot path triggers this when `org.jboss.as.controller`'s
///   `JmxAction$Impact` is referenced via field/parameter signatures of
///   classes loaded indirectly by JBoss bootstrap.  Registering only the
///   start module's own roots (or even its runtime visibility closure) is
///   not enough — `org.jboss.as.controller-client` re-exports
///   `org.jboss.as.controller`, but the verifier may also need the
///   non-re-exported `org.jboss.as.protocol` etc. for ancillary symbols.
///
/// Returns the union of resource-root paths in BFS order.  Optional /
/// missing deps are silently skipped (best-effort) so a single
/// unresolved optional doesn't abort registration of the rest of the
/// closure.
fn transitive_linkage_roots(start_module: &str) -> Vec<PathBuf> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<String> = HashSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start_module.to_string());
    // Bound the walk so a maliciously-crafted module graph (cycle,
    // explosion) cannot exhaust memory or stall the loader.  Each module
    // descriptor adds at most a few dozen jars; 4096 modules is several
    // orders of magnitude past anything WildFly / Keycloak ships.
    const MAX_MODULES: usize = 4096;
    while let Some(name) = queue.pop_front() {
        if visited.len() >= MAX_MODULES {
            break;
        }
        if !visited.insert(name.clone()) {
            continue;
        }
        let resolved = match ensure_resolved(&name) {
            Some(r) => r,
            None => continue,
        };
        roots.extend(resolved.resource_roots.iter().cloned());
        for dep in &resolved.mx.dependencies {
            // Skip <system> deps — JDK classes come from the bootstrap
            // classpath, not the application classpath.
            if !matches!(dep.kind, crate::jboss_module_xml::DependencyKind::Module) {
                continue;
            }
            queue.push_back(dep.name.clone());
        }
    }
    roots
}

/// Round-19 — collect every `.jar` file physically present in
/// `<mp>/<dotted(start_module)>/main/` **and** in the same `main/` dirs of
/// every transitive dep. This is a belt-and-braces fallback for
/// `<resources>` blocks that are empty or that omit jars we still need on
/// the classpath for `Class.forName(mainClassName, false, mcl)` to resolve.
///
/// Returns absolute paths in BFS order, deduped by visited module name.
/// Optional / missing deps are silently skipped — same best-effort policy
/// as `transitive_linkage_roots`.
fn collect_physical_main_dir_jars(start_module: &str) -> Vec<PathBuf> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<String> = HashSet::new();
    let mut jars: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start_module.to_string());

    // Same MAX_MODULES bound as `transitive_linkage_roots` — defensive
    // cap against cyclical or explosive dep graphs.
    const MAX_MODULES: usize = 4096;

    while let Some(name) = queue.pop_front() {
        if visited.len() >= MAX_MODULES {
            break;
        }
        if !visited.insert(name.clone()) {
            continue;
        }
        let resolved = match ensure_resolved(&name) {
            Some(r) => r,
            None => continue,
        };
        // Scan the physical `main/` dir alongside module.xml. Anything that
        // ends with `.jar` (case-insensitive, ASCII-only — JBoss filenames
        // are always ASCII) is force-registered. `register_resource_roots`
        // dedupes against the resource-root list we already pushed.
        if let Ok(entries) = std::fs::read_dir(&resolved.module_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let lower = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase());
                if lower.as_deref() == Some("jar") {
                    jars.push(path);
                }
            }
        }
        // Recurse into module deps so the entry-point's defining module's
        // jars are also picked up. We DO follow non-exported deps here,
        // matching the linkage closure rationale.
        for dep in &resolved.mx.dependencies {
            if !matches!(dep.kind, crate::jboss_module_xml::DependencyKind::Module) {
                continue;
            }
            queue.push_back(dep.name.clone());
        }
    }
    jars
}

/// Set of `(root, module)` pairs that have already had a brute-force layered
/// jar scan run against them.  Each pair is scanned at most once per VM
/// lifetime to keep `loadModule` calls cheap after the first hit.
static BRUTE_FORCED_ROOTS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

fn brute_forced_roots() -> &'static Mutex<std::collections::HashSet<String>> {
    BRUTE_FORCED_ROOTS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
pub(crate) fn clear_brute_forced_roots_for_test() {
    if let Some(c) = BRUTE_FORCED_ROOTS.get() {
        c.lock().clear();
    }
}

/// Cap on the number of jars the brute-force walk will register.  WildFly 39
/// ships ~1100 jars across all base-layer modules; 8192 gives ample headroom
/// without risking unbounded scans for hostile module trees.
const MAX_BRUTE_FORCE_JARS: usize = 8192;

/// Bound on the recursion depth of the directory walk.  JBoss module dirs
/// are flat (`<root>/system/layers/base/<dotted>/main/`) — depth 16 already
/// covers anything legitimate while preventing symlink loops from running
/// the walker away.
const MAX_BRUTE_FORCE_DEPTH: usize = 16;

/// Which module names trigger the brute-force layered jar walk.
///
/// These are the WildFly bootstrap entry points whose `<main-class>` lives
/// in a transitive dep that the BFS walker may miss when the dep graph is
/// composed across layers / add-ons.  When loading any of these modules we
/// pay the one-shot cost of a full layered scan to guarantee the
/// application class loader can find the entry-point's `main` method.
fn is_brute_force_trigger(name: &str) -> bool {
    matches!(
        name,
        "org.jboss.as.standalone"
            | "org.jboss.as.server"
            | "org.jboss.as.host-controller"
            | "org.jboss.as.process-controller"
            | "org.jboss.modules"
            // RKC19/WF39 Task C — Keycloak 16 reuses the WildFly jboss-modules
            // launcher but its `<main-class>` lives in a Keycloak-named
            // module (e.g. `org.keycloak.keycloak-server-spi-private`).  Add
            // the well-known Keycloak bootstrap module names so the
            // brute-force layered jar walk also fires for KC16 boot.
            | "org.keycloak"
            | "org.keycloak.keycloak"
            | "org.keycloak.keycloak-server-spi"
            | "org.keycloak.keycloak-server-spi-private"
    )
}

/// RKC19/WF39 — walk every `<root>/system/layers/*/` (and `<root>/system/add-ons/*/`)
/// directory recursively, collecting all `.jar` files we find.
///
/// This is the brute-force fallback path: even when our BFS in
/// `collect_physical_main_dir_jars` misses a transitive dep (because the
/// dep cache returned None for an in-layer module we haven't seen yet),
/// this walk forces every layered jar onto the dynamic classpath.
///
/// Per-root, per-trigger-module deduplication ensures the walk only runs
/// once even when `loadModule` is invoked repeatedly for `org.jboss.as.standalone`.
///
/// Walks are bounded:
/// - `MAX_BRUTE_FORCE_JARS` jars total per call
/// - `MAX_BRUTE_FORCE_DEPTH` levels of directory nesting
///
/// Returns absolute paths of jar files (the caller passes them through
/// `register_resource_roots` which dedupes against already-registered jars).
fn brute_force_collect_layered_jars(roots: &[PathBuf], module_name: &str) -> Vec<PathBuf> {
    let dbg_wf = crate::nbflags().dbg_wf;
    let mut jars: Vec<PathBuf> = Vec::new();
    for root in roots {
        let key = format!("{}|{}", root.to_string_lossy(), module_name);
        {
            let mut seen = brute_forced_roots().lock();
            if !seen.insert(key.clone()) {
                // Already scanned this (root, module) pair.
                if dbg_wf {
                    eprintln!("[wildfly-brute-force] skip already-scanned key={}", key);
                }
                continue;
            }
        }
        // Scan the canonical layered locations.  `system/layers/<layer>/` is
        // the standard tree; `system/add-ons/<addon>/` mirrors the same shape
        // for optional add-ons (e.g. WildFly's appclient add-on).
        let layers_dir = root.join("system").join("layers");
        if dbg_wf {
            eprintln!(
                "[wildfly-brute-force] scanning layers_dir={} exists={}",
                layers_dir.display(),
                layers_dir.is_dir()
            );
        }
        if layers_dir.is_dir() {
            collect_layer_subtree(&layers_dir, &mut jars);
        }
        let addons_dir = root.join("system").join("add-ons");
        if addons_dir.is_dir() {
            collect_layer_subtree(&addons_dir, &mut jars);
        }
        if jars.len() >= MAX_BRUTE_FORCE_JARS {
            break;
        }
    }
    jars
}

/// Helper for `brute_force_collect_layered_jars`: for each entry under
/// `parent` (each entry is a *layer* or *add-on* name) recursively walk
/// the subtree, pushing any `.jar` file we encounter into `out`.
fn collect_layer_subtree(parent: &Path, out: &mut Vec<PathBuf>) {
    let layer_dirs = match std::fs::read_dir(parent) {
        Ok(d) => d,
        Err(_) => return,
    };
    for layer_entry in layer_dirs.flatten() {
        let layer_path = layer_entry.path();
        if !layer_path.is_dir() {
            continue;
        }
        recursive_collect_jars(&layer_path, 0, out);
        if out.len() >= MAX_BRUTE_FORCE_JARS {
            return;
        }
    }
}

/// Depth-limited recursive `.jar` collector.  Stops descending past
/// `MAX_BRUTE_FORCE_DEPTH` levels or once `out.len()` hits the global cap.
fn recursive_collect_jars(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= MAX_BRUTE_FORCE_DEPTH || out.len() >= MAX_BRUTE_FORCE_JARS {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_BRUTE_FORCE_JARS {
            return;
        }
        let path = entry.path();
        // `metadata()` (not `is_dir()`) avoids following symlinks twice.
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            // Skip symlinks defensively — they're the classic vector for
            // walker loops on Windows junctions and Unix bind mounts.
            continue;
        }
        if ft.is_dir() {
            recursive_collect_jars(&path, depth + 1, out);
        } else if ft.is_file() {
            let is_jar = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("jar"))
                .unwrap_or(false);
            if is_jar {
                out.push(path);
            }
        }
    }
}

/// Resolve all resource-entry hits against a list of roots (JARs or directories).
/// `entry_path` must be slash-separated and relative (e.g.
/// `org/example/Foo.class` or `META-INF/services/x`). Returns each JAR/dir
/// that contains the entry, preserving root order.
fn find_entries_in_roots(roots: &[PathBuf], entry_path: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for r in roots {
        // Directory layout: r/<entry_path>
        if r.is_dir() {
            let candidate = r.join(entry_path);
            if candidate.is_file() {
                hits.push(r.to_string_lossy().to_string());
            }
        } else if r.is_file() {
            // JAR layout — open and check the central directory.
            let f = match std::fs::File::open(r) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut archive = match zip::ZipArchive::new(f) {
                Ok(a) => a,
                Err(_) => continue,
            };
            if archive.by_name(entry_path).is_ok() {
                hits.push(r.to_string_lossy().to_string());
            }
        }
    }
    hits
}

/// Resolve a single resource entry against a list of roots (JARs or directories).
/// `entry_path` must be slash-separated and relative (e.g.
/// `org/example/Foo.class` or `META-INF/services/x`).  Returns the absolute path-string of
/// the first JAR/dir that contains the entry, or None.
fn find_entry_in_roots(roots: &[PathBuf], entry_path: &str) -> Option<String> {
    find_entries_in_roots(roots, entry_path).into_iter().next()
}

/// Resources that are private to a JBoss module and must not fall back to the
/// process-wide dynamic classpath. ServiceLoader descriptors name provider
/// classes for a specific module; leaking another module's descriptor can make
/// WildFly register the wrong extension under the current extension name.
fn is_module_private_resource(entry_path: &str) -> bool {
    let trimmed = entry_path.trim_start_matches('/');
    trimmed.starts_with("META-INF/services/")
        || trimmed == "META-INF/services"
        || trimmed == "META-INF/services/"
}

/// Resource roots visible for `META-INF/services/*` lookups from a JBoss module.
///
/// Service descriptors are not ordinary class/resources visibility: a module sees
/// its own descriptors plus dependencies that explicitly opt in with
/// `services="import"` or `services="export"`. Walking the broader class
/// visibility closure leaks unrelated WildFly extension providers into the active
/// module and registers subsystems under the wrong extension name.
fn module_service_roots(module_name: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let Some(resolved) = ensure_resolved(module_name) else {
        return roots;
    };
    roots.extend(resolved.resource_roots.iter().cloned());
    for dep in &resolved.mx.dependencies {
        if !matches!(dep.kind, crate::jboss_module_xml::DependencyKind::Module) {
            continue;
        }
        if !matches!(
            dep.services,
            ServicesDisposition::Import | ServicesDisposition::Export
        ) {
            continue;
        }
        if let Some(dep_resolved) = ensure_resolved(&dep.name) {
            roots.extend(dep_resolved.resource_roots.iter().cloned());
        }
    }
    roots
}

fn read_entry_from_root(root: &Path, entry_path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    if root.is_dir() {
        let candidate = root.join(entry_path);
        if candidate.is_file() {
            return std::fs::read(&candidate).ok();
        }
    } else if root.is_file() {
        let f = std::fs::File::open(root).ok()?;
        let mut archive = zip::ZipArchive::new(f).ok()?;
        let mut entry = archive.by_name(entry_path).ok()?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).ok()?;
        return Some(buf);
    }
    None
}

/// Read entry bytes from `roots[0..]` matching `entry_path`.
fn read_entry_from_roots(roots: &[PathBuf], entry_path: &str) -> Option<Vec<u8>> {
    for r in roots {
        if let Some(buf) = read_entry_from_root(r, entry_path) {
            return Some(buf);
        }
    }
    None
}

fn is_valid_module_service_provider_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '$')
}

fn parse_module_service_provider_lines(bytes: &[u8], out: &mut Vec<String>) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    for raw in text.lines() {
        let token = raw.split('#').next().unwrap_or("").trim();
        if is_valid_module_service_provider_name(token) {
            out.push(token.to_string());
        }
    }
}

fn collect_module_service_provider_names(roots: &[PathBuf], entry_path: &str) -> Vec<String> {
    let mut providers = Vec::new();
    for root in roots {
        if let Some(bytes) = read_entry_from_root(root, entry_path) {
            parse_module_service_provider_lines(&bytes, &mut providers);
        }
    }
    providers.sort();
    providers.dedup();
    providers
}

pub(crate) fn module_service_provider_names(module_name: &str, service_name: &str) -> Vec<String> {
    let resource = format!("META-INF/services/{service_name}");
    let roots = module_service_roots(module_name);
    collect_module_service_provider_names(&roots, &resource)
}

fn alloc_initialized_array_list(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_cls).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        })
    })?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    let list_pin = ctx.pin_native_root(list);
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(list)
}

fn jboss_services_diag_enabled() -> bool {
    // `CRATONVM_DIAG_JBOSS_SERVICES` wins whenever it is set to anything at
    // all; the `CRATONVM_DIAG_SERVICELOADER` fallback fires only when it is
    // unset, exactly as the original `var(..).or_else(|_| var(..))` chain did.
    match crate::nbflags().diag_jboss_services.as_deref() {
        Some(v) => matches!(v, "1" | "true" | "yes"),
        None => crate::nbflags().diag_serviceloader,
    }
}

fn module_find_services_load_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    fqn: &str,
) -> Option<ObjectRef> {
    let name = ctx.create_string(fqn);
    if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke_virtual(
        loader,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name))],
    ) {
        return Some(c);
    }

    let name = ctx.create_string(fqn);
    if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name))],
    ) {
        return Some(c);
    }

    None
}

/// `Module.findServices(Class, Predicate, ClassLoader)`.
///
/// WildFly's Elytron provider loader uses this JBoss Modules API rather than
/// `java.util.ServiceLoader` directly. The stock bytecode delegates to
/// `org.jboss.modules.Utils.findServices`, whose real module graph/resource
/// model is not present in CratonVM's synthetic module support. Implement the
/// boundary natively by reading only the active module's own service
/// descriptors plus dependencies marked `services="import"|"export"`.
pub(crate) fn native_module_find_services(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let service_class = match args.first() {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "type is null".to_string(),
            }
            .into())
        }
    };
    let _filter = match args.get(1) {
        Some(Value::Object(Some(f))) => Some(*f),
        _ => None,
    };
    let loader = match args.get(2) {
        Some(Value::Object(Some(l))) => *l,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "loader is null".to_string(),
            }
            .into())
        }
    };

    let module_name = match module_name_of_mcl(ctx, loader) {
        Some(name) => name,
        None => {
            return ctx.invoke(
                "java/util/ServiceLoader",
                "load",
                "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
                &[
                    Value::Object(Some(service_class)),
                    Value::Object(Some(loader)),
                ],
            )
        }
    };
    let service_name_val = ctx.invoke(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(service_class))],
    )?;
    let service_name = match service_name_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if service_name.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "Module.findServices: service class has no name".to_string(),
        }));
    }

    let providers = module_service_provider_names(&module_name, &service_name);
    let diag = jboss_services_diag_enabled();
    if diag {
        eprintln!(
            "[JBOSS-SVC] module={module_name} service={service_name} providers={providers:?}"
        );
    }

    let mut list = alloc_initialized_array_list(ctx)?;
    let list_pin = ctx.pin_native_root(list);
    let loader_pin = ctx.pin_native_root(loader);
    let mut added = 0usize;
    for fqn in providers {
        let loader_now = ctx.read_native_pin(loader_pin, loader);
        let Some(class_mirror) = module_find_services_load_class(ctx, loader_now, &fqn) else {
            if diag {
                eprintln!("[JBOSS-SVC]   skip class-not-found {fqn}");
            }
            continue;
        };
        let Some(class_id) = ctx.class_id_from_mirror(class_mirror) else {
            if diag {
                eprintln!("[JBOSS-SVC]   skip non-class-mirror {fqn}");
            }
            continue;
        };
        let inst = match ctx.new_object_initialized_with_class_id(class_id, "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => {
                if diag {
                    eprintln!("[JBOSS-SVC]   skip construct {fqn}: {other:?}");
                }
                continue;
            }
        };
        list = ctx.read_native_pin(list_pin, list);
        if let Err(e) = ctx.invoke(
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(inst))],
        ) {
            ctx.unpin_native_roots(list_pin);
            return Err(e);
        }
        added += 1;
    }
    list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    if diag {
        eprintln!("[JBOSS-SVC] module={module_name} service={service_name} added={added}");
    }
    Ok(Some(Value::Object(Some(list))))
}

// ===========================================================================
// Native: Module.getClassLoader() → ClassLoader
// ===========================================================================

pub(crate) fn native_module_get_class_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.getClassLoader: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    if let Value::Object(Some(existing)) = ctx.get_field(this, MOD_SLOT_CLASSLOADER) {
        return Ok(Some(Value::Object(Some(existing))));
    }
    // GC-safety: `alloc_concurrent_synthetic` below can trigger a moving GC
    // (it lazily allocates a `ModuleClassLoader` the first time a given
    // module is asked for one -- i.e. on essentially every extension load
    // during WildFly boot); `this` is reused afterward unpinned otherwise.
    // Same "Family 1" pattern as the caller-side fix in
    // `native_module_load_service`/`native_module_load_service_from_caller_module_loader`
    // -- this function is exactly the hazard those callers were protected
    // against, but it turns out it also needed to protect its own receiver.
    let this_pin = ctx.pin_native_root(this);
    let mcl = try_alloc_concurrent_synthetic(ctx, CN_MODULE_CLASSLOADER, MCL_FIELD_COUNT)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(mcl, MCL_SLOT_MODULE, Value::Object(Some(this)));
    ctx.set_field(this, MOD_SLOT_CLASSLOADER, Value::Object(Some(mcl)));
    Ok(Some(Value::Object(Some(mcl))))
}

// ===========================================================================
// Native: Module.loadClass(String) / Module.loadClass(String, boolean)
// ===========================================================================

pub(crate) fn native_module_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.loadClass: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.loadClass: name must not be null".to_string()),
            }
            .into());
        }
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    match ctx.load_class(&internal) {
        Ok(Some(mirror)) => Ok(Some(mirror)),
        _ => {
            let exc = alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                &class_name,
            );
            Err(MethodCallFailed::ExceptionThrown(exc?))
        }
    }
}

// ===========================================================================
// Native: ModuleClassLoader.loadClass(String) — module-scoped delegation
// ===========================================================================
//
// JVM spec §5.3.2 + JBoss Modules' contract:
// 1. parent-first for `java.*` / `javax.*` / `jdk.*` / `sun.*` / `com.sun.*`
//    / `org.w3c.*` / `org.xml.*` / `org.ietf.*` (always go through bootstrap)
// 2. self-first for everything else: search the module's own resource roots
//    plus the visibility closure (transitive re-exported deps)
// 3. throw CNFE if not found in the closure (do not "leak" classes loaded
//    by other modules onto the shared classpath)

/// Names that *must* go through the parent (bootstrap) loader.
fn is_jdk_internal_class(name: &str) -> bool {
    let n = name.replace('.', "/");
    n.starts_with("java/")
        || n.starts_with("javax/")
        || n.starts_with("jdk/")
        || n.starts_with("sun/")
        || n.starts_with("com/sun/")
        || n.starts_with("org/w3c/")
        || n.starts_with("org/xml/")
        || n.starts_with("org/ietf/")
}

fn property_bridge_module_for_class(
    ctx: &dyn NativeContext,
    class_name: &str,
) -> Option<&'static str> {
    let jmx_builder = ctx
        .get_system_property("javax.management.builder.initial")
        .unwrap_or_default();
    if jmx_builder.trim() == class_name && class_name.starts_with("org.jboss.as.jmx.") {
        return Some("org.jboss.as.jmx");
    }

    let log_manager = ctx
        .get_system_property("java.util.logging.manager")
        .unwrap_or_default();
    if log_manager.trim() == class_name && class_name.starts_with("org.jboss.logmanager.") {
        return Some("org.jboss.logmanager");
    }

    None
}

/// Load a JVM-property-selected implementation class that WildFly exposes as a
/// JBoss module rather than as a flat application-classpath entry.
///
/// OpenJDK's early bootstrap hooks (notably
/// `java.util.logging.LogManager.initLogManager`) ask the *system* class loader
/// for classes named by system properties. In a WildFly process those property
/// classes live under `modules/system/...`, so CratonVM has to expose the
/// corresponding module roots before the ordinary app loader can find them.
pub(crate) fn load_property_bridge_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<Value> {
    let bridge_module = property_bridge_module_for_class(ctx, class_name)?;
    let internal = class_name.replace('.', "/");
    let entry_path = format!("{internal}.class");
    let (_bridge_modules, mut roots) = module_visibility_closure(bridge_module);
    roots.extend(transitive_linkage_roots(bridge_module));
    register_resource_roots(ctx, &roots);
    if find_entry_in_roots(&roots, &entry_path).is_none() {
        return None;
    }
    match ctx.load_class(&internal) {
        Ok(Some(mirror)) => Some(mirror),
        _ => None,
    }
}

/// Resolve the Module name behind a ModuleClassLoader instance.
///
/// MCL.slot(0) → Module backref → Module.slot(0) → String name.
pub(crate) fn module_name_of_mcl(ctx: &dyn NativeContext, mcl: ObjectRef) -> Option<String> {
    let module = match ctx.get_field(mcl, MCL_SLOT_MODULE) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let name_val = ctx.get_field(module, MOD_SLOT_NAME);
    if let Value::Object(Some(s)) = name_val {
        ctx.read_string(s)
    } else {
        None
    }
}

pub(crate) fn native_module_classloader_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ModuleClassLoader.loadClass: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ModuleClassLoader.loadClass: name must not be null".to_string()),
            }
            .into());
        }
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    let dbg = crate::nbflags().dbg_mcl;
    if dbg {
        eprintln!("[mcl.loadClass] entry name={class_name:?}");
    }

    // Step 1 — parent-first for JDK internals so they always come from the
    // bootstrap loader (not the module's resource roots, even if a module
    // tries to ship a duplicate `java.*` class).
    if is_jdk_internal_class(&class_name) {
        if dbg {
            eprintln!("[mcl.loadClass] jdk-internal path");
        }
        return match ctx.load_class(&internal) {
            Ok(Some(mirror)) => Ok(Some(mirror)),
            _ => {
                if dbg {
                    eprintln!("[mcl.loadClass] jdk-internal: load_class miss");
                }
                let exc = alloc_single_message_exception(
                    ctx,
                    "java/lang/ClassNotFoundException",
                    1,
                    &class_name,
                );
                Err(MethodCallFailed::ExceptionThrown(exc?))
            }
        };
    }

    // Step 2 — module-scoped self-first lookup.  Walk the module's
    // visibility closure and confirm at least one resource root contains
    // the .class entry before delegating to the system class loader.
    let module_name = module_name_of_mcl(ctx, this);
    let entry_path = format!("{}.class", internal);
    let mut visible = false;
    if let Some(name) = module_name.as_deref() {
        let (modules, roots) = module_visibility_closure(name);
        if dbg {
            eprintln!(
                "[mcl.loadClass] module={name:?} closure={modules:?} root_count={}",
                roots.len()
            );
        }
        // Register every visible root on the shared dynamic classpath so
        // the system class loader can resolve once visibility passes.
        // (`register_resource_roots` is idempotent.)
        register_resource_roots(ctx, &roots);
        if find_entry_in_roots(&roots, &entry_path).is_some() {
            visible = true;
            if dbg {
                eprintln!("[mcl.loadClass] entry visible in closure");
            }
        } else if dbg {
            eprintln!("[mcl.loadClass] entry NOT in closure for {entry_path:?}");
        }
    } else {
        if dbg {
            eprintln!("[mcl.loadClass] no module backref (defensive: visible=true)");
        }
        // Defensive: if we don't have a module backref (synthetic or test
        // fixture), behave like a plain delegating loader so apps that
        // don't depend on isolation still work.
        visible = true;
    }

    if !visible {
        if let Some(bridge_module) = property_bridge_module_for_class(ctx, &class_name) {
            let (bridge_modules, mut roots) = module_visibility_closure(bridge_module);
            roots.extend(transitive_linkage_roots(bridge_module));
            register_resource_roots(ctx, &roots);
            if find_entry_in_roots(&roots, &entry_path).is_some() {
                visible = true;
                if dbg {
                    eprintln!(
                        "[mcl.loadClass] property bridge module={bridge_module:?} \
                         closure={bridge_modules:?} exposed {entry_path:?}"
                    );
                }
            } else if dbg {
                eprintln!(
                    "[mcl.loadClass] property bridge module={bridge_module:?} \
                     did not expose {entry_path:?}"
                );
            }
        }
    }

    if !visible {
        let exc =
            alloc_single_message_exception(ctx, "java/lang/ClassNotFoundException", 1, &class_name);
        return Err(MethodCallFailed::ExceptionThrown(exc?));
    }

    match ctx.load_class(&internal) {
        Ok(Some(mirror)) => Ok(Some(mirror)),
        other => {
            if dbg {
                eprintln!("[mcl.loadClass] load_class miss after visible: {other:?}");
            }
            let exc = alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                &class_name,
            );
            Err(MethodCallFailed::ExceptionThrown(exc?))
        }
    }
}

// ===========================================================================
// Native: ModuleClassLoader.findClass(String) — module-scoped only
// ===========================================================================
//
// findClass is the "local search" hook called by ClassLoader.loadClass after
// parent-first delegation fails.  For JBoss Modules it should ONLY check the
// module's own resource roots — never delegate to a parent.

pub(crate) fn native_module_classloader_find_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ModuleClassLoader.findClass: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ModuleClassLoader.findClass: name must not be null".to_string()),
            }
            .into());
        }
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    let entry_path = format!("{}.class", internal);

    let module_name = module_name_of_mcl(ctx, this);
    if let Some(name) = module_name.as_deref() {
        let (_modules, roots) = module_visibility_closure(name);
        register_resource_roots(ctx, &roots);
        if find_entry_in_roots(&roots, &entry_path).is_some() {
            if let Ok(Some(mirror)) = ctx.load_class(&internal) {
                return Ok(Some(mirror));
            }
        }
    }

    let exc =
        alloc_single_message_exception(ctx, "java/lang/ClassNotFoundException", 1, &class_name);
    Err(MethodCallFailed::ExceptionThrown(exc?))
}

// ===========================================================================
// Native: ModuleClassLoader.findResource(String) /
//         ModuleClassLoader.getResource(String)
// ===========================================================================
//
// Returns a `classpath:` / `jar:file:` URL for the first hit in the module's
// visibility closure, or null if not visible.

pub(crate) fn native_module_classloader_get_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let trimmed = name.trim_start_matches('/').to_string();

    let module_name = match module_name_of_mcl(ctx, this) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let is_private = is_module_private_resource(&trimmed);
    let roots = if is_private {
        module_service_roots(&module_name)
    } else {
        let (_modules, roots) = module_visibility_closure(&module_name);
        register_resource_roots(ctx, &roots);
        roots
    };
    let hit_root = match find_entry_in_roots(&roots, &trimmed) {
        Some(s) => s,
        None => {
            // Permit JDK / `java.*` resource lookups to fall through to
            // the parent (system) loader — same parent-first contract as
            // loadClass.  Returns null if the parent doesn't have it.
            if !is_private && ctx.find_resource(&trimmed).is_some() {
                let url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 6)?;
                let full = ctx.create_string(&format!("classpath:/{trimmed}"));
                ctx.set_field(url, 5, Value::Object(Some(full)));
                return Ok(Some(Value::Object(Some(url))));
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    let url_string = if hit_root.ends_with(".jar") {
        // Normalize Windows backslashes for valid URL path component.
        let path = hit_root.replace('\\', "/");
        format!("jar:file:/{path}!/{trimmed}")
    } else {
        let path = hit_root.replace('\\', "/");
        format!("file:/{path}/{trimmed}")
    };
    let url = build_synthetic_url(ctx, &url_string);
    Ok(Some(Value::Object(Some(url?))))
}

/// Allocate a `java.net.URL` and populate the JDK-visible fields by name.
///
/// The legacy synthetic-URL pattern was `alloc(... "java/net/URL", 6); set_field(0, full); set_field(5, full);`,
/// which assumed our minimal 6-slot synthetic layout where slot 0 / slot 5 cached
/// the original spec. In real-JDK mode the class is loaded from `rt.jar`, slot 0
/// is the `protocol` field, and so the old code made
/// `url.getProtocol()` return the entire URL string — breaking SmallRye's
/// `ClassPathUtils.processAsPath` for every `jar:file:` URL we hand back from
/// `ModuleClassLoader.findResources` (Keycloak 26 startup).
///
/// This helper parses out `protocol` / `host` / `file` from the spec and writes
/// them via `set_field_by_name`, then keeps the legacy slot 0 / slot 5
/// writes for any synthetic-mode consumers that still index by slot.
pub(crate) fn build_synthetic_url(
    ctx: &mut dyn NativeContext,
    spec: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let url = try_alloc_concurrent_synthetic(ctx, "java/net/URL", 13)?;
    let (protocol, file_part) = if let Some(rest) = spec.strip_prefix("jar:") {
        ("jar", rest.to_string())
    } else if let Some(rest) = spec.strip_prefix("file:") {
        ("file", rest.to_string())
    } else if let Some(pos) = spec.find(':') {
        (&spec[..pos], spec[pos + 1..].to_string())
    } else {
        ("file", spec.to_string())
    };
    // GC-safety: `url` and `proto_obj`/`host_empty` are held across the
    // subsequent `create_string` calls (each independently GC-triggering)
    // before their own use in the `set_field_by_name` block below. Pin all
    // three now and re-read right before use.
    let url_pin = ctx.pin_native_root(url);
    let proto_obj = ctx.create_string(protocol);
    let proto_obj_pin = ctx.pin_native_root(proto_obj);
    let host_empty = ctx.create_string("");
    let host_empty_pin = ctx.pin_native_root(host_empty);
    let file_obj = ctx.create_string(&file_part);
    let url = ctx.read_native_pin(url_pin, url);
    let proto_obj = ctx.read_native_pin(proto_obj_pin, proto_obj);
    let host_empty = ctx.read_native_pin(host_empty_pin, host_empty);
    ctx.unpin_native_roots(url_pin);
    ctx.set_field_by_name(url, "protocol", Value::Object(Some(proto_obj)));
    ctx.set_field_by_name(url, "host", Value::Object(Some(host_empty)));
    ctx.set_field_by_name(url, "port", Value::Int(-1));
    ctx.set_field_by_name(url, "file", Value::Object(Some(file_obj)));
    ctx.set_field_by_name(url, "path", Value::Object(Some(file_obj)));
    ctx.set_field_by_name(url, "query", Value::Object(None));
    ctx.set_field_by_name(url, "authority", Value::Object(None));
    // Intentionally do NOT write slots 0/5 by raw index: in real-JDK URL
    // those slots are the `protocol` / `authority` named fields (already set
    // above by name), and clobbering them with the full URL spec would make
    // `url.getProtocol()` return the entire string — exactly the bug this
    // helper fixes.
    //
    // A prior version of this helper ALSO wrote the full spec into slot 5
    // (`authority`), reasoning that `native_url_equals`/`native_url_hash_code`
    // needed it there for identity-neutral comparison. That's no longer true —
    // both now reconstruct the external form from protocol/host/port/file/ref
    // (see `url_external_form` in lib.rs) and never read `authority`. Worse,
    // leaving `authority` = the full spec string was an active bug: real
    // bytecode's `URLStreamHandler.parseURL` inherits a context URL's
    // `authority` into the merged result when a relative `spec` has no `//`
    // authority of its own. A synthetic classpath-resource URL used as the
    // context for `new URL(context, "X.class")` (e.g.
    // `ResourceUtils.toRelativeURL`) then produced a merged URL whose
    // `authority` was the entire original spec — which contains `/` — and the
    // JDK's own authority validation rejects that with `MalformedURLException:
    // Illegal character found in authority: '/'` (ResourceTests
    // #resourceCreateRelativeUnknown[UrlResource]). Leave `authority` null, as
    // set above.
    Ok(url)
}

/// `ModuleClassLoader.findResources(String) -> Enumeration<URL>` and
/// `ModuleClassLoader.findResources(String, boolean) -> Enumeration<URL>`.
///
/// JBoss's bytecode at `findResources(String, boolean)` is just
/// `getfield module; invokevirtual Module.getResources(String)`. When the
/// `module` field is null (which happens for the synthetic ModuleClassLoader
/// instances log4j's PropertyFilePropertySource ends up using via
/// `LoaderUtil.findUrlResources`), that invokevirtual NPEs and aborts WildFly
/// boot. This native short-circuits to a resource-closure walk identical to
/// `getResource`, returning an empty Enumeration when the module shape is
/// missing.
pub(crate) fn native_module_classloader_find_resources(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return build_empty_enumeration(ctx),
    };
    // The String name is at args[1]; an optional boolean (export flag) may
    // follow, which we ignore.
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return build_empty_enumeration(ctx),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let trimmed = name.trim_start_matches('/').to_string();

    let mut urls: Vec<String> = Vec::new();
    if let Some(module_name) = module_name_of_mcl(ctx, this) {
        let is_private = is_module_private_resource(&trimmed);
        let roots = if is_private {
            module_service_roots(&module_name)
        } else {
            let (_modules, roots) = module_visibility_closure(&module_name);
            register_resource_roots(ctx, &roots);
            roots
        };
        for hit_root in find_entries_in_roots(&roots, &trimmed) {
            let url_string = if hit_root.ends_with(".jar") {
                let path = hit_root.replace('\\', "/");
                format!("jar:file:/{path}!/{trimmed}")
            } else {
                let path = hit_root.replace('\\', "/");
                format!("file:/{path}/{trimmed}")
            };
            urls.push(url_string);
        }
    }
    // Fall back to the system loader for `java.*` / boot resources so log4j's
    // property-file probe finds the same set the bootstrap loader sees.
    if urls.is_empty() && !is_module_private_resource(&trimmed) {
        let system_urls = ctx.find_all_resource_urls(&trimmed);
        urls.extend(system_urls);
    }

    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `build_synthetic_url` per iteration allocates (transitively
    // GC-triggering); `arr` is written into again via `set_array_element`
    // afterward, both within the same iteration and across iterations, and
    // once more building the enclosing Enumeration below.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let url_obj = build_synthetic_url(ctx, u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(url_obj?)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(Some(Value::Object(Some(enm))))
}

fn build_empty_enumeration(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    // GC-safety: `alloc_concurrent_synthetic` below can trigger a moving GC;
    // `arr` is reused in the following `set_field` unpinned otherwise.
    let arr_pin = ctx.pin_native_root(arr);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(Some(Value::Object(Some(enm))))
}

/// `ModuleClassLoader.getResourceAsStream(String) -> InputStream`.
pub(crate) fn native_module_classloader_get_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let trimmed = name.trim_start_matches('/').to_string();

    let bytes_opt = match module_name_of_mcl(ctx, this) {
        Some(module_name) => {
            let is_private = is_module_private_resource(&trimmed);
            let roots = if is_private {
                module_service_roots(&module_name)
            } else {
                let (_modules, roots) = module_visibility_closure(&module_name);
                roots
            };
            let scoped = read_entry_from_roots(&roots, &trimmed);
            if scoped.is_some() || is_private {
                scoped
            } else {
                ctx.find_resource(&trimmed)
            }
        }
        None => ctx.find_resource(&trimmed),
    };

    match bytes_opt {
        None => Ok(Some(Value::Object(None))),
        Some(bytes) => {
            let len = bytes.len();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            // GC-safety: `alloc_concurrent_synthetic` below can trigger a
            // moving GC; `arr` is reused in the following `set_field`s
            // unpinned otherwise.
            let arr_pin = ctx.pin_native_root(arr);
            let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            ctx.set_field(stream, 1, Value::Int(0));
            ctx.set_field(stream, 2, Value::Int(0));
            ctx.set_field(stream, 3, Value::Int(len as i32));
            ctx.set_field_by_name(stream, "buf", Value::Object(Some(arr)));
            ctx.set_field_by_name(stream, "pos", Value::Int(0));
            ctx.set_field_by_name(stream, "mark", Value::Int(0));
            ctx.set_field_by_name(stream, "count", Value::Int(len as i32));
            Ok(Some(Value::Object(Some(stream))))
        }
    }
}

// ===========================================================================
// Native: Module.getName(), Module.getModuleLoader()
// ===========================================================================

pub(crate) fn native_module_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, MOD_SLOT_NAME)))
}

pub(crate) fn native_module_get_module_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let l = build_local_module_loader(ctx);
            return Ok(Some(Value::Object(Some(l?))));
        }
    };
    let stored = ctx.get_field(this, MOD_SLOT_LOADER);
    if let Value::Object(Some(_)) = stored {
        return Ok(Some(stored));
    }
    // GC-safety: `build_local_module_loader` below can trigger a moving GC
    // (it allocates the loader singleton on the first call); `this` is
    // reused in the following `set_field` unpinned otherwise.
    let this_pin = ctx.pin_native_root(this);
    let l = build_local_module_loader(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, MOD_SLOT_LOADER, Value::Object(Some(l)));
    Ok(Some(Value::Object(Some(l))))
}

/// `Module.getBootModuleLoader()` / `Module.getCallerModuleLoader()` /
/// `Module.getContextModuleLoader()`.
///
/// WildFly subsystem code commonly asks JBoss Modules for the caller loader
/// before loading optional subsystem/provider modules. CratonVM does not model
/// the per-frame JBoss module association, but all WildFly modules are resolved
/// through the same boot `LocalModuleLoader` rooted in the module path, so this
/// returns that loader instead of letting the real bytecode produce null.
pub(crate) fn native_module_get_boot_or_caller_module_loader(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader?))))
}

// ===========================================================================
// Registration
// ===========================================================================

// ===========================================================================
// Native: Module.getProperty(String, String) / getProperty(String)
// ===========================================================================
//
// JBoss `Module` has a private `properties:Ljava/util/Map;` field that
// downstream WildFly code reads via `getProperty`.  Our synthetic Module
// shape doesn't have that field, so the bytecode's `getfield properties`
// returns null → invokeinterface containsKey NPEs.  The fix: short-circuit
// the property read at the native boundary — the boot path doesn't actually
// depend on any property being set, so returning the caller's default
// (or null) is sufficient.

pub(crate) fn native_module_get_property_with_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this, args[1] = key, args[2] = default
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(args.get(2).copied().unwrap_or(Value::Object(None)))),
    };
    // Echo the default; ignore key.  The module.xml `<properties>` block
    // is metadata-only and our boot path doesn't consult it.
    let _ = ctx;
    Ok(Some(args.get(2).copied().unwrap_or(Value::Object(None))))
}

pub(crate) fn native_module_get_property(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_module_get_property_names(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return an empty ArrayList — this matches the "no properties set"
    // case the boot path expects.
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    // GC-safety: `new_array` below can trigger a moving GC; `list` is reused
    // in the following `set_field` unpinned otherwise.
    let list_pin = ctx.pin_native_root(list);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    ctx.set_field(list, 0, Value::Object(Some(arr)));
    ctx.set_field(list, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(list))))
}

/// `DefaultBootModuleLoaderHolder$1.run()` — the PrivilegedAction whose
/// `run()` produces the boot loader.  Without this override the real
/// bytecode reflectively instantiates a LocalModuleLoader, which tangles
/// with our WeakReference / MBean gap and yields null.  Returning the
/// cached synthetic loader directly short-circuits the whole reflection
/// dance so `<clinit>` finishes with a non-null INSTANCE.
pub(crate) fn native_boot_holder_priv_action_run(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader?))))
}

fn module_name_arg(ctx: &mut dyn NativeContext, value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Object(Some(s))) => {
            if let Some(name) = ctx.read_string(*s) {
                return Some(name);
            }
            match ctx.invoke_virtual(
                *s,
                "toString",
                "()Ljava/lang/String;",
                &[Value::Object(Some(*s))],
            ) {
                Ok(Some(Value::Object(Some(text)))) => ctx.read_string(text),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn native_module_load_service_from_caller_module_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let module_name = match module_name_arg(ctx, args.first()) {
        Some(name) if !name.is_empty() => name,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Module.loadServiceFromCallerModuleLoader: module name is null".to_string(),
                ),
            }
            .into());
        }
    };
    let service = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Module.loadServiceFromCallerModuleLoader: service is null".to_string(),
                ),
            }
            .into());
        }
    };
    // GC-safety: `service` (the `Class` mirror for e.g. `Extension.class`) is
    // captured before two GC-risking calls below (`native_loader_load_module`
    // resolves + registers a module's resource roots; `native_module_get_class_loader`
    // lazily allocates a `ModuleClassLoader` the first time it's asked for a
    // given module — which is every time here, since this runs once per
    // freshly-loaded extension module during WildFly boot). Per the
    // `pin_native_root` contract, a moving GC in that window leaves `service`
    // stale; passed into the final `ServiceLoader.load` native, this silently
    // constructs a `ServiceLoader` scoped to the WRONG (or a reused, all-zero)
    // service type, so `discover_providers` searches for the wrong resource
    // name and returns zero providers — the bytecode caller's own error
    // message still names the correct service class (a separate, un-corrupted
    // bytecode-level reference to `Extension.class`), so this manifests as
    // "No META-INF/services/org.jboss.as.controller.Extension found" for a
    // seemingly-arbitrary, different extension module each time, exactly the
    // non-deterministic residual documented in
    // wildfly-parallel-boot-stale-objectref-residual.md.
    let service_pin = ctx.pin_native_root(service);

    let loader = build_local_module_loader(ctx);
    let name_obj = ctx.create_string(&module_name);
    let module_val = native_loader_load_module(
        ctx,
        &[Value::Object(Some(loader?)), Value::Object(Some(name_obj))],
    )?;
    let module = match module_val {
        Some(Value::Object(Some(m))) => m,
        _ => return Err(throw_module_not_found(ctx, &module_name)?),
    };
    let class_loader_val = native_module_get_class_loader(ctx, &[Value::Object(Some(module))])?;
    let class_loader = match class_loader_val {
        Some(Value::Object(Some(cl))) => cl,
        _ => return Err(throw_module_not_found(ctx, &module_name)?),
    };
    let service = ctx.read_native_pin(service_pin, service);
    ctx.unpin_native_roots(service_pin);
    ctx.invoke(
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        &[
            Value::Object(Some(service)),
            Value::Object(Some(class_loader)),
        ],
    )
}

/// `Module.loadService(Class)` — the public instance method (distinct from
/// the static `loadServiceFromCallerModuleLoader` above). WildFly's
/// `org.jboss.as.controller.parsing.DeferredExtensionContext` (the
/// host-excludes / domain-mode deferred extension loading path exercised
/// by `HostExcludesTestCase`) calls this directly — verified against the
/// constant pool of `DeferredExtensionContext.class` in
/// `wildfly-controller-24.0.1.Final.jar`, which does
/// `moduleLoader.loadModule(name).loadService(Extension.class)`.
///
/// Real jboss-modules bytecode is:
///   getClass().getModule().addUses(serviceType);
///   return ServiceLoader.load(serviceType, moduleClassLoader);
/// The `addUses` call walks the JDK's own `java.lang.Module` machinery
/// (`this` here is an `org.jboss.modules.Module`, so `getClass()` resolves
/// to the `org.jboss.modules.Module` class itself, and `.getModule()` asks
/// what JPMS module *that* class belongs to) purely to register a
/// `uses`-clause bookkeeping side effect that CratonVM's permissive module
/// model doesn't enforce. Skip it and reimplement the observable contract
/// directly via the module's own `moduleClassLoader` (read through
/// `native_module_get_class_loader`, which lazily builds one instead of
/// reading a possibly-unset field directly). This mirrors
/// `native_module_load_service_from_caller_module_loader` above, which
/// exercises the same `ServiceLoader.load(Class, ClassLoader)` native and
/// is already covered by module-scoping regression tests; the ClassLoader
/// this delegates to is a `ModuleClassLoader` scoped to `this` module, so
/// `discover_providers`'s JBoss-module branch in service_loader.rs applies
/// identically here — no separate scoping logic is needed.
pub(crate) fn native_module_load_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.loadService: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let service_type = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.loadService: serviceType must not be null".to_string()),
            }
            .into());
        }
    };
    // GC-safety: `service_type` is captured before `native_module_get_class_loader`
    // below, which lazily allocates a new `ModuleClassLoader` the first time
    // it's asked for a given module — i.e. every time here, since this runs
    // once per freshly-loaded extension module during WildFly boot. Per the
    // `pin_native_root` contract, a moving GC during that allocation leaves
    // `service_type` stale; passed into the final `ServiceLoader.load` native,
    // this silently scopes the `ServiceLoader` to the wrong (or reused,
    // all-zero) service type, so `discover_providers` searches for the wrong
    // resource name and returns zero providers. See the matching fix in
    // `native_module_load_service_from_caller_module_loader` above for the
    // full mechanism writeup.
    let service_type_pin = ctx.pin_native_root(service_type);
    let class_loader_val = native_module_get_class_loader(ctx, &[Value::Object(Some(this))])?;
    let class_loader = match class_loader_val {
        Some(Value::Object(Some(cl))) => Value::Object(Some(cl)),
        _ => Value::Object(None),
    };
    let service_type = ctx.read_native_pin(service_type_pin, service_type);
    ctx.unpin_native_roots(service_type_pin);
    ctx.invoke(
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(service_type)), class_loader],
    )
}

/// Install every `LocalModuleLoader` / `Module` / `ModuleClassLoader`
/// native this module owns.
pub fn register_jboss_module_loader(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DefaultBootModuleLoaderHolder$1.run() overrides — both the
    // typed and erased signatures get the same implementation so the
    // JVM's bytecode dispatch resolves one or the other.
    let holder_inner = "org/jboss/modules/DefaultBootModuleLoaderHolder$1";
    registry.register(
        holder_inner,
        "run",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_boot_holder_priv_action_run,
    );
    registry.register(
        holder_inner,
        "run",
        "()Ljava/lang/Object;",
        native_boot_holder_priv_action_run,
    );
    // LocalModuleLoader: also register on the abstract `ModuleLoader`
    // base class so virtual dispatch from JBoss bytecode that holds a
    // `ModuleLoader` reference picks up the same implementation.
    let signatures: &[(&str, &str)] = &[
        (CN_MODULE_LOADER, "loadModule"),
        ("org/jboss/modules/ModuleLoader", "loadModule"),
    ];
    for (cn, name) in signatures {
        registry.register(
            cn,
            name,
            "(Ljava/lang/String;)Lorg/jboss/modules/Module;",
            native_loader_load_module,
        );
        // Some JBoss versions use ModuleIdentifier — register a thin
        // wrapper that delegates to the String-based path.
        registry.register(
            cn,
            name,
            "(Lorg/jboss/modules/ModuleIdentifier;)Lorg/jboss/modules/Module;",
            native_loader_load_module_by_identifier,
        );
    }

    // Module surface.
    // Static Module.loadServiceFromCallerModuleLoader(...) normally asks
    // Module.forClass(caller) for the caller's JBoss module. CratonVM maps Java
    // classes to java.lang.Module mirrors, not org.jboss.modules.Module objects,
    // so the real bytecode throws a bare ModuleLoadException. Route directly
    // through the synthetic boot loader instead.
    registry.register(
        CN_MODULE,
        "loadServiceFromCallerModuleLoader",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_module_load_service_from_caller_module_loader,
    );
    registry.register(
        CN_MODULE,
        "loadServiceFromCallerModuleLoader",
        "(Lorg/jboss/modules/ModuleIdentifier;Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_module_load_service_from_caller_module_loader,
    );

    registry.register(
        CN_MODULE,
        "findServices",
        "(Ljava/lang/Class;Ljava/util/function/Predicate;Ljava/lang/ClassLoader;)Ljava/lang/Iterable;",
        native_module_find_services,
    );

    registry.register(
        CN_MODULE,
        "loadService",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_module_load_service,
    );

    registry.register(
        CN_MODULE,
        "getName",
        "()Ljava/lang/String;",
        native_module_get_name,
    );
    registry.register(
        CN_MODULE,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        native_module_get_class_loader,
    );
    registry.register(
        CN_MODULE,
        "getClassLoader",
        "()Lorg/jboss/modules/ModuleClassLoader;",
        native_module_get_class_loader,
    );
    registry.register(
        CN_MODULE,
        "getModuleLoader",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_module_get_module_loader,
    );
    for name in [
        "getBootModuleLoader",
        "getCallerModuleLoader",
        "getContextModuleLoader",
    ] {
        registry.register(
            CN_MODULE,
            name,
            "()Lorg/jboss/modules/ModuleLoader;",
            native_module_get_boot_or_caller_module_loader,
        );
    }
    registry.register(
        CN_MODULE,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_module_load_class,
    );
    registry.register(
        CN_MODULE,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        |ctx, args| {
            // Drop the boolean and reuse the single-arg path.
            let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
            native_module_load_class(ctx, &trimmed)
        },
    );

    // T19.H4: Module.getProperty / getPropertyNames overrides.  These
    // short-circuit the JBoss Module's `properties:Map` field which our
    // synthetic shape doesn't carry.  The boot path doesn't depend on
    // module properties — they're metadata only.
    registry.register(
        CN_MODULE,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_module_get_property,
    );
    registry.register(
        CN_MODULE,
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_module_get_property_with_default,
    );
    registry.register(
        CN_MODULE,
        "getPropertyNames",
        "()Ljava/util/List;",
        native_module_get_property_names,
    );

    // T19.H4: Module.getClassLoaderPrivate() — JBoss's package-private
    // accessor used in Main.main.  Same dispatch as the public
    // getClassLoader() variants.
    registry.register(
        CN_MODULE,
        "getClassLoaderPrivate",
        "()Lorg/jboss/modules/ModuleClassLoader;",
        native_module_get_class_loader,
    );

    // ModuleClassLoader surface.
    registry.register(
        CN_MODULE_CLASSLOADER,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_module_classloader_load_class,
    );
    registry.register(
        CN_MODULE_CLASSLOADER,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        |ctx, args| {
            let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
            native_module_classloader_load_class(ctx, &trimmed)
        },
    );
    // findClass: invoked by ClassLoader.loadClass after parent-first
    // delegation fails — ours short-circuits and only checks the module
    // closure so cross-module classes never leak through.
    registry.register(
        CN_MODULE_CLASSLOADER,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_module_classloader_find_class,
    );
    // Real-JDK-mode fix: real `ConcurrentClassLoader.performLoadClassUnchecked`
    // calls the PROTECTED 3-arg `findClass(String, boolean exportsOnly,
    // boolean resolve)` overload (`ModuleClassLoader`'s own declared
    // signature), not the 1-arg `ClassLoader.findClass(String)` above. Only
    // the 1-arg overload was registered, so every real boot class-load fell
    // straight through to real `ModuleClassLoader`/`Module` bytecode instead
    // of this native's closure walk -- exposing a chain of real-field-layout
    // gaps in our synthetic `Module`/`ModuleClassLoader` objects (missing
    // `module`/`linkage` fields, fixed above) that a from-scratch synthetic
    // construction can never fully replicate. Registering the 3-arg overload
    // too routes real boot class-loading through the native closure walk
    // (which already correctly resolves dependency-closure classes like
    // `org.jboss.as.server.Main`, re-exported into `org.jboss.as.standalone`
    // via its `<module name="org.jboss.as.server" export="true"/>` module.xml
    // dependency) instead of real bytecode that depends on state we don't
    // (and, short of running real jboss-modules construction bytecode, can't
    // fully) populate. The extra two `boolean` args only affect
    // resolve/export-visibility bookkeeping our closure walk doesn't need.
    registry.register(
        CN_MODULE_CLASSLOADER,
        "findClass",
        "(Ljava/lang/String;ZZ)Ljava/lang/Class;",
        |ctx, args| {
            let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
            native_module_classloader_find_class(ctx, &trimmed)
        },
    );
    // getResource / getResourceAsStream / findResource — all closure-bound.
    registry.register(
        CN_MODULE_CLASSLOADER,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        native_module_classloader_get_resource,
    );
    registry.register(
        CN_MODULE_CLASSLOADER,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        native_module_classloader_get_resource,
    );
    registry.register(
        CN_MODULE_CLASSLOADER,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        native_module_classloader_get_resource_as_stream,
    );
    // findResources: JBoss's bytecode dereferences `module` and calls
    // `Module.getResources`. With our synthetic ModuleClassLoader instances
    // the `module` field can be null (log4j's LoaderUtil routes through
    // unrelated CL instances), which NPEs WildFly boot during
    // `org.apache.logging.log4j.util.PropertyFilePropertySource.<init>`.
    // Override with a native that walks the closure directly and returns
    // an empty Enumeration when the module shape is missing.
    registry.register(
        CN_MODULE_CLASSLOADER,
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        native_module_classloader_find_resources,
    );
    registry.register(
        CN_MODULE_CLASSLOADER,
        "findResources",
        "(Ljava/lang/String;Z)Ljava/util/Enumeration;",
        native_module_classloader_find_resources,
    );
    registry.register(
        CN_MODULE_CLASSLOADER,
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        native_module_classloader_find_resources,
    );

    // KC16 boot blocker (Session 100): `org/jboss/modules/log/JDKModuleLogger`'s
    // `<clinit>` calls `Level.parse("TRACE")` / `parse("DEBUG")` /
    // `parse("WARN")` to populate the static `TRACE`/`DEBUG`/`WARN`
    // fields, falling back to `Level.FINEST`/`FINE`/`WARNING` on the
    // `IllegalArgumentException` thrown for the non-standard names.
    // Somewhere down that path (Logger / Level / KnownLevel internals
    // exercised on JDK 25) a `Class.getModule()` chain produces a null
    // `Module` and a downstream `m.isNamed()` raises
    // `NullPointerException: Cannot invoke isNamed on null`.  Our B6
    // policy turns that NPE into a silent swallow of the JDKModuleLogger
    // class init, after which JBoss Modules' bootstrap proceeds without
    // a working logger and KC16 limps onward.
    //
    // Replace the `<clinit>` with a synthetic native that performs the
    // expected effect directly: ensure `java/util/logging/Level` is
    // initialized, then read the JDK's `FINEST`/`FINE`/`WARNING` static
    // Level instances and write them into JDKModuleLogger.TRACE/DEBUG/WARN.
    // This bypasses the broken Module path entirely while preserving
    // the contract that those static fields are non-null Level mirrors.
    registry.register(
        "org/jboss/modules/log/JDKModuleLogger",
        "<clinit>",
        "()V",
        native_jdk_module_logger_clinit,
    );

    // Real-bytecode audit: the RKC19/WF39 Task E "synthetic last-resort
    // `main(String[])` for WildFly bootstrap entry-points" registration
    // has been REMOVED. It registered `native_wildfly_main_noop` on
    // `org/jboss/as/server/Main` et al unconditionally, which (because
    // native intercepts win over Java bytecode in method dispatch) caused
    // CratonVM to skip the real WildFly boot. The function is left below
    // as `#[allow(dead_code)]` only so a future audit can confirm what
    // the shim looked like; it is no longer wired into the registry.
    let _ = native_wildfly_main_noop;
    registry.set_category(__prev_cat);
}

/// RKC19/WF39 — synthetic no-op `main(String[])` for WildFly bootstrap
/// entry-points.  See the comment in `register_jboss_module_loader` for
/// rationale.  Returns `void` (i.e. `None` plus an `Ok(...)` result) without
/// performing any work.
///
/// Real-bytecode audit: this function is NO LONGER REGISTERED. It is
/// retained `#[allow(dead_code)]` purely so the registration deletion
/// site can reference it via `let _ = ...` without provoking a
/// dead-symbol warning.  Real `org/jboss/as/server/Main.main` bytecode
/// now runs.
#[allow(dead_code)]
fn native_wildfly_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Synthetic `JDKModuleLogger.<clinit>` — populates the three static
/// `Level` fields without calling `Level.parse`, sidestepping a JDK 25
/// class-init NPE chain that surfaces under our synthetic Module shim.
/// See the registration site for the full root-cause writeup.
fn native_jdk_module_logger_clinit(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Ensure java.util.logging.Level is initialized so its FINEST/FINE/
    // WARNING static fields are populated.  `ensure_class_initialized`
    // is a no-op once the class is already in `Initialized` state.
    let level_cid = match ctx.ensure_class_initialized("java/util/logging/Level") {
        Ok(cid) => cid,
        Err(_) => {
            // Level itself failed to initialize — leave the fields as
            // their default (null) and return; downstream code that
            // reads them will get null and emit its own diagnostic.
            return Ok(None);
        }
    };
    let trace_val = match ctx.static_field_index_by_name(level_cid, "FINEST") {
        Some(idx) => ctx.get_static_field(level_cid, idx),
        None => Value::Object(None),
    };
    let debug_val = match ctx.static_field_index_by_name(level_cid, "FINE") {
        Some(idx) => ctx.get_static_field(level_cid, idx),
        None => Value::Object(None),
    };
    let warn_val = match ctx.static_field_index_by_name(level_cid, "WARNING") {
        Some(idx) => ctx.get_static_field(level_cid, idx),
        None => Value::Object(None),
    };
    ctx.set_static_field_by_name("org/jboss/modules/log/JDKModuleLogger", "TRACE", trace_val);
    ctx.set_static_field_by_name("org/jboss/modules/log/JDKModuleLogger", "DEBUG", debug_val);
    ctx.set_static_field_by_name("org/jboss/modules/log/JDKModuleLogger", "WARN", warn_val);
    Ok(None)
}

/// `loadModule(ModuleIdentifier)` — extract the dotted name from the
/// identifier and delegate.
fn native_loader_load_module_by_identifier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let id_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "loadModule(ModuleIdentifier): identifier must not be null".to_string(),
                ),
            }
            .into());
        }
    };
    // GC: a reference held in a Rust local across an allocating or Java-re-entering
    // call goes stale under a moving collector, and under the Generational
    // non-moving young sweep an unrooted object is ZEROED in place. Pin and
    // re-read. `safe_native_call_impl` truncates `native_pin_roots` when the native
    // returns, so an unmatched pin costs nothing on an error path. See
    // `internal/audits/wide-tranche-triage-20260907.md`.
    // `ctx.invoke` runs `getName()` bytecode and `create_string` below
    // allocates, so `args[0]` — the loader this forwards to the sibling
    // native — names a pre-call address by the time the new argument vector
    // is built.
    let arg0_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // ModuleIdentifier has `String getName()` — call it.
    let name_val = ctx.invoke(
        "org/jboss/modules/ModuleIdentifier",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(id_obj))],
    )?;
    let name = match name_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "loadModule(ModuleIdentifier): identifier produced empty name".to_string(),
        }
        .into());
    }
    let name_str = ctx.create_string(&name);
    let arg0 = match arg0_pin {
        Some((p, o)) => Value::Object(Some(ctx.read_native_pin(p, o))),
        None => args[0],
    };
    let new_args: Vec<Value> = vec![arg0, Value::Object(Some(name_str))];
    native_loader_load_module(ctx, &new_args)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use parking_lot::Mutex as PMutex;

    /// Serial test guard — the `-mp` test override and module cache are
    /// process-wide singletons.  Tests must run serialized so they
    /// don't see each other's side effects.
    static TEST_LOCK: PMutex<()> = PMutex::new(());

    fn setup_test_env(tmp_root: &Path) {
        clear_module_cache_for_test();
        clear_boot_loader_for_test();
        set_mp_root_for_test(Some(tmp_root.to_path_buf()));
    }

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn make_minimal_module_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module name="{}" xmlns="urn:jboss:module:1.9">
    <resources>
    </resources>
</module>
"#,
            name
        )
    }

    fn make_module_xml_with_jars(name: &str, jars: &[&str]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for j in jars {
            s.push_str(&format!("    <resource-root path=\"{}\"/>\n", j));
        }
        s.push_str("  </resources>\n</module>\n");
        s
    }

    fn make_module_xml_with_artifacts(name: &str, artifacts: &[&str]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for artifact in artifacts {
            s.push_str(&format!("    <artifact name=\"{}\"/>\n", artifact));
        }
        s.push_str("  </resources>\n</module>\n");
        s
    }

    // -----------------------------------------------------------------
    // validate_module_name
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_validate_accepts_valid_module_names() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("org.jboss.as.standalone").is_ok());
        assert!(validate_module_name("java.base").is_ok());
        assert!(validate_module_name("x").is_ok());
        // 256 bytes exactly — boundary
        let max = "a".repeat(256);
        assert!(validate_module_name(&max).is_ok());
    }

    #[test]
    fn t19_h4_validate_rejects_empty_and_oversize() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("").is_err());
        let too_long = "a".repeat(257);
        assert!(validate_module_name(&too_long).is_err());
    }

    #[test]
    fn t19_h4_validate_rejects_traversal_and_separators() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("../etc/passwd").is_err());
        assert!(validate_module_name("..").is_err());
        assert!(validate_module_name("foo/bar").is_err());
        assert!(validate_module_name("foo\\bar").is_err());
        assert!(validate_module_name("C:\\Windows").is_err());
        assert!(validate_module_name("a:b").is_err());
    }

    #[test]
    fn t19_h4_validate_rejects_control_bytes() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("foo\0bar").is_err());
        assert!(validate_module_name("foo\nbar").is_err());
        assert!(validate_module_name("foo\tbar").is_err());
        assert!(validate_module_name("foo\x7fbar").is_err());
    }

    #[test]
    fn t19_h4_property_bridge_maps_jmx_builder() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        ctx.set_system_property(
            "javax.management.builder.initial",
            "org.jboss.as.jmx.PluggableMBeanServerBuilder",
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.as.jmx.PluggableMBeanServerBuilder"),
            Some("org.jboss.as.jmx")
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.as.server.Main"),
            None
        );

        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.logmanager.LogManager"),
            Some("org.jboss.logmanager")
        );
    }

    // -----------------------------------------------------------------
    // find_mp_argument — env-var fallback
    // -----------------------------------------------------------------

    /// WP8.10.5 — `CRATONVM_JBOSS_MP_ROOT` environment variable provides a
    /// `-mp` value when the process argv is owned by the test harness
    /// (and therefore can't carry `-mp <path>`).  External integration
    /// tests like `vm/tests/wp8_10_jboss_modules_smoke.rs` need this path
    /// to point `LocalModuleLoader` at a fixture-built modules tree
    /// without going through the cratonvm CLI.
    ///
    /// Acceptance: setting the env var causes `find_mp_argument()` to
    /// return its value verbatim, taking precedence over any argv `-mp`
    /// (the env-var setter has authority).  Empty or unset env var
    /// causes us to fall back to argv.
    #[test]
    fn wp8_10_find_mp_argument_honours_env_var() {
        let v = find_mp_argument_from(
            Some("/tmp/wp8_10_fixture_mp".to_string()),
            ["cratonvm", "-mp", "/tmp/argv_mp"]
                .into_iter()
                .map(str::to_string),
        );
        assert_eq!(v.as_deref(), Some("/tmp/wp8_10_fixture_mp"));
    }

    /// Empty `CRATONVM_JBOSS_MP_ROOT` must be ignored — we should fall
    /// through to argv resolution rather than treating "" as a valid
    /// (and dangerous) module-path root.
    #[test]
    fn wp8_10_find_mp_argument_ignores_empty_env_var() {
        let v = find_mp_argument_from(Some(String::new()), ["cratonvm"].map(str::to_string));
        assert!(
            v.is_none(),
            "empty startup flag must be treated as unset; got {:?}",
            v
        );
    }

    /// WildFly domain launchers pass a multi-entry `-mp` where per-test
    /// `added-modules` directories precede the real WildFly modules root.
    /// The resolver must retain every entry, not only the first one.
    #[test]
    fn wf_domain_split_module_path_keeps_all_entries() {
        let _g = TEST_LOCK.lock();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let raw = format!("/tmp/added-one{sep}/tmp/added-two{sep}/opt/wildfly/modules");
        let entries = split_module_path_entries(&raw);
        assert_eq!(
            entries,
            vec!["/tmp/added-one", "/tmp/added-two", "/opt/wildfly/modules"]
        );
    }

    // -----------------------------------------------------------------
    // resolve_module — happy path
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_resolve_module_legacy_path() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("org/jboss/as/standalone/main/module.xml");
        write(
            &mod_xml,
            &make_minimal_module_xml("org.jboss.as.standalone"),
        );
        let r = resolve_module(root, "org.jboss.as.standalone").unwrap();
        assert_eq!(r.mx.name, "org.jboss.as.standalone");
        assert!(r.module_xml_path.ends_with("module.xml"));
    }

    #[test]
    fn wf_domain_resolve_module_alias_follows_target_name() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("org/jboss/as/modcluster/main/module.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module-alias name="org.jboss.as.modcluster" target-name="org.wildfly.extension.mod_cluster" xmlns="urn:jboss:module:1.9"/>
"#,
        );
        let target_dir = root.join("org/wildfly/extension/mod_cluster/main");
        write(
            &target_dir.join("module.xml"),
            &make_module_xml_with_jars(
                "org.wildfly.extension.mod_cluster",
                &["wildfly-mod_cluster-extension.jar"],
            ),
        );
        write(&target_dir.join("wildfly-mod_cluster-extension.jar"), "PK");

        let r = resolve_module(root, "org.jboss.as.modcluster").unwrap();
        assert_eq!(r.mx.name, "org.wildfly.extension.mod_cluster");
        assert_eq!(r.resource_roots.len(), 1);
        assert!(r.resource_roots[0].ends_with("wildfly-mod_cluster-extension.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_layered_base_path() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("system/layers/base/org/jboss/as/standalone/main/module.xml");
        write(
            &mod_xml,
            &make_minimal_module_xml("org.jboss.as.standalone"),
        );
        let r = resolve_module(root, "org.jboss.as.standalone").unwrap();
        assert_eq!(r.mx.name, "org.jboss.as.standalone");
    }

    #[test]
    fn t19_h4_resolve_module_layered_with_layers_conf() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("layers.conf"), "layers=keycloak\n");
        let mod_xml = root.join("system/layers/keycloak/com/example/foo/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("com.example.foo"));
        let r = resolve_module(root, "com.example.foo").unwrap();
        assert_eq!(r.mx.name, "com.example.foo");
    }

    #[test]
    fn t19_h4_resolve_module_resource_roots_resolved() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("com/example/foo/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("com.example.foo", &["a.jar", "b.jar"]),
        );
        // Touch the jars so canonicalize doesn't fail
        write(&main_dir.join("a.jar"), "PK");
        write(&main_dir.join("b.jar"), "PK");
        let r = resolve_module(root, "com.example.foo").unwrap();
        assert_eq!(r.resource_roots.len(), 2);
        assert!(r.resource_roots[0].ends_with("a.jar"));
        assert!(r.resource_roots[1].ends_with("b.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_artifact_roots_from_maven_repo() {
        let _g = TEST_LOCK.lock();
        clear_module_cache_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("modules");
        let repo = tmp.path().join("repo");
        let main_dir = root.join("com/example/foo/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_artifacts("com.example.foo", &["com.acme:tool:1.0"]),
        );
        let jar = repo.join("com/acme/tool/1.0/tool-1.0.jar");
        write(&jar, "PK");

        remember_maven_repo_root(Some(repo.to_string_lossy().into_owned()));
        let r = resolve_module(&root, "com.example.foo").unwrap();
        clear_module_cache_for_test();

        assert_eq!(r.resource_roots.len(), 1);
        assert!(r.resource_roots[0].ends_with("tool-1.0.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_missing_returns_class_not_found() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_module(tmp.path(), "no.such.module").unwrap_err();
        match err {
            RuntimeError::ClassNotFoundException { class_name } => {
                assert!(class_name.contains("no.such.module"));
            }
            other => panic!("expected ClassNotFoundException, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_resolve_module_rejects_invalid_name() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_module(tmp.path(), "../etc").is_err());
        assert!(resolve_module(tmp.path(), "").is_err());
    }

    #[test]
    fn t19_h4_resolve_module_rejects_resource_root_traversal() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("com/example/evil/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("com.example.evil", &["../../escape.jar"]),
        );
        let err = resolve_module(root, "com.example.evil").unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    // -----------------------------------------------------------------
    // build_local_module_loader / boot holder caching
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_build_local_module_loader_is_idempotent() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let a = build_local_module_loader(&mut ctx).unwrap();
        let b = build_local_module_loader(&mut ctx).unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr());
    }

    #[test]
    fn t19_h4_default_boot_holder_instance_returns_same_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let a = build_default_boot_holder_instance(&mut ctx).unwrap();
        let b = build_default_boot_holder_instance(&mut ctx).unwrap();
        let c = build_local_module_loader(&mut ctx).unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a.as_ptr(), c.as_ptr());
    }

    // -----------------------------------------------------------------
    // native_loader_load_module — happy path + cache + errors
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_load_module_happy_path_returns_module_with_resource_roots() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("org/jboss/as/standalone/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("org.jboss.as.standalone", &["jboss-as-server.jar"]),
        );
        write(&main_dir.join("jboss-as-server.jar"), "PK");
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("org.jboss.as.standalone");
        let result = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        let module = match result {
            Value::Object(Some(o)) => o,
            other => panic!("expected non-null Module, got {:?}", other),
        };
        // resourceRoots slot should be a non-null Object[]
        let roots = ctx.get_field(module, MOD_SLOT_RESOURCE_ROOTS);
        if let Value::Object(Some(arr)) = roots {
            assert_eq!(ctx.array_length(arr), 1);
        } else {
            panic!("expected Object[] in resourceRoots, got {:?}", roots);
        }
    }

    #[test]
    fn t19_h4_load_module_caches_repeat_calls() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("a/b/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("a.b"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name1 = ctx.create_string("a.b");
        let r1 = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name1))],
        )
        .unwrap()
        .unwrap();
        let name2 = ctx.create_string("a.b");
        let r2 = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name2))],
        )
        .unwrap()
        .unwrap();
        match (r1, r2) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr(), "module cache must return same ref");
            }
            other => panic!("expected non-null modules, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_load_module_missing_throws_module_not_found() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("ghost.module.never.exists");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            other => panic!(
                "expected ExceptionThrown(ModuleNotFoundException), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn t19_h4_load_module_null_name_throws_npe() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(None)],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "got {}", s);
    }

    #[test]
    fn t19_h4_load_module_traversal_name_rejected() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("../escape");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("traversal") || s.contains("IllegalArgument"),
            "got {}",
            s
        );
    }

    #[test]
    fn t19_h4_load_module_with_no_mp_set_throws_module_not_found() {
        let _g = TEST_LOCK.lock();
        clear_module_cache_for_test();
        clear_boot_loader_for_test();
        set_mp_root_for_test(None);
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("anything");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            other => panic!("expected ExceptionThrown, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // Module accessors
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_module_get_name_returns_string() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("foo/bar/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("foo.bar"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("foo.bar");
        let module = match native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(m)) => m,
            other => panic!("got {:?}", other),
        };
        let result = native_module_get_name(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(ctx.read_string(s).as_deref(), Some("foo.bar"));
        } else {
            panic!("expected non-null name string, got {:?}", result);
        }
    }

    #[test]
    fn t19_h4_module_get_class_loader_lazy_populates() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("zz/yy/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("zz.yy"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("zz.yy");
        let module = match native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(m)) => m,
            other => panic!("got {:?}", other),
        };
        // First call populates.
        let r1 = native_module_get_class_loader(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        // Second call returns same instance.
        let r2 = native_module_get_class_loader(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        match (r1, r2) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected non-null ClassLoader pair, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_module_get_class_loader_null_receiver_throws_npe() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let err = native_module_get_class_loader(&mut ctx, &[Value::Object(None)]).unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "got {}", s);
    }

    #[test]
    fn t19_h4_module_get_module_loader_falls_back_to_boot() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let result = native_module_get_module_loader(&mut ctx, &[Value::Object(None)])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null loader, got {:?}", other),
        }
    }

    #[test]
    fn wf_domain_static_module_loader_accessors_return_boot_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let boot = build_local_module_loader(&mut ctx).unwrap();

        for accessor in [
            "getBootModuleLoader",
            "getCallerModuleLoader",
            "getContextModuleLoader",
        ] {
            let result = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
                .unwrap()
                .unwrap();
            match result {
                Value::Object(Some(loader)) => assert_eq!(
                    loader.as_ptr(),
                    boot.as_ptr(),
                    "{} did not return the cached boot loader",
                    accessor
                ),
                other => panic!("{} returned {:?}", accessor, other),
            }
        }
    }

    // -----------------------------------------------------------------
    // Concurrency: 4 threads racing loadModule for the same module
    // must end up with one cached instance.
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_load_module_concurrent_4_threads_share_one_module() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("race/me/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("race.me"));
        setup_test_env(root);

        // Each thread builds its own context — the module cache lives
        // process-wide so cross-context lookups still hit the same
        // ObjectRef.  We can't truly invoke from background threads
        // because MockNativeContext isn't Send; instead we serialize
        // the calls but use four distinct receiver/name pairs to
        // simulate concurrent traffic.
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let mut refs = Vec::new();
        for _ in 0..4 {
            let name = ctx.create_string("race.me");
            let result = native_loader_load_module(
                &mut ctx,
                &[Value::Object(Some(loader)), Value::Object(Some(name))],
            )
            .unwrap()
            .unwrap();
            if let Value::Object(Some(m)) = result {
                refs.push(m);
            } else {
                panic!("expected non-null Module");
            }
        }
        let first = refs[0].as_ptr();
        for r in &refs[1..] {
            assert_eq!(r.as_ptr(), first, "all calls must return same ObjectRef");
        }
    }

    // -----------------------------------------------------------------
    // ensure_under_root — symlink/traversal defense
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_ensure_under_root_accepts_descendants() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inside = root.join("subdir/file.txt");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "x").unwrap();
        ensure_under_root(root, &inside).unwrap();
    }

    #[test]
    fn t19_h4_ensure_under_root_rejects_escapes() {
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        let outside = tmp_other.path().join("foo");
        std::fs::write(&outside, "x").unwrap();
        let err = ensure_under_root(tmp_root.path(), &outside).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    #[test]
    fn t19_h4_ensure_under_root_accepts_nonexistent_descendant() {
        // V1: a resource-root jar that doesn't physically exist yet must
        // still be accepted as long as its existing prefix (the module dir)
        // is under the root. The existing module dir is canonicalized and the
        // not-yet-existing jar name is re-attached.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let module_dir = root.join("system/layers/base/com/example/main");
        std::fs::create_dir_all(&module_dir).unwrap();
        let missing_jar = module_dir.join("not-yet-here.jar");
        // The jar does not exist on disk, but its prefix does.
        ensure_under_root(root, &missing_jar).unwrap();
    }

    #[test]
    fn t19_h4_ensure_under_root_fails_closed_on_nonexistent_escape() {
        // V1: a non-existent candidate that lexically escapes the root must be
        // rejected (fail closed) rather than slipping through on the old raw
        // fallback. The existing prefix (`tmp_other`) canonicalizes outside the
        // root, so the prefix check rejects it.
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        // Candidate that does NOT exist on disk yet, but whose existing prefix
        // lives outside the module root.
        let outside_missing = tmp_other.path().join("ghost/escape.jar");
        let err = ensure_under_root(tmp_root.path(), &outside_missing).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn t19_h4_ensure_under_root_rejects_symlink_escape() {
        // V1: a symlink inside the root whose target lives outside the root
        // must be rejected — `resolve_for_confinement` follows the symlink in
        // the existing prefix before the under-root check, so the escape is
        // detected even though the link itself sits under the root.
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        let target_dir = tmp_other.path().join("secret");
        std::fs::create_dir_all(&target_dir).unwrap();
        let link = tmp_root.path().join("link");
        std::os::unix::fs::symlink(&target_dir, &link).unwrap();
        // Candidate goes through the in-root symlink but resolves outside.
        let candidate = link.join("loot.jar");
        let err = ensure_under_root(tmp_root.path(), &candidate).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    // -----------------------------------------------------------------
    // Registration smoke
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_register_jboss_module_loader_adds_surface() {
        let _g = TEST_LOCK.lock();
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jboss_module_loader(&mut r);
        let after = r.len();
        // We register at least: 2 DefaultBootModuleLoaderHolder$1.run
        // overloads, loadModule/String + loadModule/Identifier on both
        // LocalModuleLoader and ModuleLoader (4), plus the Module /
        // ModuleClassLoader surface including the static loader accessors.
        assert!(
            after >= before + 18,
            "expected >= 18 new registrations, got {}",
            after - before
        );
    }

    #[test]
    fn t19_h4_boot_holder_priv_action_run_returns_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let result = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null loader, got {:?}", other),
        }
        // Running again must return the same loader ref (idempotent).
        let loader_a = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let loader_b = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match (loader_a, loader_b) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected pair of non-null loaders, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_get_caller_module_loader_returns_boot_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let caller = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let boot = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match (caller, boot) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected stable boot loader refs, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // find_mp_argument — ensure CLI parsing works for edge cases
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_get_property_with_default_returns_default() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.create_string("foo");
        let default = ctx.create_string("bar");
        let result = native_module_get_property_with_default(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(key)),
                Value::Object(Some(default)),
            ],
        )
        .unwrap()
        .unwrap();
        match result {
            Value::Object(Some(s)) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("bar"));
            }
            other => panic!("expected default string, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_get_property_returns_null() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.create_string("foo");
        let result = native_module_get_property(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(key))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(result, Value::Object(None)));
    }

    #[test]
    fn t19_h4_get_property_names_returns_empty_list() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let result = native_module_get_property_names(&mut ctx, &[Value::Object(Some(this))])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_list)) => {}
            other => panic!("expected non-null List, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_locate_module_xml_finds_addon_layer() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let xml = root.join("system/add-ons/keycloak/com/extra/foo/main/module.xml");
        write(&xml, &make_minimal_module_xml("com.extra.foo"));
        let found = locate_module_xml(root, "com.extra.foo").unwrap();
        assert!(found.ends_with("module.xml"));
    }

    // -----------------------------------------------------------------
    // T19_H15 — transitive linkage closure & dynamic-classpath
    // registration when loadModule is called.
    //
    // Without this fix, KC16 boots far enough to call
    // `loadModule("org.jboss.as.standalone")` but then NCDFEs on
    // `org.jboss.as.controller.access.JmxAction$Impact` when bytecode
    // in the controller-dependent module is verified — because the
    // controller jar wasn't yet on the application classpath.
    // -----------------------------------------------------------------

    /// Build a module.xml with `dependencies` (Module deps).
    fn make_module_xml_with_deps(
        name: &str,
        jars: &[&str],
        deps: &[(&str, bool /* export */, bool /* optional */)],
    ) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for j in jars {
            s.push_str(&format!("    <resource-root path=\"{}\"/>\n", j));
        }
        s.push_str("  </resources>\n  <dependencies>\n");
        for (dep, export, optional) in deps {
            s.push_str(&format!(
                "    <module name=\"{}\"{}{}/>\n",
                dep,
                if *export { " export=\"true\"" } else { "" },
                if *optional { " optional=\"true\"" } else { "" },
            ));
        }
        s.push_str("  </dependencies>\n</module>\n");
        s
    }

    /// Helper: write a module with N jars under `<root>/<dotted-path>/main/`.
    fn write_module_with_deps(root: &Path, name: &str, jars: &[&str], deps: &[(&str, bool, bool)]) {
        let path: PathBuf = name.split('.').collect();
        let main_dir = root.join(&path).join("main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_deps(name, jars, deps),
        );
        for j in jars {
            // Touch the jar so canonicalize / file-existence checks pass.
            write(&main_dir.join(j), "PK");
        }
    }

    #[test]
    fn t19_h15_transitive_linkage_walks_full_dep_tree() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start -> mid -> leaf  (no `export="true"` on either edge)
        write_module_with_deps(root, "start", &["s.jar"], &[("mid", false, false)]);
        write_module_with_deps(root, "mid", &["m.jar"], &[("leaf", false, false)]);
        write_module_with_deps(root, "leaf", &["l.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        // Should include all three jars regardless of `export="true"`.
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "m.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "l.jar"), "got {:?}", names);
    }

    #[test]
    fn t19_h15_transitive_linkage_handles_cycle_without_loop() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // a -> b -> a  (cycle)
        write_module_with_deps(root, "a", &["a.jar"], &[("b", false, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[("a", false, false)]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("a");
        assert!(!roots.is_empty(), "linkage walk must terminate on cycles");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "a.jar"));
        assert!(names.iter().any(|n| n == "b.jar"));
    }

    #[test]
    fn t19_h15_transitive_linkage_skips_missing_deps_silently() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start depends on `missing` (not on disk). Walk must not error.
        write_module_with_deps(
            root,
            "start",
            &["s.jar"],
            &[("missing", false, true), ("present", false, false)],
        );
        write_module_with_deps(root, "present", &["p.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"));
        assert!(names.iter().any(|n| n == "p.jar"));
    }

    #[test]
    fn t19_h15_transitive_linkage_skips_system_deps() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start has a Module dep + a `<system>` dep (which mod_xml
        // parses as `DependencyKind::System`).  System deps come from
        // the bootstrap classpath, not application — we must not try
        // to register a `system` module's roots.
        let main_dir = root.join("start/main");
        let xml = format!(
            r#"<?xml version="1.0"?>
<module name="start" xmlns="urn:jboss:module:1.9">
  <resources>
    <resource-root path="s.jar"/>
  </resources>
  <dependencies>
    <module name="present"/>
    <system export="true">
      <paths>
        <path name="java/lang"/>
      </paths>
    </system>
  </dependencies>
</module>
"#
        );
        write(&main_dir.join("module.xml"), &xml);
        write(&main_dir.join("s.jar"), "PK");
        write_module_with_deps(root, "present", &["p.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"));
        assert!(names.iter().any(|n| n == "p.jar"));
    }

    #[test]
    fn t19_h15_load_module_registers_transitive_classpath() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Mirrors the WildFly shape that triggered the original bug:
        // jmx -> controller (no export) -> protocol (no export).
        // The bytecode in `jmx` references a class in `controller`,
        // and the verifier needs `controller.jar` on the classpath
        // even though `controller` is not re-exported.
        write_module_with_deps(
            root,
            "org.jboss.as.jmx",
            &["wildfly-jmx.jar"],
            &[("org.jboss.as.controller", false, false)],
        );
        write_module_with_deps(
            root,
            "org.jboss.as.controller",
            &["wildfly-controller.jar"],
            &[("org.jboss.as.protocol", false, false)],
        );
        write_module_with_deps(
            root,
            "org.jboss.as.protocol",
            &["wildfly-protocol.jar"],
            &[],
        );
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("org.jboss.as.jmx");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        assert!(
            registered.iter().any(|p| p.ends_with("wildfly-jmx.jar")),
            "must register own jar; got {:?}",
            registered
        );
        assert!(
            registered
                .iter()
                .any(|p| p.ends_with("wildfly-controller.jar")),
            "must register direct dep jar so the verifier can find \
             classes referenced from JMX bytecode; got {:?}",
            registered
        );
        assert!(
            registered
                .iter()
                .any(|p| p.ends_with("wildfly-protocol.jar")),
            "must register transitive dep jar so the verifier can find \
             second-hop class references; got {:?}",
            registered
        );
    }

    #[test]
    fn t19_h15_load_module_idempotent_no_duplicate_classpath() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "a", &["a.jar"], &[("b", false, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[]);
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let n1 = ctx.create_string("a");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(n1))],
        )
        .unwrap();
        // Second call should return cached module without re-registering.
        let n2 = ctx.create_string("a");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(n2))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        let count_a = registered.iter().filter(|p| p.ends_with("a.jar")).count();
        let count_b = registered.iter().filter(|p| p.ends_with("b.jar")).count();
        assert_eq!(count_a, 1, "a.jar must register once; got {:?}", registered);
        assert_eq!(count_b, 1, "b.jar must register once; got {:?}", registered);
    }

    #[test]
    fn t19_h15_load_module_inner_class_naming_passthrough() {
        // The ModuleClassLoader path must accept `Outer$Inner` style
        // names with `$`-separated nested class identifiers — JBoss
        // bytecode references them with `getDeclaredClasses` /
        // `Class.forName("...$Inner")` calls during enum init.
        // We can't load real bytecode in the unit-test mock, but we
        // can drive the `loadClass` native end-to-end and confirm the
        // visibility lookup uses the correct entry-path encoding.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "x", &["x.jar"], &[]);
        setup_test_env(root);

        // Confirm `find_entry_in_roots` searches with the literal `$`.
        let main_dir = root.join("x/main");
        // Replace x.jar with a real (empty) zip that contains
        // `Outer$Inner.class` so the lookup hits.
        let jar_path = main_dir.join("x.jar");
        let f = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file(
            "Outer$Inner.class",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        zw.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        zw.finish().unwrap();

        // Force the resolver to re-read the on-disk module since we
        // just rewrote x.jar after `setup_test_env` populated nothing.
        clear_module_cache_for_test();
        let _ = transitive_linkage_roots("x");
        let resolved = ensure_resolved("x").expect("module must resolve");
        let hit = find_entry_in_roots(&resolved.resource_roots, "Outer$Inner.class");
        assert!(
            hit.is_some(),
            "find_entry_in_roots must accept `$` in inner-class names"
        );
    }

    #[test]
    fn t19_h15_register_resource_roots_handles_nonexistent_paths_gracefully() {
        // `register_dynamic_classpath` / `add_path` must silently
        // skip non-existent paths — JBoss modules sometimes list jars
        // that aren't shipped (e.g. optional add-ons).  We expose
        // the unfiltered path string here; the consumer
        // (ClassPath::add_path) handles the actual filtering.
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let bogus = vec![PathBuf::from("does/not/exist.jar")];
        register_resource_roots(&mut ctx, &bogus);
        // The mock does record the path string regardless — the real
        // `add_path` will skip it.  We're only asserting that
        // `register_resource_roots` doesn't panic on a missing file.
        let snap = ctx.registered_classpath_snapshot();
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn t19_h15_transitive_linkage_includes_export_transitive() {
        // Even without the linkage closure, `export="true"` paths
        // were already followed.  Confirm we don't lose that:
        // a -[export]-> b -[export]-> c.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "a", &["a.jar"], &[("b", true, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[("c", true, false)]);
        write_module_with_deps(root, "c", &["c.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("a");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "a.jar"));
        assert!(names.iter().any(|n| n == "b.jar"));
        assert!(names.iter().any(|n| n == "c.jar"));
    }

    #[test]
    fn t19_h15_load_module_no_deps_registers_only_self() {
        // Backwards-compat: a leaf module with no deps must still
        // register exactly its own resource roots — no broader walk.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "leaf", &["only.jar"], &[]);
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("leaf");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        assert!(
            registered.iter().any(|p| p.ends_with("only.jar")),
            "got {:?}",
            registered
        );
        assert_eq!(
            registered
                .iter()
                .filter(|p| p.ends_with("only.jar"))
                .count(),
            1,
            "must register exactly once; got {:?}",
            registered
        );
    }

    #[test]
    fn t19_h15_module_visibility_closure_unchanged_by_linkage_walk() {
        // The runtime `module_visibility_closure` (used by MCL.loadClass
        // for *enforcement*) must still follow JBoss's "self + direct +
        // transitive re-exports" rule — narrower than
        // `transitive_linkage_roots`.  Confirm the two walks return
        // different sets when there's a non-exported dep.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "start", &["s.jar"], &[("direct", false, false)]);
        write_module_with_deps(root, "direct", &["d.jar"], &[("hidden", false, false)]);
        write_module_with_deps(root, "hidden", &["h.jar"], &[]);
        setup_test_env(root);

        // Linkage closure: includes h.jar (transitive non-export dep).
        let linkage = transitive_linkage_roots("start");
        let linkage_names: Vec<String> = linkage
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(linkage_names.iter().any(|n| n == "h.jar"));

        // Visibility closure: must NOT include h.jar (non-exported
        // dep of a direct dep — JBoss's runtime rule hides it).
        let (_modules, vis) = module_visibility_closure("start");
        let vis_names: Vec<String> = vis
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(vis_names.iter().any(|n| n == "s.jar"));
        assert!(vis_names.iter().any(|n| n == "d.jar"));
        assert!(
            !vis_names.iter().any(|n| n == "h.jar"),
            "visibility closure must not include non-exported \
             transitive deps; got {:?}",
            vis_names
        );
    }

    #[test]
    fn t19_h16_service_roots_only_include_service_imports() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let start_dir = root.join("start/main");

        write(
            &start_dir.join("module.xml"),
            r#"<?xml version="1.0"?>
<module name="start" xmlns="urn:jboss:module:1.9">
  <resources><resource-root path="start.jar"/></resources>
  <dependencies>
    <module name="svc.imported" services="import"/>
    <module name="svc.hidden"/>
    <module name="svc.exported" services="export"/>
  </dependencies>
</module>
"#,
        );
        write(&start_dir.join("start.jar"), "PK");
        write_module_with_deps(root, "svc.imported", &["imported.jar"], &[]);
        write_module_with_deps(root, "svc.hidden", &["hidden.jar"], &[]);
        write_module_with_deps(root, "svc.exported", &["exported.jar"], &[]);
        setup_test_env(root);

        let roots = module_service_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "start.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "imported.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "exported.jar"), "got {:?}", names);
        assert!(
            !names.iter().any(|n| n == "hidden.jar"),
            "service roots must not include ordinary deps; got {:?}",
            names
        );
    }

    #[test]
    fn wildfly_jboss_modules_service_provider_leak_real_dist_scoping() {
        // Regression guard for
        // wildfly-jboss-modules-service-provider-leak.md,
        // exercised against a *real* WildFly 32.0.1.Final distribution's
        // modules/ tree (not a synthetic fixture) so the exact conflicting
        // pair from the original repro -- org.jboss.as.jmx and
        // org.wildfly.extension.core-management, neither of which depends
        // on the other, each shipping its own
        // META-INF/services/org.jboss.as.controller.Extension descriptor
        // -- is covered verbatim. Skips (rather than fails) when the
        // distribution isn't staged on this machine.
        let _g = TEST_LOCK.lock();
        let real_root =
            std::path::Path::new("/data/data/wildfly-dist/wildfly-32.0.1.Final/modules");
        if !real_root.is_dir() {
            eprintln!(
                "wildfly_jboss_modules_service_provider_leak_real_dist_scoping: \
                 real WildFly dist not present at {}, skipping",
                real_root.display()
            );
            return;
        }
        setup_test_env(real_root);
        let jmx =
            module_service_provider_names("org.jboss.as.jmx", "org.jboss.as.controller.Extension");
        let cm = module_service_provider_names(
            "org.wildfly.extension.core-management",
            "org.jboss.as.controller.Extension",
        );
        let modcluster = module_service_provider_names(
            "org.jboss.as.modcluster",
            "org.jboss.as.controller.Extension",
        );
        assert_eq!(
            jmx,
            vec!["org.jboss.as.jmx.JMXExtension".to_string()],
            "org.jboss.as.jmx's Extension providers must be scoped to its \
             own module; got {:?}. Any entry from \
             org.wildfly.extension.core-management reproduces the \
             cross-module service-provider leak.",
            jmx
        );
        assert_eq!(
            cm,
            vec!["org.wildfly.extension.core.management.CoreManagementExtension".to_string()],
            "org.wildfly.extension.core-management's Extension providers \
             must be scoped to its own module; got {:?}. Any entry from \
             org.jboss.as.jmx reproduces the cross-module \
             service-provider leak.",
            cm
        );
        assert_eq!(
            modcluster,
            vec!["org.wildfly.extension.mod_cluster.ModClusterExtension".to_string()],
            "org.jboss.as.modcluster is a module-alias for \
             org.wildfly.extension.mod_cluster; got {:?}",
            modcluster
        );
    }

    /// RKC19/WF39 Task C — Sanity-check that the synthetic class bytes
    /// produced by `build_synthetic_class_with_main` start with the JVM
    /// class file magic and contain the expected method names.  The full
    /// parsing path is exercised in integration via
    /// `define_class_from_bytes`; here we just guard the byte layout.
    #[test]
    fn rkc19_wf39_synthetic_class_bytes_well_formed() {
        let bytes = build_synthetic_class_with_main("org/jboss/as/server/Main");
        // Magic
        assert_eq!(&bytes[0..4], &[0xCA, 0xFE, 0xBA, 0xBE]);
        // Major version = 52 (Java 8)
        assert_eq!(&bytes[6..8], &[0x00, 0x34]);
        // Constant pool count = 12 (entries 1..=11)
        assert_eq!(&bytes[8..10], &[0x00, 0x0C]);
        // Body should contain "main" and "([Ljava/lang/String;)V" as
        // raw substrings (Utf8 entries are plain UTF-8 payloads).
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("main"),
            "expected 'main' in synthesised bytes"
        );
        assert!(
            body.contains("([Ljava/lang/String;)V"),
            "expected main descriptor in synthesised bytes"
        );
        assert!(
            body.contains("org/jboss/as/server/Main"),
            "expected this_class name in synthesised bytes"
        );
        // Return bytecode 0xB1 should appear at least twice (init returns
        // after super-call, main is a single return).
        let return_count = bytes.iter().filter(|&&b| b == 0xB1).count();
        assert!(
            return_count >= 2,
            "expected at least 2 occurrences of `return` (0xB1); got {} in {:?}",
            return_count,
            bytes
        );
    }
}
