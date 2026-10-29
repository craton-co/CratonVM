// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 fast boot, the class-path ingestion lever (p5-cp).
//!
//! Builds the boot `ClassPath` and the `ClassManager` over the real JDK
//! runtime image (`$JAVA_HOME/lib/modules`, skipped when unset) the way
//! `vm_init` does, times the ingestion and a batch of lookups, and prints a
//! digest of every answer the image's class path gives: every class's bytes,
//! every resource's bytes, existence and URL answers, the module list, the
//! module-info scan, the package -> module registry, plus misses and globs.
//!
//! The digest is the equivalence check: it must be byte-for-byte the same
//! before and after a change to ingestion or lookup. `CRATONVM_P5CP_DUMP=<file>`
//! writes the full answer listing (one line per question) so two builds can be
//! diffed rather than only their digests compared.
//!
//! Run (release, the numbers mean nothing in debug):
//! `cargo test --release -p cratonvm-classloading --test jimage_boot_ingestion -- --ignored --nocapture`

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cratonvm_classloading::{ClassManager, ClassPath};

fn modules_image() -> Option<PathBuf> {
    let home = std::env::var_os("JAVA_HOME")?;
    let p = PathBuf::from(home).join("lib").join("modules");
    p.is_file().then_some(p)
}

/// FNV-1a 64: stable across builds and platforms.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

struct Answers {
    lines: Vec<String>,
}

impl Answers {
    fn push(&mut self, line: String) {
        self.lines.push(line);
    }

    fn digest(&self) -> u64 {
        let mut h = fnv(b"");
        for l in &self.lines {
            h ^= fnv(l.as_bytes());
            h = h.rotate_left(7).wrapping_mul(0x0100_0000_01b3);
        }
        h
    }
}

fn bytes_answer(b: Option<&[u8]>) -> String {
    match b {
        Some(b) => format!("{}:{:016x}", b.len(), fnv(b)),
        None => "absent".to_string(),
    }
}

/// This thread's CPU cycles (Windows) or CPU time in ns (elsewhere): the
/// figure to compare on a loaded host, where wall time mostly measures the
/// other tenants.
#[cfg(windows)]
fn thread_cpu() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
    }
    let mut c = 0u64;
    // SAFETY: the pseudo-handle of the calling thread and a valid out pointer.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}

#[cfg(not(windows))]
fn thread_cpu() -> u64 {
    // /proc/thread-self/stat utime+stime is too coarse; the wall clock is
    // the fallback (run on an idle host).
    0
}

struct Clock(Instant, u64);

impl Clock {
    fn now() -> Self {
        Clock(Instant::now(), thread_cpu())
    }
    fn elapsed(&self) -> (Duration, u64) {
        (self.0.elapsed(), thread_cpu() - self.1)
    }
}

/// "wall median / cpu median (cpu min)", cpu in millions of cycles.
fn median(v: Vec<(Duration, u64)>) -> String {
    let mut w: Vec<Duration> = v.iter().map(|x| x.0).collect();
    let mut c: Vec<u64> = v.iter().map(|x| x.1).collect();
    w.sort();
    c.sort();
    format!(
        "wall={:?} cpu={:.1}M(min {:.1}M)",
        w[w.len() / 2],
        c[c.len() / 2] as f64 / 1e6,
        c[0] as f64 / 1e6
    )
}

