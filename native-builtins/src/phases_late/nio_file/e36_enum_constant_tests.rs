use super::*;
use crate::test_utils::MockNativeContext;
// The accessor traits must be in scope for `get_field` /
// `ensure_class_initialized` / `read_string` to resolve on the mock.
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeContext, NativeExceptionAccess, NativeHeapAccess,
    NativeSystemAccess,
};

/// Declare `class_name`'s statics at slots `0..n` so
/// `set_static_field_by_name` has something to resolve. The mock's
/// `static_field_index_by_name` reads exactly this table, and the VM's
/// resolves a DECLARED static and is a silent no-op otherwise — so a test
/// that skips this measures the mock's emptiness, not the native.
fn declare_statics(ctx: &MockNativeContext, class_id: ClassId, names: &[&str], desc: &str) {
    let fields = names
        .iter()
        .enumerate()
        .map(|(slot_index, name)| FieldMetadata {
            name: (*name).to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: class_id,
            is_static: true,
        })
        .collect();
    ctx.set_declared_fields(class_id, fields);
}

/// The fallback pair is `name → 0, ordinal → 1` and NOT the other way
/// round.
///
/// This is the assertion that would have failed on
/// `posix_file_permission_stub_clinit` before 2026-08-13, where the two
/// were `ordinal → 0, name → 1`. Inverted, the ordinal lands in the slot
/// `Enum.name()` reads and every constant is nameless: `toString()` null,
/// `compareTo` calling every pair equal, and `Enum.valueOf` matching
/// nothing. One such constant in one JDK enum zeroed fifteen netty classes.
#[test]
fn the_enum_slot_fallbacks_are_name_then_ordinal() {
    assert_eq!(
        (
            crate::lang_misc::ENUM_NAME_SLOT,
            crate::lang_misc::ENUM_ORDINAL_SLOT
        ),
        (0, 1),
        "Enum declares name then ordinal; swapping these mints nameless constants"
    );
    let mut ctx = MockNativeContext::new();
    assert_eq!(enum_name_ordinal_slots(&mut ctx), (0, 1));
}

/// The slots are RESOLVED, not hard-coded — the mutation half.
///
/// `java/lang/Enum` is declared here with its two fields in the opposite
/// order, which is the only way to tell a resolver from a literal: an
/// implementation that returned `(0, 1)` unconditionally passes the test
/// above and fails this one.
#[test]
fn the_enum_slots_follow_the_declared_layout_not_a_literal() {
    let mut ctx = MockNativeContext::new();
    let enum_cid = ctx.ensure_class_initialized("java/lang/Enum").unwrap();
    ctx.set_declared_fields(
        enum_cid,
        vec![
            FieldMetadata {
                name: "ordinal".to_string(),
                descriptor: "I".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: enum_cid,
                is_static: false,
            },
            FieldMetadata {
                name: "name".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: enum_cid,
                is_static: false,
            },
        ],
    );
    assert_eq!(
        enum_name_ordinal_slots(&mut ctx),
        (1, 0),
        "the slots must come from Enum's layout, not from the fallback literals"
    );
}

/// `p57_alloc_enum` writes the NAME as a String and the ORDINAL as an int,
/// each in the slot the layout says — not "a non-null object exists".
///
/// The receiver's own layout is deliberately not consulted: `java/lang/Enum`
/// is declared inverted again, so a constant minted through the receiver
/// (or through literals) puts the String where `ordinal()` reads.
#[test]
fn an_alloc_enum_constant_carries_a_readable_name() {
    let mut ctx = MockNativeContext::new();
    let v = p57_alloc_enum(&mut ctx, "java/nio/file/FileVisitResult", "TERMINATE", 3)
        .expect("alloc enum")
        .expect("a value");
    let obj = match v {
        Value::Object(Some(o)) => o,
        other => panic!("expected an enum constant, got {other:?}"),
    };
    let (name_slot, ordinal_slot) = enum_name_ordinal_slots(&mut ctx);
    let name_ref = match ctx.get_field(obj, name_slot) {
        Value::Object(Some(s)) => s,
        other => panic!("name slot {name_slot} holds {other:?}, not a String — NAMELESS"),
    };
    assert_eq!(ctx.read_string(name_ref).as_deref(), Some("TERMINATE"));
    assert_eq!(ctx.get_field(obj, ordinal_slot), Value::Int(3));
}

/// The nine constants are published with a populated `name`, the ordinal
/// is the DECLARATION index, and `$VALUES[i]` is the same object as the
/// static.
///
/// Driven through `posix_publish_constants`, not through
/// `posix_file_permission_stub_clinit`: the latter's first act is
/// `is_class_synthetic_stub`, which answers the trait default `false`
/// under the mock, so a test aimed there would measure the guard and reach
/// no field write at all.
#[test]
fn every_posix_permission_constant_is_published_with_its_name() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION).unwrap();
    let mut statics: Vec<&str> = POSIX_FILE_PERMISSION_CONSTANTS.to_vec();
    statics.push("$VALUES");
    declare_statics(
        &ctx,
        cid,
        &statics,
        "Ljava/nio/file/attribute/PosixFilePermission;",
    );

    posix_publish_constants(&mut ctx).expect("publish");

    let (name_slot, ordinal_slot) = enum_name_ordinal_slots(&mut ctx);
    for (ord, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let slot = ctx
            .static_field_index_by_name(cid, name)
            .unwrap_or_else(|| panic!("{name} was not declared"));
        let constant = match ctx.get_static_field(cid, slot) {
            Value::Object(Some(o)) => o,
            other => panic!("{name} published as {other:?}"),
        };
        let name_ref = match ctx.get_field(constant, name_slot) {
            Value::Object(Some(s)) => s,
            other => panic!("{name} has a nameless constant: name slot holds {other:?}"),
        };
        assert_eq!(
            ctx.read_string(name_ref).as_deref(),
            Some(name),
            "name() must answer the constant's own name"
        );
        assert_eq!(
            ctx.get_field(constant, ordinal_slot),
            Value::Int(ord as i32),
            "{name}'s ordinal is its declaration index"
        );
    }

    let values_slot = ctx.static_field_index_by_name(cid, "$VALUES").unwrap();
    let values = match ctx.get_static_field(cid, values_slot) {
        Value::Object(Some(a)) => a,
        other => panic!("$VALUES is {other:?}"),
    };
    assert_eq!(ctx.array_length(values), 9);
    for (idx, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let slot = ctx.static_field_index_by_name(cid, name).unwrap();
        assert_eq!(
            ctx.get_array_element(values, idx),
            ctx.get_static_field(cid, slot),
            "$VALUES[{idx}] must be == the {name} static, not a second object"
        );
    }
}

