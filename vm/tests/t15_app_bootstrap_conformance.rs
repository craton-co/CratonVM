// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T15 — Real-app bootstrap conformance test suite.
//!
//! Verifies that all T15.1 native methods are registered and wired to
//! real implementations, covering MethodHandleNatives, Finalizer,
//! ClassLoader.defineClass, and Array.newArray.
//!
//!     cargo test -p cratonvm-vm --test t15_app_bootstrap_conformance -- --nocapture

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

fn read_ws(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Whitespace-stripped source. `registry.register(...)` calls are frequently
/// wrapped across lines by rustfmt, so per-line text scans miss them (and would
/// conflict with `cargo fmt`); matching the whitespace-free form makes a
/// registration detectable regardless of wrapping.
fn compact_ws(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True if the whitespace-free source registers a MethodHandleNatives method,
/// whether the call uses the class literal or the local `mhn` class variable.
fn registers_mhn(compact: &str, method: &str, desc: &str) -> bool {
    let suffix = format!("\"{method}\",\"{desc}\"");
    compact.contains(&format!(
        "\"java/lang/invoke/MethodHandleNatives\",{suffix}"
    )) || compact.contains(&format!("mhn,{suffix}"))
}

/// True if the `register("class","method", ...)` call (any wrapping) is wired
/// to `func`. Scans a bounded window after the `"class","method"` key covering
/// the descriptor + handler argument. (A `;`-terminated window can't be used:
/// JVM descriptors contain `;`, which would truncate before the handler.)
fn wired(compact: &str, class: &str, method: &str, func: &str) -> bool {
    let key = format!("\"{class}\",\"{method}\"");
    match compact.find(&key) {
        Some(i) => {
            let start = i + key.len();
            let end = (start + 220).min(compact.len());
            compact[start..end].contains(func)
        }
        None => false,
    }
}

// ===========================================================================
// T15.1.1-2 — MethodHandleNatives natives registered
// ===========================================================================

// MEASURED against Temurin/TornadoVM JDK 25.0.3, 2026-08-30, with
// `javap -p -s java.lang.invoke.MethodHandleNatives`. Every entry below is
// `static native` there. Two rows were removed because they are not:
//
//   getConstant(I)I    NOT DECLARED by JDK 25 at all. The nearest thing is
//                      `private static native int getNamedCon(int, Object[])`
//                      -- different name, different descriptor. `getConstant`
//                      is a JDK 8 / 11 era method.
//
//   linkCallSite(...)  DECLARED, but as plain Java with a Code attribute, not
//                      native -- and the descriptor this list carried
//                      (`(Ljava/lang/Object;ILjava/lang/invoke/MemberName;...`)
//                      does not exist in JDK 25 either, whose signature is six
//                      Objects: `(Ljava/lang/Object;Ljava/lang/Object;
//                      Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;
//                      [Ljava/lang/Object;)Ljava/lang/invoke/MemberName;`.
//
// Both had implementations written (`native_mhn_get_constant`,
// `native_mhn_link_call_site`) that were never registered and that nothing
// could reach: neither method is declared by the real JDK in the shape this
// list wanted, the synthetic class library declares no `MethodHandleNatives`
// at all, and nothing in the tree calls either. Registering them would have
// turned this gate green with two entries that can never be dispatched in any
// mode, so the implementations are gone with this list (git history keeps them
// if invokedynamic linkage ever needs them).
//
// NOT changed, but worth a reader's attention: `linkMethod` IS registered as a
// native and is ALSO plain Java in JDK 25, so it shadows real bytecode. That
// may be deliberate -- signature-polymorphic linkage is exactly the thing a VM
// has to intercept -- but it is the same shape as the `ServiceLoader` shadow
// that was removed for being subtly wrong, and nobody has written down which
// it is here.
const MHN_NATIVES: &[(&str, &str)] = &[
    ("resolve", "(Ljava/lang/invoke/MemberName;Ljava/lang/Class;IZ)Ljava/lang/invoke/MemberName;"),
    ("init", "(Ljava/lang/invoke/MemberName;Ljava/lang/Object;)V"),
    ("linkMethod", "(Ljava/lang/Class;ILjava/lang/Class;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/invoke/MemberName;"),
    ("objectFieldOffset", "(Ljava/lang/invoke/MemberName;)J"),
    ("staticFieldOffset", "(Ljava/lang/invoke/MemberName;)J"),
    ("staticFieldBase", "(Ljava/lang/invoke/MemberName;)Ljava/lang/Object;"),
    ("getMemberVMInfo", "(Ljava/lang/invoke/MemberName;)Ljava/lang/Object;"),
    ("registerNatives", "()V"),
];

#[test]
fn t15_method_handle_natives_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in MHN_NATIVES {
        // Registration may use the class literal or the local `mhn` variable.
        if !registers_mhn(&compact, method, descriptor) {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T15: {}/{} MethodHandleNatives methods missing:\n  {}",
        missing.len(),
        MHN_NATIVES.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T15.1] ✓ All {}/{} MethodHandleNatives natives registered",
        MHN_NATIVES.len(),
        MHN_NATIVES.len(),
    );
}

// ===========================================================================
// T15.1.3-4 — ClassLoader.defineClass and findBootstrapClass
// ===========================================================================

#[test]
fn t15_classloader_natives_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));

    let required = [
        (
            "findBootstrapClass",
            "native_classloader_find_bootstrap_class",
        ),
        ("defineClass0", "native_classloader_define_class0"),
        ("defineClass1", "native_classloader_define_class1"),
    ];

    for (method, func) in &required {
        assert!(
            wired(&compact, "java/lang/ClassLoader", method, func),
            "T15: ClassLoader.{method} not wired to {func}",
        );
    }
    eprintln!(
        "[T15.2] ✓ ClassLoader.defineClass0/1 and findBootstrapClass use real implementations"
    );
}

