// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z8/fm (2026-10-13): what one loaded class costs the allocator, and which
//! of its parts cost the most. Page:
//! `docs/internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.
//!
//! z7/rs measured ~20 KB of resident memory per loaded class (HotSpot: ~4.7
//! KB). This test loads the first `N` `java/util/**` and `java/lang/**`
//! classes of the JDK's `java.base` through a bare `ClassManager` (load and
//! link, no `<clinit>`, no VM), counts the bytes the global allocator holds
//! before and after, and then walks the loaded `Class`es to attribute the
//! shallow sizes it can name. Ignored by default (it needs `JAVA_HOME` with
//! `jmods/`); run with
//! `cargo test -p cratonvm-classloading --test zr8_fm_class_metadata_census -- --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

/// zr z11/md: net live BYTES per allocation size (index = size, the last
/// bucket = everything larger), so the deep census can name what its walk
/// cannot reach (hash tables are large power-of-two blocks, an `Arc<str>` is
/// 16 + len, ...).
/// Exact sizes up to 4096; above that one bucket per power of two
/// (`4096 + k` holds sizes in `(2^(11+k), 2^(12+k)]`).
const EXACT: usize = 4096;
const SIZE_BUCKETS: usize = EXACT + 1 + 24;
static BY_SIZE: [AtomicIsize; SIZE_BUCKETS] = [const { AtomicIsize::new(0) }; SIZE_BUCKETS];

fn bucket(size: usize) -> usize {
    if size <= EXACT {
        size
    } else {
        let log = usize::BITS - (size - 1).leading_zeros(); // ceil(log2)
        (EXACT + log as usize - 12).min(SIZE_BUCKETS - 1)
    }
}

fn note(size: usize, delta: isize) {
    LIVE.fetch_add(delta, Ordering::Relaxed);
    BY_SIZE[bucket(size)].fetch_add(delta, Ordering::Relaxed);
}

// SAFETY: forwards to `System` and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size(), layout.size() as isize);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(layout.size(), -(layout.size() as isize));
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size(), layout.size() as isize);
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(layout.size(), -(layout.size() as isize));
        note(new_size, new_size as isize);
        System.realloc(ptr, layout, new_size)
    }
}

