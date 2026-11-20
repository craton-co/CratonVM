use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn normalize_supported_accepts_known_aliases() {
    assert_eq!(normalize_supported("UTF8").as_deref(), Some("UTF-8"));
    assert_eq!(normalize_supported("ascii").as_deref(), Some("US-ASCII"));
    assert_eq!(normalize_supported("KOI8-R").as_deref(), Some("KOI8-R"));
}

#[test]
fn normalize_supported_rejects_unknown_name() {
    assert_eq!(normalize_supported("NoSuchCharset-42"), None);
}

#[test]
fn normalize_supported_accepts_engine_supported_charsets() {
    // These were rejected by the old stale private alias table even
    // though the engine encodes them (IBM850 hand-written, the CJK
    // families via encoding_rs) — the shared table accepts them.
    assert_eq!(
        normalize_supported("Shift_JIS").as_deref(),
        Some("Shift_JIS")
    );
    assert_eq!(normalize_supported("GBK").as_deref(), Some("GBK"));
    assert_eq!(normalize_supported("ibm850").as_deref(), Some("IBM850"));
    assert_eq!(normalize_supported("Cp850").as_deref(), Some("IBM850"));
}

#[test]
fn normalize_supported_rejects_canonical_but_codecless_charsets() {
    // KOI8-U canonicalizes in the shared table but the engine has no
    // codec for it — the engine probe must still reject the name.
    assert_eq!(normalize_supported("KOI8-U"), None);
}

#[test]
fn normalize_keeps_utf8_fallback_for_object_path() {
    // The infallible `normalize` (used only on the already-validated
    // Charset-object path) still maps unknowns to UTF-8.
    assert_eq!(normalize("NoSuchCharset-42"), "UTF-8");
}

// --- gc-common w16-c (`common-w15c-native-io-identity-keyed-tables-outside-the-weak-sweep` row 2) ---

use crate::test_support::MockNativeContext;
use cratonvm_types::error::VmError;

fn se_row(key: crate::IoRowKey) -> Option<(String, Vec<u8>, usize)> {
    se_table()
        .lock()
        .get(&key)
        .map(|s| (s.name.clone(), s.pending.clone(), s.owner))
}

fn encoder(ctx: &mut MockNativeContext, charset: &str) -> ObjectRef {
    let os = ctx.alloc_object(1);
    alloc_stream_encoder(ctx, os, charset).expect("encoder")
}

fn se_write_str(
    ctx: &mut MockNativeContext,
    se: ObjectRef,
    text: &str,
    off: i32,
    len: i32,
) -> MethodCallResult {
    let s = ctx.attach_string(text);
    native_se_write_string(
        ctx,
        &[
            Value::Object(Some(se)),
            Value::Object(Some(s)),
            Value::Int(off),
            Value::Int(len),
        ],
    )
}

fn runtime(r: &MethodCallResult) -> Option<&RuntimeError> {
    match r {
        Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => Some(e),
        _ => None,
    }
}

/// The retire criterion for row 2: two VMs whose encoders have EQUAL
/// identity hashes keep their own rows. The bare-hash key made B's
/// `forOutputStreamWriter` replace A's charset and pending bytes.
#[test]
fn w16c_two_vms_with_equal_identity_hashes_keep_their_own_encoder_rows() {
    let mut a = MockNativeContext::with_address_base(0x5E23_0000);
    let mut b = MockNativeContext::with_address_base(0x5E23_0000);
    let ea = encoder(&mut a, "ISO-8859-1");
    let eb = encoder(&mut b, "UTF-16BE");
    assert_eq!(a.identity_hash_code(ea), b.identity_hash_code(eb));
    let (key_a, key_b) = (se_key(&a, ea), se_key(&b, eb));
    assert_ne!(key_a, key_b);
    assert!(se_write_str(&mut a, ea, "hi", 0, 2).is_ok());
    let row_a = se_row(key_a).expect("A's row");
    assert_eq!(
        (row_a.0.as_str(), row_a.1.as_slice()),
        ("ISO-8859-1", &b"hi"[..])
    );
    assert_eq!(row_a.2, ea.as_ptr() as usize);
    assert_eq!(se_row(key_b).map(|r| r.0).as_deref(), Some("UTF-16BE"));
    crate::forget_vm_io_side_tables(a.vm_identity());
    crate::forget_vm_io_side_tables(b.vm_identity());
}

