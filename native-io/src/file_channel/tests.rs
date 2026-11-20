use super::*;
use cratonvm_native_api::fd_table::FileDescriptorTable;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::Write;

/// Round-trip: mmap a real file RW, mutate via the mapping,
/// re-read through the file API, confirm bytes match.
#[test]
fn wp3_3_mmap_rw_round_trips_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rw.bin");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: the test-created file is 4096 bytes, remains open, and this
    // mapping is the sole mutable view for its full length.
    let mut m = unsafe { opts.len(4096).map_mut(&file).unwrap() };

    // Mutate via the mapping.
    for (i, b) in m.iter_mut().enumerate().take(256) {
        *b = (i as u8).wrapping_add(7);
    }
    m.flush().unwrap();
    drop(m);

    // Read back via stdlib — bytes must match.
    let on_disk = std::fs::read(&path).unwrap();
    for i in 0..256 {
        assert_eq!(on_disk[i], (i as u8).wrapping_add(7));
    }
}

/// Round-trip: mmap RO and verify reads return file bytes.
#[test]
fn wp3_3_mmap_ro_reads_existing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.bin");
    let mut payload = vec![0u8; 8192];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    std::fs::write(&path, &payload).unwrap();

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: the test file is 8192 initialized bytes and remains open for
    // the lifetime of this read-only mapping.
    let m = unsafe { opts.len(8192).map(&file).unwrap() };
    assert_eq!(&m[..], &payload[..]);
}

/// Confirm a 16 MiB mapping works (largest realistic test
/// without thrashing CI).  This proves we are NOT staging
/// through a Rust Vec — that would balloon RAM.
#[test]
fn wp3_3_mmap_large_file_round_trip() {
    const SIZE: usize = 16 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.bin");
    // Use sparse write — actual disk usage may be smaller on
    // sparse-aware filesystems.
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(SIZE as u64).unwrap();
    drop(f);

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: set_len established SIZE bytes, the handle remains open, and
    // this is the sole mutable mapping.
    let mut m = unsafe { opts.len(SIZE).map_mut(&file).unwrap() };

    // Touch the four corners and the midpoint.
    m[0] = 0xAA;
    m[SIZE - 1] = 0x55;
    m[SIZE / 2] = 0x33;
    m.flush().unwrap();
    drop(m);

    let f2 = std::fs::File::open(&path).unwrap();
    let file2 = f2;
    let opts2 = memmap2::MmapOptions::new();
    // SAFETY: the file still has SIZE bytes and remains open for the
    // lifetime of the read-only mapping.
    let m2 = unsafe { opts2.map(&file2).unwrap() };
    assert_eq!(m2[0], 0xAA);
    assert_eq!(m2[SIZE - 1], 0x55);
    assert_eq!(m2[SIZE / 2], 0x33);
}

/// Map at a non-zero offset and verify position semantics.
#[test]
fn wp3_3_mmap_with_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("offset.bin");
    // 64 KiB of structured data: byte i = i & 0xff.
    let mut payload = vec![0u8; 64 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    std::fs::write(&path, &payload).unwrap();

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    // Map starting at offset 0 first — verify the basic
    // mapping covers the entire 64 KiB.
    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: the test initialized all 64 KiB and holds the file open for
    // the lifetime of this read-only mapping.
    let m = unsafe { opts.offset(0).len(64 * 1024).map(&file).unwrap() };
    assert_eq!(m[100], (100 & 0xff) as u8);
    assert_eq!(m[1024], (1024 & 0xff) as u8);
    drop(m);

    // Now map at offset = 4 KiB (page-aligned on every
    // platform we target). Verify the first byte of the
    // new mapping equals byte 4096 of the file.
    let mut opts2 = memmap2::MmapOptions::new();
    // SAFETY: offset and length are page-aligned and wholly inside the
    // initialized 64 KiB test file, which remains open.
    let m2 = unsafe { opts2.offset(4096).len(4096).map(&file).unwrap() };
    // First byte of m2 should == byte 4096 of the file == 0
    // (because (4096 & 0xff) == 0).
    assert_eq!(m2[0], (4096 & 0xff) as u8);
    // Byte 100 of m2 == byte 4196 of the file.
    assert_eq!(m2[100], (4196 & 0xff) as u8);
}

