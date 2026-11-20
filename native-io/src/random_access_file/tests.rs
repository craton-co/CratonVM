use super::*;
use cratonvm_native_api::fd_table::FileDescriptorTable;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::{Read, Write};

/// SBR5 lane ncall: the `RandomAccessFile` rows claimed LEAF
/// (`NativeMethodRegistry::set_leaf`: no Java allocation, no callback into
/// Java, no GC point) are exactly the audited set, and the production code
/// of every leaf body and its helpers stays free of the calls that would
/// break that contract. `open0`/`setLength0`/`close0` and the helpers only
/// they use are exempt (they allocate a `FileDescriptor` or are not leaf).
#[test]
fn leaf_rows_are_the_audited_set_and_contain_no_java_alloc_or_callback() {
    let mut r = NativeMethodRegistry::new();
    register_random_access_file_natives(&mut r);
    let mut leaves: Vec<String> = r
        .leaf_registrations()
        .into_iter()
        .filter(|(c, _, _)| *c == "java/io/RandomAccessFile")
        .map(|(_, m, _)| m.to_string())
        .collect();
    leaves.sort();
    assert_eq!(
        leaves,
        [
            "getFilePointer",
            "length0",
            "read0",
            "readBytes0",
            "seek0",
            "write0",
            "writeBytes0"
        ],
        "the leaf claim set changed: audit the new row against set_leaf's contract"
    );
    let src = include_str!("../random_access_file.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let exempt = [
        "native_open0",
        "native_setLength0",
        "native_close0",
        "native_initIDs",
        // The i53 residual: reached only from the full funnel, after the
        // leaf path's `decline_leaf_call`
        // (`every_peer_wait_is_behind_a_leaf_decline` below).
        "peer_wait_read0",
        "peer_wait_read_bytes0",
        "peer_wait_write",
        "write_handle",
        "clear_handle",
        "register_random_access_file_natives",
    ];
    let forbidden = [
        "new_object",
        "alloc_object",
        "new_array",
        "new_ref_array",
        "create_string",
        "intern_string",
        "begin_blocking_region",
        "invoke(",
        "invoke_virtual",
        "invoke_static",
        "pin_native_root",
        "ensure_class_initialized",
        "maybe_gc",
    ];
    for item in prod.split("\nfn ").skip(1) {
        let name = item.split('(').next().unwrap_or("");
        if exempt.contains(&name) {
            continue;
        }
        for line in item.lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            for f in forbidden {
                assert!(
                    !t.contains(f),
                    "`{f}` in leaf-path fn `{name}`: {t}\nA leaf native may not allocate on the Java heap, call back into Java or reach a GC point."
                );
            }
        }
    }
}

/// The i53 residual: a leaf row reaches a `peer_wait_*` helper (which
/// enters a blocking region) only after `decline_leaf_call` answered
/// `false`, i.e. on the full funnel.
#[test]
fn every_peer_wait_is_behind_a_leaf_decline() {
    let src = include_str!("../random_access_file.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let mut calls = 0;
    for item in prod.split("\nfn ").skip(1) {
        let name = item.split('(').next().unwrap_or("");
        if name.starts_with("peer_wait_") {
            continue;
        }
        let mut declined = false;
        for line in item.lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if t.contains("decline_leaf_call()") {
                declined = true;
            }
            if t.contains("peer_wait_") {
                assert!(
                    declined,
                    "`{name}` calls a peer_wait helper before declining the leaf call: {t}"
                );
                calls += 1;
            }
        }
    }
    assert_eq!(
        calls, 4,
        "read0, readBytes0, write0 and writeBytes0 each wait once"
    );
}

/// The i53 residual: a regular file opened for `RandomAccessFile` is
/// marked regular (it keeps the leaf fast path), and the mark goes with
/// the close.
#[test]
fn a_random_access_regular_file_is_regular() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"x").unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    let table = FileDescriptorTable::new();
    let fd = table.open_random_access(&path, false).unwrap();
    assert!(table.is_regular_file(fd));
    table.close(fd).unwrap();
    assert!(!table.is_regular_file(fd));
}

