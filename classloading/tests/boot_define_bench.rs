// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 fast boot, lane `p5-define`: a lever for the cost of DEFINING the
//! boot classes of a Hello world, through the real `ClassManager`, from the
//! host JDK's `lib/modules`.
//!
//! `tests/fixtures/boot_classes_hello.txt` is the class list HotSpot 25 loads
//! from `jrt:/` for `java -Xshare:off -cp cls Hello` (452 names, load order).
//! Each repetition builds a fresh manager over the image and
//!
//! 1. `load_class`es every name in order (parse, Pass 2, registration, and
//!    -- in the `eager` arm -- the structural scan and the type-map walk);
//! 2. runs the type-map walk each class's define left to its link
//!    (`publish_pending_type_maps` + `finish_pending_type_maps`, what
//!    `vm_util::publish_pending_type_maps_at_link` does), for every class
//!    that still owes one -- in the `link` arm preceded by the structural
//!    scan, whose decodes the walk reuses.
//!
//! Arms: `eager` (`set_define_boot_maps_at_link(false)`, the `--jdk-only`
//! shape) and `link` (`true`, the `--compatible` shape), each with the store
//! as the VM builds it (rows dropped) and `+rows` (rows kept, so every walk
//! builds its rows). The arms are interleaved round by round; the test
//! prints each phase's minimum and median.
//!
//! Equivalence, asserted before any timing: with the rows kept both arms
//! publish the SAME maps for every class (verdict, fast-path veto and every
//! per-pc row) over the same defined set, and with the VM's store each arm
//! publishes exactly the verdicts its kept-row run strips to. So a saving
//! cannot change what the maps' consumers read. `CVM_BOOT_ALL=1` runs the
//! equivalence (and timing) over every `java.base` class of the image
//! instead of Hello's boot set.
//!
//! `#[ignore]`d: it needs a JDK. Run with
//!
//! ```text
//! CVM_BOOT_IMAGE=$JAVA_HOME/lib/modules [CVM_BOOT_REPEAT=15] [CVM_BOOT_ALL=1] \
//!   cargo test --release -p cratonvm-classloading --test boot_define_bench \
//!   -- --ignored --nocapture
//! ```
//!
//! On Windows, `profile_hello_boot_classes` (same switches, plus
//! `CVM_BOOT_AT_LINK=1` for the link arm and `CVM_PROF_CALLERS=a,b` for the
//! caller chains of matching frames) samples the define loop in-process
//! (`win_sampler`, no elevation needed) and prints self and inclusive time by
//! function.

// The launcher's allocator (`vm-cli/src/main.rs`).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use cratonvm_classloading::type_maps::TypeMapStore;
use cratonvm_classloading::{ClassId, ClassManager};

fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

/// Every observable of a class's published maps (as `verify_image_diff`);
/// `rows: false` digests what survives `strip_rows` (the verdicts) only.
fn maps_digest(store: &TypeMapStore, id: ClassId, rows: bool) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let Some(ctm) = store.class_type_maps(id) else {
        fnv(&mut h, b"none");
        return h;
    };
    fnv(&mut h, &[u8::from(ctm.verified())]);
    for mi in 0..ctm.method_count() {
        fnv(&mut h, &(mi as u32).to_le_bytes());
        match ctm.method(mi) {
            None => fnv(&mut h, b"-"),
            Some(m) => {
                fnv(
                    &mut h,
                    format!(
                        "{}|{}|{}|{:?}|{}|{}",
                        if rows { m.entry_count() } else { 0 },
                        m.safe_for_fast_path(),
                        m.locals_fully_described(),
                        m.fast_path_veto(),
                        m.max_locals(),
                        m.max_stack()
                    )
                    .as_bytes(),
                );
                for i in 0..if rows { m.entry_count() } else { 0 } {
                    let pc = m.pc_at_index(i).unwrap_or(u32::MAX);
                    fnv(&mut h, &pc.to_le_bytes());
                    if let Some(f) = m.frame_map_at(pc) {
                        fnv(&mut h, &f.stack_depth.to_le_bytes());
                        fnv(&mut h, f.locals.as_bytes());
                        fnv(&mut h, &[0xff]);
                        fnv(&mut h, f.stack.as_bytes());
                    }
                }
            }
        }
    }
    h
}