// ===========================================================================
// T15.1.5 — Finalizer.register
// ===========================================================================

#[test]
fn t15_finalizer_register_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let found = wired(
        &compact,
        "java/lang/ref/Finalizer",
        "register",
        "native_finalizer_register",
    );
    assert!(found, "T15: Finalizer.register not registered");
    eprintln!("[T15.3] ✓ Finalizer.register uses real implementation");
}

// ===========================================================================
// T15.1.6 — Array.newArray
// ===========================================================================

#[test]
fn t15_array_new_array_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let found = wired(
        &compact,
        "java/lang/reflect/Array",
        "newArray",
        "native_array_new_array",
    );
    assert!(found, "T15: Array.newArray not registered");
    eprintln!("[T15.4] ✓ Array.newArray registered with real implementation");
}

// ===========================================================================
// T15.1.7 — VM getuid/getgid/geteuid/getegid
// ===========================================================================

#[test]
fn t15_vm_uid_gid_registered() {
    // Whitespace-free, not line-by-line. These are registered with
    // `register_with_kind`, which rustfmt splits across four lines, so the
    // class literal and the method name are never on the same one and a
    // per-line scan reports every one of them missing. They are all there.
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let required = ["getuid", "geteuid", "getgid", "getegid"];
    let mut missing: Vec<&str> = Vec::new();

    for method in &required {
        if !compact.contains(&format!("\"jdk/internal/misc/VM\",\"{method}\"")) {
            missing.push(method);
        }
    }

    assert!(
        missing.is_empty(),
        "T15: VM methods missing: {}",
        missing.join(", "),
    );
    eprintln!("[T15.5] ✓ All VM.getuid/geteuid/getgid/getegid registered");
}

// ===========================================================================
// T15.1.8 — Named implementations exist
// ===========================================================================

