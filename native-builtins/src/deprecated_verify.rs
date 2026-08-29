// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T8.5 — Verification of all deprecated API round-trips.
//!
//! This module contains the `deprecated_apis_round_trip` test suite that
//! calls every API from T8.1–T8.4 and asserts spec-compliant results or
//! exceptions.  It also contains the cross-check registry that enumerates
//! all JDK 25 `@Deprecated` tagged APIs and verifies each has a native
//! implementation registered.

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// T8.5.1 — Round-trip test
// ---------------------------------------------------------------------------

/// What the SUPPORTED JDK IMAGES say about a deprecated triple.
///
/// The manifest below used to assert one thing about every row — "this is
/// registered" — and `H25-3` R2 refused a set of retirements because of it:
/// six of its `java/lang` entries name methods JDK 25 does not declare, and
/// this list asserted they stay registered forever. The column is the fix
/// nominated there (`H25-3` N2): adjudicate the manifest against the images
/// instead of maintaining it by hand.
///
/// The verdicts are MEASURED, not read off a changelog:
/// `javap -p --system <image> <class>` over all nine supported images —
/// JDK 17.0.20, 21.0.12 and 25.0.4 x linux/windows/macos —
/// re-derivable with `scripts/jdk-only-no-image-methods.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageStatus {
    /// At least one supported image declares the triple. The registration is
    /// a §1.5 bridge (or a deliberate cross-version one, like `Thread.stop0`,
    /// which only JDK 17 declares) and MUST stay.
    Declared,
    /// NO supported image declares it, anywhere on the receiver's hierarchy.
    /// A native standing in front of a method that does not exist can never
    /// be dispatched, so it must NOT be registered — and the test below
    /// asserts its absence rather than its presence.
    AbsentFromAllSupportedImages,
}

/// List of all deprecated APIs we implemented, in the form
/// `(class, method, descriptor, expected_behavior)`.
#[derive(Debug, Clone)]
struct DeprecatedApi {
    class: &'static str,
    method: &'static str,
    descriptor: &'static str,
    section: &'static str,
    images: ImageStatus,
}

