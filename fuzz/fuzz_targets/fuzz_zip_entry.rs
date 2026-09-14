// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for **JAR / ZIP archive ingestion**: central directory and
//! local-header parsing, entry-name handling, and bounded decompression.
//!
//! Every JAR on the class path is untrusted input. The archive format
//! itself is parsed by the `zip` crate, but everything CratonVM layers on
//! top is ours and is what this target exercises:
//!
//!   * fat-JAR / nested-JAR detection and the depth-bounded recursion into
//!     `BOOT-INF/lib/*` (`ClassPath::load_jar_data_at_depth`),
//!   * the `MAX_UNCOMPRESSED_ENTRY_BYTES` decompression-bomb clamp
//!     (`classloading/src/class_path.rs:815`, applied by
//!     `read_entry_capped`),
//!   * the entry-name safety filters `is_safe_class_name` /
//!     `is_safe_resource_name` (`classloading/src/class_path.rs:1024`,
//!     `:1035`) that stand between a central-directory name and a
//!     filesystem read.
//!
//! Surface under test (all `cratonvm_classloading::ClassPath`, in
//! `classloading/src/class_path.rs`):
//!   * `add_path` (`:2147`) — archive sniffing, `ZipArchive::new`, fat-JAR
//!     expansion.
//!   * `list_class_names` (`:4363`) — walks the central directory and
//!     surfaces every `*.class` entry name verbatim.
//!   * `find_class` (`:2394`) / `find_resource` (`:3236`) /
//!     `contains_resource` (`:3245`) — the lookup paths that must refuse a
//!     traversal-shaped name.
//!   * `read_jar_manifest` (`:2113`) — inflates `META-INF/MANIFEST.MF`.
//!   * `entry_count` (`:2103`), `list_jmod_modules` (`:4413`).
//!
//! # The traversal oracle
//!
//! `list_class_names` deliberately reports names exactly as the central
//! directory spells them, so a crafted archive containing
//! `../../etc/passwd.class` *will* show up in the enumeration. That makes
//! the assertion here strong rather than vacuous: for every enumerated name
//! that is traversal-shaped, **resolution must refuse it** — `find_class`
//! must return `Err` and `find_resource` must return `None`, even though
//! the entry demonstrably exists in the archive. If the name filter ever
//! regresses, the lookup succeeds and this target fires immediately. A
//! fixed battery of traversal probes (`../`, absolute, backslash, NUL,
//! drive letter, `./`) is checked alongside it.
//!
//! Overlong / invalid UTF-8 entry names are covered implicitly: the
//! enumeration hands back whatever the central directory carried, and the
//! same assertion applies to it. They cannot be probed directly through
//! `find_resource`, which takes `&str`.
//!
//! # Bounded decompression
//!
//! A successfully read entry must satisfy two bounds: the absolute
//! `MAX_UNCOMPRESSED_ENTRY_BYTES` clamp, and a ratio bound derived from the
//! archive's own size. DEFLATE's maximum expansion is 1032:1, so no entry
//! inside an `N`-byte archive can legitimately inflate past `N * 1032`.
//! Exceeding either means the streaming inflate ran unbounded.
//!
//! Both file entry points take a path, so the harness stages the fuzzer's
//! bytes in one reused temp file per process, removing it at the end of
//! each iteration. `CRATONVM_DISABLE_JAR_MMAP` is set at startup so the
//! archive is `read()` rather than `mmap`'d — otherwise the staging file
//! would still be mapped when the next iteration rewrote it.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_zip_entry

#![no_main]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Once, OnceLock};

use libfuzzer_sys::fuzz_target;

use cratonvm_classloading::ClassPath;

/// Small on purpose. A JAR's interesting structure — central directory,
/// EOCD, zip64 locator, per-entry local headers — all lives in the first
/// and last few kilobytes, and every byte of input multiplies the worst
/// case inflate this target must be able to hold in memory (see
/// `MAX_DEFLATE_RATIO`).
const MAX_INPUT: usize = 32 * 1024;

/// `MAX_UNCOMPRESSED_ENTRY_BYTES` from `classloading/src/class_path.rs:815`
/// (`pub(crate)`, so mirrored here). `read_entry_capped` clamps both the
/// pre-allocation and the streaming inflate to this.
const MAX_UNCOMPRESSED_ENTRY_BYTES: usize = 512 * 1024 * 1024;

/// DEFLATE's theoretical maximum expansion ratio is 1032:1. Nothing inside
/// an N-byte archive can legitimately inflate past `N * 1032`; a little
/// slack absorbs archive framing overhead.
const MAX_DEFLATE_RATIO: usize = 1032;
const RATIO_SLACK_BYTES: usize = 4096;

/// How many enumerated entries actually get inflated per input. Reading
/// every entry of a crafted archive would let one input dominate the fuzz
/// budget; the bomb behaviour under test shows up on the first entry.
const MAX_ENTRIES_READ: usize = 2;

