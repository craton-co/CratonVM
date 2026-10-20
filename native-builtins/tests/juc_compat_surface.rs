// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.concurrent` REGISTRATION CENSUS — which j.u.c natives survive a
//! real-JDK `--compatible` boot, per class and kind, and what each j.u.c family
//! row (`retired_shadow::COMPAT_FAMILIES`) takes away.
//!
//! Interpreter round i3 wave 73, lane L1
//! (`docs/internal/proposals/interpreter/i72-L1-proposal-a-juc-registration-census-test-20261105.md`).
//! Every j.u.c family row so far (`JdkLockSupport`, `JdkStampedLock`,
//! `JdkCountDownLatch`, `JdkAqls`, `JdkCopyOnWrite`) moved `stub_ratchet.rs` by
//! a DERIVED delta: a lane read which registrars run on the replayed boot path,
//! under which ambient kind and how many times. This census replays the same
//! boot path (`common/vm_init_boot_path.rs`, `real_jdk` set, the alignment set
//! stated explicitly rather than read from the environment) twice -- the
//! default rows, and every j.u.c family row off -- and prints, per class under
//! `java/util/concurrent/`, the rows by kind in each and the difference. Run
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test juc_compat_surface -- --nocapture
//! ```
//!
//! and the per-row ratchet delta is read off the print, not derived.
//!
//! [`JUC_COMPAT_SURFACE`] is the frozen per-class table of the DEFAULT
//! registry, zero slack, so a new j.u.c stand-in (or a family row that stops
//! dropping one) fails by class name instead of moving a bare total. It is
//! seeded from the first host run: while it is `None` the test prints a
//! paste-ready literal and asserts only the structural facts below. The lane
//! that wrote this could not run cargo; the orchestrator pastes the seed.

#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

use boot_path::vm_init_real_jdk_boot_path;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};
use std::collections::BTreeMap;

/// The j.u.c family rows (`retired_shadow::COMPAT_FAMILIES` entries whose
/// classes are under `java/util/concurrent/`). Append a row here when a new
/// j.u.c family lands.
const JUC_ROWS: &[Row] = &[
    Row::JdkLockSupport,
    Row::JdkStampedLock,
    Row::JdkCountDownLatch,
    Row::JdkAqls,
    Row::JdkCopyOnWrite,
    Row::JdkCopyOnWriteArraySet,
    Row::JdkPriorityBlockingQueue,
    Row::JdkConcurrentHashMap,
    Row::JdkCompletableFuture,
    Row::JdkThreadPools,
];

/// `(class, bridges, synthetic stubs, intrinsics)` of the default real-JDK
/// `--compatible` registry, one entry per class under `java/util/concurrent/`
/// that keeps at least one registration, in byte order. `None` until seeded
/// (see the module doc).
const JUC_COMPAT_SURFACE: Option<&[(&str, usize, usize, usize)]> = None;

/// Per-class `[bridges, stubs, intrinsics]`.
type Surface = BTreeMap<String, [usize; 3]>;

fn juc_surface(rows: HotSpotAlignments) -> Surface {
    let mut registry = NativeMethodRegistry::new();
    // Before any registration: the family drop is decided at `register`.
    registry.set_hotspot_alignments(rows);
    vm_init_real_jdk_boot_path(&mut registry);
    let mut surface = Surface::new();
    for (class, _, _, kind) in registry.dump_registrations() {
        if !class.starts_with("java/util/concurrent/") {
            continue;
        }
        let slot = match kind {
            NativeKind::Bridge => 0,
            NativeKind::SyntheticStub => 1,
            NativeKind::Intrinsic => 2,
        };
        surface.entry(class.to_string()).or_insert([0; 3])[slot] += 1;
    }
    surface
}

fn defaults() -> HotSpotAlignments {
    HotSpotAlignments::defaults()
}

fn juc_rows_off() -> HotSpotAlignments {
    JUC_ROWS
        .iter()
        .fold(defaults(), |set, &row| set.with(row, false))
}

fn juc_rows_on() -> HotSpotAlignments {
    JUC_ROWS
        .iter()
        .fold(defaults(), |set, &row| set.with(row, true))
}