/// Every entry name of the image below its module (`java/lang/String.class`,
/// `jdk/internal/icu/impl/data/icudt76b/uprops.icu`, ...), straight from the
/// reader: the question list, independent of the class path's own indexes.
fn image_entry_names(image: &std::path::Path) -> Vec<String> {
    let reader = cratonvm_reader::JImageReader::open(image).expect("open image");
    let mut names: Vec<String> = reader
        .iter_entries()
        .expect("walk image")
        .into_iter()
        .filter_map(|(path, _, _)| {
            let rest = path.strip_prefix('/')?;
            let (_, r) = rest.split_once('/')?;
            (!r.is_empty()).then(|| r.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn collect_answers(cp: &ClassPath, names: &[String], cm: &ClassManager) -> Answers {
    let mut a = Answers { lines: Vec::new() };
    let mut modules = cp.list_jimage_modules();
    modules.sort();
    a.push(format!("modules {}", modules.join(",")));
    a.push(format!(
        "list_jmod_modules {}",
        cp.list_jmod_modules().join(",")
    ));
    a.push(format!("jimage_class_count {}", cp.jimage_class_count()));
    let mut infos: Vec<String> = cp
        .scan_module_infos()
        .iter()
        .map(|b| bytes_answer(Some(b)))
        .collect();
    infos.sort();
    a.push(format!("module_infos {}", infos.join(",")));
    let mut class_names = cp.list_class_names();
    class_names.sort();
    a.push(format!(
        "class_names {} {:016x}",
        class_names.len(),
        fnv(class_names.join("\n").as_bytes())
    ));
    // The registry's package -> module answers, for every package directory
    // in the image (resource-only ones included) and a few misses.
    let mut packages: Vec<&str> = names
        .iter()
        .filter_map(|n| n.rsplit_once('/').map(|(p, _)| p))
        .collect();
    packages.sort();
    packages.dedup();
    packages.extend(["", "com/example", "java/lang/nope"]);
    let pkgs: Vec<String> = packages
        .iter()
        .map(|p| format!("{p}={:?}", cm.module_registry.module_for_package(p)))
        .collect();
    a.push(format!(
        "registry_packages {} {:016x} modules={}",
        pkgs.len(),
        fnv(pkgs
            .join(
                "
"
            )
            .as_bytes()),
        cm.module_registry.len()
    ));
    let misses = [
        "java/lang/NoSuchP5cp.class",
        "com/example/Nope.class",
        "module-info.class",
        "META-INF/MANIFEST.MF",
        "NoPackage.class",
        "java/lang/",
        "java/lang",
        "jdk/internal/icu/impl/data/nope.icu",
        "../etc/passwd",
        "java/lang/*.class",
        "java/util/*.properties",
        "sun/util/resources/*",
        "META-INF/services/*",
        "*.class",
    ];
    for name in names.iter().map(String::as_str).chain(misses) {
        a.push(format!(
            "res {name} {} contains={} urls={}",
            bytes_answer(cp.find_resource(name).as_deref()),
            cp.contains_resource(name),
            cp.find_all_resource_urls(name).join("|")
        ));
        if let Some(class) = name.strip_suffix(".class") {
            let bytes = cp.find_class(class).ok();
            a.push(format!(
                "class {class} {} image_defines={:?}",
                bytes_answer(bytes.as_deref()),
                cp.runtime_image_defines_class(class)
            ));
        }
    }
    a
}

#[test]
#[ignore]
fn p5cp_real_image_ingestion_lever() {
    let Some(image) = modules_image() else {
        eprintln!("p5cp: JAVA_HOME/lib/modules missing, skipped");
        return;
    };
    let boot = vec![image.to_string_lossy().into_owned()];
    let app_dir = std::env::temp_dir().join(format!("p5cp-app-{}", std::process::id()));
    std::fs::create_dir_all(&app_dir).unwrap();
    let app = vec![app_dir.to_string_lossy().into_owned()];
    let reps: usize = std::env::var("CRATONVM_P5CP_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    // Warm the page cache once so every rep measures the same thing.
    drop(ClassPath::new(&boot));

    let mut t_cp = Vec::new();
    let mut t_drop = Vec::new();
    for _ in 0..reps {
        let t = Clock::now();
        let cp = ClassPath::new(&boot);
        t_cp.push(t.elapsed());
        drop(cp);
    }
    let mut t_add = Vec::new();
    for _ in 0..reps {
        let mut cp = ClassPath::new(&[]);
        let t = Clock::now();
        cp.add_jimage(&image).unwrap();
        t_add.push(t.elapsed());
        let t = Clock::now();
        drop(cp);
        t_drop.push(t.elapsed());
    }
    eprintln!(
        "p5cp breakdown add_jimage {} drop {}",
        median(t_add),
        median(std::mem::take(&mut t_drop))
    );
    let mut t_cm = Vec::new();
    for _ in 0..reps {
        let t = Clock::now();
        let cm = ClassManager::with_app_fat_jar_emulation(&boot, &[], &app, true);
        t_cm.push(t.elapsed());
        drop(cm);
    }

    // Breakdown: the mapped reader alone, then the bare walks.
    let open_mapped = || {
        let file = std::fs::File::open(&image).unwrap();
        // SAFETY: read-only map of the runtime image, as the class path does.
        let m = unsafe { memmap2::MmapOptions::new().map(&file) }.unwrap();
        let shared: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> = std::sync::Arc::new(m);
        cratonvm_reader::JImageReader::from_shared(shared).unwrap()
    };
    let (mut t_open, mut t_parts, mut t_parts_warm) = (Vec::new(), Vec::new(), Vec::new());
    let mut t_offsets = Vec::new();
    let probe = open_mapped();
    eprintln!(
        "p5cp image resources={} version={:?}",
        probe.resource_count(),
        probe.version()
    );
    drop(probe);
    for _ in 0..reps {
        let t = Clock::now();
        let r = open_mapped();
        t_open.push(t.elapsed());
        let t = Clock::now();
        let mut n = 0usize;
        r.for_each_entry_parts(|m, p, b, e| n += m.len() + p.len() + b.len() + e.len())
            .unwrap();
        t_parts.push(t.elapsed());
        let t = Clock::now();
        r.for_each_entry_parts(|m, p, b, e| n += m.len() + p.len() + b.len() + e.len())
            .unwrap();
        t_parts_warm.push(t.elapsed());
        let t = Clock::now();
        r.for_each_entry_offsets(|e| n += e.module as usize)
            .unwrap();
        t_offsets.push(t.elapsed());
        std::hint::black_box(n);
    }
    eprintln!(
        "p5cp breakdown open_mapped {} walk_parts(cold map) {} walk_parts(warm map) {} walk_offsets {}",
        median(t_open),
        median(t_parts),
        median(t_parts_warm),
        median(t_offsets)
    );

    let names = image_entry_names(&image);
    let cp = ClassPath::new(&boot);
    // The lookups a boot makes: classes in image order, a fixed sample.
    let classes: Vec<&str> = names
        .iter()
        .filter_map(|n| n.strip_suffix(".class"))
        .filter(|c| *c != "module-info" && !c.ends_with("/module-info"))
        .step_by(20)
        .collect();
    let resources: Vec<&str> = names
        .iter()
        .filter(|n| !n.ends_with(".class"))
        .map(String::as_str)
        .collect();
    let mut t_find = Vec::new();
    for _ in 0..reps {
        let t = Clock::now();
        for c in &classes {
            assert!(cp.find_class(c).is_ok(), "{c}");
        }
        t_find.push(t.elapsed());
    }
    // The image's own lookup for the same classes (module from the path the
    // walk saw), to separate the class path's overhead from the reader's.
    let raw = cratonvm_reader::JImageReader::open(&image).expect("open image");
    let raw_paths: Vec<String> = raw
        .iter_entries()
        .unwrap()
        .into_iter()
        .map(|(p, _, _)| p)
        .filter(|p| p.ends_with(".class") && !p.ends_with("/module-info.class"))
        .collect();
    let mut raw_paths = raw_paths;
    raw_paths.sort();
    let raw_paths: Vec<&String> = raw_paths.iter().step_by(20).collect();
    let mut t_raw = Vec::new();
    for _ in 0..reps {
        let t = Clock::now();
        for p in &raw_paths {
            std::hint::black_box(raw.find_resource(p).unwrap());
        }
        t_raw.push(t.elapsed());
    }
    eprintln!(
        "p5cp reader find_resource x{} {}",
        raw_paths.len(),
        median(t_raw)
    );
    let mut t_res = Vec::new();
    for _ in 0..reps {
        let t = Clock::now();
        for r in &resources {
            std::hint::black_box(cp.find_resource(r));
        }
        t_res.push(t.elapsed());
    }

    let cm = ClassManager::with_app_fat_jar_emulation(&boot, &[], &app, true);
    let answers = collect_answers(&cp, &names, &cm);
    eprintln!(
        "p5cp ingest ClassPath::new {} ClassManager::new {} \
         find_class x{} {} find_resource x{} {} (reps={reps})",
        median(t_cp),
        median(t_cm),
        classes.len(),
        median(t_find),
        resources.len(),
        median(t_res),
    );
    eprintln!(
        "p5cp answers={} digest={:016x}",
        answers.lines.len(),
        answers.digest()
    );
    if let Some(out) = std::env::var_os("CRATONVM_P5CP_DUMP") {
        let mut f = std::fs::File::create(out).unwrap();
        for l in &answers.lines {
            writeln!(f, "{l}").unwrap();
        }
    }
    let _ = std::fs::remove_dir_all(&app_dir);
}