struct Run {
    define: Duration,
    link: Duration,
    defined: usize,
    walked: usize,
    /// class name -> (digest as asked, verdict digest); empty for `Digest::None`.
    digests: BTreeMap<String, (u64, u64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Digest {
    /// Timing run: no digest, the store as the VM builds it (rows dropped).
    None,
    /// Timing run with the rows kept: every walk builds its rows, the cost
    /// each walk paid before verdict-only builders (the A/B arm).
    NoneKeepRows,
    /// Rows kept (`set_drop_rows(false)`), every row digested.
    Rows,
    /// The store as the VM builds it (rows dropped, so verdict-only walks);
    /// the verdicts digested.
    Verdicts,
}

fn run_once(image: &str, names: &[String], maps_at_link: bool, digest: Digest) -> Run {
    let mut cm = ClassManager::new(&[image.to_string()], &[], &[]);
    if matches!(digest, Digest::Rows | Digest::NoneKeepRows) {
        // Compare the rows themselves, not only the verdicts.
        cm.class_store.type_maps().set_drop_rows(false);
    }
    cm.set_type_maps_at_link(true);
    cm.set_define_boot_maps_at_link(maps_at_link);

    let t0 = Instant::now();
    for name in names {
        // A name the image does not hold (none, for a matching JDK) is skipped.
        let _ = cm.load_class(name);
    }
    let define = t0.elapsed();

    let ids: Vec<ClassId> = cm.class_store.iter().map(|c| c.id).collect();
    let t1 = Instant::now();
    let mut walked = 0;
    for &id in &ids {
        if cm.type_maps_pending(id) {
            match cm.publish_pending_type_maps(id) {
                Some(Ok(loads)) => {
                    cm.finish_pending_type_maps(id, loads);
                    walked += 1;
                }
                Some(Err(e)) => panic!("the link of a JDK class refused: {e}"),
                None => {}
            }
        }
    }
    let link = t1.elapsed();

    let mut digests = BTreeMap::new();
    if matches!(digest, Digest::Rows | Digest::Verdicts) {
        for c in cm.class_store.iter() {
            let store = cm.class_store.type_maps();
            let rows = maps_digest(store, c.id, digest == Digest::Rows);
            let verdicts = maps_digest(store, c.id, false);
            digests.insert(c.name.to_string(), (rows, verdicts));
        }
    }
    Run {
        define,
        link,
        defined: ids.len(),
        walked,
        digests,
    }
}

fn stats(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[0], v[v.len() / 2])
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[cfg(windows)]
#[path = "win_sampler/mod.rs"]
mod win_sampler;

#[cfg(windows)]
#[test]
#[ignore = "needs CVM_BOOT_IMAGE"]
fn profile_hello_boot_classes() {
    let Ok(image) = std::env::var("CVM_BOOT_IMAGE") else {
        return;
    };
    let at_link = std::env::var("CVM_BOOT_AT_LINK").is_ok_and(|v| v == "1");
    let names = boot_names();
    let (_, prof) = win_sampler::profile(|| {
        for _ in 0..8 {
            run_once(&image, &names, at_link, Digest::None);
        }
    });
    prof.report(70);
}

fn boot_names() -> Vec<String> {
    let list = include_str!("fixtures/boot_classes_hello.txt");
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore = "needs CVM_BOOT_IMAGE=<jdk>/lib/modules"]
fn define_hello_boot_classes() {
    let Ok(image) = std::env::var("CVM_BOOT_IMAGE") else {
        eprintln!("CVM_BOOT_IMAGE not set; nothing to do");
        return;
    };
    let repeat: usize = std::env::var("CVM_BOOT_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9)
        .max(1);
    // `CVM_BOOT_ALL=1`: the equivalence corpus is every `java.base` class of
    // the image (in path order), not only Hello's boot set.
    let names = if std::env::var("CVM_BOOT_ALL").is_ok_and(|v| v == "1") {
        let reader = cratonvm_reader::jimage::JImageReader::open(&image).expect("open jimage");
        let mut all: Vec<String> = reader
            .iter_entries()
            .expect("entries")
            .into_iter()
            .filter_map(|(p, _, _)| {
                p.strip_prefix("/java.base/")
                    .and_then(|p| p.strip_suffix(".class"))
                    .filter(|p| *p != "module-info")
                    .map(str::to_string)
            })
            .collect();
        all.sort();
        all
    } else {
        boot_names()
    };

    // Equivalence first. With the rows kept, both arms must publish the same
    // maps, row for row; with the VM's store (rows dropped: verdict-only
    // walks) both arms must publish the verdicts the kept-row runs strip to.
    let eager = run_once(&image, &names, false, Digest::Rows);
    let link = run_once(&image, &names, true, Digest::Rows);
    let eager_v = run_once(&image, &names, false, Digest::Verdicts);
    let link_v = run_once(&image, &names, true, Digest::Verdicts);
    let same = |a: &Run, b: &Run, pick: fn(&(u64, u64)) -> u64, what: &str| {
        assert_eq!(
            a.digests.keys().collect::<Vec<_>>(),
            b.digests.keys().collect::<Vec<_>>(),
            "{what}: the same classes are defined"
        );
        let differing: Vec<&String> = a
            .digests
            .iter()
            .filter(|(k, v)| b.digests.get(*k).map(pick) != Some(pick(v)))
            .map(|(k, _)| k)
            .collect();
        assert!(
            differing.is_empty(),
            "{what}: {} classes publish different maps, e.g. {:?}",
            differing.len(),
            &differing[..differing.len().min(8)]
        );
    };
    same(&eager, &link, |d| d.0, "rows, eager vs link");
    same(
        &eager,
        &eager_v,
        |d| d.1,
        "verdicts, kept rows vs dropped (eager)",
    );
    same(
        &link,
        &link_v,
        |d| d.1,
        "verdicts, kept rows vs dropped (link)",
    );
    let with_maps = eager
        .digests
        .values()
        .map(|d| d.0)
        .filter(|&d| {
            let mut none = 0xcbf29ce484222325u64;
            fnv(&mut none, b"none");
            d != none
        })
        .count();
    eprintln!(
        "equivalence: {} classes, {} with maps, identical in both arms",
        eager.digests.len(),
        with_maps
    );

    // Timing: the store as the VM builds it (rows dropped), and the `+rows`
    // arms with every row built (the walks' cost before verdict-only
    // builders). The arms are interleaved round by round, so a burst of host
    // load lands on all of them.
    let arms = [
        ("eager", false, Digest::None),
        ("link", true, Digest::None),
        ("eager+rows", false, Digest::NoneKeepRows),
        ("link+rows", true, Digest::NoneKeepRows),
    ];
    let mut samples: Vec<(Vec<Duration>, Vec<Duration>, Vec<Duration>, (usize, usize))> = arms
        .iter()
        .map(|_| (Vec::new(), Vec::new(), Vec::new(), (0, 0)))
        .collect();
    for _ in 0..repeat {
        for (i, &(_, at_link, digest)) in arms.iter().enumerate() {
            let r = run_once(&image, &names, at_link, digest);
            samples[i].0.push(r.define);
            samples[i].1.push(r.link);
            samples[i].2.push(r.define + r.link);
            samples[i].3 = (r.defined, r.walked);
        }
    }
    for (&(arm, _, _), (defines, links, totals, (defined, walked))) in arms.iter().zip(samples) {
        let (dmin, dmed) = stats(defines);
        let (lmin, lmed) = stats(links);
        let (tmin, tmed) = stats(totals);
        eprintln!(
            "[{arm:10}] {defined} defined, {walked} link walks | define min {:.2} med {:.2} ms | \
             link-walk min {:.2} med {:.2} ms | total min {:.2} med {:.2} ms (n={repeat})",
            ms(dmin),
            ms(dmed),
            ms(lmin),
            ms(lmed),
            ms(tmin),
            ms(tmed),
        );
    }
}
