// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP8.10.7 — `Throwable.getMessage()` / `printStackTrace(PrintStream)` on
//! synthetic-stub Throwable subclasses.
//!
//! Background (from `bench/wildfly-boot/diagnostic.md` §WP8.10.7): jboss-modules
//! and WildFly catch-block code paths frequently look like:
//!
//! ```java
//! catch (Throwable t) {
//!     System.err.println("probe caught: " + t.getClass().getName()
//!         + ": " + String.valueOf(t.getMessage()));
//! }
//! ```
//!
//! The native registry dispatch is keyed on the *runtime* class name, so when
//! `t` is a `NoClassDefFoundError` synthetic stub the lookup `find(
//! "java/lang/NoClassDefFoundError", "getMessage", "()Ljava/lang/String;")`
//! had no entry and tripped a secondary `NoSuchMethodError` from inside the
//! catch handler — masking the original boot failure.
//!
//! These tests verify that:
//!   1. `getMessage()` round-trips on a `Throwable` instance (regression).
//!   2. `getMessage()` works on `NoClassDefFoundError` (WP8.10.7 fix).
//!   3. `printStackTrace(PrintStream)` does not NPE / NSME on a non-null
//!      stream argument.
//!   4. `printStackTrace(PrintStream)` handles a *null* stream argument
//!      gracefully (a real-WildFly scenario where the synthetic-stub
//!      `System.err` may be null at that boot phase).
//!   5. `getMessage()` and `printStackTrace(PrintStream)` are registered
//!      across the full Throwable subclass family (registry-only check).

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_vm::classloading::ClassId;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{create_java_string, NativeContextImpl, SharedVm};
use std::sync::Arc;

fn build_registry() -> NativeMethodRegistry {
    // Use `register_essential_natives` directly — it's the universal
    // registration block that runs in BOTH `synthetic-jdk` and real-JDK
    // feature configurations. The vm-crate `register_builtins` re-export
    // is a no-op shim when the `synthetic-jdk` feature is OFF (default
    // for `cargo test --release -p cratonvm-vm`), so going through it
    // would silently produce an empty registry.
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    r
}

/// Allocate a synthetic Throwable-shaped object with 2 fields (message, cause)
/// and populate `field 0` (detailMessage) with `message` if `Some`.
fn alloc_synthetic_throwable(
    shared: &SharedVm,
    message: Option<&str>,
) -> cratonvm_types::ObjectRef {
    // ClassId is irrelevant for the native — the dispatch is keyed by the
    // *registered* class-name string at lookup time, not the heap object's
    // ClassId.  We only need the object to have ≥1 field so `get_field(this, 0)`
    // returns sensibly.  ClassId::new(0) is the conventional sentinel for
    // "synthetic, not yet linked to a real class id".
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    if let Some(m) = message {
        let s = create_java_string(shared, m);
        shared.mem.heap.set_field(obj, 0, Value::Object(Some(s)));
    }
    obj
}

// ---------------------------------------------------------------------------
// Test 1 — getMessage round-trip on java/lang/Throwable (regression)
// ---------------------------------------------------------------------------

#[test]
fn throwable_get_message_roundtrips() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let exc = alloc_synthetic_throwable(&shared, Some("boom"));

    let registry = build_registry();
    let cb = registry
        .find("java/lang/Throwable", "getMessage", "()Ljava/lang/String;")
        .expect("Throwable.getMessage must be registered");

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result = cb(&mut ctx, &[Value::Object(Some(exc))]);
    let val = result.expect("getMessage call must succeed");
    let Some(Value::Object(Some(s))) = val else {
        panic!("expected non-null String return, got {val:?}");
    };
    let read_back = cratonvm_vm::vm::read_java_string(&shared.mem.heap, s)
        .expect("getMessage return must be a readable String");
    assert_eq!(read_back, "boom");
}

// ---------------------------------------------------------------------------
// Test 2 — getMessage works on NoClassDefFoundError (the actual fix)
// ---------------------------------------------------------------------------

#[test]
fn no_class_def_found_error_get_message_registered() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let exc = alloc_synthetic_throwable(&shared, Some("missing module: cratonvm.fixture"));

    let registry = build_registry();
    let cb = registry
        .find(
            "java/lang/NoClassDefFoundError",
            "getMessage",
            "()Ljava/lang/String;",
        )
        .expect(
            "WP8.10.7: NoClassDefFoundError.getMessage must be registered \
             so jboss-modules catch-block `t.getMessage()` doesn't NSME",
        );

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result = cb(&mut ctx, &[Value::Object(Some(exc))]);
    let val = result.expect("getMessage call must succeed");
    let Some(Value::Object(Some(s))) = val else {
        panic!("expected non-null String return, got {val:?}");
    };
    let read_back = cratonvm_vm::vm::read_java_string(&shared.mem.heap, s)
        .expect("getMessage return must be a readable String");
    assert_eq!(read_back, "missing module: cratonvm.fixture");
}

// ---------------------------------------------------------------------------
// Test 3 — printStackTrace(PrintStream) doesn't NPE on a real stream arg
// ---------------------------------------------------------------------------

