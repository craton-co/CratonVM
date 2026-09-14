// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! UNCONSTRUCTED-CARRIER GATE — a class may not newly become both a class this
//! VM ALLOCATES and a class whose natives are RETIRED onto the JDK's bytecode.
//!
//! # The species
//!
//! Two decisions that are individually correct compose into a defect.
//!
//! 1. A retirement table entry deletes a native so the JDK's own bytecode runs
//!    instead. That is the point of `--jdk-only` and it is usually the right
//!    move: the image's own body is the reference implementation.
//! 2. A native may hand back an instance it allocated itself, through
//!    `try_alloc_concurrent_synthetic`. In real-JDK mode that allocation is
//!    upsized to the real class's layout, so the object has every field the
//!    image declares, at the image's own slot indices.
//!
//! What no step performs is the **constructor**. `try_alloc_concurrent_synthetic`
//! allocates; it does not run `<init>`, so no field initialiser the class
//! declares ever executes. Every field arrives as the zero of its type.
//!
//! While the natives stay, nothing notices: the native bodies keep their state
//! in a side table and never read the fields. Retire one, and the JDK's own
//! bytecode reads them — and for any field whose declared initialiser is not
//! zero, **zero is a legal value that means something else**.
//!
//! # The measured instance
//!
//! `java.net.HttpURLConnection`, 2026-09-12. `URL.openConnection()` allocates
//! the carrier, and the 2026-09-11 wave retired thirteen of its triples onto
//! the image. Five declared initialisers never ran:
//!
//! ```text
//! chunkLength             = -1     arrived 0
//! fixedContentLength      = -1     arrived 0
//! fixedContentLengthLong  = -1L    arrived 0
//! responseCode            = -1     arrived 0
//! method                  = "GET"  arrived null
//! ```
//!
//! `-1` means *unset* to `HttpURLConnection`'s own bytecode, so `0` read as
//! SET. Both streaming setters then refused on a connection nobody had
//! configured, each naming the mode the other had supposedly set, and
//! `getRequestMethod()` disagreed with what went on the wire. One cause, five
//! probe rows across two sweeps, and every one of them looked like a separate
//! defect until the constructor was the answer.
//!
//! `java.util.TreeMap` was the same species, found independently on the same
//! day ("TreeMap's declared reference fields were never written").
//!
//! # What this gate does, and what it deliberately cannot
//!
//! It scores the PRECONDITION: the intersection of the retirement tables'
//! carrier classes with the classes some native allocates. Both halves are
//! source facts, so both are checkable here.
//!
//! It cannot score the third condition — whether the class actually declares a
//! non-zero initialiser, and whether a retired method reads that field —
//! because that is a question about a JDK image and needs `javap`. That
//! narrowing is recorded in
//! `docs/internal/retired/lane-6-carrier-hazard-census-20260912.md`, which is
//! also where the confirmed instances are listed. This file is the tripwire for
//! a SIXTIETH, not a verdict on the fifty-nine.
//!
//! **A new row is not automatically a bug.** It is a request to run the
//! narrowing in that record before landing.
//!
//! # Why a source scan and not the registry dump
//!
//! `--dump-native-registry` is the better oracle for who OWNS a triple, and
//! this lane uses it for exactly that. It cannot answer this question: a
//! retired triple is one that is NOT in the registry, so the dump's silence
//! about a class is indistinguishable from the class never having been
//! registered at all. The tables are the only place retirement is written down.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every crate that can register a native or mint a carrier. Kept in step with
/// `registrar_drift.rs`'s list of the same name: a crate dropped from here
/// silently shrinks the minted half, which turns a real row into a stale one
/// and reads as good news.
const CRATES: &[&str] = &[
    "native-builtins",
    "native-collections",
    "native-io",
    "native-awt",
    "vm",
    "native-builtins-crypto",
    "native-builtins-security",
];

/// The retirement tables, relative to the workspace root.
const RETIREMENT_TABLE: &str = "native-api/src/retired_shadow.rs";

