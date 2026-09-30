// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gc-common round 2026-09-23, wave 2, lane G: the safepoint deposit / resume
//! pair and the two blocked-thread wake paths.

use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{
    JitHashMapStringNodeCacheEntry, StringCaseCacheEntry, ThreadId,
};
use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame};

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; used only as a pointer-map key / value.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn frame(code: Vec<u8>, method_name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
        method_name.to_string(),
        "()V".to_string(),
        None,
        code,
        vec![],
        8,
        4,
        &[],
    )
}

fn stashed_frame_holding(addr: usize) -> ReconstructedFrame {
    ReconstructedFrame {
        method_key: "craton/probe/Stash.m:()V".to_string(),
        locals: vec![FrameValue::Object(addr as u64)],
        ..Default::default()
    }
}

/// Both JIT stashes empty on this (test) thread, whatever ran before.
fn clear_stashes() {
    while cratonvm_jit::deopt::take_last_deopt().is_some() {}
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
}

/// **THE SAFEPOINT DEPOSIT PUBLISHES THE DEOPT STASH.** A thread parked at a
/// safepoint with a stashed deopt frame is seen by a peer's collection only
/// through its `root_snapshot`; the stash is a `jit/` thread-local no peer can
/// read.
#[test]
fn the_safepoint_deposit_publishes_the_deopt_stash() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "stash-deposit-test");
    let held = shared.mem.heap.alloc_object(ClassId::new(7), 1);
    let held_addr = held.as_ptr() as usize;
    if shared.mem.heap.is_object_address(held_addr).is_none() {
        // The scan half filters through `is_object_address`, exactly as the
        // blocked deposit does; a heap that does not recognise a fresh object
        // cannot exercise it.
        eprintln!("skipping: fresh object not recognised by is_object_address");
        return;
    }
    clear_stashes();
    cratonvm_jit::deopt::restash_last_deopt(stashed_frame_holding(held_addr));

    update_root_snapshot(&shared, &mut thread);

    let published = thread
        .root_snapshot
        .lock()
        .iter()
        .any(|r| r.as_ptr() as usize == held_addr);
    clear_stashes();
    assert!(
        published,
        "an object held only by a stashed deopt frame must be in the safepoint deposit"
    );
}

/// **THE SAFEPOINT RESUME FORWARDS THE DEOPT STASH.** The remap half, paired
/// with the scan above.
#[test]
fn the_safepoint_resume_forwards_the_deopt_stash() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut thread = JvmThread::new(ThreadId(0), "stash-resume-test");
    clear_stashes();
    cratonvm_jit::deopt::restash_last_deopt(stashed_frame_holding(0x5000));
    // The deposit that precedes every resume offered the stash to a scan;
    // `remap_stashed_deopt_objects` debug-asserts that pairing.
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|_| {});

    let pointer_map = cratonvm_types::PointerMap::from_iter([(0x5000usize, 0x9000usize)]);
    apply_pointer_map_to_thread(
        &mut thread,
        &pointer_map,
        &heap,
        &cratonvm_classloading::TypeMapStore::new(),
    );

    let frame = cratonvm_jit::deopt::take_last_deopt().expect("the stashed frame is still there");
    clear_stashes();
    assert!(
        matches!(frame.locals[0], FrameValue::Object(0x9000)),
        "the stashed frame's reference must follow its object: {:?}",
        frame.locals[0]
    );
}

