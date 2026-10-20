// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s5/nd5 tests (ZGC round zs, wave s5, lane nd5), a child module of
//! `vm_exec` (`#[path]`).
//!
//! A user loader's `java.lang.Module` recorded by `Module.defineModule0`
//! (`record_layer_module_packages`) must live exactly as long as its loader
//! on ZGC: alive (and found by `layer_module_of_package`) while the loader
//! is reachable, collected with it once the loader is not. Before the fix it
//! was a permanent JNI global root, and through `Module.loader` it kept the
//! loader, every class the loader defined and their mirrors alive: the
//! `Zs3Nd3UpcallMints proxy` leak (one `jdk.proxyN` module per fresh loader).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};

const NS: u32 = 4077;
const PKG: &str = "zs5nd5/p";

fn zgc_vm() -> Option<SharedVm> {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Zgc(_)).then_some(shared)
}

/// A minimal class `zs5nd5/ModuleLike extends java/lang/Object` with one
/// instance field `Object loader`: the one field of `java.lang.Module` that
/// `record_layer_module_packages` reads. (The unit-test VM's own
/// `java/lang/Module` is a synthetic stand-in whose fields are unnamed.)
fn module_like_bytes() -> Vec<u8> {
    fn utf8(b: &mut Vec<u8>, text: &str) {
        b.push(1);
        b.extend_from_slice(&(text.len() as u16).to_be_bytes());
        b.extend_from_slice(text.as_bytes());
    }
    let mut b = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
    b.extend_from_slice(&7u16.to_be_bytes()); // cp 1..6
    b.push(7);
    b.extend_from_slice(&2u16.to_be_bytes()); // #1 Class #2
    utf8(&mut b, "zs5nd5/ModuleLike"); // #2
    b.push(7);
    b.extend_from_slice(&4u16.to_be_bytes()); // #3 Class #4
    utf8(&mut b, "java/lang/Object"); // #4
    utf8(&mut b, "loader"); // #5
    utf8(&mut b, "Ljava/lang/Object;"); // #6
    b.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&1u16.to_be_bytes()); // this
    b.extend_from_slice(&3u16.to_be_bytes()); // super
    b.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    b.extend_from_slice(&1u16.to_be_bytes()); // fields
    b.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    b.extend_from_slice(&5u16.to_be_bytes());
    b.extend_from_slice(&6u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes()); // field attributes
    b.extend_from_slice(&0u16.to_be_bytes()); // methods
    b.extend_from_slice(&0u16.to_be_bytes()); // attributes
    b
}

/// A fresh module-like object whose `loader` field names a fresh loader
/// object, recorded for [`NS`] / [`PKG`]. Answers the loader, or `None` (the
/// reason on stderr) when the unit-test VM cannot build it.
fn record_module(shared: &SharedVm, thread: &mut JvmThread) -> Option<ObjectRef> {
    let mut ctx = NativeContextImpl { shared, thread };
    let holder = match ctx.class_id_by_name("zs5nd5/ModuleLike") {
        Some(c) => c,
        None => match ctx.define_class_from_bytes("zs5nd5/ModuleLike", &module_like_bytes()) {
            Some(c) => c,
            None => {
                eprintln!("zs5nd5: the module-like class did not define here; row skipped");
                return None;
            }
        },
    };
    let Ok(loader_cid) = ctx.ensure_class_initialized("java/lang/Object") else {
        eprintln!("zs5nd5: java/lang/Object not loadable here; row skipped");
        return None;
    };
    let fields = ctx.class_num_total_fields(holder).max(1);
    let loader = ctx.alloc_object(loader_cid, 0);
    let pin = ctx.pin_native_root(loader);
    let module = ctx.alloc_object(holder, fields);
    let loader = ctx.read_native_pin(pin, loader);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(module, "loader", Value::Object(Some(loader)));
    let read = ctx.get_field_by_name(module, "loader");
    if read != Value::Object(Some(loader)) {
        eprintln!("zs5nd5: the holder's loader field is not writable ({read:?}); row skipped");
        return None;
    }
    ctx.record_layer_module_packages(NS, &[PKG.to_string()], module);
    Some(loader)
}

fn lookup(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    let ctx = NativeContextImpl { shared, thread };
    ctx.layer_module_of_package(NS, PKG).is_some()
}

fn collect(shared: &SharedVm, thread: &mut JvmThread) {
    crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, "zs5nd5-test");
}

/// Held loader: the module survives a collection; dropped loader: the next
/// collections reclaim the module and the lookup answers `None`.
#[test]
fn a_user_loaders_module_lives_exactly_as_long_as_its_loader() {
    let Some(shared) = zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(5501), "zs5nd5-module");
    let Some(loader) = record_module(&shared, &mut thread) else {
        return;
    };
    assert!(lookup(&shared, &mut thread), "recorded");
    let base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(loader);
    collect(&shared, &mut thread);
    assert!(
        lookup(&shared, &mut thread),
        "the loader is live: so is its module"
    );
    thread.native_pin_roots.truncate(base);
    collect(&shared, &mut thread);
    collect(&shared, &mut thread);
    let gone = !lookup(&shared, &mut thread);
    eprintln!("zs5nd5: module gone with its loader: {gone}");
    assert!(gone, "the loader is dead: its module must go with it");
}

