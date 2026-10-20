// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i11/jd: `CRATONVM_DBG_BASE_BODY_ENTRY`, the frame-entry witness for a
//! virtual / interface call that ran a SUPERCLASS body although the receiver's
//! own class chain overrides it.
//!
//! Page: `docs/internal/gc/gci-i4orch-compatible-concurrent-javac-reads-a-module-info-class-as-source-FIXED-20261017.md`
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
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_BASE_BODY_ENTRY")
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
mod tests {
    use super::*;

    fn link(name: &str, o: bool) -> ChainLink<'_> {
        ChainLink {
            name,
            declares_concrete_override: o,
        }
    }

    #[test]
    fn gci_i11jd_filter_value_rule() {
        assert_eq!(parse_filter(None), None);
        for off in ["", "0", "false", "OFF", "no", "  "] {
            assert_eq!(parse_filter(Some(off)), None, "{off:?}");
        }
        for on in ["1", "true", "on", "YES"] {
            assert_eq!(parse_filter(Some(on)).as_deref(), Some(DEFAULT_PREFIX), "{on:?}");
        }
        assert_eq!(parse_filter(Some("*")).as_deref(), Some(""));
        assert_eq!(parse_filter(Some("all")).as_deref(), Some(""));
        assert_eq!(parse_filter(Some(" com/acme/ ")).as_deref(), Some("com/acme/"));
    }

    /// The S2 shape: the base `LocationHandler` body entered with a
    /// `ModuleSourcePathLocationHandler` receiver, which overrides it.
    #[test]
    fn gci_i11jd_base_body_with_an_overriding_receiver_is_reported() {
        let base = "com/sun/tools/javac/file/Locations$LocationHandler";
        let chain = [
            link("com/sun/tools/javac/file/Locations$ModuleSourcePathLocationHandler", true),
            link("com/sun/tools/javac/file/Locations$BasicLocationHandler", false),
            link(base, true),
            link("java/lang/Object", false),
        ];
        assert_eq!(
            overrider_below(&chain, base),
            Some("com/sun/tools/javac/file/Locations$ModuleSourcePathLocationHandler")
        );
    }

    #[test]
    fn gci_i11jd_correct_dispatch_is_silent() {
        let base = "com/sun/tools/javac/file/Locations$LocationHandler";
        // The receiver's own body (declaring == receiver class).
        let own = [link("A", true), link(base, true)];
        assert_eq!(overrider_below(&own, "A"), None);
        // An inherited body nobody below overrides (SimpleLocationHandler).
        let inherited = [
            link("Simple", false),
            link("Basic", false),
            link(base, true),
            link("java/lang/Object", false),
        ];
        assert_eq!(overrider_below(&inherited, base), None);
        // Declaring class not on the chain (a static method's first argument,
        // an interface default method).
        assert_eq!(overrider_below(&inherited, "some/Iface"), None);
        // Empty chain.
        assert_eq!(overrider_below(&[], base), None);
    }

    /// Source pin: every frame-push funnel that holds a `SharedVm` asks the
    /// witness after its push.
    #[test]
    fn gci_i11jd_witness_is_called_at_the_push_funnels() {
        let call = "super::gci_i11jd_witness::note_entry(shared, thread);";
        assert_eq!(include_str!("invoke_fast.rs").matches(call).count(), 5);
        assert_eq!(include_str!("jvmti_events.rs").matches(call).count(), 2);
        assert_eq!(include_str!("invoke.rs").matches(call).count(), 1);
        let own = "\n    gci_i11jd_witness::note_entry(shared, thread);\n";
        assert_eq!(include_str!("../interpreter.rs").matches(own).count(), 2);
    }

    /// The filter is per-VM state: latched into `SharedVm` at construction,
    /// read through `shared` at the gate, and this module declares no static.
    #[test]
    fn gci_i11jd_filter_is_per_vm_state() {
        let own = include_str!("gci_i11jd_witness.rs").replace("\r\n", "\n");
        let kw = ["sta", "tic "].concat();
        assert!(
            !own.lines().any(|l| l.trim_start().starts_with(&kw)),
            "no process static in the witness module"
        );
        assert!(own.contains("if let Some(prefix) = shared.dbg_base_body_entry_filter.as_deref() {"));
        let init = include_str!("../../vm/vm_init.rs").replace("\r\n", "\n");
        assert!(init.contains("pub dbg_base_body_entry_filter: Option<Box<str>>,"));
        assert!(init.contains(
            "dbg_base_body_entry_filter:\n                crate::runtime::interpreter::gci_i11jd_witness::filter_from_env(),"
        ));
    }

    /// An override ABOVE the entered body is not one below it.
    #[test]
    fn gci_i11jd_override_above_the_body_does_not_count() {
        let chain = [link("C", false), link("B", true), link("A", true)];
        assert_eq!(overrider_below(&chain, "B"), None);
        assert_eq!(overrider_below(&chain, "A"), Some("B"));
    }
}
