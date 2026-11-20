// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i11/jd: `CRATONVM_DBG_BASE_BODY_ENTRY`, the frame-entry witness for a
//! virtual / interface call that ran a SUPERCLASS body although the receiver's
//! own class chain overrides it.
//!
//! Page: `docs/feature-designs/real-jdk-mode.md`
//! (S2). javac's `Locations.listLocationsForModules(Location)` returns
//! `h.listLocationsForModules()`, and the base `LocationHandler` body is
//! `return null;`. If a dispatch door picks the resolved method instead of the
//! receiver's override, the frame this VM pushes is the BASE body with a
//! receiver of an overriding subclass in local 0. No correct `invokevirtual` /
//! `invokeinterface` produces that frame; an `invokespecial super.m()` does
//! (javac's `Locations` has exactly one: `ModulePathLocationHandler.setPaths`
//! -> `SimpleLocationHandler.setPaths`), so a line naming `setPaths` is the
//! expected false positive and the rule counts named methods.
//!
//! Called at the frame-push funnels that hold a `SharedVm`
//! (`invoke_fast.rs` fast doors and `push_frame_and_fire_entry`'s by-value
//! callers there, `jvmti_events.rs` `install_cached_frame`). Not covered: a
//! body that runs COMPILED (no interpreter frame is pushed for it) and a
//! deopt-resumed frame (not an entry).
//!
//! Value of the switch, latched once per VM into
//! `SharedVm::dbg_base_body_entry_filter` (per-VM state, no process static):
//! unset / `0` / `false` / `off` / `no` -> off (the hot path is one load of
//! that field); `1` / `true` / `on` / `yes` -> the default
//! class prefix `com/sun/tools/javac/file/Locations$`; `*` or `all` -> every
//! class; anything else -> that class-name prefix (internal form).
//!
//! Line format (stderr):
//! `[BASE-BODY] <declaring>.<name><desc> entered with receiver <class> whose chain overrides it in <overrider>`

use crate::types::Value;
use crate::vm::SharedVm;
use crate::threading::jvm_thread::JvmThread;

/// The prefix `=1` selects: the javac location handlers S2 runs through.
pub(crate) const DEFAULT_PREFIX: &str = "com/sun/tools/javac/file/Locations$";

/// The switch's value rule, as a pure function (unit-tested).
pub(crate) fn parse_filter(raw: Option<&str>) -> Option<String> {
    let v = raw?.trim();
    match v.to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "no" => None,
        "1" | "true" | "on" | "yes" => Some(DEFAULT_PREFIX.to_string()),
        "*" | "all" => Some(String::new()),
        _ => Some(v.to_string()),
    }
}

/// The switch, read once per VM: `SharedVm::dbg_base_body_entry_filter` is
/// initialised from this at construction (per-VM state, no process static).
pub(crate) fn filter_from_env() -> Option<Box<str>> {
    parse_filter(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .as_deref(),
    )
    .map(String::into_boxed_str)
}

/// One class of the receiver's chain, receiver first: its name and whether it
/// itself declares a concrete (non-abstract, non-static, non-bridge) method of
/// the entered frame's name and descriptor.
pub(crate) struct ChainLink<'a> {
    pub name: &'a str,
    pub declares_concrete_override: bool,
}

/// The witness predicate, as a pure function (unit-tested): `Some(overrider)`
/// when `declaring` is a PROPER superclass in `chain` (receiver first) and a
/// class strictly below it declares a concrete override; `None` otherwise
/// (the receiver's own class is the declaring class, the declaring class is
/// not on the chain -- a static method's first argument, an interface default
/// -- or nothing below it overrides).
pub(crate) fn overrider_below<'a>(chain: &[ChainLink<'a>], declaring: &str) -> Option<&'a str> {
    let first = chain.first()?;
    if first.name == declaring {
        return None;
    }
    let pos = chain.iter().position(|l| l.name == declaring)?;
    chain[..pos]
        .iter()
        .find(|l| l.declares_concrete_override)
        .map(|l| l.name)
}

/// The hot-path entry: one load of the per-VM latched filter when the switch
/// is off.
#[inline]
pub(super) fn note_entry(shared: &SharedVm, thread: &JvmThread) {
    if let Some(prefix) = shared.dbg_base_body_entry_filter.as_deref() {
        note_entry_slow(shared, thread, prefix);
    }
}

#[cold]
#[inline(never)]
fn note_entry_slow(shared: &SharedVm, thread: &JvmThread, prefix: &str) {
    let n = thread.frames.len();
    if n == 0 {
        return;
    }
    let frame = &thread.frames[n - 1];
    let declaring = frame.class_name();
    if !declaring.starts_with(prefix) {
        return;
    }
    let name = frame.method_name();
    if name.starts_with('<') {
        return;
    }
    let desc = frame.method_descriptor();
    let recv = match frame.get_local(0) {
        Value::Object(Some(r)) => r,
        _ => return,
    };
    let recv_cid = shared.mem.heap.class_id_of(recv);
    let line = {
        let cm = shared.classes.class_manager.read();
        let mut names: Vec<std::sync::Arc<str>> = Vec::new();
        let mut flags: Vec<bool> = Vec::new();
        let mut cur = Some(recv_cid);
        let mut guard = 0;
        while let Some(cid) = cur {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(class) = cm.get_class(cid) else {
                break;
            };
            let concrete = class
                .find_method(name, desc)
                .is_some_and(|m| !m.is_abstract() && !m.is_static() && !m.is_bridge());
            names.push(class.name.clone());
            flags.push(concrete);
            cur = class.superclass;
        }
        let chain: Vec<ChainLink<'_>> = names
            .iter()
            .zip(flags.iter())
            .map(|(n, f)| ChainLink {
                name: n,
                declares_concrete_override: *f,
            })
            .collect();
        overrider_below(&chain, declaring).map(|overrider| {
            format!(
                "[BASE-BODY] {declaring}.{name}{desc} entered with receiver {} whose chain overrides it in {overrider} (frames={n})",
                chain[0].name
            )
        })
    };
    if let Some(line) = line {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests;