/// Traversal-shaped names probed on every input, independent of what the
/// archive contains. Covers `..`, absolute POSIX and Windows paths, both
/// separators, drive letters, NUL, and `./` current-directory references.
///
/// Each probe is routed only to the APIs whose filter actually rejects it,
/// as decided by [`class_name_is_hostile`] / [`resource_name_is_hostile`].
/// That matters for `/etc/passwd`: the resource APIs strip leading slashes
/// first (HotSpot parity — `Class.getResource("/foo")` means `foo`), so
/// after normalisation it is an ordinary relative name and asserting it is
/// refused would be wrong. `find_class` does no such stripping and must
/// refuse it.
const TRAVERSAL_PROBES: [&str; 11] = [
    "../etc/passwd",
    "../../../../etc/shadow",
    "/etc/passwd",
    "\\windows\\win.ini",
    "..\\..\\windows\\system32\\config\\sam",
    "C:/Windows/win.ini",
    "a/../../b/c",
    "./secret",
    "a\u{0}b",
    "META-INF/../../outside",
    "java/lang/../../../etc/passwd",
];

/// Exactly the negation of `is_safe_class_name`
/// (`classloading/src/class_path.rs:1035`). `find_class` applies no
/// normalisation, so this predicate is applied to the raw name.
///
/// This is a **hand-maintained mirror**, because the real predicate is
/// private to that module. That is a silent-drift hazard in one direction:
/// tightening `is_safe_class_name` without updating this copy makes the
/// assertions below quietly weaker rather than failing. Loosening it
/// produces a visible false failure. Making the real predicate `pub` and
/// calling it directly is the fix — see
/// `docs/feature-designs/fuzzing-state.md`.
fn class_name_is_hostile(name: &str) -> bool {
    name.contains("..")
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\u{0}')
        || name.contains(':')
        || name.contains("./")
}

/// Exactly the negation of `is_safe_resource_name`
/// (`classloading/src/class_path.rs:1024`) **after** the leading-slash
/// strip every resource entry point performs first. `contains('\\')`
/// subsumes that predicate's `starts_with('\\')` and `".\\"` clauses, and
/// the trim rules out its `starts_with('/')` clause, so the two agree
/// exactly.
fn resource_name_is_hostile(name: &str) -> bool {
    let name = name.trim_start_matches('/');
    name.contains("..")
        || name.contains('\\')
        || name.contains('\u{0}')
        || name.contains(':')
        || name.contains("./")
}

/// Path of the single staging archive this target reuses.
fn staging_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("cratonvm-fuzz-zip-{}.jar", std::process::id()))
    })
}

/// Force the `read()` archive path instead of `mmap`.
///
/// This used to `set_var`, which does not work for a DECLARED flag: the
/// snapshot latches on first read, so an environment write only takes effect
/// if it wins the race to initialise it. `flag_env_mutation_guard` fails over
/// exactly that. Installing the snapshot is the supported way to say "for the
/// rest of this process", and the `Once` still matters, because `install`
/// refuses a second attempt and this must land before the first `ClassPath`.
fn disable_jar_mmap_once() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let cfg = cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_DISABLE_JAR_MMAP",
            Some("1"),
        )]);
        // An error here only means something already latched the snapshot, in
        // which case there is nothing to do and the mmap path is what gets
        // fuzzed — which is worth fuzzing too.
        let _ = cratonvm_types::flags::install(cfg);
    });
}

/// Largest byte count anything inside an `archive_len`-byte archive can
/// legitimately inflate to.
fn inflate_bound(archive_len: usize) -> usize {
    archive_len
        .saturating_mul(MAX_DEFLATE_RATIO)
        .saturating_add(RATIO_SLACK_BYTES)
}