fn total(s: &Surface, class: &str) -> usize {
    s.get(class).map_or(0, |k| k.iter().sum())
}

fn print_surface(label: &str, s: &Surface) {
    let sum = s
        .values()
        .fold([0usize; 3], |a, k| [a[0] + k[0], a[1] + k[1], a[2] + k[2]]);
    println!(
        "juc-surface[{label}]: {} classes, {} bridges, {} stubs, {} intrinsics",
        s.len(),
        sum[0],
        sum[1],
        sum[2]
    );
}

/// The default registry against the frozen table (or the seed, unfrozen).
#[test]
fn juc_compat_surface_is_frozen() {
    let surface = juc_surface(defaults());
    print_surface("default", &surface);
    let mut seed = String::from(
        "const JUC_COMPAT_SURFACE: Option<&[(&str, usize, usize, usize)]> = Some(&[\n",
    );
    for (class, k) in &surface {
        seed.push_str(&format!(
            "    (\"{class}\", {}, {}, {}),\n",
            k[0], k[1], k[2]
        ));
    }
    seed.push_str("]);\n");
    let Some(frozen) = JUC_COMPAT_SURFACE else {
        println!("juc-surface: JUC_COMPAT_SURFACE is not seeded; paste:\n{seed}");
        return;
    };
    let frozen: Surface = frozen
        .iter()
        .map(|&(c, b, s, i)| (c.to_string(), [b, s, i]))
        .collect();
    let classes: std::collections::BTreeSet<&String> =
        frozen.keys().chain(surface.keys()).collect();
    let mut diffs: Vec<(String, [usize; 3], [usize; 3])> = Vec::new();
    for class in classes {
        let was = frozen.get(class).copied().unwrap_or([0; 3]);
        let now = surface.get(class).copied().unwrap_or([0; 3]);
        if was != now {
            diffs.push((class.clone(), was, now));
        }
    }
    assert!(
        diffs.is_empty(),
        "the j.u.c registrations surviving a real-JDK --compatible boot moved \
         ([bridges, stubs, intrinsics], frozen -> now): {diffs:?}.\n\
         A new stand-in on a j.u.c class needs an explicit NativeKind and a \
         removal record (AGENTS.md); a family row that now drops more is a win. \
         Either way, re-freeze JUC_COMPAT_SURFACE from this seed in the same \
         change:\n{seed}"
    );
}