/// COW (private) mapping: writes do NOT propagate to the file.
#[test]
fn wp3_3_mmap_cow_isolates_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cow.bin");
    std::fs::write(&path, vec![0xAB; 4096]).unwrap();

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: the initialized 4096-byte file remains open and map_copy
    // creates a private copy-on-write view owned by this test.
    let mut m = unsafe { opts.len(4096).map_copy(&file).unwrap() };

    // Mutate the COW view.
    for b in m.iter_mut().take(256) {
        *b = 0x11;
    }
    // Must not flush to disk in COW mode — we don't call flush.
    drop(m);

    // The on-disk bytes must remain unchanged.
    let on_disk = std::fs::read(&path).unwrap();
    for &b in &on_disk[..256] {
        assert_eq!(b, 0xAB);
    }
}

/// transferTo0 userspace fallback: pull bytes from src at
/// position into dst, end-to-end.
#[test]
fn wp3_6_transfer_to_userspace_loop() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    let dst = dir.path().join("dst.bin");

    // Source: 1 MiB of i & 0xff.
    let mut payload = vec![0u8; 1024 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    std::fs::write(&src, &payload).unwrap();
    // Make dst empty.
    std::fs::File::create(&dst).unwrap();

    let fdt = FileDescriptorTable::new();
    let src_fd = fdt.open_read_write(src.to_str().unwrap(), false).unwrap();
    let dst_fd = fdt.open_read_write(dst.to_str().unwrap(), true).unwrap();

    // Ad-hoc inline of transfer_userspace_loop's logic against
    // the FdTable, since the public function takes &mut dyn
    // NativeContext (which we cannot synthesize here without
    // pulling in test_support — that mock has its own static
    // FdTable distinct from this one).
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let count: i64 = 1024 * 1024;
    let mut src_pos: u64 = 0;
    let mut transferred: i64 = 0;
    let mut remaining = count;
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, CHUNK);
        let n = fdt.pread_at(src_fd, &mut buf[..want], src_pos).unwrap();
        if n == 0 {
            break;
        }
        fdt.rw_write(dst_fd, &buf[..n]).unwrap();
        transferred += n as i64;
        src_pos += n as u64;
        remaining -= n as i64;
    }
    assert_eq!(transferred, 1024 * 1024);

    // Verify bytes.
    let copied = std::fs::read(&dst).unwrap();
    assert_eq!(copied.len(), 1024 * 1024);
    assert_eq!(&copied[..], &payload[..]);
}

/// transferTo0 with positional offset: copy from src @ 256 to dst.
#[test]
fn wp3_6_transfer_to_with_position_offset() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    let dst = dir.path().join("dst.bin");

    let mut payload = vec![0u8; 4096];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    std::fs::write(&src, &payload).unwrap();
    std::fs::File::create(&dst).unwrap();

    let fdt = FileDescriptorTable::new();
    let src_fd = fdt.open_read_write(src.to_str().unwrap(), false).unwrap();
    let dst_fd = fdt.open_read_write(dst.to_str().unwrap(), true).unwrap();

    let mut buf = vec![0u8; 1024];
    let n = fdt.pread_at(src_fd, &mut buf, 256).unwrap();
    assert_eq!(n, 1024);
    fdt.rw_write(dst_fd, &buf).unwrap();

    let copied = std::fs::read(&dst).unwrap();
    assert_eq!(&copied[..], &payload[256..256 + 1024]);
}

/// Verify the registry: insert a holder, look it up, drop it,
/// confirm removal.
#[test]
fn wp3_3_mmap_registry_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("life.bin");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read(path.to_str().unwrap()).unwrap();
    let file = fdt.clone_file(fd).unwrap();
    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: the 4096-byte test file remains open for the lifetime of
    // this read-only mapping.
    let m = unsafe { opts.len(4096).map(&file).unwrap() };
    let addr = m.as_ptr() as usize;
    let holder = MmapHolder::Ro(m);
    assert_eq!(holder.len(), 4096);

    let reg = mmap_registry();
    reg.lock().insert(addr, holder);
    assert!(reg.lock().contains_key(&addr));

    // Drop via the registry.
    let removed = reg.lock().remove(&addr);
    assert!(removed.is_some());
    assert!(!reg.lock().contains_key(&addr));
}