const T15_IMPLEMENTATIONS: &[(&str, &str)] = &[
    ("lang_invoke.rs", "native_mhn_resolve"),
    ("lang_invoke.rs", "native_mhn_init"),
    ("lang_invoke.rs", "native_mhn_link_method"),
    ("lang_invoke.rs", "native_mhn_object_field_offset"),
    ("lang_invoke.rs", "native_mhn_static_field_offset"),
    ("lang_invoke.rs", "native_mhn_static_field_base"),
    ("lang_invoke.rs", "native_mhn_get_member_vm_info"),
    ("lang_system.rs", "native_finalizer_register"),
    ("lang_system.rs", "native_array_new_array"),
    ("lang_system.rs", "native_classloader_define_class0"),
    ("lang_system.rs", "native_classloader_define_class1"),
];

#[test]
fn t15_named_implementations_exist() {
    let mut missing: Vec<String> = Vec::new();

    for &(file, func) in T15_IMPLEMENTATIONS {
        let path = format!("native-builtins/src/{}", file);
        let src = read_ws(&path);
        let pattern = format!("fn {func}(");
        if !src.contains(&pattern) {
            missing.push(format!("{file}::{func}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T15: {} implementations missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T15.6] ✓ All {}/{} T15 named implementations found",
        T15_IMPLEMENTATIONS.len(),
        T15_IMPLEMENTATIONS.len(),
    );
}

// ===========================================================================
// T15.7 — No stubs in critical paths
// ===========================================================================

#[test]
fn t15_no_stubs_in_mhn_registrations() {
    let lib = read_ws("native-builtins/src/lib.rs");
    let stub_markers = ["todo!()", "unimplemented!()", "STUB"];
    let mut stubs: Vec<String> = Vec::new();

    for line in lib.lines() {
        if !(line.contains("MethodHandleNatives") || line.contains("mhn,"))
            || !line.contains("register")
        {
            continue;
        }
        let lower = line.to_lowercase();
        for marker in &stub_markers {
            if lower.contains(&marker.to_lowercase()) {
                stubs.push(line.trim().to_string());
                break;
            }
        }
    }

    assert!(
        stubs.is_empty(),
        "T15: stub patterns found in MHN registrations:\n  {}",
        stubs.join("\n  "),
    );
    eprintln!("[T15.7] ✓ No stub patterns in MethodHandleNatives registrations");
}

// ===========================================================================
// T15.8 — Unit tests exist
// ===========================================================================

#[test]
fn t15_unit_tests_exist() {
    let src = read_ws("native-builtins/src/lang_system.rs");

    let required_patterns = [
        "finalizer_register_with_object",
        "finalizer_register_with_null",
        "array_new_array_int",
        "array_new_array_negative_size",
        "define_class1_empty_bytes_throws",
        "define_class1_out_of_bounds",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &pattern in &required_patterns {
        if !src.contains(pattern) {
            missing.push(pattern);
        }
    }

    assert!(
        missing.is_empty(),
        "T15: {} unit test patterns missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T15.8] ✓ All {}/{} T15 unit test patterns found",
        required_patterns.len(),
        required_patterns.len(),
    );
}

// ===========================================================================
// T15.9 — defineClass uses real implementation (not null stub)
// ===========================================================================

#[test]
fn t15_define_class_not_stub() {
    // Compacted for the same reason as `t15_vm_uid_gid_registered`: the
    // registration spans lines, so "the line holding both strings" does not
    // exist. The stub check then runs over a bounded window after the
    // class+method key rather than over one line -- see `wired`, which
    // already does this and explains why the window cannot end at `;`.
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));

    for method in &["defineClass0", "defineClass1"] {
        let key = format!("\"java/lang/ClassLoader\",\"{method}\"");
        let at = compact
            .find(&key)
            .unwrap_or_else(|| panic!("defineClass registration not found for {method}"));
        let end = (at + 400).min(compact.len());
        let line = &compact[at..end];

        assert!(
            !line.contains("|_ctx,_args|"),
            "T15: ClassLoader.{method} still uses inline stub: {}",
            line.trim(),
        );
    }
    eprintln!("[T15.9] ✓ ClassLoader.defineClass0/1 no longer use inline stubs");
}
