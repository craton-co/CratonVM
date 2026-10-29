//! HotSpot 25 `jni_RegisterNatives`'s platform re-registration warning (j27
//! wave 8, lane W8DSP, page
//! `docs/internal/jit/j27-jni-registernatives-platform-rebind-warning-missing-FIXED-20261029.md`).
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
mod j27_w8dsp_platform_rebind_tests {
    use super::*;
    use crate::runtime::unified_logging::warning_default_line;

    #[test]
    fn a_boot_class_in_a_named_module_warns_for_an_application_caller() {
        let app = Some(ClassLoaderId::Application);
        assert!(rebind_warns(ClassLoaderId::Bootstrap, true, app));
        assert!(rebind_warns(ClassLoaderId::Extension, true, app));
        assert!(rebind_warns(ClassLoaderId::Bootstrap, true, Some(ClassLoaderId::UserDefined(7))));
        // A platform class rebound from boot code: different loader, warns.
        assert!(rebind_warns(ClassLoaderId::Extension, true, Some(ClassLoaderId::Bootstrap)));
        // No Java caller (an attached host thread): HotSpot's `caller == nullptr`.
        assert!(rebind_warns(ClassLoaderId::Bootstrap, true, None));
    }

    #[test]
    fn same_loader_unnamed_module_and_application_classes_stay_silent() {
        // The JDK's own `registerNatives` from the class's `<clinit>`.
        assert!(!rebind_warns(ClassLoaderId::Bootstrap, true, Some(ClassLoaderId::Bootstrap)));
        assert!(!rebind_warns(ClassLoaderId::Extension, true, Some(ClassLoaderId::Extension)));
        // `-Xbootclasspath/a` classes are boot-loaded but in the unnamed module.
        assert!(!rebind_warns(ClassLoaderId::Bootstrap, false, Some(ClassLoaderId::Application)));
        // Application and user-loader classes never warn.
        assert!(!rebind_warns(ClassLoaderId::Application, true, None));
        assert!(!rebind_warns(ClassLoaderId::UserDefined(9), true, Some(ClassLoaderId::Application)));
    }

    #[test]
    fn the_line_is_hotspots_default_stdout_shape() {
        let text = rebind_warning_text("java.lang.Thread", "yield0", "()V");
        assert_eq!(
            text,
            "Re-registering of platform native method: java.lang.Thread.yield0()V \
             from code in a different classloader"
        );
        assert_eq!(
            warning_default_line(0.0456, LogTag::JniResolve, &text),
            format!("[0.046s][warning][jni,resolve] {text}")
        );
    }

    #[test]
    fn the_jni_resolve_tag_set_is_selectable() {
        assert_eq!(LogTag::from_str("jni+resolve"), Ok(LogTag::JniResolve));
        assert_eq!(LogTag::JniResolve.as_str(), "jni,resolve");
        assert!(LogTag::all().contains(&LogTag::JniResolve));
    }
}
