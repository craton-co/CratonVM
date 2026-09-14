// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T8 — Deprecated API conformance test suite.
//!
//! Verifies that CratonVM correctly implements all deprecated JDK 25 APIs
//! per roadmap T8.1–T8.6. Each test exercises a specific sub-section.
//!
//!     cargo test -p cratonvm-vm --test t8_deprecated_conformance -- --nocapture

use cratonvm_native_api::NativeMethodRegistry;

// ---------------------------------------------------------------------------
// Helper: build a full registry with all natives (essential + deprecated)
// ---------------------------------------------------------------------------
fn full_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    r
}

// ===========================================================================
// T8.1 — Deprecated java.lang.*
// ===========================================================================

/// `stop()V` is RETIRED and `stop0` is KEPT, and the split is measured, not
/// stylistic: `javap -p --system <image> java.lang.Thread` over the nine
/// supported images finds `public final void stop()` with a `Code` attribute on
/// all nine, and `private native void stop0(java.lang.Object)` on the three
/// JDK 17 images only. A method every image implements needs no native; a
/// method one supported image declares must keep one.
#[test]
fn t8_1_1_thread_stop_retired_stop0_kept() {
    let r = full_registry();
    assert!(
        r.find("java/lang/Thread", "stop", "()V").is_none(),
        "Thread.stop()V is served by real bytecode on every supported image"
    );
    assert!(r
        .find("java/lang/Thread", "stop0", "(Ljava/lang/Object;)V")
        .is_some());
    eprintln!("[t8] T8.1.1 Thread.stop: retired; stop0 kept for JDK 17");
}

#[test]
fn t8_1_2_thread_suspend_resume_registered() {
    let r = full_registry();
    assert!(r.find("java/lang/Thread", "suspend0", "()V").is_some());
    assert!(r.find("java/lang/Thread", "resume0", "()V").is_some());
    eprintln!("[t8] T8.1.2 Thread.suspend/resume: registered");
}

/// Retired 2026-08-21: no supported image declares it.
#[test]
fn t8_1_3_thread_destroy_retired() {
    let r = full_registry();
    assert!(r.find("java/lang/Thread", "destroy", "()V").is_none());
    eprintln!("[t8] T8.1.3 Thread.destroy: retired (declared by no image)");
}

#[test]
fn t8_1_4_thread_count_stack_frames_registered() {
    let r = full_registry();
    assert!(r
        .find("java/lang/Thread", "countStackFrames", "()I")
        .is_some());
    eprintln!("[t8] T8.1.4 Thread.countStackFrames: registered");
}

#[test]
fn t8_1_5_finalization_tracker() {
    // FinalizationTracker is tested extensively in native-builtins unit tests.
    // Here we just verify the global registration exists.
    let r = full_registry();
    assert!(r
        .find("java/lang/Runtime", "runFinalization", "()V")
        .is_some());
    eprintln!("[t8] T8.1.5 Object.finalize tracking: OK (via FinalizationTracker)");
}

#[test]
fn t8_1_6_run_finalization_registered() {
    let r = full_registry();
    assert!(r
        .find("java/lang/Runtime", "runFinalization", "()V")
        .is_some());
    assert!(r
        .find("java/lang/System", "runFinalization", "()V")
        .is_some());
    eprintln!("[t8] T8.1.6 runFinalization: registered");
}

/// Retired 2026-08-21: no supported image declares it, and the flag it wrote
/// had no reader outside its own test.
#[test]
fn t8_1_7_run_finalizers_on_exit_retired() {
    let r = full_registry();
    assert!(r
        .find("java/lang/System", "runFinalizersOnExit", "(Z)V")
        .is_none());
    eprintln!("[t8] T8.1.7 runFinalizersOnExit: retired (declared by no image)");
}

#[test]
fn t8_1_8_security_manager_registered() {
    let r = full_registry();
    assert!(r
        .find(
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V"
        )
        .is_some());
    assert!(r
        .find(
            "java/security/AccessController",
            "doPrivileged",
            "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;"
        )
        .is_some());
    eprintln!("[t8] T8.1.8 SecurityManager + AccessController: registered");
}