/// What the j.u.c family rows take away, per class: printed for the census
/// (`--nocapture`), with the facts that hold whatever the counts are.
#[test]
fn juc_family_rows_delta_is_printed() {
    let off = juc_surface(juc_rows_off());
    let on = juc_surface(juc_rows_on());
    print_surface("j.u.c rows off", &off);
    print_surface("j.u.c rows on", &on);
    for class in off.keys() {
        let (was, now) = (off[class], on.get(class).copied().unwrap_or([0; 3]));
        if was != now {
            println!("juc-delta: {class} [bridges, stubs, intrinsics] {was:?} -> {now:?}");
        }
    }
    for class in on.keys().filter(|c| !off.contains_key(*c)) {
        println!(
            "juc-delta: {class} appears only with the rows on: {:?}",
            on[class]
        );
    }

    // A family row only ever DROPS registrations. Per class TOTAL, not per
    // kind: registration is last-write-wins, so dropping a later stub can
    // leave an earlier `Bridge` on the same triple standing (a kind can grow
    // while the class shrinks).
    for class in on.keys() {
        assert!(
            total(&on, class) <= total(&off, class),
            "{class}: a j.u.c family row ADDED registrations ({:?} -> {:?})",
            off.get(class),
            on.get(class)
        );
    }
    // `JdkAqls`: the three state stand-ins are its family's whole surface
    // (retired rows, no `bridges`), and no other registrar names the class.
    const AQLS: &str = "java/util/concurrent/locks/AbstractQueuedLongSynchronizer";
    assert_eq!(
        total(&on, AQLS),
        0,
        "JdkAqls on: an AQLS registration survives"
    );
    // `JdkCopyOnWrite`: every stand-in on the class is a retired row. Since
    // interpreter round i3 wave 76 (lane L1) the row has graduated for them:
    // the 14 `util_concurrent_ext` and four `native-collections` stand-ins
    // are registered on the synthetic library only, so the real-JDK boot path
    // keeps none of them with the row off either: the row no longer moves the
    // class (its `juc-delta:` line is gone; it printed the 18 going before).
    const COWAL: &str = "java/util/concurrent/CopyOnWriteArrayList";
    assert_eq!(
        total(&off, COWAL),
        total(&on, COWAL),
        "JdkCopyOnWrite off brought CopyOnWriteArrayList stand-ins back on the real-JDK boot path"
    );
    // `JdkCopyOnWriteArraySet` (wave 74): the family takes every `SET_CLASSES`
    // row on the class (three retired, 21 reviewed `bridges`), and no other
    // registrar names it, so the class keeps nothing.
    const COWAS: &str = "java/util/concurrent/CopyOnWriteArraySet";
    assert!(
        total(&off, COWAS) > 0,
        "no CopyOnWriteArraySet row on the boot path"
    );
    assert_eq!(
        total(&on, COWAS),
        0,
        "JdkCopyOnWriteArraySet on: a set registration survives"
    );
    // `JdkPriorityBlockingQueue` (wave 75): the nine stand-ins of
    // `native-collections` `register_priority_blocking_queue_natives` are its
    // family's whole surface (retired rows, no `bridges`), and no other
    // registrar on the real-JDK boot path names the class.
    const PBQ: &str = "java/util/concurrent/PriorityBlockingQueue";
    assert!(
        total(&off, PBQ) > 0,
        "no PriorityBlockingQueue row on the boot path"
    );
    assert_eq!(
        total(&on, PBQ),
        0,
        "JdkPriorityBlockingQueue on: a queue registration survives"
    );
    // `JdkConcurrentHashMap` (wave 76, default off): the map, its views,
    // iterators and entries lose every stand-in -- the retired rows through
    // the family's class prefix, the rest because `native-collections`' CHM
    // registrar and carrier loops skip theirs. What may stay on the prefix is
    // the `tabAt` / `setTabAt` / `casTabAt` intrinsics (over the JDK's own
    // `table`); the per-class counts are printed for the census.
    const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
    for (class, k) in on.iter().filter(|(c, _)| c.starts_with(CHM)) {
        println!("juc-chm[on]: {class} [bridges, stubs, intrinsics] {k:?}");
    }
    assert!(
        total(&off, CHM) > 0,
        "no ConcurrentHashMap row on the boot path"
    );
    assert!(
        total(&on, CHM) < total(&off, CHM),
        "JdkConcurrentHashMap on dropped no ConcurrentHashMap registration ({} -> {})",
        total(&off, CHM),
        total(&on, CHM)
    );
    // `JdkCompletableFuture` (wave 80, default off): every stand-in on the
    // class over a real JDK is a retired row (`thenCombine`, `allOf`, `anyOf`,
    // `complete`, `completeExceptionally`, `isCompletedExceptionally`,
    // `completeValue`) or one of the seven `bridges`, so the family drops them;
    // the counts are printed for the census.
    const CF: &str = "java/util/concurrent/CompletableFuture";
    println!(
        "juc-cf: CompletableFuture rows off {:?} -> on {:?}",
        off.get(CF),
        on.get(CF)
    );
    assert!(
        total(&off, CF) > 0,
        "no CompletableFuture row on the boot path"
    );
    assert!(
        total(&on, CF) < total(&off, CF),
        "JdkCompletableFuture on dropped no CompletableFuture registration ({} -> {})",
        total(&off, CF),
        total(&on, CF)
    );
    // `JdkThreadPools` (wave 82, default off): every `ThreadPoolExecutor`
    // registration on the real-JDK boot path is a retired row (the fifteen of
    // `native-builtins` `register_executor_natives` and `native-collections`
    // `register_concurrent_completeness_natives`) or `execute`'s stated stub,
    // so the class keeps nothing with the row on.
    // `ScheduledThreadPoolExecutor` keeps nothing either way over a real JDK
    // (`drop_real_layout_synthetic`); the counts are printed for the census.
    const TPE: &str = "java/util/concurrent/ThreadPoolExecutor";
    const STPE: &str = "java/util/concurrent/ScheduledThreadPoolExecutor";
    println!(
        "juc-tpe: ThreadPoolExecutor rows off {:?} -> on {:?}; ScheduledThreadPoolExecutor off {:?} -> on {:?}",
        off.get(TPE),
        on.get(TPE),
        off.get(STPE),
        on.get(STPE)
    );
    assert!(
        total(&off, TPE) > 0,
        "no ThreadPoolExecutor row on the boot path"
    );
    assert_eq!(
        total(&on, TPE),
        0,
        "JdkThreadPools on: a ThreadPoolExecutor registration survives"
    );
    assert_eq!(
        total(&on, STPE),
        0,
        "JdkThreadPools on: a scheduled-pool registration survives"
    );
}

