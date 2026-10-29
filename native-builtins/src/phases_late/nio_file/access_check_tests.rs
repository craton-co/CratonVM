//! `WindowsNativeDispatcher.AccessCheck` over a real descriptor: the
//! `WindowsSecurity.checkAccessMask` sequence (`OpenProcessToken`,
//! `DuplicateTokenEx`, `GetFileSecurity0`, `AccessCheck`) driven natively, with
//! the same constants the JDK passes.

use super::*;
use crate::test_utils::MockNativeContext;

const CLS: &str = "sun/nio/fs/WindowsNativeDispatcher";
// `WindowsSecurity.checkAccessMask`'s mapping: FILE_GENERIC_READ/WRITE/
// EXECUTE and FILE_ALL_ACCESS.
const GENERIC: [i32; 4] = [0x0012_0089, 0x0012_0116, 0x0012_00A0, 0x001F_01FF];
const FILE_READ_DATA: i32 = 0x1;
const FILE_WRITE_DATA: i32 = 0x2;
const FILE_EXECUTE: i32 = 0x20;
const TOKEN_DUPLICATE_QUERY: i32 = 0x2 | 0x8;
const TOKEN_QUERY: i32 = 0x8;
const OWNER_GROUP_DACL: i32 = 1 | 2 | 4;

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    win32_register_security_natives(&mut r);
    win32_register_token_natives(&mut r);
    r
}

fn call(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    name: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = r
        .find(CLS, name, desc)
        .unwrap_or_else(|| panic!("{name}{desc} is not registered"));
    cb(ctx, args)
}

fn long(v: MethodCallResult) -> i64 {
    match v {
        Ok(Some(Value::Long(l))) => l,
        other => panic!("expected a long, got {other:?}"),
    }
}

/// A NUL-terminated UTF-16 path in a Vec the mock reads as a raw pointer.
fn path_buffer(path: &std::path::Path) -> Vec<u8> {
    let mut bytes: Vec<u8> = path
        .to_string_lossy()
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

/// The impersonation token id `WindowsSecurity` holds.
fn impersonation_token(r: &NativeMethodRegistry, ctx: &mut MockNativeContext) -> i64 {
    let process = long(call(r, ctx, "GetCurrentProcess", "()J", &[]));
    let primary = long(call(
        r,
        ctx,
        "OpenProcessToken",
        "(JI)J",
        &[Value::Long(process), Value::Int(TOKEN_DUPLICATE_QUERY)],
    ));
    long(call(
        r,
        ctx,
        "DuplicateTokenEx",
        "(JI)J",
        &[Value::Long(primary), Value::Int(TOKEN_QUERY)],
    ))
}

/// `GetFileSecurity0` into a buffer, as `WindowsSecurity.getFileSecurity`
/// does after its first (size-asking) call.
fn fetch_descriptor(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    path: &std::path::Path,
) -> (MethodCallResult, Vec<u8>) {
    let wide = path_buffer(path);
    let mut sd = vec![0u8; 4096];
    let got = call(
        r,
        ctx,
        "GetFileSecurity0",
        "(JIJI)I",
        &[
            Value::Long(wide.as_ptr() as i64),
            Value::Int(OWNER_GROUP_DACL),
            Value::Long(sd.as_mut_ptr() as i64),
            Value::Int(sd.len() as i32),
        ],
    );
    (got, sd)
}

fn access_check(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    token: i64,
    sd_addr: i64,
    mask: i32,
) -> MethodCallResult {
    call(
        r,
        ctx,
        "AccessCheck",
        "(JJIIIII)Z",
        &[
            Value::Long(token),
            Value::Long(sd_addr),
            Value::Int(mask),
            Value::Int(GENERIC[0]),
            Value::Int(GENERIC[1]),
            Value::Int(GENERIC[2]),
            Value::Int(GENERIC[3]),
        ],
    )
}

#[test]
fn access_check_grants_the_owner_read_write_on_their_own_file() {
    let r = registry();
    let mut ctx = MockNativeContext::new();
    let file = std::env::temp_dir().join(format!("cvm-accesscheck-{}.txt", std::process::id()));
    std::fs::write(&file, b"x").expect("create temp file");
    let token = impersonation_token(&r, &mut ctx);
    let (got, mut sd) = fetch_descriptor(&r, &mut ctx, &file);
    assert!(
        matches!(got, Ok(Some(Value::Int(4096)))),
        "descriptor fits in 4 KiB: {got:?}"
    );
    let addr = sd.as_mut_ptr() as i64;
    for mask in [
        FILE_READ_DATA,
        FILE_WRITE_DATA,
        FILE_READ_DATA | FILE_WRITE_DATA,
    ] {
        let answer = access_check(&r, &mut ctx, token, addr, mask);
        assert!(
            matches!(answer, Ok(Some(Value::Int(1)))),
            "mask {mask:#x}: {answer:?}"
        );
    }
    // Generic bits go through `MapGenericMask` before the check, so a bare
    // GENERIC_READ (0x80000000) is granted exactly when FILE_GENERIC_READ is.
    let generic = access_check(&r, &mut ctx, token, addr, i32::MIN);
    assert!(matches!(generic, Ok(Some(Value::Int(1)))), "{generic:?}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn access_check_answers_a_clean_boolean_for_a_protected_directory() {
    let r = registry();
    let mut ctx = MockNativeContext::new();
    let token = impersonation_token(&r, &mut ctx);
    // `System Volume Information` grants an ordinary user nothing; the
    // descriptor fetch itself may be refused on a locked-down machine, in
    // which case there is no descriptor to check.
    let (fetched, mut sd) = fetch_descriptor(
        &r,
        &mut ctx,
        std::path::Path::new("C:/System Volume Information"),
    );
    if fetched.is_err() {
        return;
    }
    let read = access_check(
        &r,
        &mut ctx,
        token,
        sd.as_mut_ptr() as i64,
        FILE_READ_DATA | FILE_EXECUTE,
    );
    // An elevated run may be granted it; the point is a boolean, not a throw.
    assert!(
        matches!(read, Ok(Some(Value::Int(0))) | Ok(Some(Value::Int(1)))),
        "{read:?}"
    );
}

#[test]
fn access_check_rejects_a_bad_token_and_a_bad_descriptor() {
    let r = registry();
    let mut ctx = MockNativeContext::new();
    let token = impersonation_token(&r, &mut ctx);
    let (got, mut sd) = fetch_descriptor(&r, &mut ctx, &std::env::temp_dir());
    assert!(
        matches!(got, Ok(Some(Value::Int(4096)))),
        "descriptor fits in 4 KiB: {got:?}"
    );
    let addr = sd.as_mut_ptr() as i64;
    // An id the token table never minted.
    assert!(access_check(&r, &mut ctx, 0x1234, addr, FILE_READ_DATA).is_err());
    // A zeroed buffer is not a self-relative descriptor.
    let mut junk = vec![0u8; 64];
    assert!(access_check(
        &r,
        &mut ctx,
        token,
        junk.as_mut_ptr() as i64,
        FILE_READ_DATA
    )
    .is_err());
    // The null descriptor.
    assert!(access_check(&r, &mut ctx, token, 0, FILE_READ_DATA).is_err());
}