fn by_size_snapshot() -> Vec<isize> {
    BY_SIZE.iter().map(|a| a.load(Ordering::Relaxed)).collect()
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
#[ignore]
fn zr8_fm_per_class_allocator_cost_and_its_largest_parts() {
    use cratonvm_classloading::ClassManager;
    use cratonvm_reader::attribute::LazyAttribute;
    use std::collections::HashSet;

    let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
        eprintln!("JAVA_HOME unset: skipped");
        return;
    };
    let jmods = std::path::PathBuf::from(home).join("jmods");
    let base = jmods.join("java.base.jmod");
    if !base.exists() {
        eprintln!("no jmods/java.base.jmod: skipped");
        return;
    }
    let n: usize = cratonvm_types::flags::runtime_var("ZR8FM_CLASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);

    // The class names, straight from the JMOD's central directory.
    let bytes = std::fs::read(&base).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes[4..])).unwrap();
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index_raw(i).ok().map(|e| e.name().to_string()))
        .filter_map(|e| e.strip_prefix("classes/").map(str::to_string))
        .filter(|e| {
            (e.starts_with("java/util/") || e.starts_with("java/lang/"))
                && e.ends_with(".class")
                && !e.contains("module-info")
        })
        .map(|e| e.trim_end_matches(".class").to_string())
        .collect();
    names.sort();
    drop(zip);
    drop(bytes);

    let boot_cp = vec![base.to_string_lossy().into_owned()];
    let mut cm = ClassManager::new(&boot_cp, &[], &[]);
    // Object, String, Class and friends first, outside the measurement.
    for warm in ["java/lang/Object", "java/lang/String", "java/lang/Class"] {
        let _ = cm.load_class(warm);
    }
    let classes_before = cm.class_store.len();
    let live_before = LIVE.load(Ordering::Relaxed);
    let mut loaded = 0usize;
    for name in names.iter().take(n) {
        if cm.load_class(name).is_ok() {
            loaded += 1;
        }
    }
    let live_after = LIVE.load(Ordering::Relaxed);
    let classes_after = cm.class_store.len();
    let grown = classes_after.saturating_sub(classes_before).max(1);
    let delta = live_after - live_before;
    eprintln!(
        "zr8fm-census asked={} loaded_ok={loaded} classes_grown={grown} live_delta_b={delta} per_class_b={}",
        n.min(names.len()),
        delta / grown as isize
    );

    // Shallow attribution over every class in the store.
    let cpe = std::mem::size_of::<cratonvm_reader::constant_pool::ConstantPoolEntry>();
    let lazy = std::mem::size_of::<LazyAttribute>();
    let meth = std::mem::size_of::<cratonvm_reader::method::ClassFileMethod>();
    let fld = std::mem::size_of::<cratonvm_reader::field::ClassFileField>();
    let class = std::mem::size_of::<cratonvm_classloading::Class>();
    let (mut n_classes, mut cp_slots, mut methods, mut fields, mut attrs) = (0usize, 0, 0, 0, 0);
    let (mut decoded_attrs, mut src_bytes, mut cache_bytes) = (0usize, 0usize, 0usize);
    let mut seen_src: HashSet<usize> = HashSet::new(); // addr-keyed: host buffer starts, test-local, all held alive while the set lives
    for c in cm.class_store.iter() {
        n_classes += 1;
        cp_slots += c.constant_pool.len();
        methods += c.methods.len();
        fields += c.fields.len();
        for a in c
            .methods
            .iter()
            .flat_map(|m| m.attributes.iter())
            .chain(c.fields.iter().flat_map(|f| f.attributes.iter()))
        {
            attrs += 1;
            match a {
                LazyAttribute::Raw(raw) => {
                    let source = &raw.source;
                    let key = source.as_ref().as_ptr() as usize;
                    if seen_src.insert(key) {
                        src_bytes += source.len();
                    }
                }
                LazyAttribute::Decoded(_) => decoded_attrs += 1,
            }
        }
    }
    for b in cm.class_bytes_cache.values() {
        cache_bytes += b.len();
    }
    eprintln!(
        "zr8fm-census-sizes size_of: Class={class} ConstantPoolEntry={cpe} ClassFileMethod={meth} ClassFileField={fld} LazyAttribute={lazy}"
    );
    eprintln!(
        "zr8fm-census-parts classes={n_classes} cp_slots={cp_slots} cp_b={} methods={methods} methods_b={} fields={fields} fields_b={} attrs={attrs} decoded_attrs={decoded_attrs} attrs_b={} retained_class_file_b={src_bytes} class_bytes_cache_b={cache_bytes} class_structs_b={}",
        cp_slots * cpe,
        methods * meth,
        fields * fld,
        attrs * lazy,
        n_classes * class,
    );
    assert!(loaded > 0, "positive control: classes loaded");
}

// ---------------------------------------------------------------------------
// zr z11/md (2026-10-13): the deep census. Page:
// `docs/internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.
//
// z8/fm's census named ~11 KB of the 25.5 KB per class and left ~14 KB
// unattributed. This one walks every container by CAPACITY (what the
// allocator really holds), descends into decoded `Code` bodies and the
// annotation trees, counts each distinct class-file buffer once by address,
// adds the `ClassManager` vtable descriptors, and splits the live delta into
// the reader's share (a parse plus `force_decode_all` of the same bytes, kept
// alive) and the class manager's share (everything `load_class` adds on top).
// ---------------------------------------------------------------------------

mod deep {
    use cratonvm_reader::attribute::{Annotation, Attribute, ElementValue, LazyAttribute};
    use std::collections::BTreeMap;
    use std::mem::size_of;