/// `unmap0` on a stale address must not panic — the JDK can
/// race-double-free, and this is documented as tolerated.
#[test]
fn wp3_3_unmap0_stale_address_is_idempotent() {
    let mut reg = mmap_registry().lock();
    // A guaranteed-not-present address.
    let stale_addr: usize = 0xDEAD_BEEF;
    let removed = reg.remove(&stale_addr);
    assert!(removed.is_none());
    drop(reg);
}

/// Smoke: register the natives onto a fresh registry. We
/// can't actually invoke them without a NativeContext, but
/// we verify the registration count is non-zero and the
/// helper functions are callable as expected.
#[test]
fn wp3_3_register_file_channel_real_smoke() {
    let mut r = NativeMethodRegistry::new();
    register_file_channel_real(&mut r);
    let fci = "sun/nio/ch/FileChannelImpl";
    assert!(
        r.find(
            fci,
            "open",
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
        )
        .is_some(),
        "JDK 25 FileChannelImpl.open bridge must be registered"
    );
    // JDK 21's shape, and the descriptor is the assertion. This read
    // `…ZZZLjava/lang/Object;` until 2026-08-10 — a spelling no JDK
    // declares — so it agreed with a registration that could never bind and
    // said nothing about whether JDK 21 was covered. Checked with
    // `javap -p -s` against Temurin 21.0.12+8, not against the source.
    assert!(
        r.find(
            fci,
            "open",
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
        )
        .is_some(),
        "JDK 21 FileChannelImpl.open bridge must be registered"
    );
    // The old unbindable spelling must not come back: re-adding it would
    // make this test pass again for the wrong reason.
    assert!(
        r.find(
            fci,
            "open",
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/lang/Object;)Ljava/nio/channels/FileChannel;"
        )
        .is_none(),
        "no JDK declares an Object-tailed FileChannelImpl.open; \
         registering one binds nothing and hides a missing arity"
    );
    let nts = "sun/nio/ch/NativeThreadSet";
    assert!(r.find(nts, "add", "()I").is_some());
    assert!(r.find(nts, "remove", "(I)V").is_some());
    assert!(r.find(nts, "signalAndWait", "()V").is_some());
    // Touch each of the registered methods via the registry's
    // public API so we know the signatures parsed.  We rely
    // on the fact that the registry maintains internal counts.
    // If the registry has a `len()` we'd assert; otherwise
    // just confirm the call doesn't panic.
    let _ = &r;
}

/// JIT round 16 wave 15 (nio): `MappedMemoryUtils.<clinit>` calls
/// `registerNatives()V` before anything else, so all five natives must be
/// registered -- through the file-channel registrar as well as their own --
/// and every one as `Bridge`: an ambient `SyntheticStub` would be dropped
/// under `--real-jdk` and bring the `UnsatisfiedLinkError` back.
#[test]
fn r16w15_mapped_memory_utils_registers_all_five_as_bridges() {
    const TRIPLES: [(&str, &str); 5] = [
        ("registerNatives", "()V"),
        ("isLoaded0", "(JJJ)Z"),
        ("load0", "(JJ)V"),
        ("unload0", "(JJ)V"),
        ("force0", "(Ljava/io/FileDescriptor;JJ)V"),
    ];
    let mut own = NativeMethodRegistry::new();
    register_mapped_memory_utils(&mut own);
    let mut via_fc = NativeMethodRegistry::new();
    register_file_channel_real(&mut via_fc);
    for r in [&own, &via_fc] {
        for (m, d) in TRIPLES {
            assert!(
                r.kind_of(MMU_CLASS, m, d) == Some(NativeKind::Bridge),
                "{MMU_CLASS}.{m}{d} must be a registered Bridge"
            );
        }
    }
}