// ---------------------------------------------------------------------------
// Copy-on-write minting sites (interpreter round i3 wave 74, lane L1;
// `docs/internal/fixed-bugs/interpreter-i73-L1-proposal-decide-jdk-copy-on-write-without-the-spring-sample-20261106-DONE-20261107.md`,
// step 1).
//
// With the `JdkCopyOnWrite` row on, every `CopyOnWriteArrayList` constructor,
// `clone()` and `readObject` is the JDK's bytecode, so `lock` can be null (the
// JDK bodies' `synchronized (lock)` NPE, the 2026-07-14 symptom) only on an
// instance a VM native allocates WITHOUT running a constructor. This scan
// lists every such allocation in production source and fails on one that is
// not in the reviewed list, so a new minting site is reviewed before it can
// reach a JDK mutator. (`new_object_initialized` runs the constructor and is
// not a mint here.)
// ---------------------------------------------------------------------------

/// Allocation calls that do not run a constructor.
const RAW_MINT_NEEDLES: &[&str] = &[
    "new_object(",
    "alloc_object(",
    "allocate_instance(",
    "try_alloc_concurrent_synthetic(",
];

/// `(file, enclosing fn)` of every reviewed raw copy-on-write mint, with why a
/// null `lock` there cannot reach a JDK body that reads it.
const REVIEWED_COW_MINTS: &[(&str, &str)] = &[
    // The `CopyOnWriteArraySet` snapshot-iterator delegate: a list minted over
    // a snapshot array only to call `iterator()` on it (the JDK's reads
    // `array`, never `lock`); the list never escapes. Reached only for a
    // FABRICATED set (a real one is routed to its own bytecode first).
    ("native-collections/src/lib.rs", "cow_set_snapshot_iterator"),
];

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-builtins has a parent (the workspace root)")
        .to_path_buf()
}

