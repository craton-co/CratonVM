use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Compute the public CRC-32C checksum of `data` (fresh state).
fn crc32c(data: &[u8]) -> u32 {
    !crc32c_step(0xFFFF_FFFF, data)
}

#[test]
fn crc32c_known_vectors() {
    // The canonical CRC-32C check value: CRC of "123456789" is 0xE3069283
    // (RFC 3720 / iSCSI, and the JDK's CRC32C).
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);

    // Empty input — running state untouched, public value is ~0xFFFFFFFF.
    assert_eq!(crc32c(b""), 0x0000_0000);

    // 32 zero bytes — well-known CRC-32C vector (RFC 3720 appendix).
    assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);

    // 32 0xFF bytes — RFC 3720 appendix vector.
    assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);

    // Single byte.
    assert_eq!(crc32c(b"a"), 0xC1D0_4330);
}

#[test]
fn crc32c_incremental_matches_oneshot() {
    // Folding in two chunks must equal a single-shot computation.
    let full = b"The quick brown fox jumps over the lazy dog";
    let oneshot = crc32c_step(0xFFFF_FFFF, full);
    let (a, b) = full.split_at(17);
    let two = crc32c_step(crc32c_step(0xFFFF_FFFF, a), b);
    assert_eq!(oneshot, two);
}

#[test]
fn crc32c_byte_at_a_time_matches_bulk() {
    let data = b"123456789";
    let mut running = 0xFFFF_FFFFu32;
    for &byte in data {
        running = crc32c_step(running, &[byte]);
    }
    assert_eq!(!running, 0xE306_9283);
}

/// The running state stored in field slot 0 is the *uncomplemented* value;
/// confirm the complement relationship the JIT contract relies on.
#[test]
fn crc32c_running_state_is_uncomplemented() {
    let running = crc32c_step(0xFFFF_FFFF, b"123456789");
    assert_eq!(!running, 0xE306_9283);
    assert_eq!(running, !0xE306_9283u32);
}

// Round 12 wave 7 (lane compat): the overrides run under `--compatible`
// only (their `SyntheticStub` rows are refused by `--real-jdk`, which runs
// the JDK body), and they now do the JDK body's null and range checks.
// `checked == false` is the `CRATONVM_CRC32C_RANGE_CHECK=0` arm.

use crate::test_utils::MockNativeContext;
use cratonvm_types::error::{MethodCallFailed, VmError};
use cratonvm_types::{ArrayElementType, ClassId};

fn receiver_and_bytes(ctx: &mut MockNativeContext, data: &[u8]) -> (ObjectRef, ObjectRef) {
    let this = ctx.alloc_object(ClassId::new(1), 1);
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(-1));
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, &b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(i32::from(b as i8)));
    }
    (this, arr)
}

fn state(ctx: &MockNativeContext, this: ObjectRef) -> i32 {
    ctx.get_field(this, CRC_FIELD_SLOT).as_int().unwrap_or(0)
}

fn runtime_error(r: MethodCallResult) -> RuntimeError {
    match r {
        Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => e,
        other => panic!("expected a Java runtime exception, got {other:?}"),
    }
}

#[test]
fn crc32c_update_range_outside_the_array_throws_the_jdk_aioobe() {
    let mut ctx = MockNativeContext::new();
    let (this, arr) = receiver_and_bytes(&mut ctx, b"0123456789");
    for (off, len) in [(8, 3), (-1, 2), (2, -1), (11, 0), (i32::MAX, i32::MAX)] {
        let args = [
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(off),
            Value::Int(len),
        ];
        let err = runtime_error(crc32c_update_bytes_off_len_with(&mut ctx, &args, true));
        let want = format!("Range [{off}, {off} + {len}) out of bounds for length 10");
        match err {
            RuntimeError::ArrayIndexOutOfBoundsException { message, .. } => {
                assert_eq!(message.as_deref(), Some(want.as_str()));
            }
            other => panic!("({off}, {len}): expected AIOOBE, got {other:?}"),
        }
        // The checksum is untouched, as on HotSpot.
        assert_eq!(state(&ctx, this), -1, "({off}, {len}) folded bytes");
    }
    // The edges are in range: an empty range at the end, the whole array.
    for (off, len) in [(10, 0), (0, 10), (0, 0)] {
        let args = [
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(off),
            Value::Int(len),
        ];
        assert!(crc32c_update_bytes_off_len_with(&mut ctx, &args, true).is_ok());
    }
    assert_eq!(
        state(&ctx, this) as u32,
        crc32c_step(0xFFFF_FFFF, b"0123456789")
    );
}

#[test]
fn crc32c_update_null_array_throws_npe() {
    let mut ctx = MockNativeContext::new();
    let (this, _) = receiver_and_bytes(&mut ctx, b"");
    let three = [
        Value::Object(Some(this)),
        Value::Object(None),
        Value::Int(0),
        Value::Int(0),
    ];
    match runtime_error(crc32c_update_bytes_off_len_with(&mut ctx, &three, true)) {
        RuntimeError::NullPointerException { message } => assert_eq!(message, None),
        other => panic!("expected NPE, got {other:?}"),
    }
    let one = [Value::Object(Some(this)), Value::Object(None)];
    match runtime_error(crc32c_update_bytes_with(&mut ctx, &one, true)) {
        RuntimeError::NullPointerException { message } => {
            assert_eq!(message.as_deref(), Some(CHECKSUM_UPDATE_NULL_ARRAY_MESSAGE))
        }
        other => panic!("expected NPE, got {other:?}"),
    }
    assert_eq!(state(&ctx, this), -1);
}

/// The kill-switch arm keeps the old answer: no exception, the range is
/// clamped to the array.
#[test]
fn crc32c_update_unchecked_arm_keeps_the_old_clamp() {
    let mut ctx = MockNativeContext::new();
    let (this, arr) = receiver_and_bytes(&mut ctx, b"0123456789");
    let args = [
        Value::Object(Some(this)),
        Value::Object(Some(arr)),
        Value::Int(8),
        Value::Int(3),
    ];
    assert!(crc32c_update_bytes_off_len_with(&mut ctx, &args, false).is_ok());
    assert_eq!(state(&ctx, this) as u32, crc32c_step(0xFFFF_FFFF, b"89"));
    let null = [Value::Object(Some(this)), Value::Object(None)];
    assert!(crc32c_update_bytes_with(&mut ctx, &null, false).is_ok());
}
