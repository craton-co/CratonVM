// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(feature = "synthetic-jdk")]
// These tests exercise the legacy synthetic JDK native surface. The default VM
// build is real-JDK mode and intentionally does not compile those natives.

//! I/O bootstrap tests (Session 12).
//!
//! Tests cover: FileInputStream, FileOutputStream, BufferedReader,
//! ByteArrayInputStream, ByteArrayOutputStream, ByteBuffer, CharBuffer,
//! and the java.io class hierarchy.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::{MethodCallFailed, MethodCallResult};
use cratonvm_vm::memory::ArrayElementType;
use cratonvm_vm::types::ObjectRef;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/IoBootstrapTest.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

fn read_test_java_string(vm: &Vm, obj: ObjectRef) -> Option<String> {
    let heap = &vm.shared.mem.heap;
    let value_array = match heap.get_field(obj, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };

    match heap.array_element_type(value_array) {
        Some(ArrayElementType::Byte) => {
            let coder = match heap.get_field(obj, 1) {
                Value::Int(value) => value,
                _ => 0,
            };
            let bytes: Vec<u8> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(value) => Some(value as u8),
                    _ => None,
                })
                .collect();
            if coder == 1 {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                Some(String::from_utf16_lossy(&units))
            } else {
                Some(bytes.into_iter().map(char::from).collect())
            }
        }
        Some(ArrayElementType::Char) => {
            let units: Vec<u16> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(value) => Some(value as u16),
                    _ => None,
                })
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

fn throwable_detail_message(vm: &Vm, exc: ObjectRef) -> Option<String> {
    let class_id = vm.shared.mem.heap.class_id_of(exc);
    let msg_ref = vm
        .instance_field_index(class_id, "detailMessage")
        .and_then(|idx| match vm.shared.mem.heap.get_field(exc, idx) {
            Value::Object(Some(msg)) => Some(msg),
            _ => None,
        })?;
    read_test_java_string(vm, msg_ref)
}

fn describe_result(vm: &Vm, result: &MethodCallResult) -> String {
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let class_id = vm.shared.mem.heap.class_id_of(*exc);
            let class_name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|class| class.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {:?}>", class_id));
            match throwable_detail_message(vm, *exc) {
                Some(message) => {
                    format!("Err(ExceptionThrown({class_name}: {message}, {exc:?}))")
                }
                None => format!("Err(ExceptionThrown({class_name}, {exc:?}))"),
            }
        }
        other => format!("{other:?}"),
    }
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: IoBootstrapTest.class not available");
            return;
        }
    };
}

fn invoke_expect_int(method: &str, expected: i32) {
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/IoBootstrapTest", method, "()I", &[]);
    match result {
        Ok(Some(Value::Int(v))) => {
            assert_eq!(v, expected, "{method} returned {v}, expected {expected}")
        }
        other => panic!("{method} failed: {}", describe_result(&vm, &other)),
    }
}

// ---------------------------------------------------------------------------
// File I/O tests
// ---------------------------------------------------------------------------

/// Write a file and read it back — end-to-end FileOutputStream + FileInputStream.
#[test]
fn io_file_write_read() {
    require_class_files!();
    invoke_expect_int("testFileWriteRead", 1);
    // Cleanup
    let _ = std::fs::remove_file("io_test_output.txt");
}

/// FileOutputStream append mode: write ABC, then append DEF, read ABCDEF.
#[test]
fn io_file_append() {
    require_class_files!();
    invoke_expect_int("testFileAppend", 1);
    let _ = std::fs::remove_file("io_test_append.txt");
}

/// FileInputStream.available() returns >= file size.
#[test]
fn io_file_available() {
    require_class_files!();
    invoke_expect_int("testFileAvailable", 1);
    let _ = std::fs::remove_file("io_test_avail.txt");
}

/// FileInputStream single-byte read and EOF detection.
#[test]
fn io_file_read_single_byte() {
    require_class_files!();
    invoke_expect_int("testFileReadSingleByte", 1);
    let _ = std::fs::remove_file("io_test_single.txt");
}

// ---------------------------------------------------------------------------
// BufferedReader tests
// ---------------------------------------------------------------------------

/// BufferedReader.readLine() reads lines from a file.
#[test]
fn io_buffered_reader_readline() {
    require_class_files!();
    invoke_expect_int("testBufferedReaderReadLine", 1);
    let _ = std::fs::remove_file("io_test_lines.txt");
}

// ---------------------------------------------------------------------------
// ByteArray stream tests
// ---------------------------------------------------------------------------

/// ByteArrayOutputStream write + ByteArrayInputStream read round-trip.
#[test]
fn io_byte_array_streams() {
    require_class_files!();
    invoke_expect_int("testByteArrayStreams", 1);
}

/// ByteArrayOutputStream bulk write with offset/length.
#[test]
fn io_byte_array_bulk_write() {
    require_class_files!();
    invoke_expect_int("testByteArrayBulkWrite", 1);
}

/// ByteArrayOutputStream.toString() returns the written content as a String.
#[test]
fn io_byte_array_to_string() {
    require_class_files!();
    invoke_expect_int("testByteArrayToString", 1);
}

// ---------------------------------------------------------------------------
// ByteBuffer tests
// ---------------------------------------------------------------------------

/// ByteBuffer.allocate(), put/get, flip, position, limit, capacity.
#[test]
fn io_byte_buffer_allocate() {
    require_class_files!();
    invoke_expect_int("testByteBufferAllocate", 1);
}