/// **THE SAFEPOINT DEPOSIT PUBLISHES `monitor_on_exit`.** The initiator and
/// the blocked deposit root a synchronized frame's implicit `monitorexit`
/// target; the safepoint deposit only remapped it. A frame whose receiver is
/// in no live local is exactly the case the local scan cannot cover.
#[test]
fn the_safepoint_deposit_publishes_monitor_on_exit() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "monitor-deposit-test");
    let lock = shared.mem.heap.alloc_object(ClassId::new(9), 0);
    // Two frames so the cached path (when on) has a frozen frame and a top.
    let mut deep = frame(vec![0xb1], "deep"); // return
    deep.monitor_on_exit = Some(lock);
    thread.frames.push(deep);
    let mut top = frame(vec![0xb1], "top");
    top.monitor_on_exit = Some(lock);
    thread.frames.push(top);

    update_root_snapshot(&shared, &mut thread);
    let count = thread
        .root_snapshot
        .lock()
        .iter()
        .filter(|r| r.as_ptr() == lock.as_ptr())
        .count();
    assert!(
        count >= 2,
        "each synchronized frame's monitor object must be published ({count} found)"
    );

    // A second deposit (the cached path reuses the deep frame's roots) must
    // still carry it.
    update_root_snapshot(&shared, &mut thread);
    assert!(
        thread
            .root_snapshot
            .lock()
            .iter()
            .any(|r| r.as_ptr() == lock.as_ptr()),
        "a re-deposit must keep publishing the monitor object"
    );
}

/// A non-moving pause re-tags a parked peer's frozen-frame cache instead of
/// discarding it: `remap_rs_cache_after_gc` with an empty map keeps every
/// entry and advances the tag (the branch `safepoint_check` now takes on an
/// empty map).
#[test]
fn an_empty_map_retags_the_root_cache_instead_of_dropping_it() {
    if !crate::runtime::env_cache::rootsnap_cache()
        || !crate::runtime::env_cache::rootsnap_cache_survive_gc()
    {
        return;
    }
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut thread = JvmThread::new(ThreadId(0), "rs-cache-retag-test");
    thread.rs_cache.push(((1, 1), vec![fake(0x7000)]));
    thread.rs_cache_gen = u64::MAX;

    remap_rs_cache_after_gc(&mut thread, &cratonvm_types::PointerMap::default(), &heap);

    assert_eq!(thread.rs_cache_gen, heap.collection_count());
    assert_eq!(thread.rs_cache.len(), 1);
    assert_eq!(thread.rs_cache[0].1[0].as_ptr() as usize, 0x7000);
}

/// **THE LEAKED-REGION FALLBACK FORWARDS THE SHARED LIST.** It used to miss
/// both JIT caches and both parked throwables, all published by the blocked
/// deposit.
#[test]
fn the_leaked_region_fallback_forwards_every_off_frame_family() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "leaked-fallback-test");
    clear_stashes();
    thread.jit_pending_exception = Some(fake(0x4000));
    thread.uncaught_exception_pending = Some(fake(0x4100));
    thread
        .jit_hashmap_string_node_cache
        .push(JitHashMapStringNodeCacheEntry {
            map: fake(0x4200),
            node: fake(0x4300),
            key_object: None,
            key: String::new(),
            mod_count_slot: 0,
            mod_count: 0,
            chm_generation: None,
            chm_segment_id: None,
        });
    thread.string_case_cache.push(StringCaseCacheEntry {
        source: fake(0x4400),
        locale: None,
        upper: false,
        first: fake(0x4500),
        second: fake(0x4600),
        next: false,
    });
    {
        let mut fixup = thread.gc_block_state.fixup.lock();
        for k in 0x40..=0x46usize {
            fixup.insert(k * 0x100, k * 0x100 + 0x8000);
        }
    }

    crate::vm::vm_exec::apply_pending_blocked_fixups(&shared, &mut thread);

    let at = |o: ObjectRef| o.as_ptr() as usize;
    assert_eq!(at(thread.jit_pending_exception.unwrap()), 0xC000);
    assert_eq!(at(thread.uncaught_exception_pending.unwrap()), 0xC100);
    assert_eq!(at(thread.jit_hashmap_string_node_cache[0].map), 0xC200);
    assert_eq!(at(thread.jit_hashmap_string_node_cache[0].node), 0xC300);
    assert_eq!(at(thread.string_case_cache[0].source), 0xC400);
    assert_eq!(at(thread.string_case_cache[0].first), 0xC500);
    assert_eq!(at(thread.string_case_cache[0].second), 0xC600);
}

