//! Native method bridging tests (Session 10).
//!
//! Tests cover: System.arraycopy, Object.hashCode, Thread.currentThread,
//! System.identityHashCode, System.currentTimeMillis, System.nanoTime,
//! and inherited native method dispatch.

use rustjvm_vm::config::VmConfig;
use rustjvm_vm::types::Value;
use rustjvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/rustjvm/NativeBridgeTest.class")).exists()
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
    let result = vm.invoke(
        "rustjvm/NativeBridgeTest",
        method,
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(v))) => assert_eq!(
            v, expected,
            "{method} returned {v}, expected {expected}"
        ),
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
// JNI name mangling unit tests
// ---------------------------------------------------------------------------

#[test]
fn jni_name_mangling_short() {
    use rustjvm_vm::native::jni::jni_short_name;

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
    use rustjvm_vm::native::jni::jni_long_name;

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
    use rustjvm_vm::native::jni::jni_short_name;

    // Method with underscore in name should get _1 encoding
    assert_eq!(
        jni_short_name("com/example/My_Class", "my_method"),
        "Java_com_example_My_1Class_my_1method"
    );
}

#[test]
fn jni_name_mangling_array_descriptor() {
    use rustjvm_vm::native::jni::jni_long_name;

    // Array types in descriptor: [ → _3
    assert_eq!(
        jni_long_name("java/lang/System", "arraycopy", "([III[III)V"),
        "Java_java_lang_System_arraycopy___3III_3III"
    );
}
