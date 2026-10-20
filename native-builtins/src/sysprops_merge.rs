// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One reader for the system properties: the Java `System.props`, merged with the VM's store.
//!
//! SBR round 5, lane `sysprops`. Under `--jdk-only` `System.setProperty`,
//! `clearProperty` and `getProperties` run the JDK's own bytecode, which writes
//! the real `System.props` (`Properties` over a `ConcurrentHashMap`) and nothing
//! else. The ~130 Rust readers of the property store
//! (`NativeContext::get_system_property` / `list_system_properties`) read the
//! VM's private `SharedVm.system_properties` map, and so never saw such a write
//! (`Sbr4LangPropStore`: `InetAddress.getLoopbackAddress` after a run-time
//! `java.net.preferIPv4Stack=true` answered the IPv6 loopback).
//!
//! The fix is ONE reader helper that every `get_system_property` goes through:
//! [`resolve`]. It reads the Java map WITHOUT running Java (`&self` heap reads
//! only: `System.props.map.table`, the bin chain, `String` keys and values), then
//! merges it with the store, latest writer wins:
//!
//! * the *baseline* is what the Java map held when the VM first looked at it;
//!   a Java entry that differs from the baseline (a write, or a removal of a
//!   baseline key) is an application write since then and WINS;
//! * otherwise the store has no competitor: the store value wins (a Rust-side
//!   `set_system_property` after the baseline re-baselines the key to the Java
//!   value of that instant, so the store's later write beats the stale Java one),
//!   and a key the store never had falls back to the Java value (`java.version`).
//!
//! A bin the walk cannot read (`ForwardingNode`/`TreeBin`/`ReservedNode`,
//! negative hash) answers "unavailable" and the store decides, as before.
//!
//! Gated on [`jdk_bodies_own_system_props`]: `--jdk-only`, or `--compatible`
//! with the `jdk-concurrent-hash-map` row (nio21 wave 2 L1), which drops the
//! same three `System` rows. Plain `--compatible` keeps its natives, which mirror both
//! sides themselves.

use std::collections::{HashMap, HashSet};

use cratonvm_native_api::registry::NativeContext;
use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_types::{ClassId, ObjectRef, Value};

#[derive(Default)]
struct Merge {
    /// `java/lang/System` and the static slot of `props`, resolved once.
    props_slot: Option<(ClassId, usize)>,
    /// Java-side value per key at the baseline, `None` = absent then. Only keys
    /// listed here carry a baseline; every other key's baseline is "absent".
    base: HashMap<String, Option<String>>,
    baseline_taken: bool,
    /// Keys the VM itself removed from the store since (a Java value equal to the
    /// baseline must not resurrect them).
    rust_removed: HashSet<String>,
}

static STATE: VmScoped<Merge> = VmScoped::new();

/// `CRATONVM_SYSPROPS_READ_THROUGH=0` switches the read-through off, leaving the VM store as
/// the only reader's source (the pre-SBR5 behaviour). It exists as the positive control of
/// the lane's probes: with the three property rows retired and this off,
/// `Sbr5SyspropsStore` fails on `InetAddress.getLoopbackAddress` exactly as
/// `Sbr4LangPropStore` did. Read once per process.
fn read_through_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_SYSPROPS_READ_THROUGH")
    })
}

/// Do the JDK's own `System.setProperty` / `clearProperty` / `getProperties`
/// bodies own `System.props` in this VM? `--jdk-only` (SBR5 sysprops), or the
/// `--compatible` `jdk-concurrent-hash-map` row, whose family drops the same
/// three rows (`retired_shadow::is_chm_pair_system_property_row`). Per-VM: the
/// row is the VM registry's bit (`NativeContext::aligned`), which answers
/// exactly `is_jdk_only()` while the row is off.
pub(crate) fn jdk_bodies_own_system_props(ctx: &dyn NativeContext) -> bool {
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// Drop the per-VM row (called from `forget_vm_system_singletons`).
pub fn forget_vm(vm_identity: usize) {
    STATE.forget(vm_identity);
}

/// `System.setProperties(p)` swapped `System.props`: forget the baseline of the old
/// object (the next read takes a new one from the new object).
pub fn rebaseline(vm_identity: usize) {
    STATE.with(vm_identity, |m| {
        m.base.clear();
        m.rust_removed.clear();
        m.baseline_taken = false;
    });
}

enum Lookup {
    Found(String),
    Absent,
    Unavailable,
}

fn java_hash(key: &str) -> i32 {
    let mut h: i32 = 0;
    for u in key.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(i32::from(u));
    }
    h
}