/// Kill control (`CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED=0`): the permanent
/// strong root again -- the module (and through it the loader) outlives
/// every collection. This is the leak the default arm closes.
#[test]
fn the_kill_switch_keeps_the_module_forever() {
    let Some(shared) = zgc_vm() else {
        return;
    };
    let _off = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED",
            Some("0"),
        )]),
    );
    let mut thread = JvmThread::new(ThreadId(5502), "zs5nd5-module-kill");
    if record_module(&shared, &mut thread).is_none() {
        return;
    }
    collect(&shared, &mut thread);
    collect(&shared, &mut thread);
    assert!(lookup(&shared, &mut thread), "kill arm: rooted for good");
    assert!(shared.classes.layer_module_pins.lock().is_empty());
}

/// Generational: unchanged (strong root, no pin row).
#[test]
fn other_collectors_keep_the_strong_root() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(5503), "zs5nd5-module-gen");
    if record_module(&shared, &mut thread).is_none() {
        return;
    }
    assert!(shared.classes.layer_module_pins.lock().is_empty());
    assert!(lookup(&shared, &mut thread));
}

/// The upcall containment's VM half: `native_alloc_oom_failure` names the
/// VM's heap-exhaustion payload as the `OutOfMemoryError` on ZGC, and
/// nothing else (any other payload; any payload off ZGC).
#[test]
fn the_heap_exhaustion_payload_is_recognised_on_zgc_only() {
    let oom = crate::runtime::native_oom::NativeAllocOom {
        length: 8,
        what: "object",
    };
    let other: Box<dyn std::any::Any + Send> = Box::new(String::from("not it"));
    if let Some(shared) = zgc_vm() {
        let mut thread = JvmThread::new(ThreadId(5504), "zs5nd5-payload");
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let got = ctx.native_alloc_oom_failure(&oom);
        assert!(
            matches!(
                got,
                Some(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::OutOfMemoryError { .. }
                )))
            ),
            "{got:?}"
        );
        assert!(ctx.native_alloc_oom_failure(&*other).is_none());
    }
    let gen = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(5505), "zs5nd5-payload-gen");
    let ctx = NativeContextImpl {
        shared: &gen,
        thread: &mut thread,
    };
    assert!(ctx.native_alloc_oom_failure(&oom).is_none());
}

/// zs s7/nd6 (rv4 residual 1): when `prune_layer_module_pins` frees a dead
/// module's handles, no package row may still name them (a freed handle's
/// address is reused by the next `add`, and a reader could resolve it as
/// whatever that handle names). Answers `(observer fired, rows that still
/// named a freed handle at the free)`, or `None` when the unit-test VM
/// cannot build the row.
fn prune_rows_naming_freed_handles() -> Option<(bool, usize)> {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    let shared = zgc_vm()?;
    let mut thread = JvmThread::new(ThreadId(5506), "zs7nd6-prune");
    record_module(&shared, &mut thread)?;
    // The loader is unreferenced: two collections clear the module's weak.
    collect(&shared, &mut thread);
    collect(&shared, &mut thread);
    if lookup(&shared, &mut thread) {
        eprintln!("zs7nd6: the module survived (loader kept elsewhere); row skipped");
        return None;
    }
    let fired = Rc::new(Cell::new(false));
    let stale = Rc::new(Cell::new(0usize));
    {
        let (fired, stale) = (fired.clone(), stale.clone());
        zs7nd6_prune_observer::set(Some(Box::new(move |shared, dead| {
            fired.set(true);
            let map = shared.classes.layer_module_packages.read();
            let n = map
                .values()
                .filter(|h| dead.iter().any(|&(_, m)| m == **h))
                .count();
            stale.set(stale.get() + n);
        })));
    }
    shared
        .classes
        .layer_module_prune_at
        .store(0, Ordering::Relaxed);
    {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.prune_layer_module_pins();
    }
    zs7nd6_prune_observer::set(None);
    assert!(shared.classes.layer_module_pins.lock().is_empty());
    assert!(shared.classes.layer_module_packages.read().is_empty());
    Some((fired.get(), stale.get()))
}

#[test]
fn zs7nd6_a_pruned_row_is_gone_before_its_handle_is_freed() {
    let Some((fired, stale)) = prune_rows_naming_freed_handles() else {
        return;
    };
    assert!(fired, "the prune freed the dead module's handles");
    assert_eq!(stale, 0, "a package row still named a freed handle");
}

/// Control (`CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST=0`, the s5 order): the
/// handles are freed while the row still names them -- the window rv4
/// residual 1 describes. This is the "before" of the test above.
#[test]
fn zs7nd6_the_kill_switch_frees_handles_under_a_live_row() {
    let _off = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST",
            Some("0"),
        )]),
    );
    let Some((fired, stale)) = prune_rows_naming_freed_handles() else {
        return;
    };
    assert!(fired);
    assert_eq!(stale, 1, "s5 order: the row outlives its freed handle");
}
