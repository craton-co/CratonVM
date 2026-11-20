use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// **THIS TEST CANNOT FAIL, and it is standing in for a SECURITY control.**
///
/// It re-implements the `..`-segment scan over its own literal `Path` and
/// asserts its own copy's result. `native_create_resource_loader` is never
/// called, so deleting the traversal guard from production — the check that
/// stops a malicious `module.xml` from escaping its module root — leaves
/// this green.
///
/// What the repair needs, and why it is not done here: the guard sits
/// roughly forty lines into `native_create_resource_loader`, behind
/// `ctx.invoke("java/io/File", "getAbsolutePath", …)` whose answer must be
/// a non-empty string or the function returns early with a different error.
/// **No `NativeContext` mock in this tree can script `invoke`** — both
/// `native-builtins/src/test_utils.rs` and `native-api/src/test_mock.rs`
/// answer `Ok(None)` for every call except `Class.getName`, so the native
/// bails at the "getAbsolutePath() returned null" arm before the guard is
/// reached — and hand-rolling a `NativeContext` here would mean
/// implementing ~93 required trait methods.
///
/// Two routes close it, both production changes:
///  * lift the scan into a callable predicate — e.g.
///    `fn path_escapes_root(target: &Path) -> bool` — that
///    `native_create_resource_loader` calls and this test can assert on
///    directly (both the positive `..` case AND the negative cases below,
///    which must NOT be rejected); or
///  * give the shared mock a scriptable `invoke`, after which this test can
///    drive the whole native and assert it returns `Err` for a `..` path
///    and does not for a clean one.
///
/// Until then, the assertions below are a property of `std::path::Path`'s
/// iterator — namely that it does NOT normalise `..` away, which is the
/// premise the production guard rests on. That premise is worth pinning
/// (a future switch to a normalising API would silently defeat the guard),
/// but it is not cover for the guard.
#[test]
fn path_rejects_parent_segment() {
    fn has_parent_segment(p: &Path) -> bool {
        p.iter().any(|seg| seg == std::ffi::OsStr::new(".."))
    }

    // `Path::iter` must SURFACE `..` rather than normalise it away — the
    // premise `native_create_resource_loader`'s guard depends on.
    assert!(has_parent_segment(Path::new(
        "/opt/kc/modules/foo/../../etc/passwd"
    )));
    assert!(has_parent_segment(Path::new("/opt/kc/modules/..")));
    assert!(has_parent_segment(Path::new("../evil.jar")));
    // And it must not fire on names that merely CONTAIN dots, or the guard
    // would refuse legitimate WildFly/Keycloak module roots.
    assert!(!has_parent_segment(Path::new(
        "/opt/kc/modules/foo/bar.jar"
    )));
    assert!(!has_parent_segment(Path::new("/opt/kc/modules/./foo.jar")));
    assert!(!has_parent_segment(Path::new(
        "/opt/kc/modules/..foo/a.jar"
    )));
    assert!(!has_parent_segment(Path::new(
        "/opt/kc/modules/foo../a.jar"
    )));
}

#[test]
fn absolute_root_path_is_preserved() {
    let root = "/opt/keycloak-16/modules";
    let loader_path = "foo.jar";
    let target: std::path::PathBuf = Path::new(root).join(loader_path);
    // PathBuf::join uses the platform separator; only assert the
    // components, not the exact byte form.
    let comps: Vec<String> = target
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert!(comps.iter().any(|c| c == "modules"));
    assert!(comps.iter().any(|c| c == "foo.jar"));
}