fn rs_files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // `.claude` holds sibling worktrees; `target` holds build output.
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if p.is_dir() {
            rs_files_under(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

/// The name of the `fn` a line declares, if it declares one.
fn declared_fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with("//") {
        return None;
    }
    let mut words = t.split_whitespace();
    for _ in 0..5 {
        let w = words.next()?;
        if w == "fn" {
            let name: String = words
                .next()?
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    }
    None
}

/// Does the call starting right after a needle name a copy-on-write class?
/// Reads the rest of the line and, for a call rustfmt wrapped, up to three
/// more lines, stopping at the call's first `)`.
fn call_names_cow_class(rest: &str, following: &[&str]) -> bool {
    let mut text = rest.to_string();
    for next in following.iter().take(3) {
        if text.contains(')') {
            break;
        }
        text.push(' ');
        text.push_str(next.trim());
    }
    let call = text.split(')').next().unwrap_or("");
    call.contains("java/util/concurrent/CopyOnWriteArray")
        || call.contains("COWAL")
        || call.contains("COW_SET_CLASS")
}

/// Every raw copy-on-write mint in one file's production text, as the
/// enclosing `fn` names. `#[cfg(test)]` regions are skipped (the walk
/// `unconstructed_carrier_gate.rs` uses): a fixture's mint never reaches the
/// runtime.
fn cow_mints_in(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut current_fn = String::from("<top level>");
    let mut pending_test = false;
    let mut depth: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        let line = *line;
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
        if let Some(name) = declared_fn_name(line) {
            current_fn = name;
        }
        for needle in RAW_MINT_NEEDLES {
            let mut rest = line;
            while let Some(at) = rest.find(needle) {
                // `new_object(` must not match inside a longer name such as
                // `try_new_object(`; the `_initialized` form cannot match (the
                // needle ends in `(`).
                let before = &rest[..at];
                rest = &rest[at + needle.len()..];
                if before.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if call_names_cow_class(rest, &lines[i + 1..]) {
                    out.push(current_fn.clone());
                }
            }
        }
    }
    out
}

#[test]
fn the_cow_mint_parse_reads_wrapped_calls_and_skips_tests() {
    let src = [
        "fn a() {",
        "    let x = ctx.new_object(\"java/util/concurrent/CopyOnWriteArrayList\")?;",
        "}",
        "fn b() {",
        "    let y = try_alloc_concurrent_synthetic(",
        "        ctx,",
        "        \"java/util/concurrent/CopyOnWriteArraySet\",",
        "        1,",
        "    );",
        "}",
        "fn c() {",
        "    let z = ctx.new_object_initialized(\"java/util/concurrent/CopyOnWriteArrayList\", \"()V\", &[]);",
        "    let w = ctx.new_object(\"java/util/ArrayList\");",
        "    let v = ctx.try_new_object(COWAL_CLASS);",
        "}",
        "#[cfg(test)]",
        "mod tests {",
        "    fn d() { ctx.new_object(\"java/util/concurrent/CopyOnWriteArrayList\"); }",
        "}",
        "pub(crate) fn e() {",
        "    let u = ctx.alloc_object(COWAL_CLASS);",
        "}",
    ]
    .join("\n");
    assert_eq!(
        cow_mints_in(&src),
        vec!["a".to_string(), "b".to_string(), "e".to_string()]
    );
}

/// Every raw copy-on-write mint in the workspace's production source is a
/// reviewed one, and every reviewed one still exists.
#[test]
fn every_copy_on_write_mint_without_a_constructor_is_reviewed() {
    let root = workspace_root();
    let mut files = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else {
        panic!("cannot read the workspace root {}", root.display());
    };
    let mut crates: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    crates.sort();
    for c in crates {
        let name = c
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let src = c.join("src");
        if src.is_dir() {
            rs_files_under(&src, &mut files);
        }
    }
    // Vacuity floor: a walk that found nothing would pass for the wrong reason.
    assert!(
        files.len() > 250,
        "the walk found only {} .rs files",
        files.len()
    );
    let mut found: Vec<(String, String)> = Vec::new();
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        let rel = f
            .strip_prefix(&root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        for func in cow_mints_in(&src) {
            found.push((rel.clone(), func));
        }
    }
    for (file, func) in &found {
        println!("cow-mint: {file} {func}");
    }
    let unreviewed: Vec<&(String, String)> = found
        .iter()
        .filter(|(file, func)| {
            !REVIEWED_COW_MINTS
                .iter()
                .any(|(rf, rn)| rf == file && rn == func)
        })
        .collect();
    assert!(
        unreviewed.is_empty(),
        "a native allocates a CopyOnWriteArrayList / CopyOnWriteArraySet without running a \
         constructor: {unreviewed:?}. Its `lock` (or its list's) is null, and with the \
         JdkCopyOnWrite row (or --jdk-only) every JDK mutator NPEs on `synchronized (lock)` \
         -- the 2026-07-14 symptom. Construct it (`new_object_initialized`), or review that \
         no JDK body reading `lock` can reach it and add it to REVIEWED_COW_MINTS with the reason."
    );
    for (file, func) in REVIEWED_COW_MINTS {
        assert!(
            found.iter().any(|(f, n)| f == file && n == func),
            "REVIEWED_COW_MINTS names {file} {func}, which no longer mints: drop the row"
        );
    }
}