fn int_of(v: &Value) -> Option<i32> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

/// `System.props.map` (the `ConcurrentHashMap`), when the VM has booted far
/// enough for it to exist.
fn java_map(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let cached = STATE.peek(vm, |m| m.props_slot).flatten();
    let (cid, idx) = match cached {
        Some(slot) => slot,
        None => {
            let cid = ctx.class_id_by_name("java/lang/System")?;
            let idx = ctx.static_field_index_by_name(cid, "props")?;
            STATE.with(vm, |m| m.props_slot = Some((cid, idx)));
            (cid, idx)
        }
    };
    let Value::Object(Some(props)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    match ctx.get_field_by_name(props, "map") {
        Value::Object(Some(map)) => Some(map),
        _ => None,
    }
}

fn node_fields(ctx: &dyn NativeContext, node: ObjectRef) -> [Value; 4] {
    let mut out = [
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
        Value::Object(None),
    ];
    ctx.get_fields_by_name(node, &["hash", "key", "val", "next"], &mut out);
    out
}

/// `ConcurrentHashMap.get(key)` for a `String` key, with no Java call.
fn lookup_in(ctx: &dyn NativeContext, map: ObjectRef, key: &str) -> Lookup {
    let Value::Object(Some(table)) = ctx.get_field_by_name(map, "table") else {
        // A map that has never been written has no table: no entries.
        return Lookup::Absent;
    };
    let n = ctx.array_length(table);
    if n == 0 {
        return Lookup::Absent;
    }
    let h0 = java_hash(key);
    let h = (h0 ^ ((h0 as u32) >> 16) as i32) & 0x7fff_ffff;
    let Value::Object(Some(mut node)) = ctx.get_array_element(table, (n - 1) & (h as usize)) else {
        return Lookup::Absent;
    };
    // A bin is short; the cap only protects against a corrupt cycle.
    for _ in 0..4096 {
        let f = node_fields(ctx, node);
        let Some(nh) = int_of(&f[0]) else {
            return Lookup::Unavailable;
        };
        if nh < 0 {
            return Lookup::Unavailable;
        }
        if nh == h {
            if let Value::Object(Some(k)) = f[1] {
                if ctx.read_string(k).as_deref() == Some(key) {
                    return match f[2] {
                        Value::Object(Some(v)) => match ctx.read_string(v) {
                            Some(s) => Lookup::Found(s),
                            // A non-String value: `Properties.getProperty` answers null.
                            None => Lookup::Absent,
                        },
                        _ => Lookup::Absent,
                    };
                }
            }
        }
        match f[3] {
            Value::Object(Some(next)) => node = next,
            _ => return Lookup::Absent,
        }
    }
    Lookup::Unavailable
}

/// Every String entry of the Java map; the flag is true when a bin could not be
/// read, so the list is a lower bound.
fn enumerate(ctx: &dyn NativeContext, map: ObjectRef) -> (Vec<(String, String)>, bool) {
    let mut out = Vec::new();
    let Value::Object(Some(table)) = ctx.get_field_by_name(map, "table") else {
        return (out, false);
    };
    let n = ctx.array_length(table);
    let mut partial = false;
    for i in 0..n {
        let Value::Object(Some(mut node)) = ctx.get_array_element(table, i) else {
            continue;
        };
        for _ in 0..4096 {
            let f = node_fields(ctx, node);
            match int_of(&f[0]) {
                Some(nh) if nh >= 0 => {
                    if let (Value::Object(Some(k)), Value::Object(Some(v))) = (&f[1], &f[2]) {
                        if let (Some(ks), Some(vs)) = (ctx.read_string(*k), ctx.read_string(*v)) {
                            out.push((ks, vs));
                        }
                    }
                }
                _ => {
                    partial = true;
                    break;
                }
            }
            match f[3] {
                Value::Object(Some(next)) => node = next,
                _ => break,
            }
        }
    }
    (out, partial)
}

fn ensure_baseline(ctx: &dyn NativeContext, map: ObjectRef) {
    let vm = ctx.vm_identity();
    if STATE.peek(vm, |m| m.baseline_taken).unwrap_or(false) {
        return;
    }
    let (entries, _) = enumerate(ctx, map);
    STATE.with(vm, |m| {
        if !m.baseline_taken {
            for (k, v) in entries {
                m.base.entry(k).or_insert(Some(v));
            }
            m.baseline_taken = true;
        }
    });
}

fn baseline_of(m: &Merge, key: &str) -> Option<String> {
    m.base.get(key).cloned().flatten()
}

fn decide(vm: usize, key: &str, java: Option<&str>, store: Option<String>) -> Option<String> {
    STATE.with(vm, |m| {
        let b = baseline_of(m, key);
        if java != b.as_deref() {
            // The application wrote (or removed) this key since the baseline.
            return java.map(str::to_string);
        }
        if m.rust_removed.contains(key) {
            return None;
        }
        store.or_else(|| java.map(str::to_string))
    })
}

/// The merged answer for `key`. `store` is the VM's private store (already read
/// by the caller, so this holds no store lock across the heap reads).
pub fn resolve(
    ctx: &dyn NativeContext,
    key: &str,
    store: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if !jdk_bodies_own_system_props(ctx) || !read_through_enabled() {
        return store();
    }
    let Some(map) = java_map(ctx) else {
        return store();
    };
    ensure_baseline(ctx, map);
    match lookup_in(ctx, map, key) {
        Lookup::Unavailable => store(),
        Lookup::Found(v) => decide(ctx.vm_identity(), key, Some(&v), store()),
        Lookup::Absent => decide(ctx.vm_identity(), key, None, store()),
    }
}

/// Every property, merged. `store` is the VM store's snapshot.
pub fn list(ctx: &dyn NativeContext, store: Vec<(String, String)>) -> Vec<(String, String)> {
    if !jdk_bodies_own_system_props(ctx) || !read_through_enabled() {
        return store;
    }
    let Some(map) = java_map(ctx) else {
        return store;
    };
    ensure_baseline(ctx, map);
    let (entries, partial) = enumerate(ctx, map);
    let java: HashMap<String, String> = entries.into_iter().collect();
    let store: HashMap<String, String> = store.into_iter().collect();
    let vm = ctx.vm_identity();
    let mut keys: Vec<&String> = java.keys().chain(store.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let s = store.get(k).cloned();
        let merged = if partial && !java.contains_key(k) {
            // Not found in a partially readable map: not proof of removal.
            s
        } else {
            decide(vm, k, java.get(k).map(String::as_str), s)
        };
        if let Some(v) = merged {
            out.push((k.clone(), v));
        }
    }
    out
}

/// The VM wrote `key` into the store (`removed == false`) or removed it from the
/// store: the store is now the newer writer for that key.
pub fn note_store_write(ctx: &dyn NativeContext, key: &str, removed: bool) {
    if !jdk_bodies_own_system_props(ctx) || !read_through_enabled() {
        return;
    }
    let Some(map) = java_map(ctx) else {
        return;
    };
    ensure_baseline(ctx, map);
    let java = match lookup_in(ctx, map, key) {
        Lookup::Found(v) => Some(v),
        Lookup::Absent => None,
        Lookup::Unavailable => return,
    };
    STATE.with(ctx.vm_identity(), |m| {
        m.base.insert(key.to_string(), java);
        if removed {
            m.rust_removed.insert(key.to_string());
        } else {
            m.rust_removed.remove(key);
        }
    });
}
