// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot 25 `jni_RegisterNatives`'s platform re-registration warning (j27
//! wave 8, lane W8DSP, page
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! HotSpot (`prims/jni.cpp`, as recalled; no checkout here) decides ONCE per
//! call, before the entry loop:
//!
//! ```text
//! do_warning = k is an InstanceKlass
//!           && (loader == boot || loader is the platform loader)
//!           && k's module is named
//!           && (caller == nullptr || caller->class_loader() != loader)
//! ```
//!
//! where `caller` is `thread->security_get_caller_class(1)`: depth 0 is the
//! native frame that called `RegisterNatives`, depth 1 its Java caller.
//! CratonVM pushes no frame for a native, so the innermost Java frame here IS
//! HotSpot's depth 1. Then, for each entry whose name and signature passed the
//! symbol-table probe (an unknown symbol throws `NoSuchMethodError` before the
//! warning), and before `Method::register_native` (whose refusal comes after
//! it), HotSpot logs at `warning` on tag set `jni,resolve`:
//!
//! ```text
//! Re-registering of platform native method: <external class>.<name><sig> from code in a different classloader
//! ```
//!
//! and registers the method anyway. No switch: it is HotSpot's output.

use cratonvm_types::ClassLoaderId;

use crate::classloading::ClassId;
use crate::runtime::unified_logging::{log_warning_line, LogTag};
use crate::threading::JvmThread;
use crate::vm::SharedVm;

/// HotSpot's `do_warning` from its facts: the named class's defining loader,
/// whether its module is named, and the caller's loader (`None`: no Java
/// caller). Pure.
pub(super) fn rebind_warns(
    target: ClassLoaderId,
    target_in_named_module: bool,
    caller: Option<ClassLoaderId>,
) -> bool {
    let platform = matches!(target, ClassLoaderId::Bootstrap | ClassLoaderId::Extension);
    platform && target_in_named_module && caller != Some(target)
}

/// The text of one entry's warning. Pure.
pub(super) fn rebind_warning_text(external_class: &str, name: &str, sig: &str) -> String {
    format!(
        "Re-registering of platform native method: {external_class}.{name}{sig} \
         from code in a different classloader"
    )
}

/// Once per `RegisterNatives` call: the external name of `class_id` when its
/// entries warn, `None` when they do not (or when a fact is missing: a class
/// or caller this VM cannot find stays silent, as before).
pub(super) fn platform_rebind_class(
    shared: &SharedVm,
    thread: &JvmThread,
    class_id: ClassId,
) -> Option<String> {
    // The frame walk takes no class-manager lock; do it before the read.
    let caller =
        crate::runtime::stackwalker::frame_class_ids_tail(&thread.frames, 1).0.first().copied();
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    // Only an InstanceKlass: not an array class, not a primitive mirror (no
    // package; a named module never holds the unnamed package).
    if class.name.starts_with('[') || !class.name.contains('/') {
        return None;
    }
    let named = crate::classloading::access_control::layer_module_of(class, &cm.module_registry)
        .is_some()
        || class.module_name.as_deref().is_some_and(|m| !m.is_empty());
    let caller_loader = match caller {
        None => None,
        Some(id) => Some(cm.get_class(id)?.loader_id),
    };
    if !rebind_warns(class.loader_id, named, caller_loader) {
        return None;
    }
    Some(crate::runtime::exceptions::hotspot_external_name_of_class(
        &class.name,
        class.hidden,
    ))
}

/// Log one entry's warning (`-Xlog` routing, or HotSpot's default stdout).
pub(super) fn warn_platform_rebind(external_class: &str, name: &str, sig: &str) {
    let uptime = super::with_shared_vm(|s| s.debug.diagnostic_counters.uptime_secs())
        .unwrap_or(0.0);
    log_warning_line(
        LogTag::JniResolve,
        &rebind_warning_text(external_class, name, sig),
        uptime,
    );
}

#[cfg(test)]
mod j27_w8dsp_platform_rebind_tests;