/// Assert an entry the archive handed back respects both decompression
/// bounds.
fn assert_bounded_inflate(what: &str, len: usize, archive_len: usize) {
    assert!(
        len <= MAX_UNCOMPRESSED_ENTRY_BYTES,
        "{what} inflated to {len} bytes, past the \
         {MAX_UNCOMPRESSED_ENTRY_BYTES}-byte decompression-bomb clamp"
    );
    assert!(
        len <= inflate_bound(archive_len),
        "{what} inflated to {len} bytes from a {archive_len}-byte archive, \
         past DEFLATE's {MAX_DEFLATE_RATIO}:1 maximum expansion"
    );
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT {
        return;
    }
    disable_jar_mmap_once();

    let path = staging_path();
    {
        let Ok(mut f) = std::fs::File::create(path) else {
            return;
        };
        if f.write_all(data).is_err() || f.flush().is_err() {
            let _ = std::fs::remove_file(path);
            return;
        }
    }
    let archive_len = data.len();
    let path_str = path.to_string_lossy().into_owned();

    // Scope the ClassPath so its archive handles are dropped before the
    // staging file is removed.
    {
        let mut cp = ClassPath::new(&[]);
        // Fails closed: an unparseable archive simply pushes no entry.
        cp.add_path(&path_str);

        // A single file can expand into more than one classpath entry
        // (fat JARs add their nested archives), but every one of them
        // needs a central-directory record to exist at all.
        //
        // The bound is the *inflated* archive size, not `archive_len`: a
        // fat JAR's nested archives are themselves compressed entries, so
        // their central directories live in inflated bytes. Bounding
        // against the raw file size would be a false positive on any
        // well-compressed fat JAR, not a finding. What this still catches
        // is the failure that matters — an entry count taken from the
        // end-of-central-directory record and believed rather than parsed,
        // which is a `u16`/`u64` unrelated to how many bytes exist.
        let bound = inflate_bound(archive_len);
        assert!(
            cp.entry_count() <= bound,
            "one {archive_len}-byte archive produced {} classpath entries",
            cp.entry_count()
        );

        // ---- Central-directory enumeration ----
        let names = cp.list_class_names();
        assert!(
            names.len() <= bound,
            "{} class names enumerated from a {archive_len}-byte archive",
            names.len()
        );

        // ---- Traversal oracle over names the archive really contains ----
        //
        // This is the non-vacuous half: the entry is genuinely present in
        // the central directory, so if the name filter ever regressed the
        // lookup below would succeed and this assertion would fire.
        let mut read_budget = MAX_ENTRIES_READ;
        for name in &names {
            let resource = format!("{name}.class");
            let hostile_class = class_name_is_hostile(name);
            let hostile_resource = resource_name_is_hostile(&resource);

            if hostile_class {
                assert!(
                    cp.find_class(name).is_err(),
                    "find_class resolved traversal-shaped archive entry {name:?}"
                );
            }
            if hostile_resource {
                assert!(
                    cp.find_resource(&resource).is_none(),
                    "find_resource resolved traversal-shaped archive entry {resource:?}"
                );
                assert!(
                    !cp.contains_resource(&resource),
                    "contains_resource reported traversal-shaped entry {resource:?}"
                );
                assert!(
                    cp.find_all_resource_bytes(&resource).is_empty(),
                    "find_all_resource_bytes returned data for {resource:?}"
                );
            }
            if hostile_class || hostile_resource {
                continue;
            }

            // Benign names may resolve — but only to a bounded number of
            // bytes. This is the compression-ratio-bomb check.
            if read_budget > 0 {
                read_budget -= 1;
                if let Some(bytes) = cp.find_resource(&resource) {
                    assert_bounded_inflate("resource entry", bytes.len(), archive_len);
                }
                if let Ok(shared) = cp.find_class(name) {
                    let bytes: &[u8] = shared.as_ref();
                    assert_bounded_inflate("class entry", bytes.len(), archive_len);
                }
            }
        }

        // ---- Fixed traversal probes, archive-independent ----
        for probe in TRAVERSAL_PROBES {
            if class_name_is_hostile(probe) {
                assert!(
                    cp.find_class(probe).is_err(),
                    "find_class accepted traversal probe {probe:?}"
                );
            }
            if resource_name_is_hostile(probe) {
                assert!(
                    cp.find_resource(probe).is_none(),
                    "find_resource accepted traversal probe {probe:?}"
                );
                assert!(
                    !cp.contains_resource(probe),
                    "contains_resource accepted traversal probe {probe:?}"
                );
                assert!(
                    cp.find_all_resource_bytes(probe).is_empty(),
                    "find_all_resource_bytes returned data for traversal probe {probe:?}"
                );
                assert!(
                    cp.find_all_resource_urls(probe).is_empty(),
                    "find_all_resource_urls returned a URL for traversal probe {probe:?}"
                );
            }
        }

        // ---- Other archive-driven walks, panic-only ----
        let _ = cp.list_jmod_modules();
        let _ = cp.jmod_class_count();
        let _ = cp.is_empty();
    }

    // Manifest read: inflates `META-INF/MANIFEST.MF` and folds its
    // continuation lines. Any `Class-Path:` it declares is resolved
    // against the archive's directory — a permissive-by-spec path that
    // must still stay bounded.
    //
    // Note the bound is the *inflated* size, not `archive_len`: the
    // manifest is a compressed entry, so a 200-byte archive can carry a
    // manifest with thousands of attributes. Bounding against the raw
    // archive size here would be a false positive, not a finding.
    if let Some(info) = ClassPath::read_jar_manifest(path) {
        let bound = inflate_bound(archive_len);
        assert!(
            info.attributes.len() <= bound,
            "manifest of a {archive_len}-byte archive declared {} attributes",
            info.attributes.len()
        );
        let resolved = info.resolve_class_path(path);
        assert!(
            resolved.len() <= bound,
            "manifest Class-Path resolved to {} entries from a {archive_len}-byte archive",
            resolved.len()
        );
    }

    let _ = std::fs::remove_file(path);
});