#[test]
fn t8_1_9_classloader_define_class_3arg() {
    let r = full_registry();
    assert!(r
        .find(
            "java/lang/ClassLoader",
            "defineClass",
            "([BII)Ljava/lang/Class;"
        )
        .is_some());
    eprintln!("[t8] T8.1.9 ClassLoader.defineClass(byte[],int,int): registered");
}

#[test]
fn t8_1_10_compiler_class() {
    let r = full_registry();
    assert!(r
        .find("java/lang/Compiler", "compileClass", "(Ljava/lang/Class;)Z")
        .is_some());
    assert!(r
        .find(
            "java/lang/Compiler",
            "compileClasses",
            "(Ljava/lang/String;)Z"
        )
        .is_some());
    assert!(r.find("java/lang/Compiler", "enable", "()V").is_some());
    assert!(r.find("java/lang/Compiler", "disable", "()V").is_some());
    assert!(r
        .find(
            "java/lang/Compiler",
            "command",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        )
        .is_some());
    eprintln!("[t8] T8.1.10 Compiler: all 5 methods registered");
}

// ===========================================================================
// T8.2 — Deprecated java.io / java.util / java.text
// ===========================================================================

#[test]
fn t8_2_1_date_constructors() {
    let r = full_registry();
    assert!(r.find("java/util/Date", "<init>", "(III)V").is_some());
    assert!(r.find("java/util/Date", "<init>", "(IIIII)V").is_some());
    assert!(r.find("java/util/Date", "<init>", "(IIIIII)V").is_some());
    eprintln!("[t8] T8.2.1 Date multi-arg constructors: registered");
}

#[test]
fn t8_2_2_date_getters() {
    let r = full_registry();
    for method in &[
        "getYear",
        "getMonth",
        "getDate",
        "getDay",
        "getHours",
        "getMinutes",
        "getSeconds",
    ] {
        assert!(
            r.find("java/util/Date", method, "()I").is_some(),
            "Date.{method} should be registered"
        );
    }
    eprintln!("[t8] T8.2.2 Date getters: all 7 registered");
}

#[test]
fn t8_2_3_string_hibyte_constructor() {
    let r = full_registry();
    assert!(r.find("java/lang/String", "<init>", "([BIII)V").is_some());
    eprintln!("[t8] T8.2.3 String(byte[],int,int,int): registered");
}

#[test]
fn t8_2_4_string_get_bytes_deprecated() {
    let r = full_registry();
    assert!(r.find("java/lang/String", "getBytes", "(II[BI)V").is_some());
    eprintln!("[t8] T8.2.4 String.getBytes(int,int,byte[],int): registered");
}

#[test]
fn t8_2_5_character_deprecated() {
    let r = full_registry();
    assert!(r
        .find("java/lang/Character", "isJavaLetter", "(C)Z")
        .is_some());
    assert!(r
        .find("java/lang/Character", "isJavaLetterOrDigit", "(C)Z")
        .is_some());
    assert!(r.find("java/lang/Character", "isSpace", "(C)Z").is_some());
    eprintln!("[t8] T8.2.5 Character deprecated methods: registered");
}

#[test]
fn t8_2_6_class_new_instance() {
    let r = full_registry();
    assert!(r
        .find("java/lang/Class", "newInstance", "()Ljava/lang/Object;")
        .is_some());
    eprintln!("[t8] T8.2.6 Class.newInstance(): registered");
}