/// A CHAINED composed fixup (`A -> B`, `B -> C`) is applied to the fallback's
/// off-frame references exactly once: a reference that named `A` ends at `B`.
#[test]
fn the_leaked_region_fallback_applies_a_chained_fixup_once() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "leaked-chain-test");
    clear_stashes();
    const A: usize = 0xD3A0;
    const B: usize = 0xD3A8;
    const C: usize = 0xD3B0;
    thread.uncaught_exception_pending = Some(fake(A));
    thread.native_pin_roots.push(fake(B));
    {
        let mut fixup = thread.gc_block_state.fixup.lock();
        fixup.insert(A, B);
        fixup.insert(B, C);
    }

    crate::vm::vm_exec::apply_pending_blocked_fixups(&shared, &mut thread);

    assert_eq!(thread.uncaught_exception_pending.unwrap().as_ptr() as usize, B);
    assert_eq!(thread.native_pin_roots[0].as_ptr() as usize, C);
}

/// **EVERY REFERENCE-BEARING `JvmThread` FIELD IS WIRED INTO ALL FOUR LISTS.**
///
/// A census, in the `addr_keyed` pattern (proposal
/// `docs/internal/gc-common-round-20260923/common-b-proposal-one-per-thread-root-visitor-RETIRED-20260923.md`, step 2): parse the
/// `JvmThread` struct for fields whose type names `ObjectRef`, `Value` or one
/// of the two JIT cache entry types, and require each to be named by
///
/// * the shared remap (`memory::gc::remap_thread_off_frame_refs` +
///   `remap_thread_single_slots`),
/// * the safepoint deposit (`update_root_snapshot` +
///   `roots::push_off_frame_thread_roots`), and
/// * the blocked deposit (`deposit_root_snapshot_inner` +
///   `push_off_frame_thread_roots`),
///
/// or to be on the explicit allow-list below with its reason. A new field then
/// fails here instead of shipping wired into one path and missing from
/// another -- every divergence among these lists found so far was a
/// use-after-free or a stale reference. (A field whose type hides its
/// references behind an unlisted type name is not seen; the marker list is the
/// thing to extend when that happens.)
#[test]
fn every_reference_bearing_thread_field_is_scanned_and_remapped() {
    let read = |rel: &str| -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    };
    let body = |src: &str, sig: &str, end: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest.find(end).unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    };
    // `.name` followed by a non-identifier character, so `printed` is not
    // satisfied by `printed_lines`.
    let mentions = |text: &str, name: &str| -> bool {
        let needle = format!(".{name}");
        let mut from = 0;
        while let Some(i) = text[from..].find(&needle) {
            let after = from + i + needle.len();
            let next = text[after..].chars().next();
            if !matches!(next, Some(c) if c.is_ascii_alphanumeric() || c == '_') {
                return true;
            }
            from = after;
        }
        false
    };

    let thread_src = read("src/threading/jvm_thread.rs");
    let gc_src = read("src/memory/gc.rs");
    let roots_src = read("src/memory/roots.rs");
    let alloc_src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let exec_src = read("src/vm/vm_exec.rs");

    let shared_scan = body(
        &roots_src,
        "pub(crate) fn push_off_frame_thread_roots(",
        "\n}\n",
    );
    let remap = body(&gc_src, "pub(crate) fn remap_thread_off_frame_refs(", "\n}\n")
        + &body(&gc_src, "pub(crate) fn remap_thread_single_slots(", "\n}\n");
    let safepoint_deposit =
        body(&alloc_src, "pub(crate) fn update_root_snapshot(", "\n}\n") + &shared_scan;
    let blocked_deposit =
        body(&exec_src, "fn deposit_root_snapshot_inner(", "\n    }\n") + &shared_scan;

    // Reference-bearing fields that are deliberately in none of the lists.
    const ALLOWED: &[(&str, &str)] = &[
        (
            "root_snapshot",
            "the deposit's own output; forwarded by memory::gc::remap_root_snapshot \
             (safepoint resume), update_all_roots §11 and the blocked-thread fold",
        ),
        (
            "rs_cache",
            "a cache of frame roots, rebuilt by update_root_snapshot and forwarded \
             by remap_rs_cache_after_gc",
        ),
        (
            "jmx_locked_synchronizers",
            "registry-owned Arc shared with ThreadRegistry, which scans and remaps it",
        ),
    ];

    let struct_start = thread_src
        .find("\npub struct JvmThread {\n")
        .expect("JvmThread struct not found");
    let struct_body = &thread_src[struct_start..];
    let struct_body = &struct_body[..struct_body.find("\n}\n").expect("JvmThread end")];
    const MARKERS: &[&str] = &[
        "ObjectRef",
        "Value",
        "JitHashMapStringNodeCacheEntry",
        "StringCaseCacheEntry",
    ];
    let mut fields: Vec<String> = Vec::new();
    for line in struct_body.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        let Some((name, ty)) = t.split_once(':') else {
            continue;
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            continue;
        }
        let tokens: Vec<&str> = ty
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .collect();
        if MARKERS.iter().any(|m| tokens.contains(m)) {
            fields.push(name.to_string());
        }
    }
    assert!(
        fields.len() >= 12,
        "the JvmThread field parse found only {fields:?} — the parser is broken, \
         failing rather than passing vacuously"
    );

    let mut findings = Vec::new();
    for name in &fields {
        if ALLOWED.iter().any(|(a, _)| a == name) {
            continue;
        }
        for (list, text) in [
            ("the shared off-frame remap", &remap),
            ("the safepoint deposit (update_root_snapshot)", &safepoint_deposit),
            ("the blocked deposit (deposit_root_snapshot_inner)", &blocked_deposit),
        ] {
            if !mentions(text, name) {
                findings.push(format!("JvmThread::{name} is missing from {list}"));
            }
        }
    }
    assert!(
        findings.is_empty(),
        "per-thread root lists are out of step with JvmThread:\n  {}\n\
         Wire the field into memory::gc::remap_thread_off_frame_refs and \
         roots::push_off_frame_thread_roots (which both deposits call), or add \
         it to ALLOWED with the reason it needs neither.",
        findings.join("\n  ")
    );
}