/// The crux of the TC0622 fix: a file opened via `open_random_access`
/// (the path RAF.open0 now takes) is resolvable by BOTH the sequential
/// RAF read path (`rw_read`) AND the nio channel size path (`file_size`)
/// through the SAME fd. Previously RAF used a private ≥1000 handle map
/// that `file_size` could not see → `IOException: size0: bad fd for size`.
#[test]
fn open_random_access_shares_fd_for_read_and_size() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"hello world").unwrap();
    let path = tmp.path().to_str().unwrap().to_string();

    let table = FileDescriptorTable::new();
    let fd = table.open_random_access(&path, false).unwrap();

    // nio FileChannel.size() → FileDispatcherImpl.size0 → file_size
    assert_eq!(table.file_size(fd).unwrap(), 11);

    // RAF.readBytes0 → rw_read at the cursor
    let mut buf = [0u8; 5];
    let n = table.rw_read(fd, &mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"hello");

    // ...and size0 still works after the cursor moved (it saves/restores).
    assert_eq!(table.file_size(fd).unwrap(), 11);

    table.close(fd).unwrap();
}

#[test]
fn open_random_access_seek_and_length() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789").unwrap();
    let path = tmp.path().to_str().unwrap().to_string();

    let table = FileDescriptorTable::new();
    let fd = table.open_random_access(&path, false).unwrap();

    assert_eq!(table.file_size(fd).unwrap(), 10);

    table.rw_seek(fd, SeekFrom::Start(3)).unwrap();
    assert_eq!(table.rw_position(fd).unwrap(), 3);
    let mut buf = [0u8; 3];
    let n = table.rw_read(fd, &mut buf).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&buf, b"345");

    table.close(fd).unwrap();
}

/// "rw" mode opens read+write+create; write-then-read-back roundtrips,
/// and the durable-sync path (`rw_sync`, used by "rws"/"rwd") succeeds.
#[test]
fn open_random_access_write_and_sync() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raf_rw.bin");
    let path_s = path.to_str().unwrap().to_string();

    let table = FileDescriptorTable::new();
    let fd = table.open_random_access(&path_s, true).unwrap();
    table.write_bytes(fd, b"durable").unwrap();
    // "rwd"/"rws" per-write durability hook.
    table.rw_sync(fd, true).unwrap();
    table.rw_sync(fd, false).unwrap();
    table.close(fd).unwrap();

    let mut s = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "durable");
}

/// "r" mode must open the underlying file read-only: a write fails at
/// the OS level (justifying why RAF "r" rejects writes).
#[test]
fn raf_native_read_write_bytes_roundtrip_nonzero_payload() {
    use crate::test_support::MockNativeContext;
    use cratonvm_native_api::NativeContext;
    use cratonvm_types::ArrayElementType;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raf-bytes.bin");
    let path_s = path.to_str().unwrap().to_string();

    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(0);
    let fd_obj = ctx.alloc_object(0);
    ctx.set_field_by_name(this, "fd", Value::Object(Some(fd_obj)));
    let path_obj = ctx.attach_string(&path_s);

    native_open0(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(path_obj)),
            Value::Int(O_RDWR),
        ],
    )
    .unwrap();

    let src = ctx.new_array(ArrayElementType::Byte, 4);
    for (i, b) in [0x11_i32, 0x22, 0x33, 0x44].into_iter().enumerate() {
        ctx.set_array_element(src, i, Value::Int(b));
    }
    native_writeBytes0(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Int(4),
        ],
    )
    .unwrap();
    native_seek0(&mut ctx, &[Value::Object(Some(this)), Value::Long(0)]).unwrap();

    let dst = ctx.new_array(ArrayElementType::Byte, 4);
    let n = native_readBytes0(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(4),
        ],
    )
    .unwrap();
    assert_eq!(n, Some(Value::Int(4)));
    for (i, b) in [0x11_i32, 0x22, 0x33, 0x44].into_iter().enumerate() {
        assert_eq!(ctx.get_array_element(dst, i), Value::Int(b));
    }

    native_close0(&mut ctx, &[Value::Object(Some(this))]).unwrap();
}

