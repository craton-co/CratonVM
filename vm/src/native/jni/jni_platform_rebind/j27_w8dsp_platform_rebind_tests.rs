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