/// The sweep, through the crate hook, and teardown.
#[test]
fn w16c_sweep_follows_a_moved_encoder_and_drops_a_dead_one() {
    let mut ctx = MockNativeContext::new();
    let mut other = MockNativeContext::new();
    let moved = encoder(&mut ctx, "UTF-8");
    let dead = encoder(&mut ctx, "UTF-8");
    let foreign = encoder(&mut other, "UTF-8");
    let (moved_key, dead_key) = (se_key(&ctx, moved), se_key(&ctx, dead));
    let foreign_key = se_key(&other, foreign);

    // A real move keeps the identity hash (the mock's is the low 32 bits
    // of the address); the row is re-filed under the new address
    // (gc-common w29-c).
    let to = moved.as_ptr() as usize + (1usize << 32);
    let to_key = crate::io_row_key(crate::io_side_key(&ctx, moved), to);
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(moved.as_ptr() as usize, to);
    assert_eq!(
        crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map, &|_| false),
        1
    );
    assert_eq!(se_row(to_key).map(|r| r.2), Some(to));
    assert!(se_row(moved_key).is_none(), "re-filed, not copied");
    assert!(se_row(dead_key).is_none());
    assert!(
        se_row(foreign_key).is_some(),
        "another VM's row is untouched"
    );
    assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 1);
    crate::forget_vm_io_side_tables(other.vm_identity());
}

/// `write(String, int, int)` and `write(char[], int, int)` check like the
/// JDK instead of clamping: `write(s, 0, 500)` on a 3-char string used to
/// write 3 chars and return normally.
#[test]
fn w16c_writes_check_their_regions_like_the_jdk() {
    let mut ctx = MockNativeContext::with_address_base(0x5E24_0000);
    let se = encoder(&mut ctx, "UTF-8");
    let key = se_key(&ctx, se);

    let r = se_write_str(&mut ctx, se, "abc", 0, 500);
    assert!(
        matches!(
            runtime(&r),
            Some(RuntimeError::StringIndexOutOfBoundsException { .. })
        ),
        "write(s, 0, 500) gave {r:?}"
    );
    let r = se_write_str(&mut ctx, se, "abc", 0, -1);
    assert!(
        matches!(
            runtime(&r),
            Some(RuntimeError::IndexOutOfBoundsException { .. })
        ),
        "write(s, 0, -1) gave {r:?}"
    );
    let r = native_se_write_string(
        &mut ctx,
        &[
            Value::Object(Some(se)),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
        ],
    );
    assert!(matches!(
        runtime(&r),
        Some(RuntimeError::NullPointerException { .. })
    ));
    assert_eq!(
        se_row(key).map(|r| r.1),
        Some(Vec::new()),
        "nothing was buffered"
    );

    let cbuf = ctx.new_array(ArrayElementType::Char, 2);
    ctx.set_array_element(cbuf, 0, Value::Int(b'x' as i32));
    ctx.set_array_element(cbuf, 1, Value::Int(b'y' as i32));
    for (off, len) in [(-1, 1), (0, -1), (1, 2), (3, 0)] {
        let r = native_se_write_chars(
            &mut ctx,
            &[
                Value::Object(Some(se)),
                Value::Object(Some(cbuf)),
                Value::Int(off),
                Value::Int(len),
            ],
        );
        assert!(
            matches!(
                runtime(&r),
                Some(RuntimeError::IndexOutOfBoundsException { .. })
            ),
            "write(cbuf, {off}, {len}) gave {r:?}"
        );
    }
    let r = native_se_write_chars(
        &mut ctx,
        &[
            Value::Object(Some(se)),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
        ],
    );
    assert!(matches!(
        runtime(&r),
        Some(RuntimeError::NullPointerException { .. })
    ));

    assert!(se_write_str(&mut ctx, se, "abc", 1, 2).is_ok());
    let r = native_se_write_chars(
        &mut ctx,
        &[
            Value::Object(Some(se)),
            Value::Object(Some(cbuf)),
            Value::Int(0),
            Value::Int(2),
        ],
    );
    assert!(r.is_ok(), "an in-range write gave {r:?}");
    assert_eq!(se_row(key).map(|r| r.1), Some(b"bcxy".to_vec()));
    crate::forget_vm_io_side_tables(ctx.vm_identity());
}
