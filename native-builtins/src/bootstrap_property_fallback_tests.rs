use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
/// Run `f` with the two `jboss.home.dir` inputs set as given.
///
/// `CRATONVM_JBOSS_MP_ROOT` is a **declared** flag, served from the
/// process-wide snapshot that latches on the first read of any flag. In
/// this crate's test binary that has always already happened by the time
/// these tests run, so the `set_var`/`remove_var` pair they used to do
/// changed `environ` and nothing `jboss_home_dir_fallback` would read:
/// whatever `CRATONVM_JBOSS_MP_ROOT` the developer happened to have
/// exported stayed in force, and the "must fall back" assertions held for
/// the wrong reason. It is overridden on the snapshot instead — thread
/// scope, because every reader here runs on this thread.
static JBOSS_CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_jboss_env<R>(home: Option<&str>, mp_root: Option<&str>, f: impl FnOnce() -> R) -> R {
    let _guard = JBOSS_CWD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Both names go through one thread-scoped override. `JBOSS_HOME` used
    // to be stashed into `environ` with `set_var` under a local mutex —
    // which serialised these tests against each other and against nothing
    // else, while thousands of tests ran on other threads. `set_var` is a
    // process-wide data race, so there was no lock that could have made it
    // safe; the override never touches `environ` at all.
    cratonvm_types::flags::with_thread_overrides(
        &[("JBOSS_HOME", home), ("CRATONVM_JBOSS_MP_ROOT", mp_root)],
        f,
    )
}

#[test]
fn jboss_home_dir_falls_back_to_jboss_home_env() {
    with_jboss_env(Some("/opt/wildfly"), None, || {
        assert_eq!(
            bootstrap_property_fallback("jboss.home.dir"),
            Some("/opt/wildfly".to_string())
        );
    });
}

#[test]
fn jboss_home_dir_falls_back_to_module_path_property() {
    with_jboss_env(None, None, || {
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_system_property("module.path", "/opt/wildfly/modules");
        assert_eq!(
            system_property_fallback(&ctx, "jboss.home.dir"),
            Some("/opt/wildfly".to_string())
        );
    });
}

#[test]
fn jboss_home_dir_falls_back_to_java_class_path_property() {
    with_jboss_env(None, None, || {
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_system_property("java.class.path", "/opt/wildfly/jboss-modules.jar");
        assert_eq!(
            system_property_fallback(&ctx, "jboss.home.dir"),
            Some("/opt/wildfly".to_string())
        );
    });
}

/// `CRATONVM_JBOSS_MP_ROOT` beats the current-directory probe below it.
///
/// The override is what makes this assertion mean anything: with the flag
/// served from the ambient snapshot, this test could not distinguish
/// "the module-path root won" from "nothing was set and the cwd probe
/// answered".
#[test]
fn jboss_home_dir_prefers_the_module_path_root_over_the_cwd() {
    let tmp = std::env::temp_dir().join(format!(
        "cratonvm-wildfly-mp-root-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("modules")).unwrap();
    let mp_root = tmp.join("modules");
    with_jboss_env(None, mp_root.to_str(), || {
        assert_eq!(
            bootstrap_property_fallback("jboss.home.dir"),
            Some(tmp.to_string_lossy().into_owned())
        );
    });
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn jboss_home_dir_falls_back_to_wildfly_current_dir() {
    let old_cwd = std::env::current_dir().unwrap();
    let tmp =
        std::env::temp_dir().join(format!("cratonvm-wildfly-home-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("modules")).unwrap();
    std::fs::write(tmp.join("jboss-modules.jar"), []).unwrap();

    let result = with_jboss_env(None, None, || {
        std::env::set_current_dir(&tmp).unwrap();
        let found = bootstrap_property_fallback("jboss.home.dir");
        std::env::set_current_dir(&old_cwd).unwrap();
        found
    });

    assert_eq!(result, Some(tmp.to_string_lossy().into_owned()));
    let _ = std::fs::remove_dir_all(&tmp);
}