/// Source ratchet: both blocked-wake paths forward off-frame references
/// through the shared list, and run the native-stack write-back AFTER the JIT
/// remap. A hand-written copy of the list reappearing in either is how the
/// drift this lane closed began.
#[test]
fn both_wake_paths_use_the_shared_off_frame_remap_in_order() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/vm/vm_exec.rs");
    let src = std::fs::read_to_string(path)
        .expect("vm_exec.rs is readable")
        .replace("\r\n", "\n");
    let body = |sig: &str, end: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start..];
        let stop = rest.find(end).unwrap_or_else(|| panic!("end of {sig} not found"));
        rest[..stop].to_string()
    };
    for (name, text) in [
        (
            "apply_pending_blocked_fixups",
            body("pub(crate) fn apply_pending_blocked_fixups(", "\n}\n"),
        ),
        (
            "check_post_block_gc_refs",
            body("fn check_post_block_gc_refs(", "\n    }\n"),
        ),
    ] {
        assert!(
            text.contains("remap_thread_off_frame_refs("),
            "{name} must forward off-frame refs through memory::gc::remap_thread_off_frame_refs"
        );
        assert!(
            text.contains("remap_stashed_deopt_objects("),
            "{name} must remap the deopt stash the blocked deposit scanned"
        );
        assert!(
            !text.contains("self.thread.string_case_cache") && !text.contains("thread.native_alloc_pool"),
            "{name} carries a hand-written copy of the off-frame list again"
        );
        let jit = text
            .find("apply_blocked_wake_jit_remap(")
            .unwrap_or_else(|| panic!("{name} lost its JIT remap"));
        let native = text
            .find("apply_native_slot_fixups(")
            .unwrap_or_else(|| panic!("{name} lost its native-stack write-back"));
        assert!(
            jit < native,
            "{name}: the native-stack write-back must run AFTER the JIT remap"
        );
    }
}

