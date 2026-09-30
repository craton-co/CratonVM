// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RA.5 — JBoss Modules `ResourceRootFactory.createResourceLoader` native.
//!
//! `ResourceRootFactory.createResourceLoader(File, String, String)` is
//! called by `ModuleXmlParser` (and by RA.4's replacement Rust parser)
//! for every `<resource-root path="foo.jar"/>` entry. It returns a
//! `ResourceLoader` (an interface) that can enumerate classes/resources
//! within the jar or directory.
//!
//! JBoss Modules ships two canonical implementations:
//! * `PathResourceLoader` — backed by a `java.nio.file.Path` to a directory.
//! * `JarFileResourceLoader` — backed by a `java.util.jar.JarFile`.
//!
//! We delegate to the module's own factory helpers (`ResourceLoaders`),
//! which are plain Java code, so they don't depend on the bug the RA
//! roadmap is working around. If the target file is a directory we
//! use the Path-backed factory; otherwise we assume a jar and use the
//! JarFile-backed one.
//!
//! The native registered here is just the glue that inspects the
//! caller-supplied `java.io.File` and dispatches the right factory.

use std::path::Path;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::Value;

/// Static method signature:
/// ```text
/// ResourceRootFactory.createResourceLoader(
///     Ljava/io/File;
///     Ljava/lang/String;
///     Ljava/lang/String;
/// )Lorg/jboss/modules/ResourceLoader;
/// ```
///
/// Args layout (static method, no `this`):
///   0: File root (or File pointing at the jar)
///   1: String loaderPath — usually relative, resolved against root
///   2: String loaderName — cosmetic name used in stack traces
pub fn native_create_resource_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if args.len() < 3 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "ResourceRootFactory.createResourceLoader: expected 3 args, got {}",
                args.len()
            ),
        }));
    }
    let root_file = args[0];
    let loader_path = args[1];
    let loader_name_val = args[2];

    // Resolve root File → absolute filesystem path string (via real JDK File.getAbsolutePath()).
    let root_abs_val = ctx.invoke(
        "java/io/File",
        "getAbsolutePath",
        "()Ljava/lang/String;",
        &[root_file],
    )?;
    let root_abs = match root_abs_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if root_abs.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "createResourceLoader: root.getAbsolutePath() returned null".to_string(),
        }));
    }

    // Resolve the loaderPath against root.
    let loader_path_s = match loader_path {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };

    let target: std::path::PathBuf = if loader_path_s.is_empty() {
        Path::new(&root_abs).to_path_buf()
    } else {
        Path::new(&root_abs).join(&loader_path_s)
    };

    // Reject paths containing `..` segments to prevent a malicious
    // module.xml from escaping its module root — the legitimate
    // module.xml files shipped by WildFly/Keycloak never do this.
    for seg in target.iter() {
        if seg == std::ffi::OsStr::new("..") {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!(
                    "createResourceLoader: refusing to follow `..` segment in path: {}",
                    target.display()
                ),
            }));
        }
    }

    let is_dir = target.is_dir();

    // Reconstruct a File object pointing to the (possibly resolved)
    // target and pass it down to the JBoss Modules factory helpers.
    // For directory: build a Path via Paths.get, then
    //   ResourceLoaders.createPathResourceLoader(String name, Path root).
    // For jar: build a JarFile and call
    //   ResourceLoaders.createJarResourceLoader(String name, JarFile jar).

    let target_str = target.to_string_lossy().into_owned();
    let target_java_str = ctx.create_string(&target_str);

    if is_dir {
        // Build a NIO Path from the string.
        let path = path_from_string(ctx, &target_str)?;
        let loader = ctx.invoke(
            "org/jboss/modules/ResourceLoaders",
            "createPathResourceLoader",
            "(Ljava/lang/String;Ljava/nio/file/Path;)Lorg/jboss/modules/ResourceLoader;",
            &[loader_name_val, path],
        )?;
        return Ok(loader);
    }

    // Treat as JAR. new JarFile(File).
    let target_file = ctx.invoke(
        "java/io/File",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(target_java_str))],
    );
    let target_file = match target_file {
        Ok(Some(v)) => v,
        _ => {
            // Fallback: pass the original `root_file` File object.
            root_file
        }
    };
    let jar = ctx.invoke(
        "java/util/jar/JarFile",
        "<init>",
        "(Ljava/io/File;)V",
        &[target_file],
    )?;
    let jar_val = jar.unwrap_or(Value::Object(None));
    let loader = ctx.invoke(
        "org/jboss/modules/ResourceLoaders",
        "createJarResourceLoader",
        "(Ljava/lang/String;Ljava/util/jar/JarFile;)Lorg/jboss/modules/ResourceLoader;",
        &[loader_name_val, jar_val],
    )?;
    Ok(loader)
}

fn path_from_string(ctx: &mut dyn NativeContext, s: &str) -> Result<Value, MethodCallFailed> {
    let js = ctx.create_string(s);
    // Paths.get(String, String...) — pass empty String[] for varargs.
    let empty = ctx.new_ref_array(
        ctx.class_id_by_name("java/lang/String")
            .unwrap_or(cratonvm_types::ClassId::new(0)),
        0,
    );
    let v = ctx.invoke(
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(js)), Value::Object(Some(empty))],
    )?;
    Ok(v.unwrap_or(Value::Object(None)))
}

pub fn register_resource_loader_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "org/jboss/modules/xml/ModuleXmlParser$ResourceRootFactory",
        "createResourceLoader",
        "(Ljava/io/File;Ljava/lang/String;Ljava/lang/String;)Lorg/jboss/modules/ResourceLoader;",
        native_create_resource_loader,
    );
    // Also register the public helper class under its sibling name.
    r.register(
        "org/jboss/modules/ResourceRootFactory",
        "createResourceLoader",
        "(Ljava/io/File;Ljava/lang/String;Ljava/lang/String;)Lorg/jboss/modules/ResourceLoader;",
        native_create_resource_loader,
    );
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
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
}