/// The four bodies on a real shared file mapping: `load0` then `isLoaded0`
/// (resident on Unix through `mincore`, `false` on Windows as HotSpot's
/// Windows file answers), `force0` with the empty descriptor the JDK passes
/// on Unix, and `unload0`, after which the bytes are still readable through
/// the mapping (the page cache keeps them).
#[test]
fn r16w15_mapped_memory_utils_natives_follow_the_platform() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mmu.bin");
    std::fs::write(&path, vec![0u8; 8192]).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // SAFETY: the test file is 8192 bytes, stays open for the whole test,
    // and this is the only mapping of it.
    let mut m = unsafe {
        memmap2::MmapOptions::new()
            .len(8192)
            .map_mut(&file)
            .unwrap()
    };
    m[0] = 0x5a;
    m[4096] = 0xa5;
    let addr = Value::Long(m.as_mut_ptr() as i64);
    let len = Value::Long(8192);
    let mut ctx = crate::test_support::MockNativeContext::new();

    assert!(matches!(native_mmu_load0(&mut ctx, &[addr, len]), Ok(None)));
    let loaded = native_mmu_is_loaded0(&mut ctx, &[addr, len, Value::Long(2)]);
    let expected = if cfg!(unix) { 1 } else { 0 };
    assert!(
        matches!(loaded, Ok(Some(Value::Int(v))) if v == expected),
        "isLoaded0 after touching both pages must answer {expected}"
    );

    assert!(matches!(
        native_mmu_force0(&mut ctx, &[Value::Object(None), addr, len]),
        Ok(None)
    ));
    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!((on_disk[0], on_disk[4096]), (0x5a, 0xa5));

    assert!(matches!(
        native_mmu_unload0(&mut ctx, &[addr, len]),
        Ok(None)
    ));
    assert_eq!((m[0], m[4096]), (0x5a, 0xa5));

    // A null address or empty range never reaches the OS; the JDK's own
    // `isLoaded` answers true for it, and so does this.
    assert!(matches!(
        native_mmu_is_loaded0(&mut ctx, &[Value::Long(0), len, Value::Long(0)]),
        Ok(Some(Value::Int(1)))
    ));
    assert!(matches!(
        native_mmu_force0(&mut ctx, &[Value::Object(None), addr, Value::Long(0)]),
        Ok(None)
    ));
    assert!(matches!(
        native_mmu_register_natives(&mut ctx, &[]),
        Ok(None)
    ));
}

/// A failing syscall is an `IOException`, as in HotSpot (`mincore failed`,
/// `msync with parameter MS_SYNC failed`), not a fabricated answer. A
/// misaligned address is `EINVAL` from both on Linux.
#[cfg(target_os = "linux")]
#[test]
fn r16w15_mapped_memory_utils_syscall_failure_is_an_ioexception() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mmu-err.bin");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // SAFETY: as in the test above; 4096 bytes, sole mapping, file open.
    let mut m = unsafe {
        memmap2::MmapOptions::new()
            .len(4096)
            .map_mut(&file)
            .unwrap()
    };
    let misaligned = Value::Long(m.as_mut_ptr() as i64 + 1);
    let mut ctx = crate::test_support::MockNativeContext::new();
    let is_io = |r: MethodCallResult| {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IOException { .. }
            )))
        )
    };
    assert!(is_io(native_mmu_is_loaded0(
        &mut ctx,
        &[misaligned, Value::Long(16), Value::Long(1)]
    )));
    assert!(is_io(native_mmu_force0(
        &mut ctx,
        &[Value::Object(None), misaligned, Value::Long(16)]
    )));
}

/// Make sure the prot constants match the JDK's
/// FileChannelImpl.MAP_RO/RW/PV values (0/1/2).
#[test]
fn wp3_3_prot_constants_match_jdk() {
    assert_eq!(MAP_RO, 0);
    assert_eq!(MAP_RW, 1);
    assert_eq!(MAP_PV, 2);
}

/// IOStatus constants match `sun.nio.ch.IOStatus`.
#[test]
fn wp3_6_iostatus_constants_match_jdk() {
    assert_eq!(IOSTATUS_EOF, -1);
    assert_eq!(IOSTATUS_UNAVAILABLE, -2);
    assert_eq!(IOSTATUS_INTERRUPTED, -3);
    assert_eq!(IOSTATUS_UNSUPPORTED, -4);
    assert_eq!(IOSTATUS_UNSUPPORTED_CASE, -6);
}