/// gc-common w19-a
/// (`common-w18d-peer-interpreter-activations-do-not-keep-their-class`):
/// a PEER parked at a safepoint inside a method of a user-loader class
/// publishes that class's defining loader. A collection another thread
/// initiates sees this thread only through its deposit, so without the loader
/// there a loader-conditional cycle deferred the class's mirror and statics to
/// an unmarked loader and unloaded the class under the running frame. Read
/// back through the registry as well: that is the collector's own view.
#[test]
fn a_parked_peers_activation_keeps_its_class_loader() {
    let shared = SharedVm::new(VmConfig::default());
    let vm = shared.vm_identity;
    // A class id no class manager holds and no other test uses.
    let cid = ClassId::new(0x7ff1_9a11);
    let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    cratonvm_native_builtins::classloader::register_defining_loader(vm, cid.as_u32(), loader);
    let tid = ThreadId(0);
    shared.threads.thread_registry.register(tid, "w19a-peer", None);
    let mut thread = JvmThread::new(tid, "w19a-peer");
    shared
        .threads
        .thread_registry
        .set_root_snapshot(tid, thread.root_snapshot.clone());
    // A static method: no receiver, no local names the loader or the mirror.
    let mut worker = frame(vec![0xb1], "worker");
    worker.class_id = cid;
    thread.frames.push(worker);
    // Two frames so the frozen-frame cache (when on) has a cached frame too.
    let mut top = frame(vec![0xb1], "top");
    top.class_id = cid;
    thread.frames.push(top);

    update_root_snapshot(&shared, &mut thread);
    let deposited = thread
        .root_snapshot
        .lock()
        .iter()
        .filter(|r| r.as_ptr() == loader.as_ptr())
        .count();
    let collected = shared
        .threads
        .thread_registry
        .collect_all_root_snapshots()
        .iter()
        .any(|r| r.as_ptr() == loader.as_ptr());

    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
    cratonvm_types::loader_pin::forget_vm_loader_pins(vm);

    assert_eq!(
        deposited, 1,
        "the safepoint deposit must publish the frames' defining loader, once"
    );
    assert!(collected, "the collector's registry view must carry the loader");
}

/// Source ratchet for the fix above: both peer deposits publish the class
/// owners through the one shared list, `roots::frame_class_owners`, gathered
/// before the snapshot lock is taken.
#[test]
fn both_peer_deposits_publish_frame_class_owners() {
    let read = |rel: &str| -> String {
        let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .replace("\r\n", "\n")
    };
    let body = |src: &str, sig: &str, end: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let stop = rest.find(end).unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    };
    let alloc_src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let exec_src = read("src/vm/vm_exec.rs");
    for (name, text) in [
        (
            "update_root_snapshot",
            body(&alloc_src, "pub(crate) fn update_root_snapshot(", "\n}\n"),
        ),
        (
            "deposit_root_snapshot_inner",
            body(&exec_src, "fn deposit_root_snapshot_inner(", "\n    }\n"),
        ),
    ] {
        let owners = text
            .find("frame_class_owners(")
            .unwrap_or_else(|| panic!("{name} no longer publishes the frames' class owners"));
        // `update_root_snapshot` locks through an `Arc` clone (`snap_arc`).
        let lock = text
            .find("root_snapshot.lock()")
            .or_else(|| text.find("snap_arc.lock()"))
            .unwrap_or_else(|| panic!("{name} no longer locks its snapshot"));
        assert!(
            owners < lock,
            "{name}: gather the class owners BEFORE locking the snapshot (they take other locks)"
        );
        assert!(
            text.contains("extend_from_slice(&class_owners)"),
            "{name} gathers the class owners but never publishes them"
        );
    }
}