    pub type Parts = BTreeMap<&'static str, usize>;

    fn add(p: &mut Parts, k: &'static str, b: usize) {
        *p.entry(k).or_default() += b;
    }

    fn vec_b<T>(v: &Vec<T>) -> usize {
        v.capacity() * size_of::<T>()
    }

    fn annotation(p: &mut Parts, a: &Annotation) {
        add(p, "annotations_deep", vec_b(&a.element_value_pairs));
        for pair in &a.element_value_pairs {
            element(p, &pair.value);
        }
    }

    fn element(p: &mut Parts, v: &ElementValue) {
        match v {
            ElementValue::AnnotationValue(a) => annotation(p, a),
            ElementValue::Array(items) => {
                add(p, "annotations_deep", vec_b(items));
                for i in items {
                    element(p, i);
                }
            }
            _ => {}
        }
    }

    /// The heap an `Attribute` owns beyond its own slot (the slot is counted
    /// by the vector that holds it).
    pub fn attribute(p: &mut Parts, a: &Attribute) {
        match a {
            Attribute::Code(c) => {
                add(p, "code_exception_tables", vec_b(&c.exception_table));
                add(p, "code_nested_attr_slots", vec_b(&c.attributes));
                for n in &c.attributes {
                    attribute(p, n);
                }
            }
            Attribute::LineNumberTable(v) => add(p, "line_number_tables", vec_b(v)),
            Attribute::LocalVariableTable(v) => add(p, "local_variable_tables", vec_b(v)),
            Attribute::LocalVariableTypeTable(v) => add(p, "local_variable_tables", vec_b(v)),
            Attribute::Exceptions { exception_indices } => {
                add(p, "exceptions_attrs", vec_b(exception_indices))
            }
            Attribute::RuntimeVisibleAnnotations(v) | Attribute::RuntimeInvisibleAnnotations(v) => {
                add(p, "annotations_deep", vec_b(v));
                for x in v {
                    annotation(p, x);
                }
            }
            Attribute::RuntimeVisibleParameterAnnotations(v)
            | Attribute::RuntimeInvisibleParameterAnnotations(v) => {
                add(p, "annotations_deep", vec_b(v));
                for inner in v {
                    add(p, "annotations_deep", vec_b(inner));
                    for x in inner {
                        annotation(p, x);
                    }
                }
            }
            Attribute::RuntimeVisibleTypeAnnotations(v)
            | Attribute::RuntimeInvisibleTypeAnnotations(v) => {
                add(p, "annotations_deep", vec_b(v));
                for t in v {
                    add(
                        p,
                        "annotations_deep",
                        vec_b(&t.target_info) + vec_b(&t.type_path),
                    );
                    annotation(p, &t.annotation);
                }
            }
            Attribute::AnnotationDefault(e) => element(p, e),
            Attribute::MethodParameters(v) => add(p, "method_parameters", vec_b(v)),
            _ => {}
        }
    }

    pub fn lazy_vec(p: &mut Parts, slot_key: &'static str, v: &Vec<LazyAttribute>) {
        add(p, slot_key, vec_b(v));
        for a in v {
            if let LazyAttribute::Decoded(d) = a {
                attribute(p, d);
            }
        }
    }

    /// The kind histogram's key.
    pub fn kind(a: &Attribute) -> &'static str {
        match a {
            Attribute::Code(_) => "Code",
            Attribute::LineNumberTable(_) => "LineNumberTable",
            Attribute::LocalVariableTable(_) => "LocalVariableTable",
            Attribute::LocalVariableTypeTable(_) => "LocalVariableTypeTable",
            Attribute::StackMapTable { .. } => "StackMapTable",
            Attribute::Signature(_) => "Signature",
            Attribute::Exceptions { .. } => "Exceptions",
            Attribute::RuntimeVisibleAnnotations(_) => "RuntimeVisibleAnnotations",
            Attribute::RuntimeInvisibleAnnotations(_) => "RuntimeInvisibleAnnotations",
            Attribute::MethodParameters(_) => "MethodParameters",
            Attribute::ConstantValue { .. } => "ConstantValue",
            Attribute::Deprecated => "Deprecated",
            Attribute::Synthetic => "Synthetic",
            _ => "other",
        }
    }
}