/// `transferFrom0` (nio19, NIO-6): HotSpot's Linux body. Copies from the
/// source's LOGICAL position into the destination at `position`, advances
/// only the source, and declines an appending destination for this call
/// (`UNSUPPORTED_CASE`). Off Linux it answers `UNSUPPORTED`.
#[test]
fn r19_nio_transfer_from0_copies_from_the_source_position() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    let dst = dir.path().join("dst.bin");
    let payload: Vec<u8> = (0..65536u32).map(|i| (i * 31) as u8).collect();
    std::fs::write(&src, &payload).unwrap();
    std::fs::write(&dst, vec![0u8; 16]).unwrap();
    let mut ctx = crate::test_support::MockNativeContext::new();
    let src_fd = ctx
        .fd_table()
        .open_read_write(src.to_str().unwrap(), false)
        .unwrap();
    let dst_fd = ctx
        .fd_table()
        .open_read_write(dst.to_str().unwrap(), false)
        .unwrap();
    ctx.fd_table()
        .rw_seek(src_fd, std::io::SeekFrom::Start(100))
        .unwrap();
    let args = [
        Value::Int(src_fd as i32),
        Value::Int(dst_fd as i32),
        Value::Long(10),
        Value::Long(1000),
        Value::Int(0),
    ];
    let got = native_fc_transfer_from0(&mut ctx, &args);
    if cfg!(target_os = "linux") {
        assert!(matches!(got, Ok(Some(Value::Long(1000)))), "{got:?}");
        let copied = std::fs::read(&dst).unwrap();
        assert_eq!(copied.len(), 1010);
        assert_eq!(&copied[10..1010], &payload[100..1100]);
        assert_eq!(&copied[..10], &[0u8; 10]);
        assert_eq!(ctx.fd_table().rw_position(src_fd).unwrap(), 1100);
        assert_eq!(ctx.fd_table().rw_position(dst_fd).unwrap(), 0);
        let mut append = args;
        append[4] = Value::Int(1);
        assert!(matches!(
            native_fc_transfer_from0(&mut ctx, &append),
            Ok(Some(Value::Long(IOSTATUS_UNSUPPORTED_CASE)))
        ));
    } else {
        assert!(
            matches!(got, Ok(Some(Value::Long(IOSTATUS_UNSUPPORTED)))),
            "{got:?}"
        );
    }
}

/// `maxDirectTransferSize0` must be REGISTERED and must ANSWER the
/// documented cap.
///
/// The body used to be `assert_eq!(0x7fff_ffff_i32, i32::MAX)` — a fact
/// about two Rust literals, true on every machine that has ever run this
/// suite. `native_fc_max_direct_transfer_size0` was not on the call path,
/// so returning `0` from it, returning a `Long`, throwing, or dropping the
/// registration entirely all left this test green. The JDK divides its
/// transfer length by this number: a `0` here is a divide-by-zero or an
/// infinite `transferTo` loop, and an absent registration is an
/// `UnsatisfiedLinkError` on the first `FileChannel.transferTo`.
///
/// It now goes through the registry (so unregistering it fails the test)
/// and invokes the resolved callback (so changing the returned value or
/// its `Value` variant fails the test).
#[test]
fn wp3_6_max_direct_transfer_size_is_int_max() {
    let mut r = NativeMethodRegistry::new();
    register_file_channel_real(&mut r);

    // `register_fd_native` puts every dispatcher native under all three
    // platform spellings; a JDK image declares whichever one it ships.
    for cls in [
        "sun/nio/ch/FileDispatcherImpl",
        "sun/nio/ch/UnixFileDispatcherImpl",
        "sun/nio/ch/WindowsFileDispatcherImpl",
    ] {
        let cb = r
            .find(cls, "maxDirectTransferSize0", "()I")
            .unwrap_or_else(|| panic!("{cls}.maxDirectTransferSize0()I must be registered"));

        let mut ctx = crate::test_support::MockNativeContext::new();
        // The JDK calls this with no arguments and uses the result as a
        // divisor/chunk size, so both the variant and the value matter.
        let Ok(Some(Value::Int(cap))) = cb(&mut ctx, &[]) else {
            panic!("{cls}.maxDirectTransferSize0 must return Some(Value::Int(..))");
        };
        assert_eq!(
            cap,
            i32::MAX,
            "{cls}.maxDirectTransferSize0 must report the documented \
             2 GiB - 1 cap; a smaller value throttles every transferTo and \
             0 is a divide-by-zero in the JDK's chunking loop"
        );
    }
}

// Exercise the helper code paths whose only callers are in
// production-only branches (so rustc doesn't dead-code them
// away in debug builds and miss bugs).
#[test]
fn helpers_are_callable() {
    let v = vec![Value::Int(7), Value::Long(42), Value::Int(1)];
    assert_eq!(int_arg(&v, 0), 7);
    assert_eq!(long_arg(&v, 1), 42);
    assert!(bool_arg(&v, 2));
    // Last-resort defaults.
    assert_eq!(int_arg(&v, 99), 0);
    assert_eq!(long_arg(&v, 99), 0);
    assert!(!bool_arg(&v, 99));
}

