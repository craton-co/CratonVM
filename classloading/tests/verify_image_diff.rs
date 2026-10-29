// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential harness for Pass 3 (SBR round 5, lane `verify`).
//!
//! Runs `verify_class_bytecode_with_strictness` over **every class of a JDK
//! `lib/modules` image**, twice (as a trusted bootstrap class, lenient; and as
//! an application class, strict), and writes one line per (class, mode):
//!
//! ```text
//! <class>\t<T|A>\tOK\t<fnv64 of every published type map>
//! <class>\t<T|A>\tERR\t<LinkageError display>
//! ```
//!
//! A verifier change that must be behaviour-identical is checked by running the
//! harness on the old and the new source and `cmp`-ing the two files: accept /
//! reject, the exact message and every published oop map must match. The
//! hierarchy is a fixed map built from the image itself, so the result does not
//! depend on load order.
//!
//! `#[ignore]`d: it needs a JDK. Run with
//!
//! ```text
//! CVM_VERIFY_IMAGE=$JAVA_HOME/lib/modules CVM_VERIFY_OUT=/tmp/verify.tsv \
//!   [CVM_VERIFY_REPEAT=3] cargo test --release -p cratonvm-classloading \
//!   --test verify_image_diff -- --ignored --nocapture
//! ```
//!
//! The per-mode verify-only time is printed (parse and decode are outside the
//! timed region).

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_classloading::class_origin::ClassOrigin;
use cratonvm_classloading::type_maps::TypeMapStore;
use cratonvm_classloading::verifier::verify_class_bytecode_with_strictness;
use cratonvm_classloading::vtype::ClassHierarchy;
use cratonvm_classloading::{Class, ClassId, ClassLoaderId, ClassState};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::jimage::JImageReader;
use cratonvm_reader::{force_decode_all, read_class};

struct Info {
    super_class: Option<Arc<str>>,
    interfaces: Vec<Arc<str>>,
    is_interface: bool,
}

struct ImageHierarchy {
    map: HashMap<Arc<str>, Info>,
    store: TypeMapStore,
    subclass_calls: AtomicU64,
    common_calls: AtomicU64,
    interface_calls: AtomicU64,
}

impl ImageHierarchy {
    fn walk_is_sub(&self, child: &str, parent: &str, depth: usize) -> bool {
        if child == parent {
            return true;
        }
        if depth > 64 {
            return false;
        }
        let Some(info) = self.map.get(child) else {
            return false;
        };
        if let Some(s) = &info.super_class {
            if self.walk_is_sub(s, parent, depth + 1) {
                return true;
            }
        }
        info.interfaces
            .iter()
            .any(|i| self.walk_is_sub(i, parent, depth + 1))
    }
}

impl ClassHierarchy for ImageHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        self.subclass_calls.fetch_add(1, Ordering::Relaxed);
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        if !self.map.contains_key(child) || !self.map.contains_key(parent) {
            return true; // unresolved: optimistic, like the VM's adapter
        }
        self.walk_is_sub(child, parent, 0)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.map
            .get(child)
            .and_then(|i| i.super_class.as_deref())
            .is_some_and(|s| s == parent)
            || (parent == "java/lang/Object" && !self.map.contains_key(child))
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.common_calls.fetch_add(1, Ordering::Relaxed);
        if a == b {
            return a.to_string();
        }
        let mut cur: Option<&str> = Some(a);
        let mut depth = 0;
        while let Some(c) = cur {
            if depth > 64 {
                break;
            }
            depth += 1;
            if self.walk_is_sub(b, c, 0) {
                return c.to_string();
            }
            cur = self.map.get(c).and_then(|i| i.super_class.as_deref());
        }
        "java/lang/Object".to_string()
    }

    fn is_interface(&self, name: &str) -> bool {
        self.interface_calls.fetch_add(1, Ordering::Relaxed);
        self.map.get(name).is_some_and(|i| i.is_interface)
    }

    fn type_map_store(&self) -> Option<&TypeMapStore> {
        Some(&self.store)
    }
}

fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn maps_digest(store: &TypeMapStore, id: ClassId) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let Some(ctm) = store.class_type_maps(id) else {
        fnv(&mut h, b"none");
        return h;
    };
    for mi in 0..ctm.method_count() {
        fnv(&mut h, &(mi as u32).to_le_bytes());
        match ctm.method(mi) {
            None => fnv(&mut h, b"-"),
            Some(m) => {
                fnv(
                    &mut h,
                    format!(
                        "{}|{}|{}|{:?}|{}|{}",
                        m.entry_count(),
                        m.safe_for_fast_path(),
                        m.locals_fully_described(),
                        m.fast_path_veto(),
                        m.max_locals(),
                        m.max_stack()
                    )
                    .as_bytes(),
                );
                for i in 0..m.entry_count() {
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

fn make_class(id: ClassId, loader: ClassLoaderId, cf: ClassFile) -> Class {
    let ClassFile {
        version,
        constant_pool,
        access_flags,
        this_class,
        methods,
        ..
    } = cf;
    Class {
        id,
        loader_id: loader,
        name: this_class,
        source_file: None,
        version,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool,
        access_flags,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: ClassOrigin::VmInternal,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: Arc::new(AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    }
}

fn read_decoded(bytes: &[u8]) -> Option<ClassFile> {
    let mut parsed = read_class(bytes).ok()?;
    for method in parsed.methods.iter_mut() {
        force_decode_all(&mut method.attributes, &parsed.constant_pool).ok()?;
    }
    Some(parsed)
}

#[test]
#[ignore = "needs CVM_VERIFY_IMAGE=<jdk>/lib/modules"]
fn verify_every_image_class() {
    let Ok(image) = std::env::var("CVM_VERIFY_IMAGE") else {
        eprintln!("CVM_VERIFY_IMAGE not set; nothing to do");
        return;
    };
    let repeat: usize = std::env::var("CVM_VERIFY_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let limit: usize = std::env::var("CVM_VERIFY_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let reader = JImageReader::open(&image).expect("open jimage");
    let mut entries: Vec<(String, u64, u64)> = reader
        .iter_entries()
        .expect("entries")
        .into_iter()
        .filter(|(p, _, _)| p.ends_with(".class") && !p.ends_with("module-info.class"))
        .collect();
    entries.sort();
    entries.truncate(limit);

    // Pass 1: hierarchy.
    let mut map: HashMap<Arc<str>, Info> = HashMap::new();
    for (path, _, _) in &entries {
        let Some(bytes) = reader.find_resource(path).ok().flatten() else {
            continue;
        };
        let Ok(cf) = read_class(&bytes) else { continue };
        map.insert(
            cf.this_class.clone(),
            Info {
                super_class: cf.super_class.clone(),
                interfaces: cf.interfaces.clone(),
                is_interface: cf.is_interface(),
            },
        );
    }
    eprintln!("hierarchy: {} classes", map.len());
    let hierarchy = ImageHierarchy {
        map,
        store: TypeMapStore::new(),
        subclass_calls: AtomicU64::new(0),
        common_calls: AtomicU64::new(0),
        interface_calls: AtomicU64::new(0),
    };

    let mut out_lines: Vec<String> = Vec::new();
    let next_id = AtomicU32::new(1);
    let mut totals = [Duration::ZERO; 2];
    let mut rep_totals: Vec<[Duration; 2]> = vec![[Duration::ZERO; 2]; repeat];
    let deferred = std::env::var("CVM_VERIFY_DEFERRED").is_ok_and(|v| v == "1");
    let mut deferred_total = Duration::ZERO;
    let mut methods_verified = 0usize;
    let mut classes = 0usize;
    for rep in 0..repeat {
        for (path, _, _) in &entries {
            let Some(bytes) = reader.find_resource(path).ok().flatten() else {
                continue;
            };
            for (mi, (tag, loader)) in [
                ("T", ClassLoaderId::Bootstrap),
                ("A", ClassLoaderId::Application),
            ]
            .into_iter()
            .enumerate()
            {
                let Some(cf) = read_decoded(&bytes) else {
                    if rep == 0 {
                        out_lines.push(format!("{path}\t{tag}\tUNREADABLE\t"));
                    }
                    continue;
                };
                let id = ClassId::new(next_id.fetch_add(1, Ordering::Relaxed));
                let class = make_class(id, loader, cf);
                if rep == 0 && mi == 0 {
                    classes += 1;
                    methods_verified += class.methods.len();
                }
                let t0 = Instant::now();
                let r = verify_class_bytecode_with_strictness(&class, &hierarchy, false);
                let took = t0.elapsed();
                totals[mi] += took;
                rep_totals[rep][mi] += took;
                if rep == 0 {
                    match r {
                        Ok(()) => out_lines.push(format!(
                            "{}\t{tag}\tOK\t{:016x}",
                            class.name,
                            maps_digest(&hierarchy.store, id)
                        )),
                        Err(e) => out_lines.push(format!(
                            "{}\t{tag}\tERR\t{}",
                            class.name,
                            e.to_string().replace('\n', " ")
                        )),
                    }
                }
            }
            // The deferred-maps walk (user-loader classes: type maps without a
            // verdict), `CVM_VERIFY_DEFERRED=1`.
            if deferred {
                if let Some(cf) = read_decoded(&bytes) {
                    let id = ClassId::new(next_id.fetch_add(1, Ordering::Relaxed));
                    let class = make_class(id, ClassLoaderId::Bootstrap, cf);
                    let t0 = Instant::now();
                    cratonvm_classloading::verifier::publish_deferred_class_type_maps(
                        &class, &hierarchy,
                    );
                    deferred_total += t0.elapsed();
                    if rep == 0 {
                        out_lines.push(format!(
                            "{}\tD\tOK\t{:016x}",
                            class.name,
                            maps_digest(&hierarchy.store, id)
                        ));
                    }
                }
            }
        }
    }
    eprintln!(
        "classes {classes} methods {methods_verified} repeat {repeat}: verify-only trusted {:.1} ms/pass, strict {:.1} ms/pass; hierarchy calls (all passes): is_subclass {} common {} is_interface {}",
        totals[0].as_secs_f64() * 1e3 / repeat as f64,
        totals[1].as_secs_f64() * 1e3 / repeat as f64,
        hierarchy.subclass_calls.load(Ordering::Relaxed),
        hierarchy.common_calls.load(Ordering::Relaxed),
        hierarchy.interface_calls.load(Ordering::Relaxed),
    );
    {
        // Best pass of each mode (a pass that lost the CPU to something else
        // is the slow one, never the fast one); passes after the first only,
        // when there are any.
        let from = if repeat > 1 { 1 } else { 0 };
        let best = |mode: usize| {
            rep_totals[from..]
                .iter()
                .map(|t| t[mode])
                .min()
                .unwrap_or(Duration::ZERO)
        };
        eprintln!(
            "BEST pass: trusted {:.1} ms, strict {:.1} ms",
            best(0).as_secs_f64() * 1e3,
            best(1).as_secs_f64() * 1e3
        );
    }
    if deferred {
        eprintln!(
            "deferred-maps walk: {:.1} ms/pass",
            deferred_total.as_secs_f64() * 1e3 / repeat as f64
        );
    }
    if let Ok(out) = std::env::var("CVM_VERIFY_OUT") {
        let mut f = std::fs::File::create(out).expect("create out");
        for l in &out_lines {
            writeln!(f, "{l}").unwrap();
        }
    }
    let errs = out_lines.iter().filter(|l| l.contains("\tERR\t")).count();
    eprintln!("{} lines, {} ERR", out_lines.len(), errs);
}

// ---------------------------------------------------------------------------
// Mutants: malformed bytecode with a verdict to preserve
// ---------------------------------------------------------------------------

/// A hierarchy that answers as `inner` but publishes into its own store.
struct StoreView<'a> {
    inner: &'a ImageHierarchy,
    store: &'a TypeMapStore,
}

impl ClassHierarchy for StoreView<'_> {
    fn is_subclass(&self, c: &str, p: &str) -> bool {
        self.inner.is_subclass(c, p)
    }
    fn is_direct_superclass(&self, c: &str, p: &str) -> bool {
        self.inner.is_direct_superclass(c, p)
    }
    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.inner.common_superclass(a, b)
    }
    fn is_interface(&self, n: &str) -> bool {
        self.inner.is_interface(n)
    }
    fn type_map_store(&self) -> Option<&TypeMapStore> {
        Some(self.store)
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One deterministic mutation of a random method's code bytes inside `raw`.
fn mutate(raw: &[u8], cf: &ClassFile, rng: &mut Rng) -> Option<Vec<u8>> {
    let with_code: Vec<&cratonvm_reader::attribute::CodeAttribute> = cf
        .methods
        .iter()
        .filter_map(|m| m.code())
        .filter(|c| c.code.len() >= 4)
        .collect();
    if with_code.is_empty() {
        return None;
    }
    let code_attr = with_code[rng.below(with_code.len())];
    let mut code: &[u8] = &code_attr.code[..];
    // One mutant in three corrupts the method's StackMapTable instead of its
    // bytecode (the declared-frame path: adoption, merges, handler frames).
    if rng.below(3) == 0 {
        if let Some(entries) = code_attr.attributes.iter().find_map(|a| match a {
            cratonvm_reader::attribute::Attribute::StackMapTable { entries } => Some(&entries[..]),
            _ => None,
        }) {
            if entries.len() >= 2 {
                code = entries;
            }
        }
    }
    let probe = &code[..code.len().min(24)];
    let base = find_sub(raw, probe)?;
    let mut out = raw.to_vec();
    let edits = 1 + rng.below(3);
    for _ in 0..edits {
        let at = base + rng.below(code.len());
        if at >= out.len() {
            continue;
        }
        match rng.below(7) {
            0 => out[at] = rng.next() as u8,
            1 => out[at] = out[at].wrapping_add(1),
            2 => out[at] = out[at].wrapping_sub(1),
            3 => out[at] = 0,
            4 => {
                let other = base + rng.below(code.len());
                if other < out.len() {
                    out[at] = out[other];
                }
            }
            5 => {
                if at + 1 < out.len() {
                    out.swap(at, at + 1);
                }
            }
            _ => out[at] ^= 1 << rng.below(8),
        }
    }
    Some(out)
}

#[test]
#[ignore = "needs CVM_VERIFY_IMAGE=<jdk>/lib/modules and CVM_VERIFY_MUTANTS=<per class>"]
fn verify_mutated_image_classes() {
    let Ok(image) = std::env::var("CVM_VERIFY_IMAGE") else {
        eprintln!("CVM_VERIFY_IMAGE not set; nothing to do");
        return;
    };
    let per_class: usize = std::env::var("CVM_VERIFY_MUTANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stride: usize = std::env::var("CVM_VERIFY_STRIDE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    if per_class == 0 {
        return;
    }
    let reader = JImageReader::open(&image).expect("open jimage");
    let mut entries: Vec<(String, u64, u64)> = reader
        .iter_entries()
        .expect("entries")
        .into_iter()
        .filter(|(p, _, _)| p.ends_with(".class") && !p.ends_with("module-info.class"))
        .collect();
    entries.sort();

    let mut map: HashMap<Arc<str>, Info> = HashMap::new();
    for (path, _, _) in &entries {
        let Some(bytes) = reader.find_resource(path).ok().flatten() else {
            continue;
        };
        let Ok(cf) = read_class(&bytes) else { continue };
        map.insert(
            cf.this_class.clone(),
            Info {
                super_class: cf.super_class.clone(),
                interfaces: cf.interfaces.clone(),
                is_interface: cf.is_interface(),
            },
        );
    }
    let hierarchy = ImageHierarchy {
        map,
        store: TypeMapStore::new(),
        subclass_calls: AtomicU64::new(0),
        common_calls: AtomicU64::new(0),
        interface_calls: AtomicU64::new(0),
    };

    std::panic::set_hook(Box::new(|_| {}));
    let mut lines: Vec<String> = Vec::new();
    let next_id = AtomicU32::new(5_000_000);
    let mut store = TypeMapStore::new();
    let mut since_store = 0usize;
    let mut total = Duration::ZERO;
    let (mut ok, mut err, mut unreadable, mut panics) = (0usize, 0usize, 0usize, 0usize);
    for (ci, (path, _, _)) in entries.iter().enumerate() {
        if ci % stride != 0 {
            continue;
        }
        let Some(raw) = reader.find_resource(path).ok().flatten() else {
            continue;
        };
        let Some(cf) = read_decoded(&raw) else {
            continue;
        };
        let mut rng = Rng(0x9E3779B97F4A7C15 ^ ((ci as u64 + 1).wrapping_mul(0xBF58476D1CE4E5B9)));
        for m in 0..per_class {
            let Some(mutant) = mutate(&raw, &cf, &mut rng) else {
                continue;
            };
            for (tag, loader) in [
                ("T", ClassLoaderId::Bootstrap),
                ("A", ClassLoaderId::Application),
            ] {
                let Some(cf2) = read_decoded(&mutant) else {
                    if tag == "T" {
                        unreadable += 1;
                        lines.push(format!("{path}#{m}\t-\tUNREADABLE\t"));
                    }
                    continue;
                };
                since_store += 1;
                if since_store > 3000 {
                    store = TypeMapStore::new();
                    since_store = 0;
                }
                let id = ClassId::new(next_id.fetch_add(1, Ordering::Relaxed));
                let class = make_class(id, loader, cf2);
                let view = StoreView {
                    inner: &hierarchy,
                    store: &store,
                };
                let t0 = Instant::now();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    verify_class_bytecode_with_strictness(&class, &view, false)
                }));
                total += t0.elapsed();
                match r {
                    Ok(Ok(())) => {
                        ok += 1;
                        lines.push(format!(
                            "{path}#{m}\t{tag}\tOK\t{:016x}",
                            maps_digest(&store, id)
                        ));
                    }
                    Ok(Err(e)) => {
                        err += 1;
                        lines.push(format!(
                            "{path}#{m}\t{tag}\tERR\t{}",
                            e.to_string().replace('\n', " ")
                        ));
                    }
                    Err(_) => {
                        panics += 1;
                        lines.push(format!("{path}#{m}\t{tag}\tPANIC\t"));
                    }
                }
            }
        }
    }
    let _ = std::panic::take_hook();
    eprintln!(
        "mutants: ok {ok} err {err} unreadable {unreadable} panic {panics}; verify-only {:.1} ms",
        total.as_secs_f64() * 1e3
    );
    if let Ok(out) = std::env::var("CVM_VERIFY_MUT_OUT") {
        let mut f = std::fs::File::create(out).expect("create out");
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
    }
}