#[test]
#[ignore]
fn zr11_md_deep_per_class_census() {
    use cratonvm_classloading::ClassManager;
    use cratonvm_reader::attribute::{force_decode_all, Attribute, LazyAttribute};
    use std::collections::{BTreeMap, HashSet};
    use std::mem::size_of;

    let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
        eprintln!("JAVA_HOME unset: skipped");
        return;
    };
    let base = std::path::PathBuf::from(home)
        .join("jmods")
        .join("java.base.jmod");
    if !base.exists() {
        eprintln!("no jmods/java.base.jmod: skipped");
        return;
    }
    let n: usize = cratonvm_types::flags::runtime_var("ZR8FM_CLASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);

    let bytes = std::fs::read(&base).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes[4..])).unwrap();
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index_raw(i).ok().map(|e| e.name().to_string()))
        .filter_map(|e| e.strip_prefix("classes/").map(str::to_string))
        .filter(|e| {
            (e.starts_with("java/util/") || e.starts_with("java/lang/"))
                && e.ends_with(".class")
                && !e.contains("module-info")
        })
        .map(|e| e.trim_end_matches(".class").to_string())
        .collect();
    names.sort();
    names.truncate(n);

    // Phase R: the reader's share. Parse and decode the same class files and
    // keep the results alive, outside any ClassManager. The class-file buffers
    // are allocated BEFORE the window, so this delta excludes them.
    let mut raw: Vec<cratonvm_reader::SharedBytes> = Vec::with_capacity(names.len());
    for name in &names {
        let mut e = zip.by_name(&format!("classes/{name}.class")).unwrap();
        let mut v = Vec::with_capacity(e.size() as usize);
        std::io::Read::read_to_end(&mut e, &mut v).unwrap();
        raw.push(cratonvm_reader::SharedBytes::from(v));
    }
    drop(zip);
    drop(bytes);
    let raw_file_b: usize = raw.iter().map(|b| b.len()).sum();
    let mut kept = Vec::with_capacity(raw.len());
    let before_r = LIVE.load(Ordering::Relaxed);
    for shared in &raw {
        let Ok(mut cf) = cratonvm_reader::read_class_shared(shared.clone()) else {
            continue;
        };
        let _ = force_decode_all(&mut cf.attributes, &cf.constant_pool);
        for m in cf.methods.iter_mut() {
            let _ = force_decode_all(&mut m.attributes, &cf.constant_pool);
        }
        for f in cf.fields.iter_mut() {
            let _ = force_decode_all(&mut f.attributes, &cf.constant_pool);
        }
        kept.push(cf);
    }
    let reader_delta = LIVE.load(Ordering::Relaxed) - before_r;
    let parsed = kept.len().max(1);
    eprintln!(
        "zr11md-reader parsed={parsed} reader_delta_b={reader_delta} per_class_b={} (class file itself, excluded: {} per class)",
        reader_delta / parsed as isize,
        raw_file_b / parsed
    );
    drop(kept);
    drop(raw);

    // Phase L: the full load through a bare ClassManager.
    let boot_cp = vec![base.to_string_lossy().into_owned()];
    let mut cm = ClassManager::new(&boot_cp, &[], &[]);
    for warm in ["java/lang/Object", "java/lang/String", "java/lang/Class"] {
        let _ = cm.load_class(warm);
    }
    let classes_before = cm.class_store.len();
    // zs s1/fp: the side tables' bytes before the window, to report each
    // table's growth over it (`zs1fp-map` lines below).
    let maps_before: BTreeMap<&'static str, usize> = cm.footprint_census().into_iter().collect();
    let sizes_before = by_size_snapshot();
    let live_before = LIVE.load(Ordering::Relaxed);
    let mut loaded = 0usize;
    for name in &names {
        if cm.load_class(name).is_ok() {
            loaded += 1;
        }
    }
    let delta = LIVE.load(Ordering::Relaxed) - live_before;
    let grown = cm.class_store.len().saturating_sub(classes_before).max(1);
    let maps_after: BTreeMap<&'static str, usize> = cm.footprint_census().into_iter().collect();
    // The net live bytes by allocation size, largest first.
    let mut by_size: Vec<(usize, isize)> = by_size_snapshot()
        .into_iter()
        .zip(sizes_before)
        .enumerate()
        .map(|(size, (after, before))| (size, after - before))
        .filter(|&(_, b)| b != 0)
        .collect();
    by_size.sort_by_key(|&(_, b)| std::cmp::Reverse(b));
    for (size, b) in by_size.iter().take(24) {
        let (label, blocks) = if *size <= EXACT {
            (size.to_string(), b / (*size).max(1) as isize)
        } else {
            let hi = 1usize << (*size - EXACT + 12);
            (format!("<={hi}"), -1)
        };
        eprintln!(
            "zr11md-size alloc_size={label:<8} net_b={b:<9} per_class_b={} blocks={blocks}",
            b / grown as isize,
        );
    }

    let mut parts: deep::Parts = BTreeMap::new();
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut buffers: HashSet<usize> = HashSet::new(); // addr-keyed: class-file buffer starts, test-local, all held alive while the set lives
    let mut buffer_b = 0usize;
    let mut note_buffer = |b: &cratonvm_reader::SharedBytes| {
        if buffers.insert(b.as_ref().as_ptr() as usize) {
            buffer_b += b.len();
        }
    };
    let cpe = size_of::<cratonvm_reader::constant_pool::ConstantPoolEntry>();
    let mut n_classes = 0usize;
    let mut vt_b = 0usize;
    // zs s2/fp2: a vtable slot is a shared handle; each distinct slot block
    // is counted once (its `Arc` block: two counts + the data).
    let mut vt_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: slot data blocks, all held alive by the manager while the set lives
                                                        // Interned Utf8 strings, each counted once: the `Arc<str>` block (two
                                                        // counts + the bytes, rounded to 8) and the global pool's map slot (the
                                                        // key's fat pointer + one control byte). The warm-up classes' strings
                                                        // are counted too, so this slightly over-attributes.
    let mut utf8_seen: HashSet<usize> = HashSet::new(); // addr-keyed: interned Arc<str> starts, all held alive by the store while the set lives
    let mut utf8_b = 0usize;
    let mut code_b = 0usize;
    let mut smt_b = 0usize;
    // zs s1/fp: the verifier type maps' parts (`zs1fp-typemaps` line).
    let (mut tm_methods, mut tm_with, mut tm_rows, mut tm_data) = (0usize, 0usize, 0usize, 0usize);
    for c in cm.class_store.iter() {
        n_classes += 1;
        if let Some(tm) = cm.class_store.type_maps().class_type_maps(c.id) {
            tm_methods += tm.method_count();
            for i in 0..tm.method_count() {
                if let Some(m) = tm.method(i) {
                    tm_with += 1;
                    tm_rows += m.entry_count();
                    tm_data += m.heap_bytes();
                }
            }
        }
        for i in 1..c.constant_pool.len() {
            if let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Utf8(s)) =
                c.constant_pool.get(i as u16)
            {
                if utf8_seen.insert(s.as_ptr() as usize) {
                    utf8_b += (16 + s.len()).div_ceil(8) * 8 + 17;
                }
            }
        }
        for m in &c.methods {
            if let Some(code) = m.code() {
                code_b += code.code.len();
                for n in &code.attributes {
                    if let Attribute::StackMapTable { entries } = n {
                        smt_b += entries.len();
                    }
                }
            }
        }
        *parts.entry("constant_pool").or_default() += c.constant_pool.len() * cpe;
        *parts.entry("method_records").or_default() +=
            c.methods.capacity() * size_of::<cratonvm_reader::method::ClassFileMethod>();
        *parts.entry("field_records").or_default() +=
            c.fields.capacity() * size_of::<cratonvm_reader::field::ClassFileField>();
        // zs s2/fp2: the inner-class names share the pool's interned
        // strings (counted under `interned_utf8_unique_est`), so they own no
        // bytes here; their 12-byte handles are in `class_level_vecs`.
        let strings: usize = c.nest_members.iter().map(|s| s.capacity()).sum::<usize>()
            + c.permitted_subclasses
                .iter()
                .map(|s| s.capacity())
                .sum::<usize>()
            + c.signature.as_ref().map_or(0, |s| s.capacity())
            + c.source_file.as_ref().map_or(0, |s| s.capacity())
            + c.module_name.as_ref().map_or(0, |s| s.capacity())
            + c.nest_host.as_ref().map_or(0, |s| s.capacity());
        *parts.entry("class_level_strings").or_default() += strings;
        *parts.entry("class_level_vecs").or_default() += c.interfaces.capacity() * 4
            + c.bootstrap_methods.capacity()
                * size_of::<cratonvm_reader::attribute::BootstrapMethod>()
            + c.bootstrap_methods
                .iter()
                .map(|b| b.bootstrap_arguments.capacity() * 2)
                .sum::<usize>()
            + c.annotations.capacity() * size_of::<cratonvm_reader::attribute::Annotation>()
            + c.inner_classes.capacity() * size_of::<cratonvm_classloading::InnerClassEntry>()
            + c.nest_members.capacity() * size_of::<String>()
            + c.permitted_subclasses.capacity() * size_of::<String>();
        for m in &c.methods {
            deep::lazy_vec(&mut parts, "member_attr_slots", &m.attributes);
            for a in &m.attributes {
                match a {
                    LazyAttribute::Decoded(d) => {
                        *kinds.entry(deep::kind(d)).or_default() += 1;
                        if let Attribute::Code(code) = d {
                            note_buffer(code.code.backing());
                            for nested in &code.attributes {
                                *kinds.entry(deep::kind(nested)).or_default() += 1;
                                if let Attribute::StackMapTable { entries } = nested {
                                    note_buffer(entries.backing());
                                }
                            }
                        }
                    }
                    LazyAttribute::Raw(raw) => note_buffer(&raw.source),
                }
            }
        }
        for f in &c.fields {
            deep::lazy_vec(&mut parts, "member_attr_slots", &f.attributes);
            for a in &f.attributes {
                if let LazyAttribute::Decoded(d) = a {
                    *kinds.entry(deep::kind(d)).or_default() += 1;
                }
            }
        }
        if let Some(vt) = cm.vtable_descriptors_of(c.id) {
            vt_b += std::mem::size_of_val(vt);
            for slot in vt.iter().flatten() {
                if vt_blocks.insert(slot.data() as *const _ as usize) {
                    vt_b += 16 + size_of::<cratonvm_classloading::VtableSlotData>();
                }
            }
            // The dispatch snapshots this class declares (an inherited slot
            // shares its declaring class's `Arc`, so count own slots only).
            for slot in vt.iter().flatten() {
                if slot.declaring_class_id != c.id.as_u32() {
                    continue;
                }
                if let Some(s) = &slot.dispatch {
                    // Since z11/md the code is a view into the pinned class
                    // file and the names are shared `Arc<str>`s, so only
                    // the block and the exception table are the snapshot's
                    // own (base: + a bytecode copy + two `String`s).
                    *parts.entry("vtable_snapshots").or_default() += 16
                        + size_of::<cratonvm_classloading::VtableMethodSnapshot>()
                        + s.exception_table.capacity()
                            * size_of::<cratonvm_reader::attribute::ExceptionTableEntry>();
                }
            }
        }
    }
    *parts.entry("vtable_descriptors").or_default() += vt_b;
    *parts.entry("class_structs").or_default() +=
        n_classes * size_of::<cratonvm_classloading::Class>();
    *parts.entry("class_file_buffers_pinned").or_default() += buffer_b;
    *parts.entry("interned_utf8_unique_est").or_default() += utf8_b;
    let named: usize = parts.values().sum();
    // Inside the pinned buffers: what a class file could shrink to if only
    // the bytecode and the stack maps were kept (the residual page's option).
    eprintln!(
        "zr11md-inside-buffers bytecode_per_class_b={} stackmap_per_class_b={} buffer_per_class_b={}",
        code_b / n_classes.max(1),
        smt_b / n_classes.max(1),
        buffer_b / n_classes.max(1)
    );
    eprintln!(
        "zr11md-load loaded_ok={loaded} classes_grown={grown} live_delta_b={delta} per_class_b={} size_of: Attribute={} LazyAttribute={} CodeAttribute={} ConstantPoolEntry={cpe} Class={}",
        delta / grown as isize,
        size_of::<Attribute>(),
        size_of::<LazyAttribute>(),
        size_of::<cratonvm_reader::attribute::CodeAttribute>(),
        size_of::<cratonvm_classloading::Class>(),
    );
    for (k, v) in &parts {
        eprintln!(
            "zr11md-part {k:<30} per_class_b={:>6} total_b={v}",
            v / n_classes.max(1)
        );
    }
    eprintln!(
        "zr11md-part {:<30} per_class_b={:>6}",
        "UNNAMED(delta-named)",
        (delta - named as isize) / n_classes.max(1) as isize
    );
    let per = |v: usize| v / n_classes.max(1);
    eprintln!(
        "zs1fp-typemaps methods_per_class={:.1} with_maps_per_class={:.1} rows_per_class={} slot_b={} (size_of Option<MethodTypeMaps>={}) sig_b={} row_data_b={}",
        tm_methods as f64 / n_classes.max(1) as f64,
        tm_with as f64 / n_classes.max(1) as f64,
        per(tm_rows),
        per(tm_methods * size_of::<Option<cratonvm_classloading::MethodTypeMaps>>()),
        size_of::<Option<cratonvm_classloading::MethodTypeMaps>>(),
        per(tm_methods * (4 + 2 * size_of::<cratonvm_types::ThinArcStr>())), // zs s2/fp2: thin names
        per(tm_data),
    );
    // zs s1/fp: the per-class side tables (`ClassManager::footprint_census`),
    // each table's growth over the load window. Their sum is the share of
    // UNNAMED that the maps explain; what is left after it is printed last.
    let mut maps_named = 0isize;
    for (k, after) in &maps_after {
        let grew = *after as isize - maps_before.get(k).copied().unwrap_or(0) as isize;
        maps_named += grew;
        eprintln!(
            "zs1fp-map {k:<32} per_class_b={:>6} total_b={after} grew_b={grew}",
            grew / n_classes.max(1) as isize
        );
    }
    eprintln!(
        "zs1fp-map {:<32} per_class_b={:>6} UNNAMED_after_maps_per_class_b={:>6}",
        "SUM(maps)",
        maps_named / n_classes.max(1) as isize,
        (delta - named as isize - maps_named) / n_classes.max(1) as isize
    );
    for (k, v) in &kinds {
        eprintln!(
            "zr11md-kind {k:<30} count={v} per_class={:.2}",
            *v as f64 / n_classes.max(1) as f64
        );
    }
    assert!(loaded > 0, "positive control: classes loaded");
}