/// Frozen 2026-09-12 (60; 59 at the freeze, plus `java/io/PrintStream`
/// from lane 4 wave 6 the same day -- see the comment on that row).
/// Sorted.
///
/// `java/net/HttpURLConnection` is NOT here because it is not in the
/// intersection: the wave that found this species did not stop retiring its
/// natives, it made the mint site write the declared defaults. A class stays
/// on this list while it meets the precondition, whether or not it has been
/// audited — the audit lives in the record, and the two tests below only ask
/// whether the population has moved.
const BASELINE: &[&str] = &[
    "java/io/ByteArrayInputStream",
    "java/io/ByteArrayOutputStream",
    "java/io/File",
    "java/io/FileDescriptor",
    "java/io/FileOutputStream",
    // Added by lane 4 wave 6 (2026-09-12), which retires 29 triples onto this
    // class. The narrowing this gate's message asks for, run before landing:
    //
    //  * `javap -p -c` on 17, 21 and 25: the ONLY field with a non-zero
    //    declared initialiser anywhere on the class or its superclasses is
    //    `FilterOutputStream.closeLock = new Object()`. Every other field is
    //    written from a constructor PARAMETER (`out`, `autoFlush`, `charOut`,
    //    `textOut`, `charset`) or initialised to the zero of its type
    //    (`trouble`, `closing`, `closed`, `formatter`).
    //  * **No retired method reads it.** `PrintStream.close()` overrides
    //    `FilterOutputStream.close()` and synchronizes on `this`
    //    (`aload_0; dup; astore_1; monitorenter`), never on `closeLock`, so
    //    the field the hazard would turn into a wrong answer is not on any
    //    retired path.
    //  * It is written BY NAME at the mint site regardless -- see
    //    `install_real_stream_fields` in `native-builtins/src/lang_system.rs`
    //    -- because `System.out.closeLock` read `null` against HotSpot's
    //    `java.lang.Object` and that was a wrong answer whether or not
    //    anything read it.
    //  * The other mint site, `try_alloc_concurrent_synthetic(ctx,
    //    "java/io/PrintStream", 1)` in `t3_impl.rs`, writes a message into
    //    slot 0 and returns `Int(2)`; the object itself never reaches Java.
    //
    // Precondition met, hazard not. Same disposition as the rest of this list.
    "java/io/PrintStream",
    "java/lang/Class",
    "java/lang/ClassLoader",
    "java/lang/ClassNotFoundException",
    "java/lang/Module",
    "java/lang/ModuleLayer",
    "java/lang/Object",
    "java/lang/Package",
    "java/lang/Thread$State",
    "java/lang/invoke/MethodType",
    "java/lang/management/ClassLoadingMXBean",
    "java/lang/management/CompilationMXBean",
    "java/lang/management/MemoryMXBean",
    "java/lang/management/MemoryUsage",
    "java/lang/management/RuntimeMXBean",
    "java/lang/module/Configuration",
    "java/lang/module/ModuleDescriptor",
    "java/lang/module/ModuleDescriptor$Provides",
    "java/lang/module/ModuleDescriptor$Requires",
    "java/lang/reflect/Method",
    "java/nio/ByteBuffer",
    "java/nio/CharBuffer",
    "java/nio/channels/FileChannel",
    "java/nio/file/attribute/FileTime",
    "java/text/BreakIterator",
    "java/time/ZoneId",
    "java/util/ArrayList",
    "java/util/ArrayList$Itr",
    "java/util/ArrayList$ListItr",
    "java/util/Collections$EmptyIterator",
    "java/util/Date",
    "java/util/HashMap",
    "java/util/HashSet",
    "java/util/Locale",
    "java/util/Optional",
    "java/util/OptionalLong",
    "java/util/Properties",
    "java/util/TreeMap",
    "java/util/concurrent/CompletableFuture",
    "java/util/concurrent/ConcurrentHashMap",
    "java/util/concurrent/ThreadPoolExecutor",
    "java/util/concurrent/TimeUnit",
    "java/util/jar/Attributes$Name",
    "java/util/jar/JarEntry",
    "java/util/logging/Level",
    "java/util/logging/LogManager",
    "java/util/logging/Logger",
    "java/util/stream/DoubleStream",
    "java/util/stream/IntStream",
    "java/util/stream/LongStream",
    "java/util/stream/Stream",
];