/// WP6.5 regression marker — `Class.newInstance()` (the deprecated JDK 1.0
/// entry point used by `BouncyCastleProvider.loadServiceClass`) MUST
/// instantiate the *target* class encoded in the receiver Class mirror,
/// not the receiver's own class (i.e. not `java.lang.Class`).
///
/// Before the WP6.5 fix, `deprecated_io_util::register_class_new_instance`
/// in real-JDK mode called `class_id_of_object(this_class)`, which returns
/// the heap class_id of the receiver — meaning every `Class.newInstance()`
/// invocation allocated a fresh `java.lang.Class` instance and the
/// subsequent `checkcast` to the intended target type failed with
/// `ClassCastException`. The fix routes through `mirror_class_id`, which
/// decodes the encoded class_id from the mirror's slot 0 / class-mirrors
/// reverse map.
///
/// The full behavioural test runs under
/// `C:/craton/cratonvm/apps/bc_probe/BcProbe.java` (compiled into
/// `C:/craton/ejbca-test-run/classes/`); this unit test is a registration
/// smoke check so the regression at least surfaces fast in CI.
#[test]
fn t8_2_6_class_new_instance_wp6_5_mirror_decoded() {
    let r = full_registry();
    // The native must be registered in real-JDK mode (essential natives only).
    assert!(
        r.find("java/lang/Class", "newInstance", "()Ljava/lang/Object;")
            .is_some(),
        "Class.newInstance must be registered for real-JDK BC startup",
    );
    eprintln!("[t8] WP6.5 Class.newInstance mirror_class_id wiring: registered");
}

#[test]
fn t8_2_10_string_buffer_input_stream() {
    let r = full_registry();
    assert!(r
        .find("java/io/StringBufferInputStream", "read", "()I")
        .is_some());
    assert!(r
        .find("java/io/StringBufferInputStream", "available", "()I")
        .is_some());
    assert!(r
        .find("java/io/StringBufferInputStream", "reset", "()V")
        .is_some());
    eprintln!("[t8] T8.2.10 StringBufferInputStream: registered");
}

#[test]
fn t8_2_11_line_number_input_stream() {
    let r = full_registry();
    assert!(r
        .find("java/io/LineNumberInputStream", "getLineNumber", "()I")
        .is_some());
    assert!(r
        .find("java/io/LineNumberInputStream", "setLineNumber", "(I)V")
        .is_some());
    assert!(r
        .find("java/io/LineNumberInputStream", "read", "()I")
        .is_some());
    eprintln!("[t8] T8.2.11 LineNumberInputStream: registered");
}