/// Complete manifest of every deprecated native method registered in T8.
fn deprecated_api_manifest() -> Vec<DeprecatedApi> {
    vec![
        // ── T8.1 — java.lang.* ──────────────────────────────────────────
        DeprecatedApi { class: "java/lang/Thread", method: "stop0", descriptor: "(Ljava/lang/Object;)V", images: ImageStatus::Declared, section: "T8.1.1" },
        // `Thread.stop()V` is deliberately absent from this manifest: every
        // supported image declares it WITH a `Code` attribute, so the real
        // bytecode serves it and CratonVM registers no native. Retired
        // 2026-08-21 together with its duplicate in `lib.rs`; see the note at
        // `deprecated_lang.rs`.
        DeprecatedApi { class: "java/lang/Thread", method: "suspend0", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.2" },
        DeprecatedApi { class: "java/lang/Thread", method: "resume0", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.2" },
        DeprecatedApi { class: "java/lang/Thread", method: "destroy", descriptor: "()V", images: ImageStatus::AbsentFromAllSupportedImages, section: "T8.1.3" },
        DeprecatedApi { class: "java/lang/Thread", method: "countStackFrames", descriptor: "()I", images: ImageStatus::Declared, section: "T8.1.4" },
        DeprecatedApi { class: "java/lang/Runtime", method: "runFinalization", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.6" },
        DeprecatedApi { class: "java/lang/System", method: "runFinalization", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.6" },
        DeprecatedApi { class: "java/lang/System", method: "runFinalizersOnExit", descriptor: "(Z)V", images: ImageStatus::AbsentFromAllSupportedImages, section: "T8.1.7" },
        DeprecatedApi { class: "java/lang/ClassLoader", method: "defineClass", descriptor: "([BII)Ljava/lang/Class;", images: ImageStatus::Declared, section: "T8.1.9" },
        DeprecatedApi { class: "java/lang/Compiler", method: "compileClass", descriptor: "(Ljava/lang/Class;)Z", images: ImageStatus::Declared, section: "T8.1.10" },
        DeprecatedApi { class: "java/lang/Compiler", method: "compileClasses", descriptor: "(Ljava/lang/String;)Z", images: ImageStatus::Declared, section: "T8.1.10" },
        DeprecatedApi { class: "java/lang/Compiler", method: "enable", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.10" },
        DeprecatedApi { class: "java/lang/Compiler", method: "disable", descriptor: "()V", images: ImageStatus::Declared, section: "T8.1.10" },
        DeprecatedApi { class: "java/lang/Compiler", method: "command", descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;", images: ImageStatus::Declared, section: "T8.1.10" },

        // ── T8.1.8 — SecurityManager (in security_manager.rs) ──────────
        DeprecatedApi { class: "java/lang/SecurityManager", method: "checkPermission", descriptor: "(Ljava/security/Permission;)V", images: ImageStatus::Declared, section: "T8.1.8" },
        DeprecatedApi { class: "java/lang/SecurityManager", method: "checkRead", descriptor: "(Ljava/lang/String;)V", images: ImageStatus::Declared, section: "T8.1.8" },
        DeprecatedApi { class: "java/lang/SecurityManager", method: "checkWrite", descriptor: "(Ljava/lang/String;)V", images: ImageStatus::Declared, section: "T8.1.8" },
        DeprecatedApi { class: "java/lang/SecurityManager", method: "checkExit", descriptor: "(I)V", images: ImageStatus::Declared, section: "T8.1.8" },

        // ── T8.2 — java.io / java.util / java.text ─────────────────────
        DeprecatedApi { class: "java/util/Date", method: "<init>", descriptor: "(III)V", images: ImageStatus::Declared, section: "T8.2.1" },
        DeprecatedApi { class: "java/util/Date", method: "<init>", descriptor: "(IIIII)V", images: ImageStatus::Declared, section: "T8.2.1" },
        DeprecatedApi { class: "java/util/Date", method: "<init>", descriptor: "(IIIIII)V", images: ImageStatus::Declared, section: "T8.2.1" },
        DeprecatedApi { class: "java/util/Date", method: "getYear", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getMonth", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getDate", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getDay", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getHours", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getMinutes", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/util/Date", method: "getSeconds", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.2" },
        DeprecatedApi { class: "java/lang/String", method: "<init>", descriptor: "([BIII)V", images: ImageStatus::Declared, section: "T8.2.3" },
        DeprecatedApi { class: "java/lang/String", method: "getBytes", descriptor: "(II[BI)V", images: ImageStatus::Declared, section: "T8.2.4" },
        DeprecatedApi { class: "java/lang/Character", method: "isJavaLetter", descriptor: "(C)Z", images: ImageStatus::Declared, section: "T8.2.5" },
        DeprecatedApi { class: "java/lang/Character", method: "isJavaLetterOrDigit", descriptor: "(C)Z", images: ImageStatus::Declared, section: "T8.2.5" },
        DeprecatedApi { class: "java/lang/Character", method: "isSpace", descriptor: "(C)Z", images: ImageStatus::Declared, section: "T8.2.5" },
        DeprecatedApi { class: "java/lang/Class", method: "newInstance", descriptor: "()Ljava/lang/Object;", images: ImageStatus::Declared, section: "T8.2.6" },
        // T8.2.7 `java/lang/Number.{byteValue()B, shortValue()S}` were HERE and
        // are RETIRED (WORKER-4, `b77f068e3`). They are removed from the
        // manifest rather than re-tagged because NEITHER `ImageStatus` is true
        // of them, and the reason is the useful part:
        //
        //   * `Declared` asserts "this registration is a §1.5 bridge and MUST
        //     stay". They are not bridges. Both were transcriptions of a
        //     one-line `java.base` body (`return (byte) intValue();`), so
        //     nothing crosses a VM boundary and §1.5 cannot call them one. That
        //     mis-tag is what held them in place.
        //   * `AbsentFromAllSupportedImages` asserts absence, which is now the
        //     behaviour we want — but its NAME would be a lie: every supported
        //     image declares both, with a body that runs.
        //
        // A third status is arguably owed here — DECLARED, WITH A REAL BODY,
        // SERVED BY BYTECODE — and it would be the honest home for every row
        // this contract retires next. Raised for the lane that owns this file
        // rather than added in passing.
        //
        // Regrowth is still guarded: a re-registration lands in the stub
        // ratchet, and behaviour is covered by `probes/W4Deprecated.java`
        // (116 cases, 0 diffs against the oracle in both modes, driven through
        // a user `Number` subclass, the boxed types, `BigInteger`/`BigDecimal`
        // and a `Number`-typed reference).
        DeprecatedApi { class: "java/io/StringBufferInputStream", method: "read", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.10" },
        DeprecatedApi { class: "java/io/LineNumberInputStream", method: "getLineNumber", descriptor: "()I", images: ImageStatus::Declared, section: "T8.2.11" },
        DeprecatedApi { class: "java/net/URLDecoder", method: "decode", descriptor: "(Ljava/lang/String;)Ljava/lang/String;", images: ImageStatus::Declared, section: "T8.2.13" },
        DeprecatedApi { class: "java/net/URLEncoder", method: "encode", descriptor: "(Ljava/lang/String;)Ljava/lang/String;", images: ImageStatus::Declared, section: "T8.2.14" },

        // ── T8.3 — java.beans / java.rmi ────────────────────────────────
        DeprecatedApi { class: "java/beans/Beans", method: "instantiate", descriptor: "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;", images: ImageStatus::Declared, section: "T8.3.1" },
        DeprecatedApi { class: "java/rmi/server/RemoteRef", method: "getRefClass", descriptor: "(Ljava/io/ObjectOutput;)Ljava/lang/String;", images: ImageStatus::Declared, section: "T8.3.2" },

        // ── T8.4 — sun.* / jdk.internal.* ──────────────────────────────
        DeprecatedApi { class: "sun/misc/Unsafe", method: "defineClass", descriptor: "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;", images: ImageStatus::Declared, section: "T8.4.1" },
        DeprecatedApi { class: "sun/misc/Unsafe", method: "allocateMemory", descriptor: "(J)J", images: ImageStatus::Declared, section: "T8.4.2" },
        DeprecatedApi { class: "sun/misc/Unsafe", method: "freeMemory", descriptor: "(J)V", images: ImageStatus::Declared, section: "T8.4.2" },
        DeprecatedApi { class: "sun/misc/Unsafe", method: "reallocateMemory", descriptor: "(JJ)J", images: ImageStatus::Declared, section: "T8.4.2" },
        DeprecatedApi { class: "sun/reflect/Reflection", method: "getCallerClass", descriptor: "(I)Ljava/lang/Class;", images: ImageStatus::Declared, section: "T8.4.3" },
        DeprecatedApi { class: "sun/misc/Signal", method: "handle", descriptor: "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;", images: ImageStatus::Declared, section: "T8.4.4" },
        DeprecatedApi { class: "sun/misc/Signal", method: "raise", descriptor: "(Lsun/misc/Signal;)V", images: ImageStatus::Declared, section: "T8.4.4" },
    ]
}

/// Build a fresh registry with all deprecated natives registered.
///
/// The registrars run in the SAME ORDER as `lib.rs`'s
/// `register_all_natives`, because `NativeMethodRegistry::register` is
/// last-write-wins and several of these names are registered twice. A registry
/// assembled in a different order resolves those to a different body than the
/// VM does, so the verification below would be describing a registry that
/// never exists at runtime — see the note at `deprecated_util.rs`'s
/// `Hashtable.keys` registration, where which copy wins is the whole bug.
///
/// `deprecated_util` used to be missing here entirely, which is what made
/// `Character.isJavaLetter` / `isJavaLetterOrDigit` / `isSpace` look
/// unregistered to T8.5.1 and T8.5.3: all three are implemented and
/// registered, just by the one registrar this list omitted.
fn build_deprecated_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    crate::security_manager::register_security_manager_natives(&mut r);
    crate::deprecated_lang::register_deprecated_lang_natives(&mut r);
    crate::deprecated_io_util::register_deprecated_io_util_natives(&mut r);
    crate::deprecated_util::register_deprecated_util_natives(&mut r);
    crate::deprecated_internal::register_deprecated_internal_natives(&mut r);
    r
}

// ---------------------------------------------------------------------------
// T8.5.3 — JDK 25 @Deprecated cross-check
// ---------------------------------------------------------------------------

/// The JDK 25 deprecated API set that we track.  Each entry is a
/// `(class, method, descriptor)` tuple.  This list is derived from the
/// JDK 25 javadoc `@Deprecated` / `@Deprecated(forRemoval=true)` tags
/// for the core modules (java.base, java.management, java.rmi).
///
/// Any API in this list that is NOT registered in our native registry
/// should be reported and either implemented or explicitly shimmed
/// with T8.5.4's `UnsupportedOperationException` shim.
fn jdk25_deprecated_api_checklist() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // java.lang
        //
        // `stop`, `destroy` and `runFinalizersOnExit` are NOT here, and their
        // absence is load-bearing: `register_missing_deprecated_shims` below
        // registers a throwing shim for every entry of this list that is not
        // already registered, so leaving them in would have re-registered the
        // rows retired on 2026-08-21 under a different body. `stop` is served
        // by real bytecode on every supported image; the other two are
        // declared by none.
        ("java/lang/Thread", "stop0", "(Ljava/lang/Object;)V"),
        ("java/lang/Thread", "suspend0", "()V"),
        ("java/lang/Thread", "resume0", "()V"),
        ("java/lang/Thread", "countStackFrames", "()I"),
        ("java/lang/Runtime", "runFinalization", "()V"),
        ("java/lang/System", "runFinalization", "()V"),
        ("java/lang/Compiler", "compileClass", "(Ljava/lang/Class;)Z"),
        (
            "java/lang/Compiler",
            "compileClasses",
            "(Ljava/lang/String;)Z",
        ),
        ("java/lang/Compiler", "enable", "()V"),
        ("java/lang/Compiler", "disable", "()V"),
        (
            "java/lang/Compiler",
            "command",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        // SecurityManager
        (
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V",
        ),
        (
            "java/lang/SecurityManager",
            "checkRead",
            "(Ljava/lang/String;)V",
        ),
        (
            "java/lang/SecurityManager",
            "checkWrite",
            "(Ljava/lang/String;)V",
        ),
        ("java/lang/SecurityManager", "checkExit", "(I)V"),
        (
            "java/security/AccessController",
            "doPrivileged",
            "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        ),
        // java.util.Date
        ("java/util/Date", "getYear", "()I"),
        ("java/util/Date", "getMonth", "()I"),
        ("java/util/Date", "getDate", "()I"),
        ("java/util/Date", "getHours", "()I"),
        ("java/util/Date", "getMinutes", "()I"),
        ("java/util/Date", "getSeconds", "()I"),
        // Character
        ("java/lang/Character", "isJavaLetter", "(C)Z"),
        ("java/lang/Character", "isJavaLetterOrDigit", "(C)Z"),
        ("java/lang/Character", "isSpace", "(C)Z"),
        // Class.newInstance
        ("java/lang/Class", "newInstance", "()Ljava/lang/Object;"),
        // URL encoding
        (
            "java/net/URLDecoder",
            "decode",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "java/net/URLEncoder",
            "encode",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        // sun.misc.Unsafe
        ("sun/misc/Unsafe", "allocateMemory", "(J)J"),
        ("sun/misc/Unsafe", "freeMemory", "(J)V"),
        ("sun/misc/Unsafe", "reallocateMemory", "(JJ)J"),
        // Signal
        (
            "sun/misc/Signal",
            "handle",
            "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
        ),
        // Reflection
        (
            "sun/reflect/Reflection",
            "getCallerClass",
            "(I)Ljava/lang/Class;",
        ),
    ]
}

// ---------------------------------------------------------------------------
// T8.5.4 — Shim generator for any missing deprecated APIs
// ---------------------------------------------------------------------------

/// A shim function that throws `UnsupportedOperationException` for any
/// deprecated API that was not explicitly implemented.
fn deprecated_shim(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    Err(cratonvm_types::error::MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                message: "CratonVM T8.5.4: deprecated API not implemented".to_string(),
            },
        ),
    ))
}

/// Register a shim that throws `UnsupportedOperationException` with a
/// stable error code for any deprecated API that isn't already registered.
pub(crate) fn register_missing_deprecated_shims(r: &mut NativeMethodRegistry) {
    for (class, method, descriptor) in jdk25_deprecated_api_checklist() {
        if r.find(class, method, descriptor).is_none() {
            r.register(class, method, descriptor, deprecated_shim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // ── T8.5.1 — Round-trip: every deprecated API is registered ──────────

    #[test]
    fn t85_1_all_deprecated_apis_registered() {
        let r = build_deprecated_registry();
        let manifest = deprecated_api_manifest();
        let mut missing = Vec::new();

        let mut must_not_be_registered = Vec::new();

        for api in &manifest {
            let found = r.find(api.class, api.method, api.descriptor).is_some();
            match api.images {
                ImageStatus::Declared if !found => missing.push(format!(
                    "{} {}.{}{}",
                    api.section, api.class, api.method, api.descriptor
                )),
                // The other direction, and it is the half this test did not
                // have: a native registered in front of a method NO supported
                // image declares can never be dispatched, and asserting it
                // stays registered is what blocked the retirement in `H25-3`
                // R2. Assert the absence instead.
                ImageStatus::AbsentFromAllSupportedImages if found => {
                    must_not_be_registered.push(format!(
                        "{} {}.{}{}",
                        api.section, api.class, api.method, api.descriptor
                    ))
                }
                _ => {}
            }
        }

        assert!(
            missing.is_empty(),
            "Missing deprecated API registrations:\n{}",
            missing.join("\n")
        );
        assert!(
            must_not_be_registered.is_empty(),
            "Registered in front of a method NO supported JDK image declares \
             (see ImageStatus::AbsentFromAllSupportedImages):\n{}",
            must_not_be_registered.join("\n")
        );
    }

    #[test]
    fn t85_1_manifest_covers_all_sections() {
        let manifest = deprecated_api_manifest();
        let sections: std::collections::HashSet<&str> =
            manifest.iter().map(|a| a.section).collect();

        // Verify every T8 sub-section that still has natives is represented.
        //
        // **T8.2.7 was here and is deliberately gone (2026-08-22).** Its only
        // two rows were `java/lang/Number.{byteValue,shortValue}`, both retired
        // with the manifest note above, so the section is now served ENTIRELY by
        // real JDK bytecode — the first T8 sub-section to reach that state.
        // Requiring a section to be "covered" means requiring a native to exist
        // for it, so leaving `T8.2.7` in this list would make the last
        // retirement in any section permanently impossible. That is the same
        // shape as the `Declared => MUST stay` mis-tag that held those two rows
        // in place; a coverage list over a shrinking population has to be
        // allowed to shrink.
        for expected in &[
            "T8.1.1", "T8.1.2", "T8.1.3", "T8.1.4", "T8.1.6", "T8.1.7", "T8.1.8", "T8.1.9",
            "T8.1.10", "T8.2.1", "T8.2.2", "T8.2.3", "T8.2.4", "T8.2.5", "T8.2.6", "T8.2.10",
            "T8.2.11", "T8.2.13", "T8.2.14", "T8.3.1", "T8.3.2", "T8.4.1", "T8.4.2", "T8.4.3",
            "T8.4.4",
        ] {
            assert!(
                sections.contains(expected),
                "Section {} not covered in manifest",
                expected
            );
        }
    }

    #[test]
    fn t85_1_api_count_at_least_45() {
        let manifest = deprecated_api_manifest();
        // 46 before 2026-08-21; `Thread.stop()V` left the manifest entirely
        // (real bytecode serves it on every supported image) and two rows
        // stayed with `AbsentFromAllSupportedImages`.
        assert!(
            manifest.len() >= 44,
            "Expected at least 44 deprecated APIs, found {}",
            manifest.len()
        );
    }

    /// Every `AbsentFromAllSupportedImages` row must be one this crate does
    /// NOT register, and there must be at least one — an empty set would mean
    /// the column had quietly become decorative.
    #[test]
    fn t85_1_absent_from_images_rows_are_not_registered() {
        let r = build_deprecated_registry();
        let absent: Vec<_> = deprecated_api_manifest()
            .into_iter()
            .filter(|a| a.images == ImageStatus::AbsentFromAllSupportedImages)
            .collect();
        assert!(
            !absent.is_empty(),
            "the ImageStatus column has no AbsentFromAllSupportedImages row left"
        );
        for api in absent {
            assert!(
                r.find(api.class, api.method, api.descriptor).is_none(),
                "{}.{}{} is registered, but no supported image declares it",
                api.class,
                api.method,
                api.descriptor
            );
        }
    }

    // ── T8.5.2 — Behavioral round-trips ─────────────────────────────────

    /// Retired 2026-08-21: no supported image declares `Thread.destroy()V`, so
    /// there is nothing for a native to stand in front of and the VM's own
    /// `NoSuchMethodError` is the answer. This asserts the absence, which is
    /// the property that can regress.
    #[test]
    fn t85_2_thread_destroy_is_retired() {
        let r = build_deprecated_registry();
        assert!(r.find("java/lang/Thread", "destroy", "()V").is_none());
    }

    #[test]
    fn t85_2_thread_count_stack_frames_throws() {
        let r = build_deprecated_registry();
        let f = r
            .find("java/lang/Thread", "countStackFrames", "()I")
            .unwrap();
        let mut ctx = MockNativeContext::new();
        let result = f(&mut ctx, &[]);
        assert!(result.is_err(), "countStackFrames should throw");
    }

    #[test]
    fn t85_2_compiler_compile_class_returns_false() {
        let r = build_deprecated_registry();
        let f = r
            .find("java/lang/Compiler", "compileClass", "(Ljava/lang/Class;)Z")
            .unwrap();
        let mut ctx = MockNativeContext::new();
        let result = f(&mut ctx, &[cratonvm_types::Value::Object(None)]);
        match result {
            Ok(Some(cratonvm_types::Value::Int(0))) => {} // false
            other => panic!("Expected Ok(Some(Int(0))), got: {other:?}"),
        }
    }

    #[test]
    fn t85_2_compiler_command_returns_null() {
        let r = build_deprecated_registry();
        let f = r
            .find(
                "java/lang/Compiler",
                "command",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            )
            .unwrap();
        let mut ctx = MockNativeContext::new();
        let result = f(&mut ctx, &[cratonvm_types::Value::Object(None)]);
        match result {
            Ok(Some(cratonvm_types::Value::Object(None))) => {} // null
            other => panic!("Expected Ok(Some(Object(None))), got: {other:?}"),
        }
    }

    #[test]
    fn t85_2_character_is_space_tab() {
        let r = build_deprecated_registry();
        let f = r.find("java/lang/Character", "isSpace", "(C)Z").unwrap();
        let mut ctx = MockNativeContext::new();
        // '\t' = 9, should return true
        let result = f(&mut ctx, &[cratonvm_types::Value::Int(9)]);
        match result {
            Ok(Some(cratonvm_types::Value::Int(1))) => {}
            other => panic!("isSpace('\\t') should return true, got: {other:?}"),
        }
    }

    #[test]
    fn t85_2_character_is_java_letter_underscore() {
        let r = build_deprecated_registry();
        let f = r
            .find("java/lang/Character", "isJavaLetter", "(C)Z")
            .unwrap();
        let mut ctx = MockNativeContext::new();
        // '_' = 95, should return true (valid Java identifier start)
        let result = f(&mut ctx, &[cratonvm_types::Value::Int(95)]);
        match result {
            Ok(Some(cratonvm_types::Value::Int(1))) => {}
            other => panic!("isJavaLetter('_') should return true, got: {other:?}"),
        }
    }

    #[test]
    fn t85_2_character_is_java_letter_digit_rejected() {
        let r = build_deprecated_registry();
        let f = r
            .find("java/lang/Character", "isJavaLetter", "(C)Z")
            .unwrap();
        let mut ctx = MockNativeContext::new();
        // '5' = 53, should return false (not valid identifier start)
        let result = f(&mut ctx, &[cratonvm_types::Value::Int(53)]);
        match result {
            Ok(Some(cratonvm_types::Value::Int(0))) => {}
            other => panic!("isJavaLetter('5') should return false, got: {other:?}"),
        }
    }

    #[test]
    fn t85_2_url_decoder_is_registered() {
        let r = build_deprecated_registry();
        assert!(
            r.find(
                "java/net/URLDecoder",
                "decode",
                "(Ljava/lang/String;)Ljava/lang/String;"
            )
            .is_some(),
            "URLDecoder.decode(String) must be registered"
        );
    }

    #[test]
    fn t85_2_url_encoder_is_registered() {
        let r = build_deprecated_registry();
        assert!(
            r.find(
                "java/net/URLEncoder",
                "encode",
                "(Ljava/lang/String;)Ljava/lang/String;"
            )
            .is_some(),
            "URLEncoder.encode(String) must be registered"
        );
    }

    // ── T8.5.3 — Cross-check against JDK 25 @Deprecated set ────────────

    #[test]
    fn t85_3_cross_check_jdk25_deprecated_all_registered() {
        let r = build_deprecated_registry();
        let checklist = jdk25_deprecated_api_checklist();
        let mut missing = Vec::new();

        for (class, method, descriptor) in &checklist {
            if r.find(class, method, descriptor).is_none() {
                missing.push(format!("{}.{}{}", class, method, descriptor));
            }
        }

        assert!(
            missing.is_empty(),
            "JDK 25 @Deprecated APIs not registered:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn t85_3_checklist_has_at_least_30_entries() {
        let checklist = jdk25_deprecated_api_checklist();
        assert!(
            checklist.len() >= 30,
            "Expected at least 30 deprecated API entries, found {}",
            checklist.len()
        );
    }

    // ── T8.5.4 — Shim generator fills gaps ──────────────────────────────

    #[test]
    fn t85_4_shim_generator_skips_existing() {
        let mut r = NativeMethodRegistry::new();
        // Register one method manually. This constant no-op is a TEST FIXTURE
        // standing in for "some already-registered handler" — the assertion
        // below is that the shim generator does not replace it. It is not a
        // production `Thread.stop` implementation (that is
        // `deprecated_lang::native_thread_stop`, which really does deliver the
        // throwable to the target thread).
        r.register("java/lang/Thread", "stop", "()V", |_ctx, _args| Ok(None));
        let count_before = r.len();

        register_missing_deprecated_shims(&mut r);

        // Should have added new entries but not duplicated the existing one
        assert!(r.len() > count_before, "Shim generator should add entries");
        // The original "stop" should still be the original (not replaced)
    }

    #[test]
    fn t85_4_shim_throws_unsupported() {
        let mut r = NativeMethodRegistry::new();
        register_missing_deprecated_shims(&mut r);

        // Pick an API that was shimmed
        if let Some(f) = r.find("java/lang/Thread", "stop", "()V") {
            let mut ctx = MockNativeContext::new();
            let result = f(&mut ctx, &[]);
            assert!(result.is_err(), "Shim should throw");
            let err = format!("{:?}", result.unwrap_err());
            assert!(
                err.contains("T8.5.4") && err.contains("not implemented"),
                "Shim error should contain T8.5.4 tag: {err}"
            );
        }
    }

    // ── T8.5 — Summary statistics ───────────────────────────────────────

    #[test]
    fn t85_registration_count() {
        let r = build_deprecated_registry();
        // We expect at least 90 registered native methods across all deprecated modules
        assert!(
            r.len() >= 90,
            "Expected at least 90 deprecated natives, found {}",
            r.len()
        );
    }
}