/// Vacuity floors. A scan that stops matching passes every set comparison
/// below while proving nothing, which is a failure mode the source-scanning
/// gates in this directory have each hit at least once.
const MIN_FILES: usize = 250;
const MIN_MINTED: usize = 300;
const MIN_RETIRED_CARRIERS: usize = 150;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-builtins has a parent (the workspace root)")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // `.claude` holds sibling worktrees — whole extra copies of this repo.
        // Descending into one doubles every count and makes the answer depend
        // on which other lanes happen to be checked out.
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            rs_files(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

/// Classes some native ALLOCATES.
///
/// Comment lines are skipped: this file's own prose names
/// `try_alloc_concurrent_synthetic` beside a class, and so does `servlet.rs`,
/// and counting either would put a class in the population on the strength of
/// a sentence.
fn minted_classes() -> (BTreeSet<String>, usize) {
    let ws = workspace();
    let mut paths = Vec::new();
    for c in CRATES {
        let dir = ws.join(c).join("src");
        if dir.is_dir() {
            rs_files(&dir, &mut paths);
        }
    }
    const NEEDLE: &str = "try_alloc_concurrent_synthetic(";
    let mut out = BTreeSet::new();
    for p in &paths {
        let Ok(src) = std::fs::read_to_string(p) else {
            continue;
        };
        // `#[cfg(test)]` regions are skipped, on the same walk
        // `lock_discipline_ratchet.rs` uses. A carrier minted in a test
        // fixture is not one the runtime ever hands to the JDK's bytecode,
        // and counting them put eight classes in this population on the
        // strength of `try_alloc_concurrent_synthetic(&mut ctx, ..).unwrap()`
        // inside `mod tests` -- among them `java/lang/String` and
        // `java/lang/Throwable`, which no native mints in anger.
        let mut pending_test = false;
        let mut depth: i32 = 0;
        for line in src.lines() {
            let t = line.trim_start();
            let is_comment = t.starts_with("//") || t.starts_with('*');
            if !is_comment && depth == 0 && !pending_test && t.starts_with("#[cfg(test)]") {
                pending_test = true;
                continue;
            }
            if pending_test {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if line.contains('{') && depth <= 0 {
                    pending_test = false;
                    depth = 0;
                } else if depth > 0 {
                    pending_test = false;
                }
                continue;
            }
            if depth > 0 {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if is_comment {
                continue;
            }
            let mut rest = line;
            while let Some(i) = rest.find(NEEDLE) {
                rest = &rest[i + NEEDLE.len()..];
                // `(ctx, "java/x/Y", n)` — step over the context argument, then
                // take the first string literal. A call whose class comes from
                // a variable yields nothing, which is the conservative
                // direction: this gate under-reports rather than inventing a
                // row nobody can act on.
                let Some(comma) = rest.find(',') else {
                    break;
                };
                let after = rest[comma + 1..].trim_start();
                if !after.starts_with('"') {
                    continue;
                }
                if let Some(end) = after[1..].find('"') {
                    out.insert(after[1..1 + end].to_string());
                }
            }
        }
    }
    (out, paths.len())
}

/// Classes carrying at least one RETIRED triple.
///
/// Only `("class", "name", "descriptor"),` tuples count. A class NAMED in the
/// file's prose is not a retirement, and counting prose is how the first pass
/// of this census reported sixty-six where the tables say fifty-nine.
fn retired_carriers() -> BTreeSet<String> {
    let path = workspace().join(RETIREMENT_TABLE);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let t = line.trim_start();
        if !t.starts_with("(\"") {
            continue;
        }
        // ["(", class, ", ", name, ", ", descriptor, "),"]
        let fields: Vec<&str> = t.split('"').collect();
        if fields.len() < 6 {
            continue;
        }
        let class = fields[1];
        if class.contains('/') {
            out.insert(class.to_string());
        }
    }
    out
}

fn population() -> (BTreeSet<String>, usize, usize, usize) {
    let (minted, files) = minted_classes();
    let retired = retired_carriers();
    let both: BTreeSet<String> = minted.intersection(&retired).cloned().collect();
    (both, files, minted.len(), retired.len())
}

#[test]
fn the_scan_still_finds_its_population() {
    let (both, files, minted, retired) = population();
    println!(
        "unconstructed-carrier census: {files} files, {minted} minted classes, \
         {retired} retired carriers, {} in both (baseline {})",
        both.len(),
        BASELINE.len()
    );
    assert!(
        files >= MIN_FILES,
        "only {files} .rs files across {CRATES:?} (floor {MIN_FILES}). The directory \
         walk broke rather than the workspace shrinking — fix the scan before \
         touching the baseline."
    );
    assert!(
        minted >= MIN_MINTED,
        "only {minted} minted classes found (floor {MIN_MINTED}); the \
         `try_alloc_concurrent_synthetic` needle stopped matching, and an empty \
         population passes both set comparisons below while proving nothing."
    );
    assert!(
        retired >= MIN_RETIRED_CARRIERS,
        "only {retired} retired carriers found (floor {MIN_RETIRED_CARRIERS}); the \
         tuple parse for {RETIREMENT_TABLE} stopped matching."
    );
}

#[test]
fn no_new_class_is_both_minted_and_retired() {
    let (both, ..) = population();
    let baseline: BTreeSet<String> = BASELINE.iter().map(|s| s.to_string()).collect();
    let new: Vec<&String> = both.difference(&baseline).collect();
    assert!(
        new.is_empty(),
        "{} class(es) newly BOTH allocated by a native and carrying retired \
         triples:\n{}\n\n\
         This VM allocates the carrier and no constructor runs, so every field \
         arrives as the zero of its type — and the JDK's own bytecode, which \
         the retirement handed the method to, reads those fields. For any field \
         whose declared initialiser is not zero, zero is a legal value meaning \
         something else. Measured on `java.net.HttpURLConnection`: four `-1` \
         sentinels arrived as 0, `-1` means UNSET, and both streaming setters \
         then refused a connection nobody had configured.\n\n\
         What to do, in order:\n\
         1. `javap -p -c <class>` and read the constructors. If no field has a \
            non-zero declared initialiser, add the class to BASELINE here and \
            put that verdict in the commit message — the precondition is met \
            and the hazard is not.\n\
         2. If any does, write those defaults BY NAME at the mint site before \
            the object escapes. `huc_write_declared_field_defaults` in \
            `native-builtins/src/http_url_connection.rs` is the worked example. \
            BY NAME matters: a slot index depends on the image's field order, \
            which is not this crate's to assume.\n\
         3. Then add the class to BASELINE.\n\n\
         Full narrowing and the confirmed instances: \
         `docs/internal/retired/lane-6-carrier-hazard-census-20260912.md`.",
        new.len(),
        new.iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_baseline_has_no_stale_rows() {
    let (both, ..) = population();
    let baseline: BTreeSet<String> = BASELINE.iter().map(|s| s.to_string()).collect();
    let gone: Vec<&String> = baseline.difference(&both).collect();
    assert!(
        gone.is_empty(),
        "{} baseline class(es) are no longer both minted and retired:\n{}\n\n\
         Either the last mint site went away or the retirement was reverted. \
         Both are good news, and both want the row deleted from BASELINE in the \
         same change, so the list keeps meaning what it says. Do NOT delete a \
         row to quiet the other test in this file.\n\n\
         Read it the other way too: this is also what fires when a scan is \
         accidentally blinded. A `registrar_drift` baseline went stale in \
         exactly this shape when registrations moved into a macro the source \
         scanner cannot expand — the registrations were real the whole time, \
         and regenerating the baseline would have thrown away eleven records.",
        gone.len(),
        gone.iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