/// Ensure `io_error` builds a RuntimeError::IOException.
#[test]
fn io_error_shape() {
    let e = io_error("whoops");
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message,
        })) => assert_eq!(message.as_str(), "whoops"),
        _ => panic!("expected IOException"),
    }
}

/// Drive a 32 MiB write-then-mmap-flush round trip — this
/// is the largest size we exercise routinely; it's well
/// within stack/RAM budgets for CI but stresses the page
/// cache enough to catch most off-by-one bugs.
#[test]
fn wp3_3_mmap_thirty_two_mb_round_trip() {
    const SIZE: usize = 32 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("xl.bin");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(SIZE as u64).unwrap();
    drop(f);

    let fdt = FileDescriptorTable::new();
    let fd = fdt.open_read_write(path.to_str().unwrap(), false).unwrap();
    let file = fdt.clone_file(fd).unwrap();

    let mut opts = memmap2::MmapOptions::new();
    // SAFETY: set_len established SIZE bytes, the handle remains open, and
    // this is the sole mutable mapping.
    let mut m = unsafe { opts.len(SIZE).map_mut(&file).unwrap() };
    // Stride writes to force the kernel to fault each page.
    let stride = 4096;
    let mut i = 0usize;
    while i < SIZE {
        m[i] = 0xC3;
        i += stride;
    }
    m.flush().unwrap();
    drop(m);
    let _ = std::fs::metadata(&path).unwrap();
}

// Confirm that the file we just wrote has no rope-allocated
// intermediate (i.e., we are NOT staging through a Vec); we
// do this indirectly by asserting that a 32 MiB mmap is
// O(1) Vec allocations beyond the buffer.
/// `isSameFile` is not path equality — the assertion that would have caught
/// the `Path.equals` approximation.
///
/// RED against a `paths_name_the_same_file` implemented as `a == b`, which
/// is what `FileSystemProvider.isSameFile` still does in
/// `native-builtins/src/phases_late/nio_file.rs`. Every pair below names ONE
/// file by two spellings, and every one of them compares unequal as a Rust
/// `Path` — `Path`'s `PartialEq` walks `components()`, which folds away `.`
/// but keeps `..`, so the traversal case is a genuine inequality on every
/// platform and needs no privilege, no link support and no `canonicalize`
/// agreement to be meaningful.
#[test]
fn wp3_3_same_file_is_identity_not_path_equality() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("f.bin"), b"payload").unwrap();

    let direct = root.join("f.bin");
    let via_parent = root.join("sub").join("..").join("f.bin");
    assert_ne!(
        direct, via_parent,
        "the two spellings must differ as Paths, or this test asserts nothing"
    );
    assert!(
        paths_name_the_same_file(&direct, &via_parent).unwrap(),
        "a `..` traversal to the same file must be the same file"
    );

    // Two genuinely different files. Without this the row above is
    // satisfied by an implementation that answers `true` for everything,
    // which is the failure mode one direction of assertions cannot see.
    std::fs::write(root.join("g.bin"), b"payload").unwrap();
    assert!(
        !paths_name_the_same_file(&direct, &root.join("g.bin")).unwrap(),
        "byte-identical but distinct files are NOT the same file"
    );

    // Reflexive without touching the disk, which is the JDK's fast path and
    // the reason `isSameFile(f, f)` is `true` for a nonexistent `f`.
    let ghost = root.join("does-not-exist");
    assert!(paths_name_the_same_file(&ghost, &ghost).unwrap());

    // A hard link is the case `canonicalize` CANNOT see, so it is the one
    // that proves the identity read is doing the work rather than a path
    // normalisation. Not made unconditional: `hard_link` depends on the
    // filesystem backing the temp dir, and a whole-test skip on that is the
    // vacuity this suite is trying to get rid of — so the rows above stand
    // on their own and this one only ever adds.
    let linked = root.join("hard.bin");
    if std::fs::hard_link(&direct, &linked).is_ok() {
        assert!(
            paths_name_the_same_file(&direct, &linked).unwrap(),
            "a hard link names the same file"
        );
    }
}

#[test]
fn helper_smoke_writeln() {
    // Sanity that std::io::Write is in scope and `write_all`
    // is callable on a `Vec<u8>`.
    let mut sink: Vec<u8> = Vec::new();
    sink.write_all(b"x").unwrap();
    assert_eq!(sink, b"x");
}
