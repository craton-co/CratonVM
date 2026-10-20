// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s3/fp3 (2026-10-14): what the VM's OWN vtables cost a loaded class.
//! Page: `docs/internal/proposals/gc/zgc-zs2fp2-proposal-one-vtable-copy-in-the-vm-20261014.md`.
//!
//! The class-metadata census (`classloading/tests/zr8_fm_class_metadata_census.rs`,
//! `zr11_md_deep_per_class_census`) stops at the `ClassManager`: the VM's
//! `VtableManager`, which the link-time install hook fills with one
//! `VtableEntry` per slot plus a signature index per class, is out of its
//! reach. This test loads the same classes (the first `N` `java/util/**` and
//! `java/lang/**` classes of `java.base`, `ZR8FM_CLASSES`, default 1 500)
//! through a bare `ClassManager`, then replays the install hook into a
//! `VtableManager` in link order (superclasses first, as the VM links them),
//! and reports:
//!
//! - `zs3fp3-vm-install live_delta_b`: the bytes the global allocator holds
//!   after the replay minus before it, per class: the exact cost;
//! - `zs3fp3-vm-part`: `VtableManager::footprint_census`, by owner.
//!
//! Ignored by default (it needs `JAVA_HOME` with `jmods/`); run with
//! `cargo test -p cratonvm-vm --test zs3_fp3_vm_vtable_census -- --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: forwards to `System` and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
#[ignore]
fn zs3_fp3_vm_vtable_per_class_census() {
    use cratonvm_classloading::{ClassId, ClassManager};
    use cratonvm_vm::runtime::vtable::{install_vtable_descriptors, VtableManager};
    use parking_lot::RwLock;

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
    drop(zip);
    drop(bytes);

    let boot_cp = vec![base.to_string_lossy().into_owned()];
    let mut cm = ClassManager::new(&boot_cp, &[], &[]);
    let mut loaded = 0usize;
    for name in &names {
        if cm.load_class(name).is_ok() {
            loaded += 1;
        }
    }

    // Link order: superclasses first (depth along the superclass chain).
    let depth = |mut id: ClassId| {
        let mut d = 0usize;
        while let Some(s) = cm.class_store.get(id).and_then(|c| c.superclass) {
            d += 1;
            id = s;
        }
        d
    };
    let mut order: Vec<(usize, ClassId)> = cm
        .class_store
        .iter()
        .filter(|c| cm.vtable_descriptors_of(c.id).is_some())
        .map(|c| (depth(c.id), c.id))
        .collect();
    order.sort_by_key(|&(d, id)| (d, id.as_u32()));
    let inputs: Vec<(
        u32,
        Vec<Option<cratonvm_classloading::VtableSlotDescriptor>>,
    )> = order
        .iter()
        .map(|&(_, id)| (id.as_u32(), cm.vtable_descriptors_of(id).unwrap().to_vec()))
        .collect();
    let slots: usize = inputs.iter().map(|(_, v)| v.len()).sum();
    let declared: usize = inputs
        .iter()
        .map(|(id, v)| {
            v.iter()
                .flatten()
                .filter(|d| d.declaring_class_id == *id)
                .count()
        })
        .sum();

    let manager = RwLock::new(VtableManager::new());
    let before = LIVE.load(Ordering::Relaxed);
    for (id, entries) in inputs {
        install_vtable_descriptors(&manager, id, entries);
    }
    let delta = LIVE.load(Ordering::Relaxed) - before;
    let tables = order.len().max(1);
    eprintln!(
        "zs3fp3-vm-install loaded_ok={loaded} tables={tables} slots_per_class={:.1} declared_per_class={:.1} live_delta_b={delta} per_class_b={} size_of: VtableEntry={} CachedBytecodeMethod={}",
        slots as f64 / tables as f64,
        declared as f64 / tables as f64,
        delta / tables as isize,
        std::mem::size_of::<cratonvm_vm::runtime::vtable::VtableEntry>(),
        std::mem::size_of::<cratonvm_jit_api::CachedBytecodeMethod>(),
    );
    let census = manager.read().footprint_census();
    let mut named = 0usize;
    for (k, v) in &census {
        if *k == "tables" {
            continue;
        }
        named += v;
        eprintln!(
            "zs3fp3-vm-part {k:<20} per_class_b={:>6} total_b={v}",
            v / tables
        );
    }
    eprintln!(
        "zs3fp3-vm-part {:<20} per_class_b={:>6} (live delta minus the named parts)",
        "UNNAMED",
        (delta - named as isize) / tables as isize
    );
    assert!(loaded > 0, "positive control: classes loaded");
    assert!(slots > 0, "positive control: vtable slots installed");
}