/// ByteBuffer putInt/getInt.
#[test]
fn io_byte_buffer_int() {
    require_class_files!();
    invoke_expect_int("testByteBufferInt", 1);
}

/// ByteBuffer.wrap() wraps an existing byte array.
#[test]
fn io_byte_buffer_wrap() {
    require_class_files!();
    invoke_expect_int("testByteBufferWrap", 1);
}

// ---------------------------------------------------------------------------
// Hierarchy tests
// ---------------------------------------------------------------------------

/// ByteArrayInputStream is assignable to InputStream (superclass chain correct).
#[test]
fn io_input_stream_hierarchy() {
    require_class_files!();
    invoke_expect_int("testInputStreamHierarchy", 1);
}

// ---------------------------------------------------------------------------
// Superclass chain unit tests (no Java class files needed)
// ---------------------------------------------------------------------------

#[test]
fn io_superclass_chain_file_input_stream() {
    let vm = test_vm();
    let shared = vm.shared.clone();
    let cm = shared.classes.class_manager.read();

    // Load FileInputStream
    drop(cm);
    let fis_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/FileInputStream")
        .expect("should load FileInputStream");

    let cm = shared.classes.class_manager.read();
    let fis = cm.get_class(fis_id).expect("FIS class");
    assert_eq!(&*fis.name, "java/io/FileInputStream");

    // Check superclass is InputStream
    let super_id = fis.superclass.expect("FIS should have superclass");
    let super_cls = cm.get_class(super_id).expect("superclass");
    assert_eq!(
        &*super_cls.name, "java/io/InputStream",
        "FIS should extend InputStream"
    );
}

#[test]
fn io_superclass_chain_print_stream() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let ps_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/PrintStream")
        .expect("should load PrintStream");

    let cm = shared.classes.class_manager.read();
    let ps = cm.get_class(ps_id).expect("PS class");
    let super_id = ps.superclass.expect("PS should have superclass");
    let super_cls = cm.get_class(super_id).expect("superclass");
    assert_eq!(
        &*super_cls.name, "java/io/FilterOutputStream",
        "PrintStream should extend FilterOutputStream"
    );
}

#[test]
fn io_superclass_chain_byte_buffer() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let bb_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/nio/ByteBuffer")
        .expect("should load ByteBuffer");

    let cm = shared.classes.class_manager.read();
    let bb = cm.get_class(bb_id).expect("BB class");
    let super_id = bb.superclass.expect("BB should have superclass");
    let super_cls = cm.get_class(super_id).expect("superclass");
    assert_eq!(
        &*super_cls.name, "java/nio/Buffer",
        "ByteBuffer should extend Buffer"
    );
}

#[test]
fn io_superclass_chain_buffered_reader() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let br_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/BufferedReader")
        .expect("should load BufferedReader");

    let cm = shared.classes.class_manager.read();
    let br = cm.get_class(br_id).expect("BR class");
    let super_id = br.superclass.expect("BR should have superclass");
    let super_cls = cm.get_class(super_id).expect("superclass");
    assert_eq!(
        &*super_cls.name, "java/io/Reader",
        "BufferedReader should extend Reader"
    );
}

// ---------------------------------------------------------------------------
// Field count verification
// ---------------------------------------------------------------------------

#[test]
fn io_field_count_file_streams() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let fis_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/FileInputStream")
        .expect("load FIS");
    let fos_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/FileOutputStream")
        .expect("load FOS");

    let cm = shared.classes.class_manager.read();
    let fis = cm.get_class(fis_id).expect("FIS");
    let fos = cm.get_class(fos_id).expect("FOS");

    // FileInputStream should have at least 1 field (for fd)
    assert!(
        fis.num_total_fields >= 1,
        "FileInputStream should have >= 1 field, got {}",
        fis.num_total_fields
    );
    assert!(
        fos.num_total_fields >= 1,
        "FileOutputStream should have >= 1 field, got {}",
        fos.num_total_fields
    );
}

#[test]
fn io_field_count_byte_buffer() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let bb_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/nio/ByteBuffer")
        .expect("load ByteBuffer");

    let cm = shared.classes.class_manager.read();
    let bb = cm.get_class(bb_id).expect("BB");

    // ByteBuffer should have 5 own fields + 4 from Buffer parent = 9 total
    assert!(
        bb.num_total_fields >= 5,
        "ByteBuffer should have >= 5 total fields, got {}",
        bb.num_total_fields
    );
}

// ---------------------------------------------------------------------------
// Interface verification
// ---------------------------------------------------------------------------

#[test]
fn io_input_stream_implements_closeable() {
    let vm = test_vm();
    let shared = vm.shared.clone();

    let is_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/io/InputStream")
        .expect("load InputStream");

    let cm = shared.classes.class_manager.read();
    let is_cls = cm.get_class(is_id).expect("IS");

    // Class.name is Arc<str> post T10.9.C; convert to String at the test
    // boundary so the `contains(&String)` check below stays readable.
    let iface_names: Vec<String> = is_cls
        .interfaces
        .iter()
        .filter_map(|&iid| cm.class_store.get(iid).map(|c| c.name.to_string()))
        .collect();

    assert!(
        iface_names.contains(&"java/io/Closeable".to_string()),
        "InputStream should implement Closeable, got {:?}",
        iface_names
    );
}
