// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native method bridging tests (Session 10).
//!
//! Tests cover: System.arraycopy, Object.hashCode, Thread.currentThread,
//! System.identityHashCode, System.currentTimeMillis, System.nanoTime,
//! and inherited native method dispatch.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/NativeBridgeTest.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: NativeBridgeTest.class not available");
            return;
        }
    };
}

fn invoke_expect_int(method: &str, expected: i32) {
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/NativeBridgeTest", method, "()I", &[]);
    match result {
        Ok(Some(Value::Int(v))) => {
            assert_eq!(v, expected, "{method} returned {v}, expected {expected}")
        }
        other => panic!("{method} failed: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Sanity check
// ---------------------------------------------------------------------------

#[test]
fn native_sanity_check() {
    require_class_files!();
    invoke_expect_int("testSanity", 42);
}

// ---------------------------------------------------------------------------
// System.arraycopy tests
// ---------------------------------------------------------------------------

/// Basic full-array copy.
#[test]
fn native_arraycopy_basic() {
    require_class_files!();
    invoke_expect_int("testArraycopyBasic", 150);
}

/// Partial copy with source and destination offsets.
#[test]
fn native_arraycopy_partial() {
    require_class_files!();
    invoke_expect_int("testArraycopyPartial", 230);
}

/// Overlapping copy within the same array.
#[test]
fn native_arraycopy_overlap() {
    require_class_files!();
    invoke_expect_int("testArraycopyOverlap", 11234);
}

/// Zero-length copy is a no-op.
#[test]
fn native_arraycopy_zero_length() {
    require_class_files!();
    invoke_expect_int("testArraycopyZeroLength", 1);
}

/// Copy of Object[] preserves references.
#[test]
fn native_arraycopy_objects() {
    require_class_files!();
    invoke_expect_int("testArraycopyObjects", 1);
}

// ---------------------------------------------------------------------------
// Object.hashCode tests
// ---------------------------------------------------------------------------

/// hashCode returns the same value on repeated calls.
#[test]
fn native_object_hashcode_stable() {
    require_class_files!();
    invoke_expect_int("testObjectHashCode", 1);
}

/// Different objects produce different identity hash codes.
#[test]
fn native_object_hashcode_distinct() {
    require_class_files!();
    invoke_expect_int("testObjectHashCodeDistinct", 1);
}

/// Subclass inherits Object.hashCode (native method dispatch through hierarchy).
#[test]
fn native_subclass_hashcode() {
    require_class_files!();
    invoke_expect_int("testSubclassHashCode", 1);
}

// ---------------------------------------------------------------------------
// Thread.currentThread test
// ---------------------------------------------------------------------------

/// Thread.currentThread() returns a non-null Thread object.
#[test]
fn native_current_thread() {
    require_class_files!();
    invoke_expect_int("testCurrentThread", 1);
}

// ---------------------------------------------------------------------------
// System.identityHashCode test
// ---------------------------------------------------------------------------

/// System.identityHashCode returns a stable non-zero value.
#[test]
fn native_identity_hashcode() {
    require_class_files!();
    invoke_expect_int("testIdentityHashCode", 1);
}

// ---------------------------------------------------------------------------
// System.currentTimeMillis / nanoTime tests
// ---------------------------------------------------------------------------

/// System.currentTimeMillis returns a positive value.
#[test]
fn native_current_time_millis() {
    require_class_files!();
    invoke_expect_int("testCurrentTimeMillis", 1);
}

/// System.nanoTime returns monotonically increasing positive values.
#[test]
fn native_nano_time() {
    require_class_files!();
    invoke_expect_int("testNanoTime", 1);
}

// ---------------------------------------------------------------------------
// PrintStream.print(J)V / println(J)V — CompactValue tag-erasure regression.
//
// `bench/nbody.java` ran for ~73 seconds of wall time and printed
// `Time: 0 ms`. Tracing showed:
//   - `System.currentTimeMillis()` correctly returned distinct epoch millis
//     on both calls,
//   - `lsub` correctly computed the delta (~95 sec),
//   - `lstore 4` / `lload 4` round-tripped the long,
//   - but `invokevirtual PrintStream.print(J)V` arrived at the native
//     with `args[1] = Value::Double(<denormal>)`, not `Value::Long(delta)`.
//
// Root cause: `CompactValue::long(v)` stores the long as untagged raw bits
// (see `types/src/compact_value.rs::pub fn long` — `tag()` returns Double
// for it). `invokevirtual_cached` pops args with `pop_unchecked()`
// (= `to_value()`), which trusts the tag and yields `Value::Double` with
// the long's bits reinterpreted as f64. `native_print_long` only matched
// `Some(Value::Long(v))` and silently fell through to `0`.
//
// The fix in `native-builtins/src/lib.rs::native_print_long` /
// `native_println_long` accepts `Value::Double` and recovers the raw
// long bits via `d.to_bits() as i64`. The two tests below pin that fix.
// (The deeper invoke-arg-popping bug is a separate, orchestrator-owned
// `vm_exec.rs` change.)
mod print_long_compact_tag_regression {
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::types::Value;
    use cratonvm_vm::vm::NativeContextImpl;
    use cratonvm_vm::{ClassId, JvmThread, SharedVm, ThreadId};
    use std::sync::Arc;

    fn run_print(method: &str, arg: Value) -> Vec<String> {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let dummy_ps = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mut registry = NativeMethodRegistry::new();
        cratonvm_native_builtins::register_essential_natives(&mut registry);
        let callback = registry
            .find("java/io/PrintStream", method, "(J)V")
            .unwrap_or_else(|| panic!("PrintStream.{method}(J)V must be registered"));
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let args = [Value::Object(Some(dummy_ps)), arg];
        callback(&mut ctx, &args).expect("native should not error");
        thread.printed_lines.clone()
    }

    /// `print(J)V` with a Double-tagged arg (CompactValue::long round-trip)
    /// must print the decimal long, not `0` or a denormal float.
    #[test]
    fn print_long_recovers_long_bits_from_double_tagged_arg() {
        // 94_423 ms = a typical nbody-style elapsed delta in millis.
        let bits: i64 = 94_423;
        let lines = run_print("print", Value::Double(f64::from_bits(bits as u64)));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("94423"),
            "Double-tagged long arg must print decimal long, got {lines:?}"
        );
    }

    /// Same for `println(J)V` — the print path used by
    /// `System.out.println(System.currentTimeMillis())`.
    #[test]
    fn println_long_recovers_long_bits_from_double_tagged_arg() {
        // Mid-2026 epoch millis (41-bit value, doesn't fit in i32).
        let bits: i64 = 1_778_976_926_542;
        let lines = run_print("println", Value::Double(f64::from_bits(bits as u64)));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("1778976926542"),
            "Double-tagged long arg must println decimal long, got {lines:?}"
        );
    }

    /// Sanity: a properly-tagged `Value::Long(v)` is still handled.
    #[test]
    fn print_long_still_handles_long_tagged_arg() {
        let lines = run_print("print", Value::Long(-42));
        assert_eq!(lines.last().map(String::as_str), Some("-42"));
    }

    /// Sanity: simulates the exact symptom from `bench/nbody.java` —
    /// `currentTimeMillis()` returns two distinct values; their delta
    /// (after `CompactValue::long` round-trip) lands as `Value::Double`
    /// in the print arg, and must still print as a positive elapsed
    /// millis count.
    #[test]
    fn nbody_elapsed_delta_print_round_trip() {
        let t0: i64 = 1_778_975_441_124;
        let t1: i64 = 1_778_975_549_372;
        let delta = t1 - t0;
        assert_eq!(delta, 108_248);
        let arg = Value::Double(f64::from_bits(delta as u64));
        let lines = run_print("print", arg);
        assert_eq!(lines.last().map(String::as_str), Some("108248"));
    }
}

// ---------------------------------------------------------------------------
// JNI name mangling unit tests
// ---------------------------------------------------------------------------

#[test]
fn jni_name_mangling_short() {
    use cratonvm_vm::native::jni::jni_short_name;

    assert_eq!(
        jni_short_name("java/lang/System", "arraycopy"),
        "Java_java_lang_System_arraycopy"
    );
    assert_eq!(
        jni_short_name("java/lang/Object", "hashCode"),
        "Java_java_lang_Object_hashCode"
    );
    assert_eq!(
        jni_short_name("java/lang/Thread", "currentThread"),
        "Java_java_lang_Thread_currentThread"
    );
}

#[test]
fn jni_name_mangling_long() {
    use cratonvm_vm::native::jni::jni_long_name;

    assert_eq!(
        jni_long_name(
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V"
        ),
        "Java_java_lang_System_arraycopy__Ljava_lang_Object_2ILjava_lang_Object_2II"
    );
    assert_eq!(
        jni_long_name("java/lang/Object", "hashCode", "()I"),
        "Java_java_lang_Object_hashCode__"
    );
}

#[test]
fn jni_name_mangling_underscore_escape() {
    use cratonvm_vm::native::jni::jni_short_name;

    // Method with underscore in name should get _1 encoding
    assert_eq!(
        jni_short_name("com/example/My_Class", "my_method"),
        "Java_com_example_My_1Class_my_1method"
    );
}

#[test]
fn jni_name_mangling_array_descriptor() {
    use cratonvm_vm::native::jni::jni_long_name;

    // Array types in descriptor: [ → _3
    assert_eq!(
        jni_long_name("java/lang/System", "arraycopy", "([III[III)V"),
        "Java_java_lang_System_arraycopy___3III_3III"
    );
}