#[test]
fn t8_2_13_url_decoder_single_arg() {
    let r = full_registry();
    assert!(r
        .find(
            "java/net/URLDecoder",
            "decode",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some());
    eprintln!("[t8] T8.2.13 URLDecoder.decode(String): registered");
}

#[test]
fn t8_2_14_url_encoder_single_arg() {
    let r = full_registry();
    assert!(r
        .find(
            "java/net/URLEncoder",
            "encode",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some());
    eprintln!("[t8] T8.2.14 URLEncoder.encode(String): registered");
}

// ===========================================================================
// T8.3 — Deprecated java.beans / java.rmi
// ===========================================================================

#[test]
fn t8_3_1_beans_instantiate() {
    let r = full_registry();
    assert!(r
        .find(
            "java/beans/Beans",
            "instantiate",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
        )
        .is_some());
    eprintln!("[t8] T8.3.1 Beans.instantiate: registered");
}

#[test]
fn t8_3_2_remote_ref_get_ref_class() {
    let r = full_registry();
    assert!(r
        .find(
            "java/rmi/server/RemoteRef",
            "getRefClass",
            "(Ljava/io/ObjectOutput;)Ljava/lang/String;",
        )
        .is_some());
    eprintln!("[t8] T8.3.2 RemoteRef.getRefClass: registered");
}

#[test]
fn t8_3_3_rmi_activation() {
    let r = full_registry();
    // At least ActivationGroup.getSystem should be registered
    assert!(r
        .find(
            "java/rmi/activation/ActivationGroup",
            "getSystem",
            "()Ljava/rmi/activation/ActivationSystem;",
        )
        .is_some());
    eprintln!("[t8] T8.3.3 RMI activation: registered");
}

// ===========================================================================
// T8.4 — Deprecated sun.* / jdk.internal.*
// ===========================================================================

/// **T8.4.1 asserts ABSENCE, not presence.**
///
/// No supported JDK image (17 / 21 / 25, three platforms) declares
/// `sun.misc.Unsafe.defineClass` — it went in JDK 11 — so a native standing in
/// front of it could never be dispatched, and the VM's own `NoSuchMethodError`
/// is the correct answer. `deprecated_verify`'s manifest adjudicated that and
/// retired the registration; this file kept asserting the registration was
/// there, and had been red ever since.
#[test]
fn t8_4_1_unsafe_define_class_is_retired() {
    let r = full_registry();
    assert!(
        r.find(
            "sun/misc/Unsafe",
            "defineClass",
            "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        )
        .is_none(),
        "T8.4.1: `sun/misc/Unsafe.defineClass` is absent from every supported          image, so registering a native for it would shadow the          NoSuchMethodError that is the right answer"
    );
    eprintln!("[t8] T8.4.1 Unsafe.defineClass: retired, correctly absent");
}

/// **EVERY retired triple, checked against the one adjudicated list.**
///
/// The two assertions this file got wrong were wrong the same way: it kept a
/// hand-maintained copy of `deprecated_verify`'s manifest, and a copy drifts
/// the moment an API is retired. This reads the manifest instead, so a future
/// retirement needs no edit here and cannot leave a stale claim behind.
#[test]
fn every_triple_absent_from_the_images_is_unregistered() {
    let r = full_registry();
    let absent = cratonvm_native_builtins::deprecated_verify::absent_from_all_supported_images();
    assert!(
        !absent.is_empty(),
        "the adjudicated absent list is empty — this test would pass vacuously"
    );
    for (class, method, descriptor) in absent {
        assert!(
            r.find(class, method, descriptor).is_none(),
            "{class}.{method}{descriptor} is registered, but no supported JDK              image declares it — the native can never be dispatched and only              hides the NoSuchMethodError"
        );
    }
}

#[test]
fn t8_4_2_unsafe_memory() {
    let r = full_registry();
    assert!(r
        .find("sun/misc/Unsafe", "allocateMemory", "(J)J")
        .is_some());
    assert!(r.find("sun/misc/Unsafe", "freeMemory", "(J)V").is_some());
    assert!(r
        .find("sun/misc/Unsafe", "reallocateMemory", "(JJ)J")
        .is_some());
    eprintln!("[t8] T8.4.2 Unsafe memory ops: registered");
}

#[test]
fn t8_4_3_reflection_get_caller_class() {
    let r = full_registry();
    assert!(r
        .find(
            "sun/reflect/Reflection",
            "getCallerClass",
            "(I)Ljava/lang/Class;"
        )
        .is_some());
    eprintln!("[t8] T8.4.3 Reflection.getCallerClass(int): registered");
}

#[test]
fn t8_4_4_sun_misc_signal() {
    let r = full_registry();
    assert!(r
        .find(
            "sun/misc/Signal",
            "handle",
            "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;"
        )
        .is_some());
    eprintln!("[t8] T8.4.4 sun.misc.Signal: registered");
}

// ===========================================================================
// T8.5 — Verification
// ===========================================================================

#[test]
fn t8_5_1_deprecated_api_count() {
    let r = full_registry();
    let total = r.len();
    eprintln!("[t8] Total native methods (including deprecated): {total}");
    // Spot-check: all deprecated sub-sections should be present
    // T8.1.3 — `Thread.destroy()V` is RETIRED (2026-08-21): no supported image
    // declares it, so its ABSENCE is the property. Covered by
    // `every_triple_absent_from_the_images_is_unregistered` above, which reads
    // the adjudicated manifest rather than naming triples here.

    assert!(
        r.find("java/util/Date", "getYear", "()I").is_some(),
        "T8.2.2 missing"
    );
    assert!(
        r.find("java/lang/Character", "isSpace", "(C)Z").is_some(),
        "T8.2.5 missing"
    );
    assert!(
        r.find(
            "java/beans/Beans",
            "instantiate",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;"
        )
        .is_some(),
        "T8.3.1 missing"
    );
    assert!(
        r.find(
            "sun/reflect/Reflection",
            "getCallerClass",
            "(I)Ljava/lang/Class;"
        )
        .is_some(),
        "T8.4.3 missing"
    );
}

#[test]
fn t8_5_4_readiness_measurement() {
    let r = full_registry();
    let total = r.len();
    eprintln!("[t8] T8 readiness: {total} total native methods registered");
    eprintln!("[t8] T8 deprecated API tier is functional");
    assert!(
        total >= 200,
        "Expected >= 200 total natives for T8 readiness"
    );
}