#[test]
fn print_stack_trace_to_stream_does_not_npe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Allocate a Throwable-shaped object with a message.
    let exc = alloc_synthetic_throwable(&shared, Some("trace-target"));

    // Allocate a synthetic PrintStream-shaped object (1 field for fd tag).
    // The native ignores the stream arg and writes to record_printed_line,
    // so the fd tag value is irrelevant for this test.
    let ps = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(ps, 0, Value::Int(2)); // fd=2 (stderr)

    let registry = build_registry();
    let cb = registry
        .find(
            "java/lang/ClassNotFoundException",
            "printStackTrace",
            "(Ljava/io/PrintStream;)V",
        )
        .expect(
            "WP8.10.7: ClassNotFoundException.printStackTrace(PrintStream) \
             must be registered (PrintStream-arg overload, JDK 25)",
        );

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let args = [Value::Object(Some(exc)), Value::Object(Some(ps))];
    let result = cb(&mut ctx, &args);
    assert!(
        result.is_ok(),
        "printStackTrace(PrintStream) should not error on valid stream arg, got {result:?}"
    );

    // The native records via `record_printed_line` regardless of the stream
    // argument — confirm the recorded line contains the message.
    assert!(
        thread
            .printed_lines
            .iter()
            .any(|line| line.contains("trace-target")),
        "expected printed_lines to contain the throwable message; got {:?}",
        thread.printed_lines,
    );
}

// ---------------------------------------------------------------------------
// Test 4 — printStackTrace(PrintStream) handles a null stream gracefully
// ---------------------------------------------------------------------------

#[test]
fn print_stack_trace_with_null_stream_is_no_op_safe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let exc = alloc_synthetic_throwable(&shared, Some("null-stream test"));

    let registry = build_registry();
    let cb = registry
        .find(
            "java/lang/NoClassDefFoundError",
            "printStackTrace",
            "(Ljava/io/PrintStream;)V",
        )
        .expect("WP8.10.7: NoClassDefFoundError.printStackTrace(PrintStream) must be registered");

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // Note the NULL stream argument — this happens during boot when the
    // synthetic-stub `System.err` static is still null.
    let args = [Value::Object(Some(exc)), Value::Object(None)];
    let result = cb(&mut ctx, &args);
    assert!(
        result.is_ok(),
        "printStackTrace(null) must not throw — caught code paths rely on it. Got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — registration coverage matrix (registry-only check, no execution)
// ---------------------------------------------------------------------------

/// Smoke-test that every Throwable subclass we promised to register has
/// the four critical methods plumbed.  This guards against future
/// reordering of the WP8.10.7 registration loop dropping a class.
#[test]
fn throwable_subclass_registration_coverage() {
    let registry = build_registry();
    // Subset of WP8.10.7 promise — these are the ones jboss-modules and
    // WildFly catch-block code paths actually hit.
    let critical_classes = [
        "java/lang/Throwable",
        "java/lang/NoClassDefFoundError",
        "java/lang/ClassNotFoundException",
        "java/lang/Error",
        "java/lang/Exception",
        "java/lang/RuntimeException",
        "java/lang/LinkageError",
        "java/lang/NoSuchMethodError",
        "java/lang/NoSuchFieldError",
    ];
    for cls in critical_classes {
        assert!(
            registry
                .find(cls, "getMessage", "()Ljava/lang/String;")
                .is_some(),
            "WP8.10.7: {cls}.getMessage()Ljava/lang/String; must be registered",
        );
        assert!(
            registry
                .find(cls, "getLocalizedMessage", "()Ljava/lang/String;")
                .is_some(),
            "WP8.10.7: {cls}.getLocalizedMessage()Ljava/lang/String; must be registered",
        );
        assert!(
            registry
                .find(cls, "printStackTrace", "(Ljava/io/PrintStream;)V")
                .is_some(),
            "WP8.10.7: {cls}.printStackTrace(PrintStream)V must be registered",
        );
        assert!(
            registry
                .find(cls, "toString", "()Ljava/lang/String;")
                .is_some(),
            "WP8.10.7: {cls}.toString()Ljava/lang/String; must be registered",
        );
    }
}

// ---------------------------------------------------------------------------
// Test 6 — getMessage returns null when no detailMessage was set
// ---------------------------------------------------------------------------

#[test]
fn get_message_returns_null_when_unset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Construct a Throwable with NO message set — slot 0 is the default
    // null Object reference.
    let exc = alloc_synthetic_throwable(&shared, None);

    let registry = build_registry();
    let cb = registry
        .find(
            "java/lang/NoSuchMethodError",
            "getMessage",
            "()Ljava/lang/String;",
        )
        .expect("WP8.10.7: NoSuchMethodError.getMessage must be registered");

    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result = cb(&mut ctx, &[Value::Object(Some(exc))]);
    let val = result.expect("getMessage call must succeed");
    assert_eq!(
        val,
        Some(Value::Object(None)),
        "getMessage() on a Throwable with no detailMessage must return null, got {val:?}",
    );
}