/// A second entry does not replace live constants with fresh objects that
/// fail `==`.
#[test]
fn publishing_twice_keeps_the_first_constants() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION).unwrap();
    declare_statics(
        &ctx,
        cid,
        POSIX_FILE_PERMISSION_CONSTANTS,
        "Ljava/nio/file/attribute/PosixFilePermission;",
    );
    posix_publish_constants(&mut ctx).expect("publish");
    let slot = ctx
        .static_field_index_by_name(cid, "OWNER_READ")
        .expect("declared");
    let first = ctx.get_static_field(cid, slot);
    posix_publish_constants(&mut ctx).expect("publish again");
    assert_eq!(first, ctx.get_static_field(cid, slot));
}

/// `values()` hands back a FRESH array holding the interned constants.
///
/// Both halves are the oracle's, measured on this host:
/// `values() != values()` (the real method is `$VALUES.clone()`) while
/// `values()[0] == OWNER_READ`. Returning one shared array would let a
/// single caller's `values()[0] = null` corrupt every later caller.
#[test]
fn values_is_a_fresh_array_of_the_interned_constants() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION).unwrap();
    declare_statics(
        &ctx,
        cid,
        POSIX_FILE_PERMISSION_CONSTANTS,
        "Ljava/nio/file/attribute/PosixFilePermission;",
    );
    posix_publish_constants(&mut ctx).expect("publish");

    let first = posix_file_permission_values(&mut ctx).unwrap().unwrap();
    let second = posix_file_permission_values(&mut ctx).unwrap().unwrap();
    assert_ne!(first, second, "values() must clone, not share $VALUES");
    let (a, b) = match (first, second) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => (a, b),
        other => panic!("values() answered {other:?}"),
    };
    assert_eq!(ctx.array_length(a), 9);
    for (idx, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let slot = ctx.static_field_index_by_name(cid, name).unwrap();
        let published = ctx.get_static_field(cid, slot);
        assert_eq!(ctx.get_array_element(a, idx), published);
        assert_eq!(ctx.get_array_element(b, idx), published);
    }
}

/// `valueOf` resolves THROUGH the static, and both failure shapes are the
/// oracle's — quoted, not invented:
/// `valueOf(null)` → `NullPointerException: Name is null`;
/// `valueOf("nope")` → `IllegalArgumentException: No enum constant
/// java.nio.file.attribute.PosixFilePermission.nope`.
#[test]
fn value_of_returns_the_interned_constant_and_the_jdks_two_failures() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION).unwrap();
    declare_statics(
        &ctx,
        cid,
        POSIX_FILE_PERMISSION_CONSTANTS,
        "Ljava/nio/file/attribute/PosixFilePermission;",
    );
    posix_publish_constants(&mut ctx).expect("publish");

    let wanted = ctx.create_string("GROUP_WRITE");
    let got = posix_file_permission_value_of(&mut ctx, &[Value::Object(Some(wanted))])
        .unwrap()
        .unwrap();
    let slot = ctx.static_field_index_by_name(cid, "GROUP_WRITE").unwrap();
    assert_eq!(
        got,
        ctx.get_static_field(cid, slot),
        "valueOf must answer the same object GETSTATIC yields"
    );

    match posix_file_permission_value_of(&mut ctx, &[Value::Object(None)]) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message },
        ))) => assert_eq!(message.as_deref(), Some("Name is null")),
        other => panic!("valueOf(null) answered {other:?}"),
    }

    let bogus = ctx.create_string("nope");
    match posix_file_permission_value_of(&mut ctx, &[Value::Object(Some(bogus))]) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => assert_eq!(
            message,
            "No enum constant java.nio.file.attribute.PosixFilePermission.nope"
        ),
        other => panic!("valueOf(\"nope\") answered {other:?}"),
    }
}

/// The constant order IS the ordinal, so it is pinned against a
/// plausible-looking reordering. Taken from
/// `javap -p java.nio.file.attribute.PosixFilePermission` on this host
/// (Microsoft build 25.0.3+9-LTS) — it is neither alphabetical nor sorted
/// by mode bit, and `posix_permission_bits_from_set`'s `BITS` table is
/// positionally paired with it.
#[test]
fn the_constant_order_is_the_jdk_declaration_order() {
    assert_eq!(
        POSIX_FILE_PERMISSION_CONSTANTS,
        &[
            "OWNER_READ",
            "OWNER_WRITE",
            "OWNER_EXECUTE",
            "GROUP_READ",
            "GROUP_WRITE",
            "GROUP_EXECUTE",
            "OTHERS_READ",
            "OTHERS_WRITE",
            "OTHERS_EXECUTE",
        ]
    );
}