#[test]
fn open_options_read_only_rejects_write() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"x").unwrap();
    let mut opts = OpenOptions::new();
    opts.read(true);
    let mut f = opts.open(tmp.path()).unwrap();
    assert!(f.write_all(b"y").is_err());
}

/// HIGH-severity security regression guard: under
/// `set_path_confine_to_cwd(true)`, `RandomAccessFile.open0` must
/// reject a relative-path traversal *before* opening the file. The
/// Java-visible failure must be `FileNotFoundException` (matching
/// `open0`'s declared throws), not a `SecurityException` leaking
/// out of the native into a confused Java caller.
#[test]
fn raf_open0_rejects_relative_traversal_under_confinement() {
    use crate::test_support::MockNativeContext;
    use cratonvm_native_api::NativeContext;

    let _g = crate::test_support::confine_test_lock().lock();
    crate::set_path_confine_to_cwd(true);

    // Build minimal `this`: an object whose `fd` field references
    // another (empty) FileDescriptor object. open0 will call
    // `raf_fd_object` on `this` via `get_field_by_name("fd")` —
    // our mock returns `Value::Object(None)` from that, so the
    // fd_object lookup yields None and `write_handle` becomes a
    // no-op. That doesn't matter here: validation runs and rejects
    // BEFORE any handle is written.
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(1);
    let path = ctx.attach_string("../../etc/passwd");
    let args = [
        Value::Object(Some(this)),
        Value::Object(Some(path)),
        Value::Int(1), // O_RDONLY
    ];

    let r = native_open0(&mut ctx, &args);

    crate::set_path_confine_to_cwd(false);

    assert!(r.is_err(), "RAF.open0 accepted traversal path");
    let err = format!("{:?}", r.unwrap_err());
    assert!(
        err.contains("FileNotFoundException"),
        "expected FileNotFoundException, got: {err}"
    );
    // Confirm the SecurityException did NOT leak out — the entry
    // point's contract is `throws FileNotFoundException` only.
    assert!(
        !err.contains("SecurityException"),
        "SecurityException leaked out of RAF.open0: {err}"
    );
}

/// JIT r23 wave 3 review (`r23-w3-review-fd-os-keys-reuse-audit`): a
/// `sync_modes` row whose generation is not the live entry's (an earlier
/// owner of a reused OS key, closed by a path other than `close0`) is
/// ignored and dropped; the live owner's own row still applies.
#[test]
fn a_stale_sync_mode_row_never_reaches_a_reused_id() {
    use crate::test_support::MockNativeContext;
    let ctx = MockNativeContext::new();
    let path = std::env::temp_dir().join(format!(
        "cratonvm_raf_sync_gen_{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, b"abc").unwrap();
    let path = path.to_string_lossy().into_owned();
    let fd = ctx.fd_table().open_random_access(&path, true).unwrap();
    let live = ctx.fd_table().generation(fd).expect("open");
    sync_modes().lock().insert(fd, (SyncMode::Data, live));
    assert_eq!(live_sync_mode(&ctx, fd), Some(SyncMode::Data));
    sync_modes()
        .lock()
        .insert(fd, (SyncMode::Full, live.wrapping_add(1)));
    assert_eq!(live_sync_mode(&ctx, fd), None, "a stale owner's row");
    assert!(!sync_modes().lock().contains_key(&fd), "and it is dropped");
    ctx.fd_table().close(fd).unwrap();
    let _ = std::fs::remove_file(&path);
}
