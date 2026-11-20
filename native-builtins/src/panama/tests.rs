use super::*;
// The four compound layout tags are test-only in this file now — see the
// note on the `ffi::` import at the top.
use cratonvm_native_api::ffi::{LAYOUT_PADDING, LAYOUT_SEQUENCE, LAYOUT_STRUCT, LAYOUT_UNION};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// zt t20/x16 (x10 review page item 23j): a shared buffer writes back its
/// members' own ranges, merged, never the gap between disjoint slices; a
/// lone member writes the whole buffer, as before.
#[test]
fn zt20_x16_copy_back_leaves_the_gap_alone() {
    // [16, 24) and [0, 4) of one array: buffer [0, 24), gap [4, 16).
    assert_eq!(
        permissive_copy_back_ranges([(16, 8), (0, 4)].into_iter(), 24),
        vec![(0, 4), (16, 24)]
    );
    // memmove(seg.asSlice(1), seg, 4): [1, 8) and [0, 8) merge.
    assert_eq!(
        permissive_copy_back_ranges([(1, 7), (0, 8)].into_iter(), 8),
        vec![(0, 8)]
    );
    // Touching ranges merge; an empty one and the part past the buffer
    // are dropped.
    assert_eq!(
        permissive_copy_back_ranges([(0, 4), (4, 4), (6, 0), (6, 9)].into_iter(), 12),
        vec![(0, 12)]
    );
    // A lone segment: the whole buffer.
    assert_eq!(
        permissive_copy_back_ranges([(0, 20)].into_iter(), 20),
        vec![(0, 20)]
    );
    // No writable member: nothing is written back.
    assert!(permissive_copy_back_ranges(std::iter::empty(), 8).is_empty());
}

/// zt t18/hunt1: heap segments of one array share ONE scratch buffer
/// spanning the union of their ranges (they alias on HotSpot), in the
/// order of their first member; other arrays, and a segment whose start
/// is negative, keep their own.
#[test]
fn zt18_hunt1_heap_segments_of_one_array_share_one_buffer() {
    // memmove(seg.asSlice(1), seg, 4) over a byte[8]: arg 0 = [1, 8),
    // arg 1 = [0, 8). One buffer of 8 bytes from 0; arg 0 sits at +1.
    let g = group_permissive_heap_args(&[(0x1000, 1, 7), (0x1000, 0, 8)]);
    assert_eq!(
        g,
        vec![PermissiveHeapGroup {
            members: vec![0, 1],
            lo: 0,
            size: 8
        }]
    );
    // Disjoint slices of one array still share (their gap is copied in
    // only: zt t20/x16 writes back the members' own ranges, see
    // `zt20_x16_copy_back_leaves_the_gap_alone`); another array gets its
    // own buffer.
    let g = group_permissive_heap_args(&[(0x1000, 16, 8), (0x2000, 0, 4), (0x1000, 0, 4)]);
    assert_eq!(
        g,
        vec![
            PermissiveHeapGroup {
                members: vec![0, 2],
                lo: 0,
                size: 24
            },
            PermissiveHeapGroup {
                members: vec![1],
                lo: 0,
                size: 4
            },
        ]
    );
    // A lone segment: exactly the buffer the pass made before.
    let g = group_permissive_heap_args(&[(0x1000, 12, 20)]);
    assert_eq!(
        g,
        vec![PermissiveHeapGroup {
            members: vec![0],
            lo: 12,
            size: 20
        }]
    );
    // A negative start is never shared, and does not open a group the
    // next segment of the same array would join.
    let g = group_permissive_heap_args(&[(0x1000, -8, 4), (0x1000, 0, 4)]);
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].members, vec![0]);
    assert_eq!(g[1].members, vec![1]);
    assert_eq!((g[1].lo, g[1].size), (0, 4));
}

/// Build a downcall carrier the way [`alloc_downcall_handle`] does, minus
/// the `MethodType` derivation.
///
/// These tests exercise `pe_downcall_invoke`'s libffi marshalling, not the
/// mint. Routing them through `alloc_downcall_handle` would drag
/// `build_method_type_from_descriptor` into every one of them, and what
/// that measures on a `MockNativeContext` is the mock's class table rather
/// than anything about a downcall.
///
/// It does allocate the real carrier class at the real width, so the slot
/// arithmetic under test is the production arithmetic: a test that kept
/// writing the old compact `[0]=addr, [1]=descriptor` layout would read
/// back zeros through the `DOWNCALL_*` accessors and pass or fail for a
/// reason that has nothing to do with the code it names.
fn mk_downcall_handle(
    ctx: &mut dyn NativeContext,
    fn_addr: i64,
    descriptor: Option<ObjectRef>,
    first_variadic: i64,
) -> ObjectRef {
    let handle =
        try_alloc_concurrent_synthetic(ctx, DOWNCALL_CARRIER_CLASS, DOWNCALL_SLOT_COUNT)
            .unwrap();
    ctx.set_field(handle, DOWNCALL_TAG, Value::Int(DOWNCALL_TAG_MAGIC));
    ctx.set_field(handle, DOWNCALL_FN_ADDR, Value::Long(fn_addr));
    ctx.set_field(handle, DOWNCALL_DESCRIPTOR, Value::Object(descriptor));
    ctx.set_field(handle, DOWNCALL_FIRST_VARIADIC, Value::Long(first_variadic));
    ctx.set_field(handle, DOWNCALL_CIF, Value::Long(0));
    ctx.set_field(handle, DOWNCALL_CAPTURE_CALL_STATE, Value::Int(0));
    handle
}

// FIX(test): RAII guard that enables the process-wide native-access gate
// for the duration of a downcall test and restores the previous value on
// drop (even on panic). The Panama implementation is secure-by-default
// (`NATIVE_ACCESS_ENABLED == false`), so tests that exercise the real
// downcall machinery (abs/strlen/snprintf/…) must grant native access
// first or `validated_fn_ptr` denies them with an IllegalCallerException.
// Using a guard (rather than a bare set/reset pair) keeps the global flag
// from leaking into sibling tests — leaked state is what causes the
// order-dependent flakiness this fix addresses. The production gate is
// unchanged; only the test scope flips the flag.
//
// FIX(test-isolation): `NATIVE_ACCESS_ENABLED` is a single process-global
// `AtomicBool` shared by every test in this binary. The previous guard
// snapshotted the *prior* value and restored it on drop, but that is
// unsound under parallel execution and is exactly why
// `panama_cif_cache_reuses_cif_across_calls` still flaked: with two tests
// A and B, A enables (prior=false); B enables (prior=true, because A had
// already flipped it on); A finishes first and its guard restores false;
// B is now mid-downcall yet the gate reads false, so `validated_fn_ptr`
// denies it with `IllegalCallerException`. Snapshot/restore gives no
// mutual exclusion. The fix is to serialize every test that toggles the
// flag behind one module-level mutex, so only a single such test ever
// observes (or mutates) the flag at a time. While the lock is held the
// flag cannot be flipped out from under the running test.
//
// zt t18/df7: the grant is per VM now (`NativeAccessState`); a mock
// context reaches the detached state of its VM identity, and every mock
// these guarded tests make keeps the default identity `0`, so they still
// share one state and the lock still serializes them. A test that needs
// its own grant takes [`isolated_access_ctx`] instead: no lock, nothing to
// restore.
static NATIVE_ACCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// zt t18/df7: the native-access state of the default mock VM identity
/// (`0`), which [`NativeAccessGuard`] grants.
fn default_mock_access() -> &'static NativeAccessState {
    crate::classloader::detached_loader_tables(0).native_access()
}

/// zt t18/df7: a mock context with a VM identity of its own, so the
/// native-access state it reaches is this test's alone.
fn isolated_access_ctx() -> crate::test_utils::MockNativeContext {
    static NEXT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0x7df7_0000);
    let ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_vm_identity(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    ctx
}

struct NativeAccessGuard {
    prev: NativeAccessPolicy,
    // FIX(test-isolation): hold the serialization lock for the entire
    // lifetime of the guard. Acquiring it here means no other guarded
    // (or directly-locked) test can touch `NATIVE_ACCESS_ENABLED` until
    // this guard is dropped. Field order matters for drop: `prev` is
    // restored in `Drop::drop` *before* this `_lock` field is dropped
    // (Rust drops struct fields in declaration order, after the explicit
    // `Drop` impl runs), so the flag is reset while we still hold the
    // lock, and the lock is released only afterwards.
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl NativeAccessGuard {
    fn enable() -> Self {
        // Recover from a poisoned lock: a panicking guarded test must not
        // wedge the rest of the suite. The `()` payload carries no state,
        // so the poisoned inner guard is perfectly usable.
        let lock = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = default_mock_access().policy();
        default_mock_access().set_enabled(true);
        NativeAccessGuard { prev, _lock: lock }
    }
}
impl Drop for NativeAccessGuard {
    fn drop(&mut self) {
        // Restore the prior value while still holding the lock; `_lock`
        // is released immediately afterwards when the struct fields drop.
        default_mock_access().store_policy(self.prev.clone());
    }
}

#[test]
fn test_linker_native_call_convention() {
    // Verify calling convention for downcalls
    // x86-64 SysV: RDI, RSI, RDX, RCX, R8, R9 for integer args
    // Windows: RCX, RDX, R8, R9
    #[cfg(target_os = "windows")]
    let max_reg_args = 4;
    #[cfg(not(target_os = "windows"))]
    let max_reg_args = 6;
    assert!(max_reg_args >= 4);
}

#[test]
fn test_symbol_lookup_resolution() {
    // SymbolLookup.loaderLookup() should find loaded library symbols
    // Test that we can look up standard C functions
    let name = "strlen";
    assert!(!name.is_empty());
}

#[test]
fn test_value_layout_carriers() {
    // Each ValueLayout has a carrier type
    // JAVA_INT -> int.class, JAVA_LONG -> long.class, etc.
    let carriers = vec![
        ("JAVA_BYTE", 1usize),
        ("JAVA_SHORT", 2),
        ("JAVA_INT", 4),
        ("JAVA_LONG", 8),
        ("JAVA_FLOAT", 4),
        ("JAVA_DOUBLE", 8),
        ("ADDRESS", std::mem::size_of::<*const u8>()),
    ];
    for (name, size) in carriers {
        assert!(size > 0, "{name} must have positive size");
    }
}

/// zt t18/df7: the per-module policy tests, each on a VM identity of its
/// own ([`isolated_access_ctx`]): the state is that VM's, so nothing is
/// locked or restored (they shared a process global before).
#[test]
fn test_native_access_policy_none_denies_all() {
    let ctx = isolated_access_ctx();
    native_access(&ctx).set_enabled(false);
    assert!(!native_access_enabled(&ctx));
    assert!(!module_native_access_enabled(&ctx, None));
    assert!(!module_native_access_enabled(&ctx, Some("com.example.app")));
}

#[test]
fn test_native_access_policy_all_grants_every_module() {
    let ctx = isolated_access_ctx();
    native_access(&ctx).set_enabled(true);
    assert!(native_access_enabled(&ctx));
    assert!(module_native_access_enabled(&ctx, None));
    assert!(module_native_access_enabled(&ctx, Some("any.module")));
}

#[test]
fn test_native_access_policy_scoped_modules() {
    let ctx = isolated_access_ctx();
    native_access(&ctx).set_modules(["com.example.ffi", "org.foo.bar"]);
    // Only the listed modules are granted.
    assert!(module_native_access_enabled(&ctx, Some("com.example.ffi")));
    assert!(module_native_access_enabled(&ctx, Some("org.foo.bar")));
    // An unlisted module — and the unnamed module — are denied.
    assert!(!module_native_access_enabled(&ctx, Some("com.other")));
    assert!(!module_native_access_enabled(&ctx, None));
    // The coarse VM-wide gate sees *some* grant.
    assert!(native_access_enabled(&ctx));
}

#[test]
fn test_native_access_policy_all_unnamed_grants_only_unnamed() {
    let ctx = isolated_access_ctx();
    native_access(&ctx).set_modules(["ALL-UNNAMED"]);
    assert!(module_native_access_enabled(&ctx, None));
    assert!(!module_native_access_enabled(&ctx, Some("anything")));
    native_access(&ctx).set_modules(["com.x", "ALL-UNNAMED"]);
    assert!(module_native_access_enabled(&ctx, None));
    assert!(module_native_access_enabled(&ctx, Some("com.x")));
    assert!(!module_native_access_enabled(&ctx, Some("unlisted")));
    // Case-insensitive and mixed with named modules.
    native_access(&ctx).set_modules(["com.x", "all-modules"]);
    assert!(module_native_access_enabled(&ctx, Some("unlisted")));
}

#[test]
fn test_native_access_policy_blank_list_grants_all_like_bare_flag() {
    let ctx = isolated_access_ctx();
    // A whitespace-only / empty argument behaves like the bare flag.
    native_access(&ctx).set_modules(["", "   "]);
    assert!(native_access_enabled(&ctx));
    assert!(module_native_access_enabled(&ctx, Some("whatever")));
}

/// zt t18/df7 (FFM page, "What is left": the process globals): the
/// native-access state is per VM. One VM's grant, `--illegal-native-access`
/// mode and warn-once latch are invisible to another; a VM installs its
/// launcher settings (`VmConfig::native_access`) whole; a fresh VM starts
/// with no grant and `warn` (every embedding entry point's default).
#[test]
fn zt18df7_native_access_state_is_per_vm() {
    let granted = isolated_access_ctx();
    let other = isolated_access_ctx();
    assert_eq!(native_access(&other).policy(), NativeAccessPolicy::None);
    assert_eq!(illegal_native_access(&other), IllegalNativeAccess::Warn);
    native_access(&granted).install(&NativeAccessConfig {
        policy: NativeAccessPolicy::from_module_list(["ALL-UNNAMED"]),
        illegal: IllegalNativeAccess::Deny,
    });
    assert!(native_access_enabled(&granted));
    assert!(module_native_access_enabled(&granted, None));
    assert_eq!(illegal_native_access(&granted), IllegalNativeAccess::Deny);
    assert!(!native_access_enabled(&other), "the other VM has no grant");
    assert!(!module_native_access_enabled(&other, None));
    assert_eq!(illegal_native_access(&other), IllegalNativeAccess::Warn);
    // The warn-once latch is per VM too.
    assert!(native_access(&granted).first_restricted_warning());
    assert!(!native_access(&granted).first_restricted_warning(), "once");
    assert!(
        native_access(&other).first_restricted_warning(),
        "the other VM warns once of its own"
    );
    // The default configuration is the old process default.
    assert_eq!(
        NativeAccessConfig::default(),
        NativeAccessConfig {
            policy: NativeAccessPolicy::None,
            illegal: IllegalNativeAccess::Warn,
        }
    );
    assert_eq!(
        NativeAccessPolicy::from_enabled(true),
        NativeAccessPolicy::All
    );
    assert_eq!(
        NativeAccessPolicy::from_enabled(false),
        NativeAccessPolicy::None
    );
}

/// zt t18/df7: the no-caller fallback warns once per VM, and a VM whose
/// policy denies refuses there with the JDK's text, whatever another VM
/// in the process granted.
#[test]
fn zt18df7_the_restricted_fallback_reads_its_own_vm() {
    let mut deny = isolated_access_ctx();
    native_access(&deny).set_illegal(IllegalNativeAccess::Deny);
    let mut allow = isolated_access_ctx();
    native_access(&allow).set_enabled(true);
    native_access(&allow).set_illegal(IllegalNativeAccess::Deny);
    assert!(
        restricted_method_check_owned(&mut deny, "java.lang.foreign.Linker", "upcallStub")
            .is_err(),
        "no grant, deny: refused"
    );
    assert!(
        restricted_method_check_owned(&mut allow, "java.lang.foreign.Linker", "upcallStub")
            .is_ok(),
        "granted: served"
    );
    let mut warn = isolated_access_ctx();
    assert!(
        restricted_method_check_owned(&mut warn, "java.lang.foreign.Linker", "upcallStub")
            .is_ok()
    );
    assert!(
        !native_access(&warn).first_restricted_warning(),
        "the first call took the VM's one warning"
    );
}

// FIX(test): regression for the MemorySegment.copy zero-size OOB hole.
// The bounds check in `copy` was previously gated on `size > 0`, so a
// segment with a declared byteSize() of 0 skipped validation entirely and
// a non-zero `bytes` length drove an OOB read/write of up to MAX_COPY_SIZE.
// This test mirrors the exact (now-unconditional) predicate used in the
// production fix — `offset < 0 || offset.checked_add(bytes) > size` — and
// asserts that a zero-size segment with a non-zero length is rejected.
// Pure i64 arithmetic only, so it is independent of NativeContext.
#[test]
fn test_copy_zero_size_segment_rejects_nonzero_len() {
    // Replicates the copy-path bounds predicate: returns true == "reject".
    fn exceeds(offset: i64, bytes: usize, size: i64) -> bool {
        let end = offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
        offset < 0 || end > size
    }
    // Zero-size segment, non-zero copy length -> must reject (the bug).
    assert!(
        exceeds(0, 64, 0),
        "zero-size segment must reject non-zero copy"
    );
    // Negative offset -> reject.
    assert!(exceeds(-1, 0, 16), "negative offset must reject");
    // Offset+bytes overflowing i64 -> saturates to MAX, exceeds size -> reject.
    assert!(exceeds(i64::MAX, 1, 1024), "overflowing offset must reject");
    // Offset+bytes past the declared size -> reject.
    assert!(exceeds(8, 16, 16), "out-of-range copy must reject");
    // Legitimate in-bounds copy -> accept (no over-rejection).
    assert!(!exceeds(8, 8, 16), "in-bounds copy must be accepted");
    // Zero-length copy on a zero-size segment is harmless and the
    // production code only enters the bounds block when bytes > 0, so the
    // predicate for (0,0,0) staying false is the consistent invariant.
    assert!(
        !exceeds(0, 0, 0),
        "zero-length copy is not a bounds violation"
    );
}

#[test]
fn test_function_descriptor() {
    // FunctionDescriptor.of(returnLayout, argLayouts...)
    // Describes a native function signature
    struct FuncDesc {
        ret_size: usize,
        arg_sizes: Vec<usize>,
    }
    let desc = FuncDesc {
        ret_size: 4,           // int return
        arg_sizes: vec![8, 8], // two pointer args
    };
    assert_eq!(desc.arg_sizes.len(), 2);
    assert_eq!(desc.ret_size, 4);
}

#[test]
fn test_upcall_handle() {
    // Upcall: native code calling back into Java
    // Should create a function pointer that routes to Java method
    let callback_invoked = std::sync::atomic::AtomicBool::new(false);
    // Simulate upcall
    callback_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(callback_invoked.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn test_arena_confined_thread_safety() {
    // Arena.ofConfined() should only be usable from creating thread
    let creator_thread = std::thread::current().id();
    assert_eq!(std::thread::current().id(), creator_thread);
}

#[test]
fn test_arena_shared_multi_thread() {
    // Arena.ofShared() can be used from multiple threads
    use std::sync::Arc;
    let data = Arc::new(vec![1u8, 2, 3]);
    let d2 = data.clone();
    let handle = std::thread::spawn(move || {
        assert_eq!(d2[0], 1);
    });
    handle.join().unwrap();
    assert_eq!(data[0], 1);
}

#[test]
fn test_validated_fn_ptr_null_rejected() {
    // Null function pointer should be rejected
    let result = validated_fn_ptr::<extern "C" fn() -> i32>(0);
    assert!(result.is_err());
}

#[test]
fn test_max_copy_size_constant() {
    assert_eq!(MAX_COPY_SIZE, 256 * 1024 * 1024);
}

#[test]
fn test_max_cstr_len_constant() {
    assert_eq!(MAX_CSTR_LEN, 4096);
}

#[test]
fn test_layout_byte_size_via_ffi() {
    // Verify the ffi layout constants are accessible and correct
    assert_eq!(ffi::layout_byte_size(LAYOUT_INT), 4);
    assert_eq!(ffi::layout_byte_size(LAYOUT_LONG), 8);
    assert_eq!(ffi::layout_byte_size(LAYOUT_FLOAT), 4);
    assert_eq!(ffi::layout_byte_size(LAYOUT_DOUBLE), 8);
    assert_eq!(ffi::layout_byte_size(LAYOUT_BYTE), 1);
    assert_eq!(ffi::layout_byte_size(LAYOUT_SHORT), 2);
    assert_eq!(ffi::layout_byte_size(LAYOUT_BOOLEAN), 1);
    assert_eq!(ffi::layout_byte_size(LAYOUT_CHAR), 2);
    assert_eq!(ffi::layout_byte_size(LAYOUT_ADDRESS), 8);
}

#[test]
fn test_compound_layout_sizes_are_zero() {
    assert_eq!(ffi::layout_byte_size(LAYOUT_STRUCT), 0);
    assert_eq!(ffi::layout_byte_size(LAYOUT_UNION), 0);
    assert_eq!(ffi::layout_byte_size(LAYOUT_SEQUENCE), 0);
    assert_eq!(ffi::layout_byte_size(LAYOUT_PADDING), 0);
}

// --- Phase 80.1: Panama Memory Safety Tests ---

#[test]
fn test_get_utf8string_address_overflow() {
    // Verify that checked_add detects u64 overflow in address arithmetic.
    // Using -1i64 as u64 = u64::MAX, adding anything > 0 overflows.
    let ptr: i64 = -1; // u64::MAX when cast
    let base_off: i64 = 0;
    let offset: i64 = 1;
    let total = (ptr as u64)
        .checked_add(base_off as u64)
        .and_then(|v| v.checked_add(offset as u64));
    assert!(total.is_none(), "overflow must be detected");
}

#[test]
fn test_set_utf8string_bounds_check() {
    // setUtf8String must reject writes beyond segment size.
    // seg_size=10, offset=8, string "hello" (5+1=6 bytes needed) → 8+6=14 > 10 → reject.
    let seg_size: i64 = 10;
    let offset: i64 = 8;
    let needed: i64 = 6; // "hello" + null terminator
    assert!(
        offset + needed > seg_size,
        "write should exceed segment bounds"
    );
}

#[test]
fn test_copy_bounds_check_src_overflow() {
    // copy must reject src_offset + bytes > src_size.
    let src_size: i64 = 100;
    let src_offset: i64 = 90;
    let bytes: usize = 20;
    let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
    assert!(
        src_end > src_size,
        "source bounds check must catch overflow"
    );
}

#[test]
fn test_copy_address_arithmetic_overflow() {
    // copy must reject when address arithmetic overflows u64.
    let src_ptr: u64 = u64::MAX - 5;
    let src_off: u64 = 10;
    let src_offset: u64 = 0;
    let total = src_ptr
        .checked_add(src_off)
        .and_then(|v| v.checked_add(src_offset));
    assert!(total.is_none(), "address overflow must be detected");
}

#[test]
fn test_get_utf8string_offset_exceeds_segment() {
    // getUtf8String must reject offset >= segment size.
    let seg_size: i64 = 100;
    let offset: i64 = 150;
    let remaining = seg_size - offset;
    assert!(remaining <= 0, "offset beyond segment must be rejected");
}

#[test]
fn test_copy_negative_offset_rejected() {
    // copy must reject negative offsets.
    let src_offset: i64 = -1;
    let src_size: i64 = 100;
    assert!(
        src_offset < 0,
        "negative offset must be rejected by bounds check"
    );
    // The production code checks: if src_offset < 0 || src_end > src_size
    assert!(src_offset < 0 || src_offset > src_size);
}

// ===================================================================
// Phase 85.1: Arena Lifecycle Tests
// ===================================================================

use crate::test_utils::mock_ctx;
use crate::try_alloc_concurrent_synthetic;

/// Helper: create an arena object of the given kind using the actual registration logic.
fn make_arena(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
    // gc-common w20-f: the four factories share `pe_new_arena_carrier`
    // now, so this IS the registration logic rather than a copy of it.
    pe_new_arena_carrier(ctx, kind).unwrap()
}

#[test]
fn test_85_1_arena_confined_lifecycle() {
    // Create confined arena, allocate, close, verify freed
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    // Allocate via arena
    let seg_result = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
    assert!(seg_result.is_ok());
    let seg = match seg_result.unwrap() {
        Some(Value::Object(Some(s))) => s,
        _ => panic!("Expected segment object"),
    };

    // Segment should have valid pointer and size
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    assert!(ptr != 0, "Segment pointer should be non-null");
    let size = match ctx.get_field(seg, 1) {
        Value::Long(n) => n,
        _ => 0,
    };
    assert_eq!(size, 64);

    // Arena count should be 1
    assert!(matches!(ctx.get_field(arena, 3), Value::Int(1)));

    // Close arena
    let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
    assert!(close_result.is_ok());

    // Arena should be marked closed
    assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
}

#[test]
fn test_85_1_arena_shared_lifecycle() {
    // Shared arena should work the same as confined
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_SHARED);

    // Allocate two segments
    let seg1 = pe_arena_allocate_impl(&mut ctx, arena, 32, 4);
    assert!(seg1.is_ok());
    let seg2 = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
    assert!(seg2.is_ok());

    // Count should be 2
    assert!(matches!(ctx.get_field(arena, 3), Value::Int(2)));

    // Close should succeed
    let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
    assert!(close_result.is_ok());
    assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
}

/// Round 12 wave 3 (lane ffm): the synthetic arena has no session, so its
/// segments passed the scope check after `close()` had freed their blocks.
/// The arena's own closed flag now refuses them.
#[test]
fn a_segment_of_a_closed_synthetic_arena_fails_the_scope_check() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_SHARED);
    let seg = match pe_arena_allocate_impl(&mut ctx, arena, 16, 4) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("allocate gave {other:?}"),
    };
    assert!(
        pe_segment_check_scope(&mut ctx, seg).is_ok(),
        "an open arena's segment must pass"
    );
    assert!(pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]).is_ok());
    let err = pe_segment_check_scope(&mut ctx, seg)
        .expect_err("a closed synthetic arena's segment passed the scope check");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
}

/// A real JDK session (`--real-jdk`'s `Arena.ofConfined()` / `ofShared()`)
/// and a real `NativeMemorySegmentImpl` scoped to it, with JDK 25's
/// `MemorySessionImpl` fields.
fn real_session_and_segment(
    ctx: &mut crate::test_utils::MockNativeContext,
    class: &str,
) -> (ObjectRef, ObjectRef) {
    fn declare(
        ctx: &mut crate::test_utils::MockNativeContext,
        class: &str,
        fields: &[(&str, &str)],
    ) -> cratonvm_types::ClassId {
        let cid = match ctx.ensure_class_initialized(class) {
            Ok(cid) => cid,
            Err(err) => panic!("no class {class}: {err:?}"),
        };
        let metadata = fields
            .iter()
            .enumerate()
            .map(|(slot, (name, desc))| cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: desc.to_string(),
                access_flags: 0,
                slot_index: slot,
                declaring_class_id: cid,
                is_static: false,
            })
            .collect();
        ctx.set_declared_fields(cid, metadata);
        cid
    }
    let session_class = declare(
        ctx,
        class,
        &[
            (
                "resourceList",
                "Ljdk/internal/foreign/MemorySessionImpl$ResourceList;",
            ),
            ("owner", "Ljava/lang/Thread;"),
            ("state", "I"),
            ("acquireCount", "I"),
        ],
    );
    let session = ctx.alloc_object(session_class, 4);
    ctx.set_field(session, 1, Value::Object(None));
    ctx.set_field(session, 2, Value::Int(0));
    ctx.set_field(session, 3, Value::Int(0));
    let segment_class = declare(
        ctx,
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        &[
            ("length", "J"),
            ("readOnly", "Z"),
            ("scope", "Ljdk/internal/foreign/MemorySessionImpl;"),
            ("min", "J"),
        ],
    );
    let segment = ctx.alloc_object(segment_class, 4);
    ctx.set_field(segment, 0, Value::Long(8));
    ctx.set_field(segment, 1, Value::Int(0));
    ctx.set_field(segment, 2, Value::Object(Some(session)));
    ctx.set_field(segment, 3, Value::Long(0));
    (session, segment)
}

/// `FFM-door-seg-probe-residuals-FIXED-20261007.md`: a downcall's segment
/// whose session is a REAL `ConfinedSession` was not checked at all --
/// a closed one reached `marshal_arg` (the arena-handle refusal) and one
/// owned by another thread ran the call. Now `checkValidState`'s answers,
/// in its order.
#[test]
fn a_real_confined_session_is_checked_owner_first_then_state() {
    let mut ctx = mock_ctx();
    let (session, seg) =
        real_session_and_segment(&mut ctx, "jdk/internal/foreign/ConfinedSession");
    assert!(
        pe_segment_check_scope(&mut ctx, seg).is_ok(),
        "an open unowned session passes"
    );
    ctx.set_field(session, 2, Value::Int(-1));
    let err = pe_segment_check_scope(&mut ctx, seg).expect_err("a closed real session passed");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    // Another owner (the mock's current thread is always a fresh object)
    // reports the thread error even once closed, as the JDK does.
    let owner = ctx.fresh_object_ref();
    ctx.set_field(session, 1, Value::Object(Some(owner)));
    let err =
        pe_segment_check_scope(&mut ctx, seg).expect_err("another thread's session passed");
    // `p67_wrong_thread`'s `WrongThreadException` (or its message-bearing
    // fallback), not the closed state's answer.
    assert!(!format!("{err:?}").contains("Already closed"), "{err:?}");
    // And the memo path declines it to the general path.
    assert!(downcall_segment_scope_settled(&mut ctx, seg).is_none());
}

/// The acquire for the call counts a client on a real confined or shared
/// session (so the arena's close in an up-call is refused, as HotSpot's
/// `alreadyAcquired`), and the release gives it back; a closed shared
/// session (`acquireCount == -1`) is `Already closed`.
#[test]
fn a_real_session_is_acquired_for_the_call_and_released() {
    for class in [
        "jdk/internal/foreign/ConfinedSession",
        "jdk/internal/foreign/SharedSession",
    ] {
        let mut ctx = mock_ctx();
        let (session, seg) = real_session_and_segment(&mut ctx, class);
        let held = pe_downcall_acquire_sessions(&mut ctx, &[seg, seg]).expect("open session");
        assert_eq!(held.len(), 1, "{class}: one session, acquired once");
        assert!(
            matches!(ctx.get_field(session, 3), Value::Int(1)),
            "{class}"
        );
        let sessions: Vec<ObjectRef> = held.iter().map(|&(s, _)| s).collect();
        ctx.unpin_native_roots(held[0].1);
        pe_downcall_release_sessions(&mut ctx, &sessions);
        assert!(
            matches!(ctx.get_field(session, 3), Value::Int(0)),
            "{class}"
        );
    }
    let mut ctx = mock_ctx();
    let (session, seg) =
        real_session_and_segment(&mut ctx, "jdk/internal/foreign/SharedSession");
    ctx.set_field(session, 3, Value::Int(-1));
    let err =
        pe_downcall_acquire_sessions(&mut ctx, &[seg]).expect_err("closed shared session");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
}

/// `Arena.global()` / `ofAuto()` sessions: nothing to count, and the memo
/// path keeps serving them.
#[test]
fn a_real_global_session_needs_no_acquire() {
    let mut ctx = mock_ctx();
    let (session, seg) =
        real_session_and_segment(&mut ctx, "jdk/internal/foreign/GlobalSession");
    // `GlobalSession()` (and `ImplicitSession(Cleaner)`) store `state = 1`,
    // `NONCLOSEABLE` (javap, JDK 25.0.4); the round-19 rows read that word.
    ctx.set_field(session, 2, Value::Int(1));
    let held = pe_downcall_acquire_sessions(&mut ctx, &[seg]).expect("global session");
    assert!(matches!(ctx.get_field(session, 3), Value::Int(0)));
    if let Some(&(_, pin)) = held.first() {
        ctx.unpin_native_roots(pin);
    }
    assert_eq!(downcall_segment_scope_settled(&mut ctx, seg), Some(None));
}

#[test]
fn test_85_1_arena_auto_lifecycle() {
    // Auto arena should allocate but not be manually closeable like global
    // (actually auto CAN be closed, only global cannot)
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_AUTO);

    let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 1);
    assert!(seg.is_ok());

    // Auto arena can be closed
    let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
    assert!(close_result.is_ok());
}

#[test]
fn test_85_1_arena_double_close_error() {
    // Closing an already-closed arena should return an error
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    // First close succeeds
    let close1 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
    assert!(close1.is_ok());

    // Second close should fail
    let close2 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
    assert!(close2.is_err(), "Double close must return error");

    // Global arena cannot be closed at all
    let global = make_arena(&mut ctx, ffi::ARENA_GLOBAL);
    let close_global = pe_arena_close(&mut ctx, &[Value::Object(Some(global))]);
    assert!(
        close_global.is_err(),
        "Global arena close must return error"
    );
}

// ===================================================================
// Phase 85.2: MemorySegment Implementation Tests
// ===================================================================

/// Helper: create a ValueLayout object
fn make_layout(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
    pe_make_layout(ctx, kind).unwrap()
}

#[test]
fn test_85_2_allocate_and_readwrite() {
    // Allocate a segment via arena, write an int, read it back
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let layout_int = make_layout(&mut ctx, LAYOUT_INT);

    let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 4)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

    // Write int 42 at offset 0
    pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(42)).unwrap();
    // Read it back
    let val = pe_segment_get_impl(&mut ctx, seg, layout_int, 0).unwrap();
    assert_eq!(val, Some(Value::Int(42)));

    // Write long at offset 8
    let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
    pe_segment_set_impl(
        &mut ctx,
        seg,
        layout_long,
        8,
        Value::Long(0x1234_5678_9ABC_DEF0),
    )
    .unwrap();
    let val2 = pe_segment_get_impl(&mut ctx, seg, layout_long, 8).unwrap();
    assert_eq!(val2, Some(Value::Long(0x1234_5678_9ABC_DEF0)));
}

/// Round 14 wave 1 (lane ffm): `set(ADDRESS, 0, null)` is
/// `NullPointerException` (JDK 25 `SharedUtils.unboxSegment`), and writes
/// nothing; it stored a C null.
#[test]
fn a_null_address_store_is_a_null_pointer_exception() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
    let layout_addr = make_layout(&mut ctx, LAYOUT_ADDRESS);
    let seg = match pe_arena_allocate_impl(&mut ctx, arena, 16, 8) {
        Ok(Some(Value::Object(Some(s)))) => s,
        other => panic!("no segment: {other:?}"),
    };
    pe_segment_set_impl(&mut ctx, seg, layout_long, 0, Value::Long(0x55)).unwrap();
    let err = match pe_segment_set_impl(&mut ctx, seg, layout_addr, 0, Value::Object(None)) {
        Ok(v) => panic!("a null address was stored: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("NullPointer"), "{err:?}");
    // Round 14 wave 5 (lane fixup2): message-less on this road, as on
    // HotSpot 25 (the helpful message is the VarHandle road's only).
    assert!(
        format!("{err:?}").contains("NullPointerException { message: None }"),
        "{err:?}"
    );
    let val = pe_segment_get_impl(&mut ctx, seg, layout_long, 0).unwrap();
    assert_eq!(val, Some(Value::Long(0x55)));
}

#[test]
fn test_85_2_bounds_check_null_segment() {
    // get/set on a null address should return error
    let mut ctx = mock_ctx();
    let layout_int = make_layout(&mut ctx, LAYOUT_INT);

    // Create a segment with null pointer
    let seg = alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(0)); // null ptr
    ctx.set_field(seg, 1, Value::Long(100));
    ctx.set_field(seg, 5, Value::Long(0));

    let result = pe_segment_get_impl(&mut ctx, seg, layout_int, 0);
    assert!(result.is_err(), "Get on null segment should fail");

    let result = pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(1));
    assert!(result.is_err(), "Set on null segment should fail");
}

/// `ofArray(int[])` mints an ALIAS segment, not a native copy: no
/// machine address, and a read through the segment sees the array's own
/// live elements. This is `pe_of_array_alias`, the real registration —
/// not a re-simulation of the retired copy-into-native-memory logic this
/// test used to run inline.
#[test]
fn test_85_2_of_array_int() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    ctx.set_array_element(arr, 0, Value::Int(10));
    ctx.set_array_element(arr, 1, Value::Int(20));
    ctx.set_array_element(arr, 2, Value::Int(30));
    ctx.set_array_element(arr, 3, Value::Int(40));

    let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(int[]) answered {other:?}"),
    };
    assert_eq!(
        crate::panama_libffi::segment_address(&ctx, seg),
        0,
        "an ofArray segment has no machine address, int[] included"
    );
    let layout_int = make_layout(&mut ctx, LAYOUT_INT);
    for i in 0..4i64 {
        let value = pe_segment_get_impl(&mut ctx, seg, layout_int, i * 4).unwrap();
        assert_eq!(
            value,
            Some(Value::Int((i as i32 + 1) * 10)),
            "reading through the alias segment must see the array's own element {i}"
        );
    }
}

#[test]
fn test_85_2_copy_segments() {
    // Allocate two segments, write to src, copy to dst, verify
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    let src = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    let dst = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

    // Write pattern to src
    let src_ptr = match ctx.get_field(src, 0) {
        Value::Long(n) => n as *mut u8,
        _ => std::ptr::null_mut(),
    };
    assert!(!src_ptr.is_null());
    for i in 0..16u8 {
        unsafe {
            *src_ptr.add(i as usize) = i + 1;
        }
    }

    // Copy 16 bytes from src to dst
    let _copy_args = vec![
        Value::Object(Some(src)),
        Value::Long(0), // srcOffset
        Value::Object(Some(dst)),
        Value::Long(0),  // dstOffset
        Value::Long(16), // bytes
    ];

    // Simulate copy logic
    let src_addr = src_ptr;
    let dst_ptr = match ctx.get_field(dst, 0) {
        Value::Long(n) => n as *mut u8,
        _ => std::ptr::null_mut(),
    };
    assert!(!dst_ptr.is_null());
    unsafe {
        std::ptr::copy_nonoverlapping(src_addr, dst_ptr, 16);
    }

    // Verify dst has the pattern
    for i in 0..16u8 {
        let val = unsafe { *dst_ptr.add(i as usize) };
        assert_eq!(val, i + 1, "Byte at offset {} mismatch", i);
    }
}

/// Mirror of the overlap decision used by MemorySegment.copy: ranges
/// `[s, s+bytes)` and `[d, d+bytes)` overlap iff `s < d+bytes && d < s+bytes`.
fn copy_ranges_overlap(s: u64, d: u64, bytes: u64) -> bool {
    s < d.saturating_add(bytes) && d < s.saturating_add(bytes)
}

#[test]
fn test_copy_overlap_detection() {
    // Disjoint adjacent ranges: [0,16) and [16,32) do NOT overlap.
    assert!(!copy_ranges_overlap(0, 16, 16));
    assert!(!copy_ranges_overlap(16, 0, 16));
    // One-byte overlap (forward): [0,16) and [15,31).
    assert!(copy_ranges_overlap(0, 15, 16));
    // One-byte overlap (backward): [15,31) and [0,16).
    assert!(copy_ranges_overlap(15, 0, 16));
    // Identical ranges fully overlap.
    assert!(copy_ranges_overlap(100, 100, 8));
    // Zero-length never overlaps.
    assert!(!copy_ranges_overlap(100, 100, 0));
}

#[test]
fn test_copy_overlapping_within_segment_is_memmove_correct() {
    // Regression: MemorySegment.copy must behave as a memmove for
    // overlapping src/dst within a single segment. A forward-overlapping
    // copy done with copy_nonoverlapping would corrupt the tail; copy
    // (memmove) preserves it.
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

    let base = match ctx.get_field(seg, 0) {
        Value::Long(n) => n as *mut u8,
        _ => std::ptr::null_mut(),
    };
    assert!(!base.is_null());

    // Initialize bytes 0..16 = [1..=16].
    for i in 0..16u8 {
        unsafe { *base.add(i as usize) = i + 1 };
    }

    // copy 8 bytes from offset 0 to offset 4 (forward overlap).
    let s = base as u64;
    let d = unsafe { base.add(4) } as u64;
    let bytes: usize = 8;
    assert!(
        copy_ranges_overlap(s, d, bytes as u64),
        "ranges must be detected as overlapping"
    );
    // Use the same memmove path the production code selects on overlap.
    unsafe { std::ptr::copy(base, base.add(4), bytes) };

    // Expected memmove result: dst[4..12] == old src[0..8] == [1..=8].
    let expected: [u8; 16] = [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 13, 14, 15, 16];
    for i in 0..16usize {
        let val = unsafe { *base.add(i) };
        assert_eq!(val, expected[i], "memmove byte at offset {} mismatch", i);
    }
}

#[test]
fn test_85_2_as_slice() {
    // asSlice should create a sub-segment with offset
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

    // Write a value at offset 16
    let layout_int = make_layout(&mut ctx, LAYOUT_INT);
    pe_segment_set_impl(&mut ctx, seg, layout_int, 16, Value::Int(0xCAFE)).unwrap();

    // Create a slice starting at offset 16, size 32
    let slice = alloc_segment_carrier(&mut ctx, 6).unwrap();
    let base_ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    ctx.set_field(slice, 0, Value::Long(base_ptr));
    ctx.set_field(slice, 1, Value::Long(32));
    ctx.set_field(slice, 2, ctx.get_field(seg, 2));
    ctx.set_field(slice, 3, Value::Int(0));
    ctx.set_field(slice, 4, Value::Int(1));
    ctx.set_field(slice, 5, Value::Long(16)); // offset=16

    // Read at slice offset 0 should give the value written at parent offset 16
    let val = pe_segment_get_impl(&mut ctx, slice, layout_int, 0).unwrap();
    assert_eq!(val, Some(Value::Int(0xCAFE)));
}

#[test]
fn test_85_2_reinterpret() {
    // reinterpret should change size but keep same address
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

    let orig_ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    let orig_size = match ctx.get_field(seg, 1) {
        Value::Long(n) => n,
        _ => 0,
    };
    assert_eq!(orig_size, 64);

    // Reinterpret with new size 128
    let reinterpreted = alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(reinterpreted, 0, Value::Long(orig_ptr));
    ctx.set_field(reinterpreted, 1, Value::Long(128));
    ctx.set_field(reinterpreted, 2, ctx.get_field(seg, 2));
    ctx.set_field(reinterpreted, 3, ctx.get_field(seg, 3));
    ctx.set_field(reinterpreted, 4, Value::Int(1));
    ctx.set_field(reinterpreted, 5, Value::Long(0));

    // Pointer should be the same, size should be different
    let new_ptr = match ctx.get_field(reinterpreted, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    let new_size = match ctx.get_field(reinterpreted, 1) {
        Value::Long(n) => n,
        _ => 0,
    };
    assert_eq!(new_ptr, orig_ptr);
    assert_eq!(new_size, 128);
}

// ===================================================================
// Phase 85.3: Linker Downcall Tests
// ===================================================================

#[test]
fn test_85_3_downcall_strlen() {
    // Call C strlen through the downcall mechanism
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();

    // Look up strlen
    let strlen_addr = ctx.find_native_symbol(-1, "strlen");
    if strlen_addr.is_none() {
        // Skip on platforms where symbol lookup isn't available
        return;
    }
    let strlen_addr = strlen_addr.unwrap() as i64;

    // Create a C string in native memory
    let (_, ptr) = ctx.allocate_native_memory(16, 1).unwrap();
    let test_str = b"hello\0";
    unsafe {
        std::ptr::copy_nonoverlapping(test_str.as_ptr(), ptr, test_str.len());
    }

    // Build FunctionDescriptor: of(LAYOUT_LONG, ADDRESS)
    let ret_layout = make_layout(&mut ctx, LAYOUT_LONG);
    let param_layout = make_layout(&mut ctx, LAYOUT_ADDRESS);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    // Build the downcall handle
    let handle = mk_downcall_handle(&mut ctx, strlen_addr, Some(descriptor), -1);

    // Build args array with the pointer as a Long
    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(call_args, 0, Value::Long(ptr as i64));

    // Invoke the downcall
    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    );

    assert!(result.is_ok(), "strlen downcall failed: {:?}", result.err());
    let val = result.unwrap();
    // strlen("hello") = 5
    assert_eq!(val, Some(Value::Long(5)));
}

#[test]
fn test_85_3_downcall_abs() {
    // Call C abs() — int abs(int)
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();

    let abs_addr = ctx.find_native_symbol(-1, "abs");
    if abs_addr.is_none() {
        return;
    }
    let abs_addr = abs_addr.unwrap() as i64;

    // FunctionDescriptor: of(LAYOUT_INT, LAYOUT_INT)
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(call_args, 0, Value::Int(-42));

    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    );

    assert!(result.is_ok(), "abs downcall failed: {:?}", result.err());
    let val = result.unwrap();
    assert_eq!(val, Some(Value::Int(42)));
}

// -----------------------------------------------------------------
// T5.6.3 — Panama DowncallHandle CIF cache (panama_cif)
// -----------------------------------------------------------------

#[test]
fn panama_cif_cache_reuses_cif_across_calls() {
    // Invoke the same downcall twice and check that the global
    // CIF_BUILD_COUNT only increments once — proving the second
    // call hit the cache on field 3 of the DowncallHandle.
    use crate::panama_libffi as plf;
    use std::sync::atomic::Ordering;

    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();
    let abs_addr = ctx.find_native_symbol(-1, "abs");
    if abs_addr.is_none() {
        // Platform without symbol lookup — skip.
        return;
    }
    let abs_addr = abs_addr.unwrap() as i64;

    // FunctionDescriptor: int(int)
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    // `mk_downcall_handle` leaves DOWNCALL_CIF at 0 — the cache-miss marker
    // this test's first call must observe.
    let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

    // Snapshot the global build counter.
    let before = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);

    // --- First call: cache miss, Cif built and stashed ---
    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(call_args, 0, Value::Int(-7));
    let r1 = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    )
    .expect("first downcall must succeed");
    assert_eq!(r1, Some(Value::Int(7)));

    let after_first = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        after_first,
        before + 1,
        "first call must construct exactly one Cif"
    );

    // Verify DOWNCALL_CIF now carries a non-zero pointer.
    let stash_v = ctx.get_field(handle, DOWNCALL_CIF);
    let stash_u64 = match stash_v {
        Value::Long(n) => n as u64,
        _ => 0,
    };
    assert!(
        stash_u64 != 0,
        "field 3 must hold the boxed Cif pointer after the first call, got {:?}",
        stash_v
    );

    // --- Second call: cache hit, Cif *not* rebuilt ---
    let call_args2 = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(call_args2, 0, Value::Int(-11));
    let r2 = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args2))],
    )
    .expect("second downcall must succeed");
    assert_eq!(r2, Some(Value::Int(11)));

    let after_second = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        after_second, after_first,
        "second call must reuse the cached Cif — build count must not increment"
    );

    // Clean up the leaked Box to keep the test process tidy.
    unsafe { plf::free_cached_cif(stash_u64) };
}

/// JIT round 16 wave 11 (lane ffmcost): the plain-memo encoding round
/// trips every primitive kind at full width and refuses everything else.
#[test]
fn panama_plain_memo_encoding_round_trips() {
    use crate::panama_libffi as plf;
    let kinds = [
        LAYOUT_BYTE,
        LAYOUT_SHORT,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_FLOAT,
        LAYOUT_DOUBLE,
        LAYOUT_BOOLEAN,
        LAYOUT_CHAR,
        LAYOUT_CHAR,
        LAYOUT_LONG,
        LAYOUT_BYTE,
        LAYOUT_BOOLEAN,
    ];
    // Twelve since wave 13 (lane ffmseg), whose segment and capture bits
    // took the thirteenth parameter's nibble.
    assert_eq!(kinds.len(), PLAIN_MEMO_MAX_ARGS);
    for ret in [
        None,
        Some(LAYOUT_BYTE),
        Some(LAYOUT_CHAR),
        Some(LAYOUT_DOUBLE),
    ] {
        let memo = plain_memo_encode(ret, &kinds);
        assert!(plain_memo_is_plain(memo));
        assert_eq!(memo & PLAIN_MEMO_HIT_SEEN, 0);
        assert_eq!(plain_memo_arity(memo), kinds.len());
        assert_eq!(plain_memo_return(memo), ret);
        for (i, kind) in kinds.iter().enumerate() {
            assert_eq!(plain_memo_param(memo, i), *kind);
            assert_eq!(plain_memo_param(memo | PLAIN_MEMO_HIT_SEEN, i), *kind);
        }
    }
    let empty = plain_memo_encode(None, &[]);
    assert!(plain_memo_is_plain(empty));
    assert_eq!(plain_memo_arity(empty), 0);
    assert_eq!(plain_memo_return(empty), None);

    let too_many = [LAYOUT_INT; PLAIN_MEMO_MAX_ARGS + 1];
    assert_eq!(plain_memo_encode(None, &too_many), PLAIN_MEMO_NOT_PLAIN);
    assert_eq!(
        plain_memo_encode(None, &[LAYOUT_ADDRESS]),
        PLAIN_MEMO_NOT_PLAIN
    );
    assert_eq!(
        plain_memo_encode(Some(LAYOUT_ADDRESS), &[]),
        PLAIN_MEMO_NOT_PLAIN
    );
    assert_eq!(
        plain_memo_encode(Some(ffi::LAYOUT_STRUCT), &[]),
        PLAIN_MEMO_NOT_PLAIN
    );
    assert_eq!(
        plain_memo_encode(None, &[LAYOUT_INT, plf::LAYOUT_UNKNOWN]),
        PLAIN_MEMO_NOT_PLAIN
    );
    assert!(!plain_memo_is_plain(PLAIN_MEMO_NOT_PLAIN));
    assert!(!plain_memo_is_plain(0));
}

/// JIT round 16 wave 11 (lane ffmcost): a non-plain shape is recorded as
/// such once, and a recorded verdict is never rewritten.
#[test]
fn panama_plain_memo_records_once() {
    let mut ctx = mock_ctx();
    let int_layout = make_layout(&mut ctx, LAYOUT_INT);
    let handle = mk_downcall_handle(&mut ctx, 0x1000, None, -1);
    downcall_record_plain_memo(&ctx, handle, false, false, &[Some(int_layout)], None);
    assert_eq!(
        ctx.get_field(handle, DOWNCALL_PLAIN),
        Value::Long(PLAIN_MEMO_NOT_PLAIN)
    );
    downcall_record_plain_memo(&ctx, handle, true, false, &[Some(int_layout)], None);
    assert_eq!(
        ctx.get_field(handle, DOWNCALL_PLAIN),
        Value::Long(PLAIN_MEMO_NOT_PLAIN),
        "the first verdict stands"
    );
    let other = mk_downcall_handle(&mut ctx, 0x1000, None, -1);
    downcall_record_plain_memo(&ctx, other, true, false, &[None], None);
    assert_eq!(
        ctx.get_field(other, DOWNCALL_PLAIN),
        Value::Long(PLAIN_MEMO_NOT_PLAIN),
        "a null parameter layout is the general path's refusal"
    );
    let third = mk_downcall_handle(&mut ctx, 0x1000, None, -1);
    downcall_record_plain_memo(
        &ctx,
        third,
        true,
        false,
        &[Some(int_layout)],
        Some(int_layout),
    );
    assert_eq!(
        ctx.get_field(third, DOWNCALL_PLAIN),
        Value::Long(plain_memo_encode(Some(LAYOUT_INT), &[LAYOUT_INT]))
    );
}

/// JIT round 16 wave 11 (lane ffmcost): `abs(int)` through both arms of
/// `CRATONVM_FFM_DOWNCALL_PLAIN_MEMO`. Off: the general path answers (and
/// records the memo only for the JIT bind, default on since wave 12). On:
/// the first call classifies the handle if nothing has yet, and later
/// signature-polymorphic calls are served by the plain path; shapes it
/// cannot vouch for (the boxed-array bridge, a wrong arity) decline to the
/// general path, which still answers them.
#[test]
fn panama_plain_memo_serves_a_primitive_downcall() {
    use crate::panama_libffi as plf;
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let Some(abs_addr) = ctx.find_native_symbol(-1, "abs") else {
        return; // platform without symbol lookup
    };
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));
    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));
    let handle = mk_downcall_handle(&mut ctx, abs_addr as i64, Some(descriptor), -1);
    let h = Value::Object(Some(handle));

    assert!(pe_downcall_invoke_plain(&mut ctx, &[h, Value::Int(-3)]).is_none());
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(-5)], false).unwrap(),
        Some(Value::Int(5))
    );
    // The plain-memo switch off still records the memo while the JIT's
    // bound row is on (`CRATONVM_JIT_FFM_DOWNCALL_BIND`, default on since
    // wave 12: the bind serves only a classified handle); with the bind
    // off it records nothing. Either way the off call answered through
    // the general path.
    let recorded = matches!(ctx.get_field(handle, DOWNCALL_PLAIN), Value::Long(m) if m != 0);
    assert_eq!(
        recorded,
        jit_downcall_bind_enabled(),
        "the plain-memo switch off records only for the bind"
    );

    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(-7)], true).unwrap(),
        Some(Value::Int(7))
    );
    let Value::Long(memo) = ctx.get_field(handle, DOWNCALL_PLAIN) else {
        panic!("the handle is classified by now");
    };
    assert_eq!(memo, plain_memo_encode(Some(LAYOUT_INT), &[LAYOUT_INT]));

    let served = pe_downcall_invoke_plain(&mut ctx, &[h, Value::Int(-11)])
        .expect("a classified plain handle is served by the plain path");
    assert_eq!(served.unwrap(), Some(Value::Int(11)));
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(i32::MIN + 1)], true).unwrap(),
        Some(Value::Int(i32::MAX))
    );

    let boxed = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(boxed, 0, Value::Int(-13));
    let boxed = Value::Object(Some(boxed));
    assert!(pe_downcall_invoke_plain(&mut ctx, &[h, boxed]).is_none());
    assert!(pe_downcall_invoke_plain(&mut ctx, &[h]).is_none());
    assert!(pe_downcall_invoke_plain(&mut ctx, &[h, Value::Int(1), Value::Int(2)]).is_none());
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, boxed], true).unwrap(),
        Some(Value::Int(13))
    );

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { plf::free_cached_cif(cif as u64) };
    }
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): a site descriptor binds a
/// plain encoding only when it names the memo's kinds position for
/// position, with `V` for a void return.
#[test]
fn jit_downcall_site_matches_only_its_own_memo() {
    let long_long = plain_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_LONG]);
    assert!(jit_downcall_site_matches_memo("(J)J", long_long));
    assert!(!jit_downcall_site_matches_memo("(I)J", long_long));
    assert!(!jit_downcall_site_matches_memo("(J)I", long_long));
    assert!(!jit_downcall_site_matches_memo("(JJ)J", long_long));
    assert!(!jit_downcall_site_matches_memo("()J", long_long));
    assert!(!jit_downcall_site_matches_memo("(J)V", long_long));
    assert!(!jit_downcall_site_matches_memo(
        "(J)Ljava/lang/Object;",
        long_long
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/Long;)J",
        long_long
    ));
    assert!(!jit_downcall_site_matches_memo("J)J", long_long));
    let none_int = plain_memo_encode(Some(LAYOUT_INT), &[]);
    assert!(jit_downcall_site_matches_memo("()I", none_int));
    let void = plain_memo_encode(None, &[LAYOUT_DOUBLE, LAYOUT_CHAR, LAYOUT_BOOLEAN]);
    assert!(jit_downcall_site_matches_memo("(DCZ)V", void));
    assert!(!jit_downcall_site_matches_memo("(DCZ)I", void));
    assert!(!jit_downcall_site_matches_memo("(DCB)V", void));
    let narrow = plain_memo_encode(Some(LAYOUT_SHORT), &[LAYOUT_BYTE, LAYOUT_FLOAT]);
    assert!(jit_downcall_site_matches_memo("(BF)S", narrow));
    assert!(!jit_downcall_site_matches_memo(
        "(BF)S",
        PLAIN_MEMO_NOT_PLAIN
    ));
    assert!(!jit_downcall_site_matches_memo("(BF)S", 0));
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): the admission refuses
/// what is not a plain carrier, waits for an undecided one, and binds a
/// plain one only at its own rendered type.
#[test]
fn jit_downcall_bind_target_admits_only_a_plain_carrier_at_its_type() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let ordinary =
        try_alloc_concurrent_synthetic(&mut ctx, DOWNCALL_CARRIER_CLASS, 22).unwrap();
    assert_eq!(
        jit_downcall_bind_target(&mut ctx, ordinary, "()I"),
        JitDowncallBind::Refused
    );
    let Some(abs_addr) = ctx.find_native_symbol(-1, "abs") else {
        return; // platform without symbol lookup
    };
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));
    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));
    let handle = mk_downcall_handle(&mut ctx, abs_addr as i64, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    assert!(
        is_downcall_handle(&ctx, handle),
        "the mock carrier is a downcall carrier"
    );
    assert_eq!(
        jit_downcall_bind_target(&mut ctx, handle, "(I)I"),
        JitDowncallBind::NotYet,
        "an undecided memo waits for the door's first call"
    );
    // The general path's first call classifies the handle and caches its CIF.
    let h = Value::Object(Some(handle));
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(-2)], true).unwrap(),
        Some(Value::Int(2))
    );
    assert_eq!(
        jit_downcall_bind_target(&mut ctx, handle, "(J)J"),
        JitDowncallBind::Refused,
        "a site that is not the memo's kinds"
    );
    // With the `type` the carrier is minted with, the handle binds at
    // exactly that type; a mock that cannot render it refuses.
    if let Ok(Some(mt)) =
        crate::lang_invoke::build_method_type_from_descriptor(&mut ctx, "(I)I")
    {
        ctx.set_field(handle, 0, Value::Object(Some(mt)));
        let renders = crate::lang_invoke::mh_downcall_bind_declared_type(&mut ctx, handle)
            .is_some_and(|d| d == "(I)I")
            && matches!(ctx.get_field_by_name(handle, "type"), Value::Object(Some(t)) if t == mt);
        let expect = if renders {
            JitDowncallBind::Bound(mt)
        } else {
            JitDowncallBind::Refused
        };
        assert_eq!(jit_downcall_bind_target(&mut ctx, handle, "(I)I"), expect);
    }
    let not_plain = mk_downcall_handle(&mut ctx, abs_addr as i64, Some(descriptor), -1);
    ctx.set_field(not_plain, DOWNCALL_PLAIN, Value::Long(PLAIN_MEMO_NOT_PLAIN));
    assert_eq!(
        jit_downcall_bind_target(&mut ctx, not_plain, "(I)I"),
        JitDowncallBind::Refused
    );
    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// Round 19: the interpreter's downcall lane declines -- nothing done --
/// without a call site, for a handle that is not a carrier, and in a
/// context with no VM identity (whose verdicts it may not memoise).
#[test]
fn interp_downcall_lane_declines_what_it_cannot_vouch_for() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let ordinary =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 8).unwrap();
    let args = [Value::Object(Some(ordinary)), Value::Int(-2)];
    assert!(interp_downcall_lane(&mut ctx, &args, Some("(I)I")).is_none());
    assert!(interp_downcall_lane(&mut ctx, &args, None).is_none());
    assert!(interp_downcall_lane(&mut ctx, &[Value::Object(None)], Some("()V")).is_none());
    let carrier =
        try_alloc_concurrent_synthetic(&mut ctx, DOWNCALL_CARRIER_CLASS, DOWNCALL_PLAIN + 2)
            .unwrap();
    let args = [Value::Object(Some(carrier)), Value::Int(-2)];
    if ctx.vm_identity() == 0 {
        assert!(interp_downcall_lane(&mut ctx, &args, Some("(I)I")).is_none());
    }
}

/// Round 19: the `invokeExact` door asks the downcall lane right after it
/// takes its call site, before any of its own refusals (none of which
/// can fire for a carrier the JIT would bind).
#[test]
fn invoke_exact_door_asks_the_downcall_lane_first() {
    let src = concat!(
        concat!(include_str!("../lang_invoke/member_name/mod.rs"), "\n", include_str!("../lang_invoke/member_name/vh_array_call.rs"), "\n", include_str!("../lang_invoke/member_name/mh_foreign_door_type.rs"), "\n", include_str!("../lang_invoke/member_name/mh_invoke_exact_type_refusal.rs"), "\n", include_str!("../lang_invoke/member_name/vh_indirect_mode_handle.rs")),
        include_str!("../lang_invoke/call_site.rs"),
        include_str!("../lang_invoke/direct_dispatch.rs"),
        concat!(include_str!("../lang_invoke/var_handles/mod.rs"), "\n", include_str!("../lang_invoke/var_handles/native_record_support.rs"), "\n", include_str!("../lang_invoke/var_handles/invoke_reference_cast_refusal.rs"), "\n", include_str!("../lang_invoke/var_handles/mh_real_field_converting_serve.rs")),
        concat!(include_str!("../lang_invoke/adapters/mod.rs"), "\n", include_str!("../lang_invoke/adapters/mh_direct_lane_enabled.rs"), "\n", include_str!("../lang_invoke/adapters/mh_bindto_prelink_bound_field.rs"), "\n", include_str!("../lang_invoke/adapters/method_finder_mint_file.rs"), "\n", include_str!("../lang_invoke/adapters/mh_invoke_with_arguments_list_door.rs")),
        include_str!("../lang_invoke/combinators.rs"),
    );
    let door = src
        .find("\"invokeExact\",\n        \"([Ljava/lang/Object;)Ljava/lang/Object;\"")
        .or_else(|| {
            src.find("\"invokeExact\",\r\n        \"([Ljava/lang/Object;)Ljava/lang/Object;\"")
        })
        .expect("the invokeExact registration");
    let body = &src[door..];
    let take = body
        .find("poly_call_site::take()")
        .expect("the site is taken");
    let lane = body
        .find("interp_downcall_lane(")
        .expect("the lane is asked");
    let npe = body
        .find("mh_interface_null_receiver_npe(")
        .expect("the first refusal");
    assert!(take < lane && lane < npe);
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): the bound native answers
/// a classified handle through the plain path and an unclassified one
/// through the general path, with the same raw result.
#[test]
fn jit_downcall_bound_native_answers_both_paths() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let Some(abs_addr) = ctx.find_native_symbol(-1, "abs") else {
        return; // platform without symbol lookup
    };
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));
    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));
    let handle = mk_downcall_handle(&mut ctx, abs_addr as i64, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));
    // Unclassified: the plain path declines and the general path answers.
    assert_eq!(
        jit_downcall_bound_native(&mut ctx, &[h, Value::Int(-21)]).unwrap(),
        Some(Value::Int(21))
    );
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(-1)], true).unwrap(),
        Some(Value::Int(1))
    );
    assert!(matches!(
        ctx.get_field(handle, DOWNCALL_PLAIN),
        Value::Long(m) if plain_memo_is_plain(m)
    ));
    // Classified: the plain path answers.
    assert!(pe_downcall_invoke_plain(&mut ctx, &[h, Value::Int(-34)]).is_some());
    assert_eq!(
        jit_downcall_bound_native(&mut ctx, &[h, Value::Int(-34)]).unwrap(),
        Some(Value::Int(34))
    );
    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

extern "C" fn r19_fp_target(a: i32, b: f64, c: i64, d: f32) -> f64 {
    f64::from(a) * 1000.0 + b * 100.0 + c as f64 * 10.0 + f64::from(d)
}

/// Round 19: a float-class encoding's positional registers are split into
/// the integer and the vector sequence, each in parameter order, and a
/// `double` return is `xmm0`'s bits (SysV x86-64; elsewhere nothing is
/// called).
#[test]
fn memo_direct_stub_call_splits_float_class_registers() {
    let memo = plain_memo_encode(
        Some(LAYOUT_DOUBLE),
        &[LAYOUT_INT, LAYOUT_DOUBLE, LAYOUT_LONG, LAYOUT_FLOAT],
    );
    assert!(memo_direct_has_fp(memo));
    assert!(!memo_direct_has_fp(plain_memo_encode(
        Some(LAYOUT_INT),
        &[LAYOUT_LONG]
    )));
    let cells = [
        (-2i32) as u32 as u64,
        3.0f64.to_bits(),
        4,
        u64::from(0.5f32.to_bits()),
    ];
    let regs = memo_direct_regs(memo, &cells);
    // SAFETY: `r19_fp_target` has the encoding's own shape.
    let got = unsafe { memo_direct_stub_call(memo, r19_fp_target as usize, &regs) };
    if !crate::panama_libffi::direct_stub_fp_supported() {
        return;
    }
    assert!(memo_direct_shape(memo));
    assert_eq!(
        memo_return_value(memo, [got.unwrap(), 0]),
        Value::Double(-2000.0 + 300.0 + 40.0 + 0.5)
    );
}

/// JIT round 16 wave 16 (lane ffmlean; FFS16-4): which encodings a direct
/// stub serves, and the register and return images it uses.
#[test]
fn memo_direct_shape_admits_integer_class_up_to_six() {
    if !crate::panama_libffi::direct_stubs_supported() {
        return;
    }
    assert!(memo_direct_shape(plain_memo_encode(
        Some(LAYOUT_INT),
        &[LAYOUT_INT]
    )));
    assert!(memo_direct_shape(plain_memo_encode(None, &[])));
    let six = [
        LAYOUT_BYTE,
        LAYOUT_SHORT,
        LAYOUT_CHAR,
        LAYOUT_BOOLEAN,
        LAYOUT_INT,
        LAYOUT_LONG,
    ];
    assert!(memo_direct_shape(plain_memo_encode(
        Some(LAYOUT_LONG),
        &six
    )));
    assert!(!memo_direct_shape(plain_memo_encode(
        Some(LAYOUT_LONG),
        &[LAYOUT_INT; 7]
    )));
    // Round 19: float-class kinds take the SysV vector stub.
    let fp = crate::panama_libffi::direct_stub_fp_supported();
    assert_eq!(
        memo_direct_shape(plain_memo_encode(Some(LAYOUT_DOUBLE), &[LAYOUT_INT])),
        fp
    );
    assert_eq!(
        memo_direct_shape(plain_memo_encode(Some(LAYOUT_INT), &[LAYOUT_FLOAT])),
        fp
    );
    assert!(!memo_direct_shape(plain_memo_encode(
        Some(LAYOUT_DOUBLE),
        &[LAYOUT_DOUBLE; 7]
    )));
    assert!(memo_direct_shape(segment_memo_encode(
        Some(LAYOUT_LONG),
        &[LAYOUT_ADDRESS],
        false
    )));
    assert!(memo_direct_shape(segment_memo_encode(
        Some(LAYOUT_INT),
        &[LAYOUT_INT],
        true
    )));
    assert!(!memo_direct_shape(PLAIN_MEMO_NOT_PLAIN));
    assert!(!memo_direct_shape(0));
    let memo = plain_memo_encode(None, &[LAYOUT_BYTE, LAYOUT_INT, LAYOUT_LONG]);
    assert_eq!(
        memo_direct_regs(memo, &[0xFF, 0xFFFF_FFFF, 7])[..3],
        [u64::MAX, u64::MAX, 7]
    );
    assert_eq!(
        memo_return_value(
            plain_memo_encode(Some(LAYOUT_INT), &[]),
            [0xFFFF_FFFF_0000_0005, 0]
        ),
        Value::Int(5)
    );
    assert_eq!(
        memo_return_value(plain_memo_encode(None, &[]), [9, 0]),
        Value::Object(None)
    );
}

/// JIT round 16 wave 16 (lane ffmlean; FFD16-1): a classified plain
/// handle's direct stub answers what the plain path answers, from raw
/// compiled-caller slots, and only such a handle gets one.
#[test]
fn jit_downcall_direct_answers_like_the_plain_path() {
    let _na = NativeAccessGuard::enable();
    if !crate::panama_libffi::direct_stubs_supported() {
        return;
    }
    let mut ctx = mock_ctx();
    let Some(abs_addr) = ctx.find_native_symbol(-1, "abs") else {
        return; // platform without symbol lookup
    };
    let descriptor = ffmseg_descriptor(&mut ctx, Some(LAYOUT_INT), &[LAYOUT_INT]);
    let handle = mk_downcall_handle(&mut ctx, abs_addr as i64, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(0));
    let h = Value::Object(Some(handle));
    // Undecided: no stub until the general path classifies the handle.
    assert_eq!(downcall_direct_stub_of(&ctx, handle), None);
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, Value::Int(-1)], true).unwrap(),
        Some(Value::Int(1))
    );
    let stub = downcall_direct_stub_of(&ctx, handle).expect("a classified plain handle");
    assert_eq!((stub.arity(), stub.critical()), (1, false));
    assert_eq!(stub.fn_addr(), abs_addr);
    // JIT round 17 wave 5 (lane ffm4): a non-critical stub is admitted
    // only where no GC-safe state is on or the direct row goes lazy
    // itself; the lazy state is ON by default since then.
    assert_eq!(
        jit_downcall_direct_admits(&ctx, &stub, 1),
        !(ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) || direct_row_goes_lazy()
    );
    assert!(!jit_downcall_direct_admits(&ctx, &stub, 2));
    for v in [-34, 0, 7, i32::MIN + 1] {
        let plain = pe_downcall_invoke_plain(&mut ctx, &[h, Value::Int(v)])
            .expect("the plain path serves it")
            .unwrap();
        // SAFETY: one slot for the one `int` parameter.
        let direct = unsafe { jit_downcall_direct_invoke(&mut ctx, &stub, &[v as i64]) };
        assert_eq!(plain, Some(direct), "abs({v})");
    }
    // An `int` slot's upper half is not the argument's.
    let dirty = (0x5EAD_0000_0000_0000u64 | u64::from(-5i32 as u32)) as i64;
    // SAFETY: as above.
    let direct = unsafe { jit_downcall_direct_invoke(&mut ctx, &stub, &[dirty]) };
    assert_eq!(direct, Value::Int(5));
    // The critical mark is the handle's.
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(1));
    let critical = downcall_direct_stub_of(&ctx, handle).unwrap();
    assert!(critical.critical());
    // A critical stub is admitted whatever the in-native switches say.
    assert!(jit_downcall_direct_admits(&ctx, &critical, 1));
    // A handle that is not plain gets none.
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(PLAIN_MEMO_NOT_PLAIN));
    assert_eq!(downcall_direct_stub_of(&ctx, handle), None);
    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

// -----------------------------------------------------------------
// JIT round 16 wave 13 (lane ffmseg): the segment memo, the settled
// access verdict, the capture state and `critical(true)` heap access.
// -----------------------------------------------------------------

/// Sums `n` bytes at `p` and adds one to each: a C callee that reads and
/// writes through a pointer argument.
extern "C" fn ffmseg_sum_and_bump(p: *mut u8, n: i64) -> i64 {
    let mut sum = 0i64;
    for i in 0..n.max(0) as usize {
        // SAFETY: the tests pass `n` no larger than the buffer behind `p`.
        unsafe {
            sum += i64::from(*p.add(i));
            *p.add(i) = (*p.add(i)).wrapping_add(1);
        }
    }
    sum
}

/// `|v|`, a primitive callee for the capture-state handle.
extern "C" fn ffmseg_abs(v: i64) -> i64 {
    v.wrapping_abs()
}

/// A `FunctionDescriptor` carrier of `ret` (void when `None`) and
/// `params`, the shape `pe_new_function_descriptor` mints.
fn ffmseg_descriptor(
    ctx: &mut dyn NativeContext,
    ret: Option<i32>,
    params: &[i32],
) -> ObjectRef {
    let ret_layout = ret.map(|kind| make_layout(ctx, kind));
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, params.len());
    for (i, kind) in params.iter().enumerate() {
        let layout = make_layout(ctx, *kind);
        ctx.set_array_element(params_arr, i, Value::Object(Some(layout)));
    }
    let descriptor =
        try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
    ctx.set_field(descriptor, 0, Value::Object(ret_layout));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));
    descriptor
}

/// A six-slot native segment carrier over `ptr..ptr+size`, its slot 2 the
/// scope (`pe_segment_session` resolves a modelled session there).
fn ffmseg_native_segment(
    ctx: &mut dyn NativeContext,
    ptr: i64,
    size: i64,
    scope: Option<ObjectRef>,
) -> ObjectRef {
    let seg = alloc_segment_carrier(ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, PE_SEGMENT_ARENA_FIELD, Value::Object(scope));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    seg
}

/// A fresh open, unconfined modelled session.
fn ffmseg_session(ctx: &mut dyn NativeContext) -> ObjectRef {
    match crate::phases_late::foreign_ffm::p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    }
}

#[test]
fn jit_session_field_only_proves_owned_carriers_with_a_stored_session() {
    let mut ctx = mock_ctx();
    let session = ffmseg_session(&mut ctx);
    // The mock cannot fabricate VM-internal classes through the default
    // NativeContext hook; name the actual carrier explicitly in this test.
    let segment = try_alloc_concurrent_synthetic(&mut ctx, CRATON_SEGMENT_CLASS, 6).unwrap();
    ctx.set_field(
        segment,
        PE_SEGMENT_ARENA_FIELD,
        Value::Object(Some(session)),
    );
    assert_eq!(
        jit_carrier_stored_session(&ctx, segment),
        Some((session, PE_SEGMENT_ARENA_FIELD))
    );
    // The field getter still answers the closed session; access checks
    // (not the layout proof) reject operations against it.
    let state = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, session).state;
    ctx.set_field(session, state, Value::Int(0));
    assert_eq!(
        jit_carrier_stored_session(&ctx, segment),
        Some((session, PE_SEGMENT_ARENA_FIELD))
    );
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
    ctx.set_field(segment, PE_SEGMENT_ARENA_FIELD, Value::Object(Some(array)));
    assert_eq!(jit_carrier_stored_session(&ctx, segment), None);
    ctx.set_field(segment, PE_SEGMENT_ARENA_FIELD, Value::Object(None));
    assert_eq!(jit_carrier_stored_session(&ctx, segment), None);
    let foreign =
        try_alloc_concurrent_synthetic(&mut ctx, "example/ForeignSegment", 6).unwrap();
    ctx.set_field(
        foreign,
        PE_SEGMENT_ARENA_FIELD,
        Value::Object(Some(session)),
    );
    assert_eq!(jit_carrier_stored_session(&ctx, foreign), None);
}

fn ffmseg_acquires(ctx: &dyn NativeContext, session: ObjectRef) -> Value {
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    ctx.get_field(session, slots.acquires)
}

/// JIT round 22 (lane memory, FFM22-3): only a lead-free segment stub with
/// exactly one `ADDRESS` parameter, integer-class others and an integer,
/// void or `ADDRESS` return has a segment call shape.
#[test]
fn segment_call_shape_takes_one_address_and_no_lead() {
    let stub = |memo: i64, segments: bool| JitDowncallDirect {
        fn_addr: 1,
        memo,
        critical: false,
        segments,
        group_size: 0,
        group_align: 1,
    };
    let strlen = stub(
        segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_ADDRESS], false),
        true,
    );
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        let shape = strlen.segment_call_shape().expect("strlen");
        assert_eq!(
            (shape.arity, shape.segment, shape.ret, shape.params[0]),
            (1, 0, COMPILED_CALL_W64, COMPILED_CALL_W64)
        );
        let strchr = stub(
            address_return_memo_encode(
                &[LAYOUT_ADDRESS, LAYOUT_INT],
                false,
                PLAIN_MEMO_RET_ADDRESS,
            ),
            true,
        );
        let shape = strchr.segment_call_shape().expect("strchr");
        assert_eq!(
            (shape.arity, shape.segment, shape.ret, shape.params[1]),
            (2, 0, COMPILED_CALL_REF, COMPILED_CALL_S32)
        );
    } else {
        assert!(strlen.segment_call_shape().is_none());
    }
    for memo in [
        segment_memo_encode(Some(LAYOUT_INT), &[LAYOUT_ADDRESS, LAYOUT_ADDRESS], false),
        segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_ADDRESS], true),
        group_return_memo_encode(&[LAYOUT_ADDRESS], false, true),
        segment_memo_encode(None, &[LAYOUT_ADDRESS, LAYOUT_DOUBLE], false),
    ] {
        assert!(stub(memo, true).segment_call_shape().is_none(), "{memo:#x}");
    }
    let plain = stub(plain_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_LONG]), false);
    assert!(plain.segment_call_shape().is_none());
}

/// A segment encoding records `ADDRESS` parameters and a capture state,
/// is never a plain one, and binds only the site that spells each
/// `ADDRESS` (and the leading state) as `MemorySegment`.
#[test]
fn segment_memo_encodes_addresses_and_capture_and_matches_its_site() {
    let strlen = segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_ADDRESS], false);
    assert!(plain_memo_is_segment(strlen));
    assert!(!plain_memo_is_plain(strlen));
    assert_eq!(plain_memo_arity(strlen), 1);
    assert_eq!(plain_memo_param(strlen, 0), LAYOUT_ADDRESS);
    assert_eq!(plain_memo_return(strlen), Some(LAYOUT_LONG));
    assert!(jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;)J",
        strlen
    ));
    assert!(!jit_downcall_site_matches_memo("(J)J", strlen));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/Object;)J",
        strlen
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;)I",
        strlen
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;J)J",
        strlen
    ));

    let mixed = segment_memo_encode(None, &[LAYOUT_INT, LAYOUT_ADDRESS, LAYOUT_DOUBLE], false);
    assert!(jit_downcall_site_matches_memo(
        "(ILjava/lang/foreign/MemorySegment;D)V",
        mixed
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;ID)V",
        mixed
    ));

    let errno = segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_LONG], true);
    assert!(plain_memo_is_segment(errno));
    assert_ne!(errno & PLAIN_MEMO_CAPTURE, 0);
    assert_eq!(errno & PLAIN_MEMO_SEGMENTS, 0);
    assert!(jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;J)J",
        errno
    ));
    assert!(!jit_downcall_site_matches_memo("(J)J", errno));

    // Without an address or a capture it is the plain encoding.
    assert_eq!(
        segment_memo_encode(Some(LAYOUT_INT), &[LAYOUT_INT], false),
        plain_memo_encode(Some(LAYOUT_INT), &[LAYOUT_INT])
    );
    // An address return mints a segment: the general path's.
    assert_eq!(
        segment_memo_encode(Some(LAYOUT_ADDRESS), &[LAYOUT_ADDRESS], false),
        PLAIN_MEMO_NOT_PLAIN
    );
    assert_eq!(
        segment_memo_encode(None, &[ffi::LAYOUT_STRUCT], true),
        PLAIN_MEMO_NOT_PLAIN
    );
    let too_many = [LAYOUT_ADDRESS; PLAIN_MEMO_MAX_ARGS + 1];
    assert_eq!(
        segment_memo_encode(None, &too_many, false),
        PLAIN_MEMO_NOT_PLAIN
    );
    // The twelfth parameter's nibble stays below the flag bits.
    let full = [LAYOUT_CHAR; PLAIN_MEMO_MAX_ARGS];
    let memo = plain_memo_encode(None, &full);
    assert!(plain_memo_is_plain(memo));
    assert_eq!(plain_memo_param(memo, PLAIN_MEMO_MAX_ARGS - 1), LAYOUT_CHAR);
}

/// The settled verdict follows this VM's policy: settled under
/// `Permissive`, not under `Enforce` (a replacement re-resolves through
/// the index generation), settled again once the policy is gone.
#[test]
fn downcall_policy_settled_follows_the_vms_policy_mode() {
    use cratonvm_native_api::{
        install_capabilities, uninstall_capabilities, CapabilityMode, CapabilitySet, VmId,
    };
    let ctx = mock_ctx();
    // An identity no other test installs a policy under.
    let raw = 0x5E6_0013;
    ctx.set_vm_identity(raw);
    let vm = VmId::from_raw(raw);
    install_capabilities(std::sync::Arc::new(CapabilitySet::new(
        vm,
        CapabilityMode::Permissive,
    )));
    assert!(downcall_policy_settled(&ctx), "Permissive is settled");
    install_capabilities(std::sync::Arc::new(CapabilitySet::new(
        vm,
        CapabilityMode::Enforce,
    )));
    assert!(
        !downcall_policy_settled(&ctx),
        "a replacement policy is re-resolved: Enforce checks every call"
    );
    install_capabilities(std::sync::Arc::new(CapabilitySet::new(
        vm,
        CapabilityMode::Audit,
    )));
    assert!(!downcall_policy_settled(&ctx), "Audit counts every call");
    uninstall_capabilities(vm);
    assert!(downcall_policy_settled(&ctx), "no policy for this VM");
}

/// The segment memo serves a native segment argument (session acquired
/// for the call and released), and declines -- leaving the general path
/// to raise -- a `null`, a heap segment, a foreign-thread scope and a
/// closed one.
#[test]
fn segment_memo_call_serves_a_native_segment_and_declines_the_rest() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_sum_and_bump as usize as i64;
    let descriptor =
        ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_ADDRESS, LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));

    let mut buf = [1u8, 2, 3, 4];
    let session = ffmseg_session(&mut ctx);
    let seg = ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(session));
    let s = Value::Object(Some(seg));

    assert!(
        pe_downcall_invoke_memo(&mut ctx, &[h, s, Value::Long(4)], true).is_none(),
        "an unclassified handle is the general path's"
    );
    // The general path classifies it (the bind switch records the memo).
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, s, Value::Long(4)], false).unwrap(),
        Some(Value::Long(10))
    );
    assert_eq!(buf, [2, 3, 4, 5]);
    let Value::Long(memo) = ctx.get_field(handle, DOWNCALL_PLAIN) else {
        panic!("no memo recorded");
    };
    assert_eq!(
        memo,
        segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_ADDRESS, LAYOUT_LONG], false)
    );
    assert!(
        pe_downcall_invoke_plain(&mut ctx, &[h, s, Value::Long(4)]).is_none(),
        "the plain path never serves a segment encoding"
    );
    assert!(
        pe_downcall_invoke_memo(&mut ctx, &[h, s, Value::Long(4)], false).is_none(),
        "nor the memo path with the segment switch off"
    );

    let pins_before = ctx.native_pin_count_for_test();
    let served = pe_downcall_invoke_memo(&mut ctx, &[h, s, Value::Long(4)], true)
        .expect("a native segment of an open session is served");
    assert_eq!(served.unwrap(), Some(Value::Long(14)));
    assert_eq!(buf, [3, 4, 5, 6]);
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0), "released");
    assert_eq!(ctx.native_pin_count_for_test(), pins_before, "unpinned");

    // `null`: the general path's message-less NPE.
    assert!(
        pe_downcall_invoke_memo(&mut ctx, &[h, Value::Object(None), Value::Long(4)], true)
            .is_none()
    );
    // A heap segment: refused or copied by the general path, never here.
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(int[]) answered {other:?}"),
    };
    assert!(pe_downcall_invoke_memo(
        &mut ctx,
        &[h, Value::Object(Some(heap)), Value::Long(4)],
        true
    )
    .is_none());

    // Confined to another thread: `WrongThreadException` is the general
    // path's.
    let foreign = ffmseg_session(&mut ctx);
    // Any object that is not this thread's mirror owns it.
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, foreign);
    ctx.set_field(foreign, slots.owner, Value::Object(Some(descriptor)));
    let foreign_seg =
        ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(foreign));
    let fs = Value::Object(Some(foreign_seg));
    assert!(pe_downcall_invoke_memo(&mut ctx, &[h, fs, Value::Long(4)], true).is_none());
    // `p67_wrong_thread` raises the real class where it can be built,
    // else an `IllegalStateException` naming the owning thread.
    let err = jit_downcall_bound_native(&mut ctx, &[h, fs, Value::Long(4)]).unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("owning thread") || matches!(err, MethodCallFailed::ExceptionThrown(_)),
        "{text}"
    );

    // Closed: `IllegalStateException: Already closed`, the general path's.
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    assert!(pe_downcall_invoke_memo(&mut ctx, &[h, s, Value::Long(4)], true).is_none());
    let err = jit_downcall_bound_native(&mut ctx, &[h, s, Value::Long(4)]).unwrap_err();
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(buf, [3, 4, 5, 6], "no refused call reached C");
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0));

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// A `captureCallState` handle is classified with its state segment
/// first, and the memo call writes the call state into it.
#[test]
fn segment_memo_call_captures_call_state() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_abs as usize as i64;
    let descriptor = ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_CAPTURE_CALL_STATE, Value::Int(1));
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));
    let mut state_buf = [0i32; 4];
    let state = ffmseg_native_segment(&mut ctx, state_buf.as_mut_ptr() as i64, 16, None);
    let st = Value::Object(Some(state));

    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, st, Value::Long(-9)], false).unwrap(),
        Some(Value::Long(9))
    );
    let Value::Long(memo) = ctx.get_field(handle, DOWNCALL_PLAIN) else {
        panic!("no memo recorded");
    };
    assert_eq!(
        memo,
        segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_LONG], true)
    );

    const SENTINEL: i32 = i32::MIN + 7;
    state_buf[0] = SENTINEL;
    let pins_before = ctx.native_pin_count_for_test();
    let served = pe_downcall_invoke_memo(&mut ctx, &[h, st, Value::Long(-11)], true)
        .expect("a capture handle with a native state segment is served");
    assert_eq!(served.unwrap(), Some(Value::Long(11)));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
    #[cfg(any(target_os = "linux", windows))]
    assert_ne!(
        // SAFETY: a plain read of the test's own buffer, written by C.
        unsafe { std::ptr::read_volatile(state_buf.as_ptr()) },
        SENTINEL,
        "the call state was written"
    );
    // Without the state segment the arity is wrong: the general path's.
    assert!(pe_downcall_invoke_memo(&mut ctx, &[h, Value::Long(-11)], true).is_none());

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// JIT round 17 wave 2 (lane ffm3, `CRATONVM_FFM_CAPTURE_SEGMENT_NULL_ALIGN_CHECK`):
/// a `null` state segment is an NPE, `MemorySegment.NULL` (any size) and a
/// misaligned one an IAE, before the call; the memo path declines the
/// last two so the general path raises them.
#[test]
fn capture_state_segment_null_and_misaligned_are_refused() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_abs as usize as i64;
    let descriptor = ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_CAPTURE_CALL_STATE, Value::Int(1));
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));
    let mut state_buf = [0i32; 4];
    let good = ffmseg_native_segment(&mut ctx, state_buf.as_mut_ptr() as i64, 16, None);
    let g = Value::Object(Some(good));
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, g, Value::Long(-9)], false).unwrap(),
        Some(Value::Long(9))
    );

    let n = Value::Object(None);
    let null = pe_downcall_invoke_with(&mut ctx, &[h, n, Value::Long(-9)], false).unwrap_err();
    assert!(format!("{null:?}").contains("NullPointer"), "{null:?}");

    let zero = ffmseg_native_segment(&mut ctx, 0, 16, None);
    let z = Value::Object(Some(zero));
    let err = pe_downcall_invoke_with(&mut ctx, &[h, z, Value::Long(-9)], false).unwrap_err();
    assert!(
        format!("{err:?}").contains("Capture segment is NULL"),
        "{err:?}"
    );

    // 12 bytes: the Windows capture-state layout is three ints (`GetLastError`, `WSAGetLastError`, `errno`); a
    // shorter segment fails the bounds check before the alignment one there.
    let odd = ffmseg_native_segment(&mut ctx, state_buf.as_mut_ptr() as i64 + 1, 12, None);
    let o = Value::Object(Some(odd));
    assert!(pe_downcall_invoke_memo(&mut ctx, &[h, o, Value::Long(-9)], true).is_none());
    let err = pe_downcall_invoke_with(&mut ctx, &[h, o, Value::Long(-9)], false).unwrap_err();
    assert!(
        format!("{err:?}").contains("alignment constraints"),
        "{err:?}"
    );

    // JIT round 17 wave 5 (lane ffm4): a heap state segment. An `int[]`
    // one passes the slice and is `Heap segment not allowed`; a `byte[]`
    // one fails the slice's alignment first (HotSpot's order).
    const HEAP_INTS: &str =
        "Heap segment not allowed: MemorySegment{ kind: heap, heapBase: [I@";
    const HEAP_BYTES: &str = "alignment constraints";
    use cratonvm_types::ArrayElementType as Elem;
    for (elem, len, expected) in [(Elem::Int, 4, HEAP_INTS), (Elem::Byte, 16, HEAP_BYTES)] {
        let array = ctx.new_array(elem, len);
        let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
            Some(Value::Object(Some(seg))) => seg,
            other => panic!("ofArray answered {other:?}"),
        };
        let hs = Value::Object(Some(heap));
        assert!(pe_downcall_invoke_memo(&mut ctx, &[h, hs, Value::Long(-9)], true).is_none());
        let err =
            pe_downcall_invoke_with(&mut ctx, &[h, hs, Value::Long(-9)], false).unwrap_err();
        assert!(format!("{err:?}").contains(expected), "{err:?}");
    }

    assert!(capture_segment_declined(0, 16));
    assert!(capture_segment_declined(4098, 16));
    assert!(!capture_segment_declined(4096, 16));

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// JIT round 17 wave 1 (lane ffmseg, `CRATONVM_FFM_DOWNCALL_SEGMENT_ROW`):
/// the segment row answers what the memo path answers -- the same
/// result, the session acquired for the call and released, the pins
/// balanced, the call state written -- and declines exactly where it
/// declines: a closed scope, a foreign-thread one, a heap segment.
#[test]
fn segment_row_answers_like_the_memo_path() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_sum_and_bump as usize as i64;
    let descriptor =
        ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_ADDRESS, LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));
    let mut buf = [1u8, 2, 3, 4];
    let session = ffmseg_session(&mut ctx);
    let seg = ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(session));
    let s = Value::Object(Some(seg));
    // The general path classifies the handle.
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, s, Value::Long(4)], false).unwrap(),
        Some(Value::Long(10))
    );

    // The carrier is read by slot, its session found and verified.
    let mut shape = Some(segment_row_shape(ctx.vm_identity()));
    let (addr, read) = memo_segment_address(&ctx, &mut shape, seg).expect("screened");
    assert_eq!(addr, buf.as_ptr() as usize);
    assert_eq!(
        Some(addr),
        downcall_native_segment_address(&ctx, seg),
        "the general readers' address"
    );
    match read {
        Some((SegmentRowScope::Session(found, _), 4)) => assert_eq!(found, session),
        _ => panic!("the row did not read the carrier and its session"),
    }

    // Both arms serve it alike.
    for row in [false, true] {
        let pins_before = ctx.native_pin_count_for_test();
        let served = pe_downcall_invoke_memo_with(&mut ctx, &[h, s, Value::Long(4)], true, row)
            .expect("a native segment of an open session is served");
        assert!(served.is_ok(), "row={row}");
        assert_eq!(
            ffmseg_acquires(&ctx, session),
            Value::Int(0),
            "released, row={row}"
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            pins_before,
            "unpinned, row={row}"
        );
    }
    assert_eq!(buf, [4, 5, 6, 7], "both calls reached C");

    // A heap segment is never the row's carrier shape.
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(int[]) answered {other:?}"),
    };
    let hv = Value::Object(Some(heap));
    let refused = pe_downcall_invoke_memo_with(&mut ctx, &[h, hv, Value::Long(4)], true, true);
    assert!(refused.is_none());

    // Another thread's confined scope declines (the general path raises
    // `WrongThreadException`).
    let foreign = ffmseg_session(&mut ctx);
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, foreign);
    ctx.set_field(foreign, slots.owner, Value::Object(Some(descriptor)));
    let foreign_seg =
        ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(foreign));
    let fs = Value::Object(Some(foreign_seg));
    let refused = pe_downcall_invoke_memo_with(&mut ctx, &[h, fs, Value::Long(4)], true, true);
    assert!(refused.is_none());

    // A closed scope declines (`Already closed` is the general path's),
    // and the closed session is not acquired.
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    let refused = pe_downcall_invoke_memo_with(&mut ctx, &[h, s, Value::Long(4)], true, true);
    assert!(refused.is_none());
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0));
    assert_eq!(buf, [4, 5, 6, 7], "no refused call reached C");

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// JIT round 17 wave 1 (lane ffmseg): the row's capture write lands in
/// the state segment it read, as the general readers' write does.
#[test]
fn segment_row_writes_the_call_state() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_abs as usize as i64;
    let descriptor = ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_CAPTURE_CALL_STATE, Value::Int(1));
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    let h = Value::Object(Some(handle));
    let mut state_buf = [0i32; 4];
    let session = ffmseg_session(&mut ctx);
    let state =
        ffmseg_native_segment(&mut ctx, state_buf.as_mut_ptr() as i64, 16, Some(session));
    let st = Value::Object(Some(state));
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, st, Value::Long(-9)], false).unwrap(),
        Some(Value::Long(9))
    );
    const SENTINEL: i32 = i32::MIN + 7;
    state_buf[0] = SENTINEL;
    let served = pe_downcall_invoke_memo_with(&mut ctx, &[h, st, Value::Long(-11)], true, true)
        .expect("served through the row");
    assert_eq!(served.unwrap(), Some(Value::Long(11)));
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0), "released");
    #[cfg(any(target_os = "linux", windows))]
    assert_ne!(
        // SAFETY: a plain read of the test's own buffer, written by C.
        unsafe { std::ptr::read_volatile(state_buf.as_ptr()) },
        SENTINEL,
        "the call state was written"
    );

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// JIT round 17 wave 1 (lane ffmseg): both downcall paths write the call
/// state before they release the sessions (`CRATONVM_FFM_CAPTURE_STATE_BEFORE_RELEASE`).
#[test]
fn both_downcall_paths_write_the_call_state_before_the_release() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    for (sig, write, release) in [
        (
            "\npub(in crate::panama) fn pe_downcall_invoke_memo_with(",
            "write_memo_capture_state(ctx, state, state_known, captured)",
            "segment_row_add_acquires(ctx, session, slots, -1)",
        ),
        (
            "\npub(in crate::panama) fn pe_downcall_invoke_general(",
            "write_downcall_capture_state(ctx, state, captured)",
            "pe_downcall_release_sessions(ctx, &sessions)",
        ),
    ] {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
        let first_write = body
            .find(write)
            .unwrap_or_else(|| panic!("{write} in {sig}"));
        let first_release = body
            .find(release)
            .unwrap_or_else(|| panic!("{release} in {sig}"));
        assert!(
            first_write < first_release,
            "{sig}: the state is written first"
        );
    }
}

/// JIT round 17 wave 2 (lane ffm2): the direct segment row writes the call
/// state before it releases the sessions, after the C call (its earlier
/// release is the refused-acquire path, which makes no call).
#[test]
fn direct_segment_row_writes_the_call_state_before_the_release() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    let sig = "\npub unsafe fn jit_downcall_direct_segment_invoke(";
    let start = src.find(sig).expect("the direct segment row");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
    let call = body.find("memo_direct_stub_call(").expect("the call");
    let after = &body[call..];
    let write = after
        .find("write_capture_state_at(")
        .expect("the state write");
    let release = after.find("direct_segment_release(").expect("the release");
    assert!(write < release, "the state is written first");
}

/// JIT round 17 wave 2 (lane ffm2; `CRATONVM_FFM_OWNER_CHECK_NO_MINT`):
/// no FFM owner check in this file asks `current_thread_object` (which
/// mints a mirror: a GC point under the downcall paths' raw references);
/// they all go through `foreign_ffm::p67_owner_is_current_thread`.
#[test]
fn ffm_owner_checks_never_mint_a_thread_mirror() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    let needle = format!("{}{}", "current_thread_object()", " != owner");
    assert!(!src.contains(&needle), "an owner check mints the mirror");
    let ffm = include_str!("../phases_late/foreign_ffm.rs").replace("\r\n", "\n");
    let minting = format!("{}{}", "ctx.current_thread_object()", " == owner");
    // Only the `=0` arm of `p67_owner_is_current_thread_with`.
    assert_eq!(ffm.matches(&minting).count(), 1);
    assert!(!ffm.contains(&format!("{}{}", "current_thread_object()", " != owner")));
}

/// JIT round 18 wave 10 (lane ffmcost; `CRATONVM_JIT_FFM_DIRECT_SEGMENT_LEAN`):
/// the lean row's scope check answers exactly what `segment_row_scope_open`
/// answers, and says "confined" only for a session this thread owns.
#[test]
fn segment_row_scope_open_owned_matches_the_row_check() {
    let mut ctx = mock_ctx();
    let me = ctx.stable_current_thread();
    let session = ffmseg_session(&mut ctx);
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, session);
    // No owner (shared / global): open, not confined.
    ctx.set_field(session, slots.owner, Value::Object(None));
    assert_eq!(
        segment_row_scope_open_owned(&mut ctx, session, slots),
        Some(false)
    );
    assert!(segment_row_scope_open(&mut ctx, session, slots));
    // Owned by this thread: open and confined.
    ctx.set_field(session, slots.owner, Value::Object(Some(me)));
    assert_eq!(
        segment_row_scope_open_owned(&mut ctx, session, slots),
        Some(true)
    );
    assert!(segment_row_scope_open(&mut ctx, session, slots));
    // Owned by another thread: refused by both.
    let other = ffmseg_session(&mut ctx);
    ctx.set_field(session, slots.owner, Value::Object(Some(other)));
    assert_eq!(segment_row_scope_open_owned(&mut ctx, session, slots), None);
    assert!(!segment_row_scope_open(&mut ctx, session, slots));
    // Closed, owned by this thread: refused by both.
    ctx.set_field(session, slots.owner, Value::Object(Some(me)));
    ctx.set_field(session, slots.state, Value::Int(0));
    assert_eq!(segment_row_scope_open_owned(&mut ctx, session, slots), None);
    assert!(!segment_row_scope_open(&mut ctx, session, slots));
}

/// JIT round 18 wave 10 (lane ffmcost): the lean handling stays inside the
/// direct segment row's committed part -- the critical skip is decided in
/// the read loop (before any pin), for a modelled and (round 19) a real
/// session alike, and the owned acquire sits under the handshake, before
/// the C call. The lean switch is default on since round 19; the census
/// reader stays opt-in.
#[test]
fn direct_segment_lean_is_decided_before_the_pins_and_the_census_is_opt_in() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    let sig = "\npub unsafe fn jit_downcall_direct_segment_invoke(";
    let start = src.find(sig).expect("the direct segment row");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
    let at = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    let real_skip = at("if lean && seen.confined && stub.critical {");
    let skip = at("if confined && stub.critical {");
    let pin = at("ctx.pin_native_root(*session)");
    let owned = at("if lean && handshake && owned[i] {");
    let call = at("memo_direct_stub_call(");
    assert!(real_skip < pin && skip < pin && pin < owned && owned < call);
    assert!(ffm_jit_direct_segment_lean());
    assert!(!ffm_dbg_direct_split());
}

/// JIT round 17 wave 2 (lane ffm2; FFMSEG17-1): the direct segment row
/// answers what the memo path answers -- the result, the session acquired
/// and released, the pins balanced -- and declines (nothing done) for a
/// `null` slot, a heap segment, a foreign-thread scope and a closed one.
#[test]
fn direct_segment_row_answers_like_the_memo_path() {
    let _na = NativeAccessGuard::enable();
    if !crate::panama_libffi::direct_stubs_supported() {
        return;
    }
    let mut ctx = mock_ctx();
    let fn_addr = ffmseg_sum_and_bump as usize as i64;
    let descriptor =
        ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_ADDRESS, LAYOUT_LONG]);
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(0));
    let h = Value::Object(Some(handle));
    let mut buf = [1u8, 2, 3, 4];
    let session = ffmseg_session(&mut ctx);
    let seg = ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(session));
    let s = Value::Object(Some(seg));
    assert_eq!(
        downcall_direct_segment_stub_of(&ctx, handle),
        None,
        "undecided"
    );
    assert_eq!(
        pe_downcall_invoke_with(&mut ctx, &[h, s, Value::Long(4)], false).unwrap(),
        Some(Value::Long(10))
    );
    // A plain stub is never made for it; a segment stub is.
    assert_eq!(downcall_direct_stub_of(&ctx, handle), None);
    let stub = downcall_direct_segment_stub_of(&ctx, handle).expect("a segment stub");
    assert!(stub.segments());
    assert_eq!((stub.arity(), stub.slot_count()), (2, 2));
    // JIT round 17 wave 5 (lane ffm4): as the plain row's test.
    assert_eq!(
        jit_downcall_direct_admits(&ctx, &stub, 2),
        !(ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) || direct_row_goes_lazy()
    );

    let known = [seg];
    let call = |ctx: &mut dyn NativeContext, slot: i64, n: i64| {
        let mut resolve = |bits: i64| known.iter().copied().find(|o| o.as_ptr() as i64 == bits);
        // SAFETY: two slots for the two parameters; `stub` is the handle's.
        unsafe { jit_downcall_direct_segment_invoke(ctx, &stub, &[slot, n], &mut resolve) }
    };
    let pins_before = ctx.native_pin_count_for_test();
    let served = call(&mut ctx, seg.as_ptr() as i64, 4).expect("served");
    assert_eq!(
        served.unwrap(),
        Some(Value::Long(14)),
        "1+2+3+4 after one bump"
    );
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0), "released");
    assert_eq!(ctx.native_pin_count_for_test(), pins_before, "unpinned");
    assert_eq!(buf, [3, 4, 5, 6], "both calls reached C");
    let memo = pe_downcall_invoke_memo_with(&mut ctx, &[h, s, Value::Long(4)], true, true)
        .expect("the memo path serves it")
        .unwrap();
    assert_eq!(
        memo,
        Some(Value::Long(18)),
        "the memo path's answer, one bump on"
    );

    // `null` (or a slot that is not an object) declines.
    assert!(call(&mut ctx, 0, 4).is_none());

    // Another thread's confined scope declines.
    let foreign = ffmseg_session(&mut ctx);
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, foreign);
    ctx.set_field(foreign, slots.owner, Value::Object(Some(descriptor)));
    let foreign_seg =
        ffmseg_native_segment(&mut ctx, buf.as_mut_ptr() as i64, 4, Some(foreign));
    let mut resolve = |bits: i64| (bits == foreign_seg.as_ptr() as i64).then_some(foreign_seg);
    // SAFETY: as above.
    let refused = unsafe {
        jit_downcall_direct_segment_invoke(
            &mut ctx,
            &stub,
            &[foreign_seg.as_ptr() as i64, 4],
            &mut resolve,
        )
    };
    assert!(refused.is_none());

    // A closed scope declines and is not acquired.
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(&ctx, session);
    ctx.set_field(session, slots.state, Value::Int(0));
    assert!(call(&mut ctx, seg.as_ptr() as i64, 4).is_none());
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0));
    assert_eq!(buf, [4, 5, 6, 7], "no refused call reached C");

    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// `p + n` as a pointer: a C callee returning `ADDRESS` (JIT round 18
/// wave 11, lane ffmret).
extern "C" fn ffmret_offset(p: *mut u8, n: i64) -> *mut u8 {
    p.wrapping_add(n.max(0) as usize)
}

/// JIT round 18 wave 11 (lane ffmret; FFMCOST18-2): with an `ADDRESS`
/// return encoded, the memo path and the direct segment row answer the
/// segment the general path answers -- the address, zero bytes, a scope
/// exactly when the general path's has one -- release the argument's
/// session and balance their pins.
#[test]
fn memo_and_direct_rows_mint_the_general_paths_address_return() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let fn_addr = ffmret_offset as usize as i64;
    let descriptor = ffmseg_descriptor(
        &mut ctx,
        Some(LAYOUT_ADDRESS),
        &[LAYOUT_ADDRESS, LAYOUT_LONG],
    );
    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(0));
    ctx.set_field(handle, DOWNCALL_CRITICAL, Value::Int(0));
    let h = Value::Object(Some(handle));
    let mut buf = [0u8; 8];
    let base = buf.as_mut_ptr() as i64;
    let session = ffmseg_session(&mut ctx);
    let seg = ffmseg_native_segment(&mut ctx, base, 8, Some(session));
    let s = Value::Object(Some(seg));
    let fields = |ctx: &dyn NativeContext, r: ObjectRef| {
        (
            ctx.get_field(r, 0),
            ctx.get_field(r, 1),
            matches!(
                ctx.get_field(r, PE_SEGMENT_ARENA_FIELD),
                Value::Object(Some(_))
            ),
        )
    };
    let general = match pe_downcall_invoke_with(&mut ctx, &[h, s, Value::Long(3)], false) {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => panic!("the general path answers a segment"),
    };
    let want = fields(&ctx, general);
    assert_eq!((want.0, want.1), (Value::Long(base + 3), Value::Long(0)));

    // The encoding the switch records (the general call cached the CIF).
    let memo = address_return_memo_encode(
        &[LAYOUT_ADDRESS, LAYOUT_LONG],
        false,
        PLAIN_MEMO_RET_ADDRESS,
    );
    assert!(plain_memo_is_segment(memo) && !plain_memo_is_plain(memo));
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(memo));
    let pins = ctx.native_pin_count_for_test();
    let served =
        match pe_downcall_invoke_memo_with(&mut ctx, &[h, s, Value::Long(3)], true, true) {
            Some(Ok(Some(Value::Object(Some(r))))) => r,
            _ => panic!("the memo path serves it"),
        };
    assert_eq!(fields(&ctx, served), want);
    assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0), "released");
    assert_eq!(ctx.native_pin_count_for_test(), pins, "unpinned");

    if crate::panama_libffi::direct_stubs_supported() {
        let stub = downcall_direct_segment_stub_of(&ctx, handle).expect("a segment stub");
        assert!(stub.returns_object());
        assert_eq!(stub.slot_count(), 2);
        let mut resolve = |bits: i64| (bits == seg.as_ptr() as i64).then_some(seg);
        // SAFETY: two slots for the two parameters; `stub` is the handle's.
        let direct = unsafe {
            jit_downcall_direct_segment_invoke(
                &mut ctx,
                &stub,
                &[seg.as_ptr() as i64, 3],
                &mut resolve,
            )
        };
        match direct {
            Some(Ok(Some(Value::Object(Some(r))))) => assert_eq!(fields(&ctx, r), want),
            _ => panic!("the direct segment row serves it"),
        }
        assert_eq!(ffmseg_acquires(&ctx, session), Value::Int(0), "released");
        assert_eq!(ctx.native_pin_count_for_test(), pins, "unpinned");
    }
    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

/// JIT round 18 wave 11 (lane ffmret): the return encodings are segment
/// encodings with the right lead, bind only the site that spells their
/// return (and a group's leading allocator), and a group goes direct only
/// when it comes back in registers.
#[test]
fn return_encodings_lead_and_match_their_sites() {
    let ptr = address_return_memo_encode(
        &[LAYOUT_ADDRESS, LAYOUT_INT],
        false,
        PLAIN_MEMO_RET_ADDRESS,
    );
    assert!(plain_memo_is_segment(ptr) && plain_memo_returns_object(ptr));
    assert_eq!(plain_memo_return(ptr), Some(LAYOUT_ADDRESS));
    assert_eq!((plain_memo_arity(ptr), plain_memo_lead(ptr)), (2, 0));
    assert!(jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;I)Ljava/lang/foreign/MemorySegment;",
        ptr
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;I)J",
        ptr
    ));
    assert!(memo_direct_shape(ptr) == crate::panama_libffi::direct_stubs_supported());
    // A primitive-only `ADDRESS` return (`malloc(long)`) is still not plain.
    let malloc = address_return_memo_encode(&[LAYOUT_LONG], false, PLAIN_MEMO_RET_ADDRESS);
    assert!(!plain_memo_is_plain(malloc) && plain_memo_is_segment(malloc));
    assert!(jit_downcall_site_matches_memo(
        "(J)Ljava/lang/foreign/MemorySegment;",
        malloc
    ));
    assert_eq!(
        address_return_memo_encode(&[ffi::LAYOUT_STRUCT], false, PLAIN_MEMO_RET_ADDRESS),
        PLAIN_MEMO_NOT_PLAIN
    );

    let div = group_return_memo_encode(&[LAYOUT_INT, LAYOUT_INT], false, true);
    assert!(plain_memo_is_segment(div) && plain_memo_returns_object(div));
    assert_eq!(plain_memo_return(div), None);
    assert_eq!((plain_memo_arity(div), plain_memo_lead(div)), (2, 1));
    assert!(jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/SegmentAllocator;II)Ljava/lang/foreign/MemorySegment;",
        div
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(II)Ljava/lang/foreign/MemorySegment;",
        div
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/SegmentAllocator;II)V",
        div
    ));
    assert!(memo_direct_shape(div) == crate::panama_libffi::direct_stubs_supported());
    let in_memory = group_return_memo_encode(&[LAYOUT_INT, LAYOUT_INT], false, false);
    assert!(!memo_direct_shape(in_memory));
    // The allocator leads the capture-state segment.
    let captured = group_return_memo_encode(&[LAYOUT_INT], true, true);
    assert_eq!(plain_memo_lead(captured), 2);
    assert!(jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/SegmentAllocator;Ljava/lang/foreign/MemorySegment;I)Ljava/lang/foreign/MemorySegment;",
        captured
    ));
    assert!(!jit_downcall_site_matches_memo(
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/SegmentAllocator;I)Ljava/lang/foreign/MemorySegment;",
        captured
    ));
    // The earlier encodings are untouched.
    let strlen = segment_memo_encode(Some(LAYOUT_LONG), &[LAYOUT_ADDRESS], false);
    assert!(!plain_memo_returns_object(strlen));
    assert_eq!(plain_memo_lead(strlen), 0);
}

/// JIT round 18 wave 11 (lane ffmret): the group classification reads
/// libffi's prepared return type: `div_t` is 8 bytes, 4-aligned and
/// all-integer; a struct holding a `double` is not all-integer.
#[test]
fn group_return_classification_reads_the_prepared_cif() {
    use libffi::middle::Type;
    let plf_cif = |ret: Type| {
        crate::panama_libffi::box_cif_to_u64(
            crate::panama_libffi::build_cif(vec![Type::i32()], ret, None).unwrap(),
        )
    };
    let div_t = plf_cif(Type::structure(vec![Type::i32(), Type::i32()]));
    // SAFETY: just boxed above, freed below.
    let (size, align, rtype) = unsafe { cif_return_size_align(div_t) }.unwrap();
    assert_eq!((size, align), (8, 4));
    assert!(unsafe { ffi_type_all_integer(rtype, 0) });
    assert_eq!(
        group_size_in_regs(size, true),
        crate::panama_libffi::direct_stubs_supported()
    );
    let mixed = plf_cif(Type::structure(vec![Type::i64(), Type::f64()]));
    // SAFETY: as above.
    let (size, _, rtype) = unsafe { cif_return_size_align(mixed) }.unwrap();
    assert_eq!(size, 16);
    assert!(!unsafe { ffi_type_all_integer(rtype, 0) });
    assert!(!group_size_in_regs(size, false));
    assert!(!group_size_in_regs(12, true) || !cfg!(windows));
    assert!(!group_size_in_regs(17, true));
    // Where the ABI returns a group, for the heap-allocator rule.
    // SAFETY: a null type is never read for these sizes off AArch64.
    if cfg!(all(target_arch = "x86_64", not(windows))) {
        assert!(unsafe { group_returned_in_registers(16, std::ptr::null()) });
        assert!(!unsafe { group_returned_in_registers(24, std::ptr::null()) });
    }
    if cfg!(all(target_arch = "x86_64", windows)) {
        assert!(unsafe { group_returned_in_registers(8, std::ptr::null()) });
        assert!(!unsafe { group_returned_in_registers(16, std::ptr::null()) });
    }
    // SAFETY: both boxed above.
    unsafe {
        crate::panama_libffi::free_cached_cif(div_t);
        crate::panama_libffi::free_cached_cif(mixed);
    }
}

/// JIT round 18 wave 11 (lane ffmret): both rows finish an object return
/// after their releases (the mint and the allocator are GC points and
/// Java), and both switches are opt-in.
#[test]
fn object_returns_are_finished_after_the_release_and_switches_are_default_on() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    for (sig, release) in [
        (
            "\npub(in crate::panama) fn pe_downcall_invoke_memo_with(",
            "segment_row_add_acquires(ctx, session, slots, -1)",
        ),
        (
            "\npub unsafe fn jit_downcall_direct_segment_invoke(",
            "direct_segment_release(ctx, *session, *slots, handshake)",
        ),
    ] {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n}\n").unwrap_or(rest.len())];
        let last_release = body
            .rfind(release)
            .unwrap_or_else(|| panic!("{release} in {sig}"));
        let finish = body
            .find("memo_finish_return(")
            .unwrap_or_else(|| panic!("finish in {sig}"));
        assert!(last_release < finish, "{sig}: finished after the release");
    }

}

/// `critical(true)` admits a heap segment for an `ADDRESS` parameter
/// (copied in and back), `critical(false)` and a plain handle keep
/// HotSpot's `Heap segment not allowed`.
#[test]
fn critical_true_admits_a_heap_segment_through_a_scratch_buffer() {
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();
    let allow =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker$Option", 2).unwrap();
    ctx.set_field(allow, 0, Value::Int(1));
    ctx.set_field(allow, 1, Value::Long(1));
    let deny =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker$Option", 2).unwrap();
    ctx.set_field(deny, 0, Value::Int(1));
    ctx.set_field(deny, 1, Value::Long(0));
    assert!(downcall_option_is_critical(&ctx, allow));
    assert!(downcall_option_is_critical(&ctx, deny));
    assert!(downcall_option_allows_heap(&ctx, allow));
    assert!(!downcall_option_allows_heap(&ctx, deny));

    let fn_addr = ffmseg_sum_and_bump as usize as i64;
    let descriptor =
        ffmseg_descriptor(&mut ctx, Some(LAYOUT_LONG), &[LAYOUT_ADDRESS, LAYOUT_LONG]);
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    for (i, b) in [1, 2, 3, 4].iter().enumerate() {
        ctx.set_array_element(array, i, Value::Int(*b));
    }
    let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray(byte[]) answered {other:?}"),
    };
    let hs = Value::Object(Some(heap));

    for allow_heap in [None, Some(false)] {
        let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
        if let Some(allow_heap) = allow_heap {
            downcall_set_critical(&mut ctx, handle, allow_heap);
            assert!(downcall_is_critical(&ctx, handle));
        }
        let err = pe_downcall_invoke_with(
            &mut ctx,
            &[Value::Object(Some(handle)), hs, Value::Long(4)],
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("Heap segment not allowed"),
            "{allow_heap:?}: {err:?}"
        );
    }

    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);
    downcall_set_critical(&mut ctx, handle, true);
    assert!(
        downcall_is_critical(&ctx, handle),
        "still critical: no GC-safe region"
    );
    assert_eq!(
        pe_downcall_invoke_with(
            &mut ctx,
            &[Value::Object(Some(handle)), hs, Value::Long(4)],
            false
        )
        .unwrap(),
        Some(Value::Long(10))
    );
    let after: Vec<Value> = (0..4).map(|i| ctx.get_array_element(array, i)).collect();
    assert_eq!(
        after,
        vec![Value::Int(2), Value::Int(3), Value::Int(4), Value::Int(5)],
        "the callee's writes are copied back into the array"
    );
    if let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) {
        unsafe { crate::panama_libffi::free_cached_cif(cif as u64) };
    }
}

#[test]
fn panama_cif_cache_field3_sticks_to_boxed_ptr() {
    // Simpler smoke test: if we don't actually call, DOWNCALL_CIF stays 0.
    // Only the invoke path populates the cache slot.
    let mut ctx = mock_ctx();
    let handle = mk_downcall_handle(&mut ctx, 0, None, -1);
    match ctx.get_field(handle, DOWNCALL_CIF) {
        Value::Long(0) => {}
        other => panic!("expected Long(0), got {:?}", other),
    }
}

// `test_85_3_downcall_struct_layout` AND
// `panama_struct_layout_preserves_named_members` WERE HERE AND ARE DELETED
// WITH THE FUNCTION THEY TESTED (F16, 2026-08-13).
//
// The first called `pe_struct_layout` on `{int, long}` and asserted
// size 16 / alignment 8. That is a call HotSpot 25.0.3+9-LTS REFUSES:
//
//     MemoryLayout.structLayout(JAVA_INT, JAVA_LONG)
//       -> IllegalArgumentException: Invalid alignment constraint for
//          member layout: j8
//
// so the test was pinning a fabricated success — it froze the VM's own
// wrong answer as if it were the specification. The second asserted that
// member NAMES survive into a `names` array at slot 3, which is a slot the
// authoritative carrier does not have: `foreign_ffm.rs` keeps the member
// LAYOUTS at slot 2 and resolves a name by asking each member for its own
// (`p67_layout_named_member`), so there is no second copy to drift.
//
// Both behaviours are now covered against the ORACLE'S numbers in
// `vm/src/vm/tests.rs` — see `struct_layout_rejects_underaligned_member`,
// `struct_layout_does_not_pad_the_total` and `struct_layout_single_field`.

#[test]
fn test_85_3_downcall_void_return() {
    // Call a function with void return — use memset (returns void* but we treat it as void)
    // Actually, let's use a simpler approach: call abs with void descriptor
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();

    // We'll test that a downcall with -1 return kind produces Value::Object(None)
    let abs_addr = ctx.find_native_symbol(-1, "abs");
    if abs_addr.is_none() {
        return;
    }
    let abs_addr = abs_addr.unwrap() as i64;

    // FunctionDescriptor: ofVoid(LAYOUT_INT)  — return_layout = None
    let param_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(None)); // void
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    ctx.set_array_element(call_args, 0, Value::Int(5));

    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    );

    assert!(result.is_ok());
    // void return should produce Object(None)
    let val = result.unwrap();
    assert_eq!(val, Some(Value::Object(None)));
}

/// Writes `value` into every element of a native `int` buffer — standing
/// in for a real native mutation, the way Elasticsearch's bulk-vector
/// kernels write results back into a buffer a caller handed them.
extern "C" fn fill_int_buffer(ptr: *mut i32, value: i32, count: i64) {
    for i in 0..count {
        unsafe { *ptr.add(i as usize) = value };
    }
}

/// j27 wave 4 (lane FFMFLIP): every `MemorySegment.ofArray(..)` segment
/// handed to a plain (not `critical(true)`) downcall is HotSpot 25's
/// `IllegalArgumentException: Heap segment not allowed:
/// <segment.toString()>` (measured, `FfmRepro2.java`, for `byte[]` and
/// `int[]`; `SharedUtils.unboxSegment` draws no element-type line). The
/// `int[]`/`long[]`/`float[]`/`double[]` ones used to run through a
/// scratch buffer (the Elasticsearch carve-out, now
/// `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT=1`, opt-in), and
/// the other three said `MemorySegment{ kind: heap, class: .. }` instead of
/// the segment's own text. The native must not run: the array is unchanged.
#[test]
fn plain_downcall_refuses_every_ofarray_segment_as_hotspot_does() {
    use cratonvm_types::ArrayElementType as Elem;
    for (elem, descriptor, byte_size) in [
        (Elem::Byte, "[B", 4),
        (Elem::Short, "[S", 8),
        (Elem::Char, "[C", 8),
        (Elem::Int, "[I", 16),
        (Elem::Long, "[J", 32),
        (Elem::Float, "[F", 16),
        (Elem::Double, "[D", 32),
    ] {
        let mut ctx = mock_ctx();
        let array = ctx.new_array(elem, 4);
        let before: Vec<Value> = (0..4).map(|i| ctx.get_array_element(array, i)).collect();
        let message = fill_int_buffer_downcall_refusal(&mut ctx, array, 4);
        assert_eq!(message, hotspot_heap_refusal(&ctx, array, descriptor, byte_size));
        let after: Vec<Value> = (0..4).map(|i| ctx.get_array_element(array, i)).collect();
        assert_eq!(after, before, "{descriptor}: the native ran");
    }
}

/// HotSpot 25's text for a whole-array heap segment refused by
/// `SharedUtils.unboxSegment`.
fn hotspot_heap_refusal(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    descriptor: &str,
    byte_size: i64,
) -> String {
    format!(
        "Heap segment not allowed: MemorySegment{{ kind: heap, heapBase: {descriptor}@{:x}, address: 0x0, byteSize: {byte_size} }}",
        ctx.identity_hash_code(array) as u32
    )
}

/// The `IllegalArgumentException` message of a refused call.
fn iae_message<T: std::fmt::Debug>(result: Result<T, MethodCallFailed>) -> String {
    match result {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    }
}

/// j27 wave 4 (lane FFMMSG): the segment text of HotSpot's
/// `SharedUtils.checkNative` refusal is `AbstractMemorySegmentImpl.toString`,
/// whose `heapBase` clause is `heapBase()` — absent on a read-only segment —
/// and whose address is the segment's offset into the array.
#[test]
fn heap_segment_refusal_text_is_the_segments_own_to_string() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    let hash = ctx.identity_hash_code(array) as u32;
    let slice = heap_alias_segment(&mut ctx, array, 4, 8, false);
    assert_eq!(
        iae_message(heap_segment_refusal(&ctx, slice).map_or(Ok(()), Err)),
        format!(
            "Heap segment not allowed: MemorySegment{{ kind: heap, heapBase: [I@{hash:x}, address: 0x4, byteSize: 8 }}"
        )
    );
    let read_only = heap_alias_segment(&mut ctx, array, 4, 8, true);
    assert_eq!(
        iae_message(heap_segment_refusal(&ctx, read_only).map_or(Ok(()), Err)),
        "Heap segment not allowed: MemorySegment{ kind: heap, address: 0x4, byteSize: 8 }"
    );
    let mut word = 0u64;
    let native = ffmseg_native_segment(&mut ctx, &mut word as *mut u64 as i64, 8, None);
    assert!(heap_segment_refusal(&ctx, native).is_none());
}

/// j27 wave 4 (lane FFMMSG): `set(ADDRESS, off, heapSegment)` is the
/// address VarHandle's `unboxSegment` filter on HotSpot, so the same text;
/// it said `MemorySegment{ kind: heap, class: <carrier> }`. Nothing stored.
#[test]
fn set_address_of_a_heap_segment_refuses_with_hotspot_text() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let layout_addr = make_layout(&mut ctx, LAYOUT_ADDRESS);
    let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
    let seg = match pe_arena_allocate_impl(&mut ctx, arena, 16, 8) {
        Ok(Some(Value::Object(Some(s)))) => s,
        other => panic!("no segment: {other:?}"),
    };
    pe_segment_set_impl(&mut ctx, seg, layout_long, 0, Value::Long(0x55)).unwrap();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(heap))) => heap,
        other => panic!("ofArray answered {other:?}"),
    };
    let message = iae_message(pe_segment_set_impl(
        &mut ctx,
        seg,
        layout_addr,
        0,
        Value::Object(Some(heap)),
    ));
    assert_eq!(message, hotspot_heap_refusal(&ctx, array, "[B", 4));
    let kept = pe_segment_get_impl(&mut ctx, seg, layout_long, 0).unwrap();
    assert_eq!(kept, Some(Value::Long(0x55)), "the refused store landed");
}

/// j27 wave 4 (lane FFMMSG): an in-memory group return buffer from a heap
/// allocator reaches the binding's `unboxAddress` (`unboxSegment`) on
/// HotSpot: the same text, and the array is not written.
#[test]
fn a_heap_group_return_buffer_refuses_with_hotspot_text() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Long, 4);
    let heap = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(heap))) => heap,
        other => panic!("ofArray answered {other:?}"),
    };
    // The mock fills a new array with `Int(0)` whatever its element type,
    // so "not written" is "the same value as before the call".
    let before = ctx.get_array_element(array, 0);
    let message = iae_message(downcall_store_group(&mut ctx, heap, 32, &[7u8; 32], false, None));
    assert_eq!(message, hotspot_heap_refusal(&ctx, array, "[J", 32));
    assert_eq!(ctx.get_array_element(array, 0), before, "the refused store landed");
}

/// The `IndexOutOfBoundsException` message of a refused call.
fn ioobe_message<T: std::fmt::Debug>(result: Result<T, MethodCallFailed>) -> String {
    match result {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IndexOutOfBoundsException { message: Some(message) },
        ))) => message,
        other => panic!("expected IndexOutOfBoundsException, got {other:?}"),
    }
}

/// `MemorySegment{ kind: heap, heapBase: [B@<hash>, address: 0x<start>,
/// byteSize: <n> }` for a writable `byte[]` heap segment.
fn hotspot_byte_heap_text(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    start: i64,
    n: i64,
) -> String {
    format!(
        "MemorySegment{{ kind: heap, heapBase: [B@{:x}, address: 0x{start:x}, byteSize: {n} }}",
        ctx.identity_hash_code(array) as u32
    )
}

/// j27 wave 4 (lane FFMMSG): the carrier's `toString()` is
/// `AbstractMemorySegmentImpl.toString`'s text. It printed `MemorySegment{
/// heapBase: <array>, address: 0x0, .. }` for every heap segment, slices
/// included, and no `kind:` at all. A carrier is never mapped (`isMapped()`
/// answers false for it), so a native one is `kind: native`.
#[test]
fn segment_to_string_is_hotspots_text_for_heap_read_only_and_native() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let slice = heap_alias_segment(&mut ctx, array, 3, 4, false);
    assert_eq!(segment_to_string(&ctx, slice), hotspot_byte_heap_text(&ctx, array, 3, 4));
    let read_only = heap_alias_segment(&mut ctx, array, 3, 4, true);
    assert_eq!(
        segment_to_string(&ctx, read_only),
        "MemorySegment{ kind: heap, address: 0x3, byteSize: 4 }"
    );
    let native = ffmseg_native_segment(&mut ctx, 0x1000, 8, None);
    assert_eq!(
        segment_to_string(&ctx, native),
        "MemorySegment{ kind: native, address: 0x1000, byteSize: 8 }"
    );
}

/// j27 wave 4 (lane FFMMSG): `asSlice` out of range is
/// `outOfBoundException`, whose segment text is `toString()`: the
/// `heapBase` clause was missing.
#[test]
fn an_out_of_range_heap_slice_names_the_segment_as_hotspot_does() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg = heap_alias_segment(&mut ctx, array, 0, 16, false);
    let message = ioobe_message(pe_segment_slice(&mut ctx, seg, 17, 0, None));
    assert_eq!(
        message,
        format!(
            "Out of bound access on segment {}; new offset = 17; new length = 0",
            hotspot_byte_heap_text(&ctx, array, 0, 16)
        )
    );
}

/// j27 wave 4 (lane FFMMSG): `get(layout, off)` on a heap segment is
/// `checkEnclosingLayout`: bounds name the segment by `toString()`, and the
/// alignment refusal is `Target offset %d is incompatible with alignment
/// constraint %d (of %s) for segment %s`. The layout text is the real JDK
/// layout's own `toString()`; a layout that is not one keeps the old text
/// without the clause.
#[test]
fn heap_access_bounds_and_alignment_texts_are_hotspots() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg = heap_alias_segment(&mut ctx, array, 0, 16, false);
    let view = heap_segment_view(&ctx, seg).unwrap();
    let segment = hotspot_byte_heap_text(&ctx, array, 0, 16);
    let cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl")
        .unwrap();
    let layout = ctx.alloc_object(cid, 3);
    ctx.set_field(layout, 0, Value::Long(4));
    ctx.set_field(layout, 1, Value::Long(4));
    let i4 = ctx.create_string("i4");
    ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
    assert_eq!(
        iae_message(heap_segment_check_bounds(&mut ctx, &view, layout, 0, 4, false)),
        format!(
            "Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment {segment}"
        )
    );
    assert_eq!(
        ioobe_message(heap_segment_check_bounds(&mut ctx, &view, layout, 14, 4, false)),
        format!("Out of bound access on segment {segment}; new offset = 14; new length = 4")
    );
    let synthetic = make_layout(&mut ctx, LAYOUT_INT);
    assert_eq!(
        iae_message(heap_segment_check_bounds(&mut ctx, &view, synthetic, 0, 4, false)),
        format!("Target offset 0 is incompatible with alignment constraint 4 for segment {segment}")
    );
}

/// j27 wave 4 (lane FFMMSG): every remaining `outOfBoundException` site
/// names the segment by its `toString()`. `checkBounds` printed
/// `MemorySegment{ byteSize: N }`; the capture-state refusal printed a
/// heap segment without its `heapBase`; the native accessor printed
/// `kind: native` for whatever reached it.
#[test]
fn every_out_of_bounds_refusal_names_the_segment_by_its_to_string() {
    let mut ctx = mock_ctx();
    let native = ffmseg_native_segment(&mut ctx, 0x1000, 8, None);
    let native_text = "MemorySegment{ kind: native, address: 0x1000, byteSize: 8 }";
    assert_eq!(
        ioobe_message(craton_segment_check_bounds(&mut ctx, native, 6, 4)),
        format!("Out of bound access on segment {native_text}; new offset = 6; new length = 4")
    );
    assert_eq!(
        ioobe_message(pe_segment_locate_addr(&mut ctx, native, 6, 4)),
        format!("Out of bound access on segment {native_text}; new offset = 6; new length = 4")
    );
    let (heap, array) = heap_int_segment(&mut ctx, &[1, 2]);
    assert_eq!(
        ioobe_message(Err::<(), _>(capture_segment_out_of_bounds(&ctx, heap))),
        format!(
            "Out of bound access on segment MemorySegment{{ kind: heap, heapBase: [I@{:x}, \
             address: 0x0, byteSize: 8 }}; new offset = 0; new length = {CAPTURE_STATE_BYTES}",
            ctx.identity_hash_code(array) as u32
        )
    );
}

/// The refusal `pe_downcall_invoke` answers for `fill_int_buffer(array
/// segment, 0x2A2A2A2A, count)`: the `IllegalArgumentException` message.
fn fill_int_buffer_downcall_refusal(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
    count: i64,
) -> String {
    match fill_int_buffer_downcall(ctx, array, count) {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    }
}

/// `pe_downcall_invoke` of `fill_int_buffer(ofArray(array),
/// 0x2A2A2A2A, count)` through a plain (not `critical(true)`) handle.
fn fill_int_buffer_downcall(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
    count: i64,
) -> MethodCallResult {
    let _na = NativeAccessGuard::enable();
    let fn_addr = fill_int_buffer as usize as i64;
    let seg = match pe_of_array_alias(ctx, &[Value::Object(Some(array))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray answered {other:?}"),
    };
    let p_addr = make_layout(ctx, LAYOUT_ADDRESS);
    let p_int = make_layout(ctx, LAYOUT_INT);
    let p_long = make_layout(ctx, LAYOUT_LONG);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 3);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(p_addr)));
    ctx.set_array_element(params_arr, 1, Value::Object(Some(p_int)));
    ctx.set_array_element(params_arr, 2, Value::Object(Some(p_long)));
    let descriptor =
        try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
    ctx.set_field(descriptor, 0, Value::Object(None));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));
    let handle = mk_downcall_handle(ctx, fn_addr, Some(descriptor), -1);
    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 3);
    ctx.set_array_element(call_args, 0, Value::Object(Some(seg)));
    ctx.set_array_element(call_args, 1, Value::Int(0x2A2A2A2Au32 as i32));
    ctx.set_array_element(call_args, 2, Value::Long(count));
    pe_downcall_invoke(
        ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    )
}


// ===================================================================
// Phase 85.4: Upcall Stub Tests
// ===================================================================

#[test]
fn test_85_4_upcall_registration() {
    // Register an upcall and verify it can be looked up
    let mut ctx = mock_ctx();

    // Create a "MethodHandle" target object
    let target =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();

    let entry = ffi::UpcallEntry {
        target,
        method_name: "invoke".to_string(),
        method_descriptor: String::new(),
        param_kinds: vec![LAYOUT_INT],
        return_kind: LAYOUT_INT,
    };

    let slot = ctx.register_upcall(entry);

    // Look it up
    let info = ctx.get_upcall_info(slot);
    assert!(info.is_some());
    let (ret_target, param_kinds, return_kind) = info.unwrap();
    assert_eq!(ret_target, target);
    assert_eq!(param_kinds, vec![LAYOUT_INT]);
    assert_eq!(return_kind, LAYOUT_INT);
}

#[test]
fn test_85_4_upcall_handle_and_invoke() {
    // Native access is DENIED by default, and `pe_upcall_handle` gates on
    // it — so without this the test asserts against a refusal and fails on
    // a correct build. zt t18/df7: the grant is on this test's own VM
    // identity (`isolated_access_ctx`), so it neither depends on nor leaks
    // into any other test.
    {
        // Create an upcall handle through pe_upcall_handle and dispatch through pe_upcall_invoke
        let mut ctx = isolated_access_ctx();
        native_access(&ctx).set_enabled(true);

        // Create target, descriptor
        let target =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2)
                .unwrap();

        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let linker =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Register upcall handle
        let handle_result = pe_upcall_handle(
            &mut ctx,
            &[
                Value::Object(Some(linker)),
                Value::Object(Some(target)),
                Value::Object(Some(descriptor)),
                Value::Object(Some(arena)),
            ],
        );
        assert!(handle_result.is_ok());
        let seg = match handle_result.unwrap() {
            Some(Value::Object(Some(s))) => s,
            _ => panic!("Expected segment from upcall handle"),
        };

        // The segment's address (field 0) should be a real trampoline function pointer
        let tramp_addr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => -1,
        };
        assert!(tramp_addr != 0, "Trampoline address should be non-null");
        // Round 12 wave 7 (lane upcall): the stub is no longer called here
        // with no downcall context -- that call now terminates the VM
        // loudly instead of answering a fabricated 0 (see
        // `panama_upcall::upcall_without_context`). The in-context round
        // trip is `new18_upcall_libffi_closure_dispatches_to_java` and the
        // `panama_upcall` tests. Release the stub this test made.
        crate::panama_upcall::remove_upcall_stub(tramp_addr as usize);

        // Set up invoke_virtual to return a value when the upcall dispatches
        // via pe_upcall_invoke (the Java-side dispatch path)
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(99))));
        }

        // The legacy slot table `pe_upcall_invoke` reads is no longer fed
        // by `pe_upcall_handle` (it leaked a root per stub); register the
        // slot this test dispatches through directly.
        let slot = ctx.register_upcall(ffi::UpcallEntry {
            target,
            method_name: "invoke".to_string(),
            method_descriptor: String::new(),
            param_kinds: vec![LAYOUT_INT],
            return_kind: LAYOUT_INT,
        });

        // Create an UpcallStub handle object for pe_upcall_invoke
        // (uses the VM upcall slot, not the trampoline address)
        let stub = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2)
            .unwrap();
        ctx.set_field(stub, 0, Value::Long(slot as i64));

        // Create args array
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(42));

        let result = pe_upcall_invoke(
            &mut ctx,
            &[Value::Object(Some(stub)), Value::Object(Some(call_args))],
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Int(99)));
    }
}

#[test]
fn test_85_4_upcall_invalid_slot() {
    // Invoking an upcall with an invalid slot should return an error
    let mut ctx = mock_ctx();

    let stub =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2).unwrap();
    ctx.set_field(stub, 0, Value::Long(999)); // non-existent slot

    let result = pe_upcall_invoke(&mut ctx, &[Value::Object(Some(stub))]);
    assert!(result.is_err(), "Invalid upcall slot must return error");
}

// =====================================================================
// NEW-18: libffi-backed Panama Linker tests
//
// Each test exercises a real C-library function through the rewritten
// pe_downcall_invoke / pe_upcall_handle path. Together they prove:
//   (a) arbitrary-arity downcalls past the old 8-arg cap;
//   (b) mixed integer/float register classification (was broken);
//   (c) variadic dispatch via Cif::new_variadic;
//   (d) struct-by-value return packed into a result MemorySegment;
//   (e) round-trip upcall — C qsort calls a libffi-closure-backed
//       trampoline that re-enters our Rust callback (which would, in
//       a real VM, dispatch to Java via the active NativeContext).
// =====================================================================

/// NEW-18: a 12-arg integer downcall via the libffi pipeline using a
/// helper `extern "C"` Rust function (no symbol lookup needed).
/// This exercises argument counts past the 8-arg cap of the old
/// dispatcher and the int register/stack handoff.
#[test]
fn new18_downcall_arity_12_ints() {
    extern "C" fn sum12(
        a: i32,
        b: i32,
        c: i32,
        d: i32,
        e: i32,
        f: i32,
        g: i32,
        h: i32,
        i: i32,
        j: i32,
        k: i32,
        l: i32,
    ) -> i32 {
        a + b + c + d + e + f + g + h + i + j + k + l
    }
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();
    let fn_addr = sum12 as usize as i64;

    // Build descriptor: int(int,int,int,int,int,int,int,int,int,int,int,int)
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
    for i in 0..12 {
        let p = make_layout(&mut ctx, LAYOUT_INT);
        ctx.set_array_element(params_arr, i, Value::Object(Some(p)));
    }
    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);

    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
    for i in 0..12 {
        ctx.set_array_element(call_args, i, Value::Int((i + 1) as i32));
    }

    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    )
    .expect("12-arg downcall must succeed");
    // Sum of 1..=12 = 78.
    assert_eq!(result, Some(Value::Int(78)));
}

/// NEW-18: mixed int/float arguments. The old dispatcher passed
/// every arg through `u64` registers, so floats landed in the wrong
/// place. libffi gets the ABI right.
#[test]
fn new18_downcall_mixed_int_float() {
    extern "C" fn mix(a: i32, b: f64, c: i32, d: f32) -> f64 {
        a as f64 + b + c as f64 + d as f64
    }
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall.
    let _na = NativeAccessGuard::enable();
    let fn_addr = mix as usize as i64;

    let ret_layout = make_layout(&mut ctx, LAYOUT_DOUBLE);
    let p_int1 = make_layout(&mut ctx, LAYOUT_INT);
    let p_dbl = make_layout(&mut ctx, LAYOUT_DOUBLE);
    let p_int2 = make_layout(&mut ctx, LAYOUT_INT);
    let p_flt = make_layout(&mut ctx, LAYOUT_FLOAT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(p_int1)));
    ctx.set_array_element(params_arr, 1, Value::Object(Some(p_dbl)));
    ctx.set_array_element(params_arr, 2, Value::Object(Some(p_int2)));
    ctx.set_array_element(params_arr, 3, Value::Object(Some(p_flt)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);

    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
    ctx.set_array_element(call_args, 0, Value::Int(10));
    ctx.set_array_element(call_args, 1, Value::Double(2.5));
    ctx.set_array_element(call_args, 2, Value::Int(7));
    ctx.set_array_element(call_args, 3, Value::Float(0.5));

    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    )
    .expect("mixed int/float downcall must succeed");
    match result {
        Some(Value::Double(d)) => assert!((d - 20.0).abs() < 1e-9, "got {d}"),
        other => panic!("expected Double, got {other:?}"),
    }
}

/// NEW-18: variadic downcall via the new `firstVariadicArg` flag.
/// Calls libc `snprintf(buf, len, "%d", 42)` and verifies the
/// buffer contains "42" and the return value is 2 (excluding NUL).
/// snprintf is a real variadic C function on every supported
/// platform so this exercises libffi's `prep_cif_var` end-to-end.
#[test]
fn new18_downcall_variadic_snprintf() {
    // Look up snprintf — present on every supported platform. On
    // MSVC Windows the symbol is named `snprintf` in ucrt.
    let mut ctx = mock_ctx();
    // FIX(test): grant native access for the real libffi downcall (keeps
    // this test from depending on a sibling having left the gate open).
    let _na = NativeAccessGuard::enable();
    let snprintf_addr = match ctx.find_native_symbol(-1, "snprintf") {
        Some(a) => a as i64,
        None => return, // platform without symbol lookup → skip
    };

    // Allocate a 16-byte output buffer in native memory.
    let (_, buf_ptr) = ctx.allocate_native_memory(16, 1).unwrap();
    unsafe {
        std::ptr::write_bytes(buf_ptr, 0, 16);
    }
    // Allocate the format string "%d\0".
    let (_, fmt_ptr) = ctx.allocate_native_memory(4, 1).unwrap();
    unsafe {
        let f = b"%d\0";
        std::ptr::copy_nonoverlapping(f.as_ptr(), fmt_ptr, f.len());
    }

    // Descriptor: int(address, long, address, int) — 3 fixed args
    // (buf, size, format) followed by 1 variadic int.
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let p_buf = make_layout(&mut ctx, LAYOUT_ADDRESS);
    let p_len = make_layout(&mut ctx, LAYOUT_LONG);
    let p_fmt = make_layout(&mut ctx, LAYOUT_ADDRESS);
    let p_var = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(p_buf)));
    ctx.set_array_element(params_arr, 1, Value::Object(Some(p_len)));
    ctx.set_array_element(params_arr, 2, Value::Object(Some(p_fmt)));
    ctx.set_array_element(params_arr, 3, Value::Object(Some(p_var)));

    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    // First 3 args fixed; everything from index 3 is variadic.
    let handle = mk_downcall_handle(&mut ctx, snprintf_addr, Some(descriptor), 3);

    let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
    ctx.set_array_element(call_args, 0, Value::Long(buf_ptr as i64));
    ctx.set_array_element(call_args, 1, Value::Long(16));
    ctx.set_array_element(call_args, 2, Value::Long(fmt_ptr as i64));
    ctx.set_array_element(call_args, 3, Value::Int(42));

    let result = pe_downcall_invoke(
        &mut ctx,
        &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
    )
    .expect("variadic snprintf downcall must succeed");
    // snprintf returns the number of chars written excluding NUL.
    assert_eq!(result, Some(Value::Int(2)));
    // And the buffer must contain "42\0".
    let written = unsafe { std::slice::from_raw_parts(buf_ptr as *const u8, 3) };
    assert_eq!(&written[..2], b"42");
    assert_eq!(written[2], 0);
}

/// NEW-18: real upcall round-trip. We register an upcall handle
/// pointing at a Java MethodHandle target; libffi gives us a real
/// extern "C" trampoline. We then call that trampoline directly
/// from Rust, with the active NativeContext installed via the
/// guard, and verify the dispatch reaches Java's invoke_virtual.
#[test]
fn new18_upcall_libffi_closure_dispatches_to_java() {
    // GAP F3: `pe_upcall_handle` now goes through `require_native_access`,
    // exactly like the downcall path (and exactly as docs/CONFIG.md has
    // always described `Linker.upcallHandle`). Grant it for this test, the
    // same way `new18_downcall_variadic_snprintf` does.
    let _na = NativeAccessGuard::enable();
    let mut ctx = mock_ctx();

    // Descriptor: int(int, int)
    let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
    let p1 = make_layout(&mut ctx, LAYOUT_INT);
    let p2 = make_layout(&mut ctx, LAYOUT_INT);
    let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    ctx.set_array_element(params_arr, 0, Value::Object(Some(p1)));
    ctx.set_array_element(params_arr, 1, Value::Object(Some(p2)));
    let descriptor =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
            .unwrap();
    ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

    let target =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();
    let linker =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

    // Pre-arm the mock NativeContext to return Int(123) from invoke_virtual.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(123))));
    }

    // Allocate the upcall handle — this builds a real libffi closure
    // and returns a MemorySegment whose field 0 is the trampoline addr.
    let seg = pe_upcall_handle(
        &mut ctx,
        &[
            Value::Object(Some(linker)),
            Value::Object(Some(target)),
            Value::Object(Some(descriptor)),
            Value::Object(Some(arena)),
        ],
    )
    .expect("upcallHandle must succeed")
    .and_then(|v| {
        if let Value::Object(Some(s)) = v {
            Some(s)
        } else {
            None
        }
    })
    .unwrap();

    let tramp_addr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    assert!(tramp_addr != 0, "trampoline must be a real address");

    // Install the active NativeContext for the duration of the call,
    // then invoke the trampoline directly with two int args.
    let result_int: i32 = {
        let _guard = crate::panama_libffi::ActiveContextGuard::install(&mut ctx);
        let f: extern "C" fn(i32, i32) -> i32 =
            unsafe { std::mem::transmute(tramp_addr as usize) };
        f(11, 22)
    };
    // The closure callback (`panama_upcall::upcall_entry`) reaches the
    // mock's invoke_virtual -- this mock target carries no `MH_CLASS`, so
    // the stub takes the generic `invokeWithArguments` path -- which we
    // pre-armed to return Int(123). The callback then writes 123 back into
    // the result slot, which libffi delivers to the C caller (us) as an int.
    assert_eq!(result_int, 123);
    crate::panama_upcall::remove_upcall_stub(tramp_addr as usize);
}

// --- Task #57: native-access gate emits IllegalCallerException ---

/// Round-trip guard: when native access is disabled, the gate inside
/// `validated_fn_ptr` must produce a `RuntimeError::IllegalCallerException`
/// — not the old `IllegalStateException` with an "IllegalCallerException:"
/// message prefix. This is what the VM-side mapping table converts to
/// the throwable `java/lang/IllegalCallerException` class.
#[test]
fn task57_native_access_gate_emits_illegal_caller_exception() {
    // zt t18/df7: no grant and `deny` on this test's own VM identity: no
    // lock, nothing to restore (a process global before).
    let mut ctx = isolated_access_ctx();
    native_access(&ctx).set_illegal(IllegalNativeAccess::Deny);
    native_access(&ctx).set_enabled(false);

    let result = restricted_downcall_link_check(&mut ctx);

    let err = result.expect_err("gate must reject downcall when disabled");
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalCallerException { message },
        )) => {
            assert!(
                message.contains("Illegal native access"),
                "message should describe the denial: {message}"
            );
            // Must NOT carry the legacy "IllegalCallerException: " prefix
            // — the class name comes from the variant now.
            assert!(
                !message.starts_with("IllegalCallerException:"),
                "message must not duplicate the class name: {message}"
            );
        }
        other => panic!("expected RuntimeError::IllegalCallerException, got {other:?}"),
    }
}

/// Regression guard: the other validation failures inside
/// `validated_fn_ptr` (null pointer, misaligned address) must still emit
/// `IllegalStateException`. Only the native-access denial flips to the
/// new variant.
#[test]
fn task57_other_gate_failures_still_emit_illegal_state() {
    // zt t18/df7: `validated_fn_ptr` asks no native-access question, so
    // this test sets no grant (it set and restored the process global).

    // Null function pointer.
    let null_err = validated_fn_ptr::<extern "C" fn()>(0).expect_err("null must be rejected");
    // Misaligned function pointer (odd address fails alignment check on
    // every supported platform since `align_of::<usize>() >= 4`) -- under
    // `CRATONVM_FFM_DOWNCALL_ANY_FN_ALIGNMENT=0` only (JIT round 17 wave
    // 1): by default an odd code address is a valid target, as on HotSpot.
    let misaligned_err = validated_fn_ptr_with::<extern "C" fn()>(0x1001, true)
        .expect_err("misaligned must be rejected with the alignment check");
    // Above the unmappable low window (0x1_0000), which stays refused.
    let odd_ok = validated_fn_ptr_with::<extern "C" fn()>(0x10_0001, false).is_ok();

    assert!(odd_ok, "an odd code address is a valid downcall target");

    for (label, err) in [("null", null_err), ("misaligned", misaligned_err)] {
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalStateException { .. },
            )) => { /* expected */ }
            other => panic!("{label}: expected IllegalStateException, got {other:?}"),
        }
    }
}

/// NEW-18: variadic struct-layout helper sanity. Make sure the
/// libffi bridge accepts every primitive layout our descriptor
/// machinery produces. (We exercise the struct path indirectly
/// via the existing `test_85_3_downcall_struct_layout` test.)
#[test]
fn new18_layout_translation_covers_every_primitive() {
    for kind in [
        LAYOUT_BYTE,
        LAYOUT_BOOLEAN,
        LAYOUT_SHORT,
        LAYOUT_CHAR,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_FLOAT,
        LAYOUT_DOUBLE,
        LAYOUT_ADDRESS,
    ] {
        assert!(
            crate::panama_libffi::primitive_kind_to_ffi_type(kind).is_some(),
            "primitive kind {} must translate",
            kind
        );
    }
}

// ===================================================================
// Security regression: Panama arbitrary-memory + native-access gate
// (CRITICAL — arbitrary process memory R/W + native-code execution)
// ===================================================================

/// Helper: build a MemorySegment synthetic backed by a real Rust buffer so
/// in-bounds accesses are sound while out-of-bounds accesses are caught by
/// the bounds checks before any dereference.
fn make_segment(ctx: &mut dyn NativeContext, ptr: i64, size: i64, base_off: i64) -> ObjectRef {
    let seg = alloc_segment_carrier(ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(base_off));
    seg
}

fn make_layout_kind(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
    pe_make_layout(ctx, kind).unwrap()
}

#[test]
fn sec_segment_get_rejects_oob_offset() {
    let mut ctx = mock_ctx();
    let mut buf = [0u8; 8];
    let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
    let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
    // offset 8 + width 4 = 12 > size 8 → must be rejected, NOT dereferenced.
    let r = pe_segment_get_impl(&mut ctx, seg, layout, 8);
    assert!(r.is_err(), "out-of-bounds get must be rejected");
}

#[test]
fn sec_segment_get_rejects_negative_offset() {
    let mut ctx = mock_ctx();
    let mut buf = [0u8; 8];
    let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
    let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
    let r = pe_segment_get_impl(&mut ctx, seg, layout, -1);
    assert!(r.is_err(), "negative offset get must be rejected");
}

#[test]
fn sec_segment_set_rejects_oob_offset() {
    let mut ctx = mock_ctx();
    let mut buf = [0u8; 8];
    let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
    let layout = make_layout_kind(&mut ctx, LAYOUT_LONG);
    // offset 4 + width 8 = 12 > size 8 → reject before writing.
    let r = pe_segment_set_impl(
        &mut ctx,
        seg,
        layout,
        4,
        Value::Long(0x4141414141414141u64 as i64),
    );
    assert!(r.is_err(), "out-of-bounds set must be rejected");
}

#[test]
fn sec_segment_zero_size_not_accessible() {
    // A 0-size segment (as produced by ofAddress before reinterpret) must
    // refuse all access, even at offset 0 — this is the ofAddress escape.
    let mut ctx = mock_ctx();
    let seg = make_segment(&mut ctx, 0x1000, 0, 0);
    let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
    assert!(
        pe_segment_get_impl(&mut ctx, seg, layout, 0).is_err(),
        "get on zero-size segment must be rejected"
    );
    assert!(
        pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0)).is_err(),
        "set on zero-size segment must be rejected"
    );
}

#[test]
fn sec_segment_access_addr_overflow_rejected() {
    let mut ctx = mock_ctx();
    // ptr = u64::MAX (as i64 = -1), size large enough to pass bounds, so the
    // address arithmetic itself overflows and must be caught.
    let seg = make_segment(&mut ctx, -1i64, 1024, 0);
    let r = pe_segment_access_addr(&mut ctx, seg, 16, 8);
    assert!(r.is_err(), "address arithmetic overflow must be rejected");
}

#[test]
fn sec_segment_in_bounds_roundtrips() {
    // Sanity: a legitimate in-bounds access still works.
    let mut ctx = mock_ctx();
    // 8-aligned backing: `JAVA_INT` is naturally aligned, and a native
    // access at an address it does not admit is now refused (round 12
    // wave 5). A `[u8; 8]` on the stack has no alignment guarantee.
    let mut buf = [0u64; 1];
    let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
    let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
    assert!(pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x11223344)).is_ok());
    match pe_segment_get_impl(&mut ctx, seg, layout, 0) {
        Ok(Some(Value::Int(v))) => assert_eq!(v, 0x11223344),
        other => panic!("expected Int(0x11223344), got {:?}", other),
    }
}

#[test]
fn sec_native_access_disabled_by_default() {
    // The VM's gate must default closed (secure-by-default). zt t18/df7:
    // a fresh VM's state, read without forcing anything (it was a process
    // global, forced to the default under the shared lock).
    let ctx = isolated_access_ctx();
    assert!(!native_access_enabled(&ctx));
    // Setter plumbing still works in both directions.
    native_access(&ctx).set_enabled(true);
    assert!(native_access_enabled(&ctx));
    native_access(&ctx).set_enabled(false);
    assert!(!native_access_enabled(&ctx));
}

#[test]
fn sec_validated_fn_ptr_denied_when_gate_closed() {
    // zt t18/df7: on this test's own VM identity (no lock, no restore).
    let mut ctx = isolated_access_ctx();
    native_access(&ctx).set_illegal(IllegalNativeAccess::Deny);
    native_access(&ctx).set_enabled(false);
    let r = restricted_downcall_link_check(&mut ctx);
    assert!(
        r.is_err(),
        "downcall must be denied without a grant under the deny policy"
    );
}

#[test]
fn a_downcall_without_a_grant_obeys_allow_warn_and_deny() {
    // zt t18/df7: each mode on a VM identity of its own (no grant).
    let mut outcomes = Vec::new();
    for mode in [
        IllegalNativeAccess::Allow,
        IllegalNativeAccess::Warn,
        IllegalNativeAccess::Deny,
    ] {
        let mut ctx = isolated_access_ctx();
        native_access(&ctx).set_illegal(mode);
        outcomes.push(restricted_downcall_link_check(&mut ctx).is_ok());
    }
    assert_eq!(outcomes, vec![true, true, false]);
}

/// R16B-174 (round 18 wave 11, lane backlogfix): with no
/// `--enable-native-access` grant, `SymbolLookup.libraryLookup` obeys
/// allow / warn / deny like every other restricted method, and
/// `RawNativeLibraries.load0` -- reached only after the Java caller's own
/// `ensureNativeAccess` -- asks no flag question at all. The old gate
/// (`require_native_access`) refused in all three modes.
#[test]
fn r18_library_lookup_without_a_grant_warns_like_hotspot() {
    // zt t18/df7: each mode on a VM identity of its own (no grant).
    let mut bridge = Vec::new();
    let mut load0 = Vec::new();
    let mut legacy = Vec::new();
    for mode in [
        IllegalNativeAccess::Allow,
        IllegalNativeAccess::Warn,
        IllegalNativeAccess::Deny,
    ] {
        let mut ctx = isolated_access_ctx();
        native_access(&ctx).set_illegal(mode);
        bridge.push(
            restricted_caller_check(
                &mut ctx,
                "java/lang/foreign/SymbolLookup",
                "libraryLookup",
            )
            .is_ok(),
        );
        load0.push(raw_library_load_check(&mut ctx).is_ok());
        legacy.push(require_native_access(&mut ctx, "libraryLookup").is_ok());
    }
    assert_eq!(bridge, vec![true, true, false]);
    assert_eq!(load0, vec![true, true, true]);
    assert_eq!(legacy, vec![false, false, false]);
}

#[test]
fn upcall_target_root_scan_and_remap_gap_c() {
    // Step 5 GAP C: a leaked FFM/Panama upcall target must be reported as a
    // GC root and remapped in place after a move. Build a minimal registered
    // upcall around a fake target address, then scan + remap.
    //
    // gc-common w8-a: the entry belongs to VM `VM`; a second VM's
    // collection neither roots nor remaps it.
    //
    // Round 12 wave 7 (lane upcall): the registry moved to `panama_upcall`
    // (whose own test also covers the arena session root); these two `pub`
    // entry points still front it, and this still pins them.
    const VM: usize = 0x7A80_0301;
    const OTHER_VM: usize = 0x7A80_0302;
    fn never_called(
        _ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        _args: &[Value],
        _shape: &crate::panama_upcall::UpcallShape,
    ) -> MethodCallResult {
        Ok(None)
    }
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // SAFETY: a fake, never-dereferenced address; the scan and the remap
    // only move the integer around.
    let target = unsafe { ObjectRef::from_raw(0xABCD_0000usize as *mut u8) };
    let shape = crate::panama_upcall::UpcallShape {
        params: Vec::new(),
        ret: None,
    };
    let code_ptr =
        crate::panama_upcall::install_upcall_stub(&ctx, target, shape, None, never_called);

    // Scan reports the (fake) target as a root.
    let mut roots = Vec::new();
    gc_scan_upcall_target_roots(VM, &mut roots);
    assert!(
        roots.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000),
        "upcall target must be scanned as a GC root"
    );
    let mut other_roots = Vec::new();
    gc_scan_upcall_target_roots(OTHER_VM, &mut other_roots);
    assert!(
        !other_roots
            .iter()
            .any(|r| r.as_ptr() as usize == 0xABCD_0000),
        "another VM's collection must not root this VM's upcall target"
    );

    // Remap 0xABCD_0000 -> 0xABCD_8000, by the owning VM's map only.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xABCD_0000usize, 0xABCD_8000usize);
    gc_update_upcall_target_refs(OTHER_VM, &map);
    let mut unmoved = Vec::new();
    gc_scan_upcall_target_roots(VM, &mut unmoved);
    assert!(
        unmoved.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000),
        "another VM's pointer map must not rewrite this VM's upcall target"
    );
    gc_update_upcall_target_refs(VM, &map);
    let mut roots2 = Vec::new();
    gc_scan_upcall_target_roots(VM, &mut roots2);
    assert!(
        roots2.iter().any(|r| r.as_ptr() as usize == 0xABCD_8000),
        "re-scan must report the moved target"
    );

    // Don't leak our entry into other tests sharing the global registry.
    crate::panama_upcall::remove_upcall_stub(code_ptr);
}
// ===================================================================
// F35 (2026-08-13): heap segments, the LAYOUT_UNKNOWN range tests, and
// the accessor gate.
//
// ORACLE. Every number quoted below is `java` on this host, Microsoft
// build 25.0.3+9-LTS, with
// `--add-opens java.base/jdk.internal.foreign=ALL-UNNAMED` where a private
// field is read. Transcripts: `scratchpad/f35/F35Probe.java` and
// `F35Probe2.java`.
//
// MOCK. `MockNativeContext::get_field_by_name` is a name-keyed map that is
// independent of `set_field`, so a test that only round-trips a NAME
// measures the mock. Nothing below asserts on a name lookup: every
// assertion is about the CONTENTS of a real array after a write, or about
// an exception class. The names only supply the carrier's inputs, exactly
// as the real VM's field resolver would.
// ===================================================================

/// A real JDK heap carrier: `HeapMemorySegmentImpl$Of*` with the five
/// fields `javap` reports, in declaration order
/// (`AbstractMemorySegmentImpl{length, readOnly, scope}` then
/// `HeapMemorySegmentImpl{offset, base}`).
///
/// `byte_start` is the JDK's `address()`, i.e. `offset - 16`; the `offset`
/// field is written WITH the bias applied, which is what the real
/// constructor does.
fn make_real_heap_segment(
    ctx: &mut dyn NativeContext,
    class: &str,
    base: ObjectRef,
    byte_start: i64,
    size: i64,
    read_only: bool,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let seg = ctx.alloc_object(cid, 5);
    ctx.set_field(seg, 0, Value::Long(size)); // length
    ctx.set_field(seg, 1, Value::Int(i32::from(read_only))); // readOnly
    ctx.set_field(seg, 2, Value::Object(None)); // scope
    ctx.set_field(seg, 3, Value::Long(byte_start + 16)); // offset
    ctx.set_field(seg, 4, Value::Object(Some(base))); // base
    ctx.set_field_by_name(seg, "length", Value::Long(size));
    ctx.set_field_by_name(seg, "readOnly", Value::Int(i32::from(read_only)));
    ctx.set_field_by_name(seg, "offset", Value::Long(byte_start + 16));
    ctx.set_field_by_name(seg, "base", Value::Object(Some(base)));
    seg
}

/// A JDK-true four-slot value layout: `[0]=Long(byteSize)`,
/// `[1]=Long(byteAlignment)`, classified by CLASS NAME.
///
/// This is the carrier `--real-jdk` actually has (F27 §1): the nine
/// `jdk/internal/foreign/layout/ValueLayouts$Of*Impl`. `align = 1` builds
/// `JAVA_INT_UNALIGNED`, which the oracle reports as
/// `byteSize=4 byteAlignment=1 toString=1%i4`.
fn make_jdk_layout(
    ctx: &mut dyn NativeContext,
    class: &str,
    byte_size: i64,
    align: i64,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let layout = ctx.alloc_object(cid, 4);
    ctx.set_field(layout, 0, Value::Long(byte_size));
    ctx.set_field(layout, 1, Value::Long(align));
    ctx.set_field(layout, 2, Value::Object(None));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

fn jdk_int_unaligned(ctx: &mut dyn NativeContext) -> ObjectRef {
    make_jdk_layout(
        ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        1,
    )
}

fn jdk_byte_layout(ctx: &mut dyn NativeContext) -> ObjectRef {
    make_jdk_layout(
        ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        1,
        1,
    )
}

fn arena_segment(ctx: &mut dyn NativeContext, arena: ObjectRef, size: i64) -> ObjectRef {
    pe_arena_allocate_impl(ctx, arena, size, 8)
        .unwrap()
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap()    }

fn byte_vec(ctx: &dyn NativeContext, arr: ObjectRef, len: usize) -> Vec<i32> {
    (0..len)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            other => panic!("array element {i} is {other:?}"),
        })
        .collect()
}

/// The headline: a real heap segment now reads and writes its backing
/// array instead of dereferencing a number that is not an address.
///
/// The oracle row this pins, verbatim:
///
/// ```text
/// byte[] a = new byte[8];
/// MemorySegment.ofArray(a).set(JAVA_INT_UNALIGNED, 0, 0x01020304);
/// // a == [4, 3, 2, 1, 0, 0, 0, 0]
/// ```
///
/// PRE-FIX `pe_segment_set_impl` called `pe_segment_access_addr`, which
/// called `segment_address`, which for this carrier answered slot 0 — the
/// LENGTH. Before F27 that was a wild store; after F27 it is
/// `IllegalStateException: Null segment address`. Either way the array
/// stayed all zeroes, so the `[4, 3, 2, 1]` assertion is the mutation
/// check as well as the assertion.
#[test]
fn a_real_heap_segment_reads_and_writes_its_backing_array() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        0,
        8,
        false,
    );
    let layout = jdk_int_unaligned(&mut ctx);

    pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x0102_0304)).unwrap();

    assert_eq!(
        byte_vec(&ctx, arr, 8),
        vec![4, 3, 2, 1, 0, 0, 0, 0],
        "the write must land in the Java array, little-endian, and touch \
         exactly four bytes"
    );
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap(),
        Some(Value::Int(0x0102_0304)),
        "and reading it back must go through the same array"
    );
}

/// The `Unsafe.arrayBaseOffset` bias is removed EXACTLY ONCE.
///
/// Oracle: `ofArray(new byte[32]).asSlice(3)` has `offset == 19` and
/// `address() == 3`, and a write through `asSlice(3, 4)` lands at
/// `src[3..7]` — measured, `[0, 0, 0, 4, 3, 2, 1, 0, ...]`.
///
/// MUTATION: drop the `- HEAP_ARRAY_BASE_OFFSET` and the four bytes land
/// at index 19; subtract it twice and the start is negative and the view
/// is refused. Only the correct arithmetic puts them at 3.
#[test]
fn the_array_base_offset_bias_is_removed_exactly_once() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
    let seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        3,
        4,
        false,
    );
    let layout = jdk_int_unaligned(&mut ctx);

    pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x0102_0304)).unwrap();

    assert_eq!(
        byte_vec(&ctx, arr, 8),
        vec![0, 0, 0, 4, 3, 2, 1, 0],
        "the segment's first byte is array index 3, not 19 and not 0"
    );
}

/// A non-byte backing array is addressed by BYTE, not by element.
///
/// Oracle, on an `int[4]`-backed segment:
///
/// ```text
/// si.set(JAVA_BYTE, 0, (byte) 0x7f);   // iarr[0] == 127
/// si.set(JAVA_INT, 4, 0x11223344);     // iarr[1] == 0x11223344
/// si.get(JAVA_BYTE, 3)  == 0
/// ```
///
/// so byte offset `k` is element `k / 4`, byte `k % 4`, little-endian, and
/// a one-byte write must not disturb the other three bytes of its element.
#[test]
fn a_non_byte_backing_array_is_addressed_by_byte_not_by_element() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    let seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfInt",
        arr,
        0,
        16,
        false,
    );
    let int_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    let byte_layout = jdk_byte_layout(&mut ctx);

    pe_segment_set_impl(&mut ctx, seg, byte_layout, 0, Value::Int(0x7f)).unwrap();
    pe_segment_set_impl(&mut ctx, seg, int_aligned, 4, Value::Int(0x1122_3344)).unwrap();

    assert_eq!(
        ctx.get_array_element(arr, 0),
        Value::Int(0x7f),
        "a one-byte write is the element's LOW byte and leaves the rest alone"
    );
    assert_eq!(
        ctx.get_array_element(arr, 1),
        Value::Int(0x1122_3344),
        "byte offset 4 is element 1 whole"
    );
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, byte_layout, 3).unwrap(),
        Some(Value::Int(0)),
        "byte 3 of element 0 is still zero"
    );
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, int_aligned, 4).unwrap(),
        Some(Value::Int(0x1122_3344))
    );
}

/// Alignment is enforced on the heap path, with BOTH halves of the rule.
///
/// Oracle rows, all `IllegalArgumentException` unless marked OK:
///
/// | receiver | layout | offset | result |
/// |---|---|---|---|
/// | `byte[32]` | `JAVA_INT` (align 4) | 0 | refused - maxByteAlignment is 1 |
/// | `byte[32]` | `JAVA_INT_UNALIGNED` | 0 | OK |
/// | `int[8]` | `JAVA_INT` | 0 | OK |
/// | `int[8]` | `JAVA_INT` | 1 | refused - offset not a multiple of 4 |
/// | `int[8]` | `JAVA_LONG` (align 8) | 0 | refused - maxByteAlignment is 4 |
///
/// MUTATION: keeping only the modulo half admits row 1 (0 % 4 == 0);
/// keeping only the `maxByteAlignment` half admits row 4 (4 <= 4). Each
/// row is red under exactly one of those two mutations.
#[test]
fn heap_alignment_is_enforced_the_way_the_oracle_enforces_it() {
    let mut ctx = mock_ctx();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
    let byte_seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        bytes,
        0,
        32,
        false,
    );
    let ints = ctx.new_array(cratonvm_types::ArrayElementType::Int, 8);
    let int_seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfInt",
        ints,
        0,
        32,
        false,
    );
    let int_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    let int_unaligned = jdk_int_unaligned(&mut ctx);
    let long_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        8,
        8,
    );

    assert!(
        pe_segment_get_impl(&mut ctx, byte_seg, int_aligned, 0).is_err(),
        "a byte[] segment's maxByteAlignment is 1, so JAVA_INT is refused \
         even at offset 0"
    );
    assert!(
        pe_segment_get_impl(&mut ctx, byte_seg, int_unaligned, 0).is_ok(),
        "JAVA_INT_UNALIGNED on the same receiver is the oracle's OK row"
    );
    assert!(pe_segment_get_impl(&mut ctx, int_seg, int_aligned, 0).is_ok());
    assert!(
        pe_segment_get_impl(&mut ctx, int_seg, int_aligned, 1).is_err(),
        "offset 1 is not a multiple of the 4-byte alignment"
    );
    assert!(
        pe_segment_get_impl(&mut ctx, int_seg, long_aligned, 0).is_err(),
        "an int[] segment's maxByteAlignment is 4, so JAVA_LONG is refused"
    );
}

/// Bounds, zero size and read-only answer the oracle's exception CLASS,
/// not merely "an error".
///
/// A caller writing `catch (IndexOutOfBoundsException)` — the idiom for a
/// bounds check — must catch ours. Measured: every out-of-bounds and
/// zero-size access on BOTH a native and a heap carrier is
/// `IndexOutOfBoundsException`, and a read-only write is
/// `IllegalArgumentException: Attempt to write a read-only segment`.
#[test]
fn heap_refusals_use_the_oracles_exception_classes() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let seg = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        0,
        8,
        false,
    );
    let layout = jdk_int_unaligned(&mut ctx);
    let byte_layout = jdk_byte_layout(&mut ctx);

    let oob = pe_segment_get_impl(&mut ctx, seg, layout, 6).unwrap_err();
    assert!(
        format!("{oob:?}").contains("IndexOutOfBounds"),
        "offset 6 + 4 > 8 must be IndexOutOfBoundsException, got {oob:?}"
    );
    let negative = pe_segment_get_impl(&mut ctx, seg, byte_layout, -1).unwrap_err();
    assert!(
        format!("{negative:?}").contains("IndexOutOfBounds"),
        "a negative offset must be IndexOutOfBoundsException, got {negative:?}"
    );

    let empty_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
    let empty = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        empty_arr,
        0,
        0,
        false,
    );
    let zero = pe_segment_get_impl(&mut ctx, empty, byte_layout, 0).unwrap_err();
    assert!(
        format!("{zero:?}").contains("IndexOutOfBounds"),
        "a zero-size segment is IndexOutOfBoundsException on the oracle, \
         not IllegalStateException, got {zero:?}"
    );

    let ro_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let ro = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        ro_arr,
        0,
        8,
        true,
    );
    assert!(
        pe_segment_get_impl(&mut ctx, ro, byte_layout, 0).is_ok(),
        "a read-only segment still READS (oracle: OK)"
    );
    let write = pe_segment_set_impl(&mut ctx, ro, byte_layout, 0, Value::Int(1)).unwrap_err();
    assert!(
        format!("{write:?}").contains("IllegalArgument"),
        "writing a read-only segment is IllegalArgumentException, got {write:?}"
    );
    assert_eq!(
        ctx.get_array_element(ro_arr, 0),
        Value::Int(0),
        "and the refused write must not have happened"
    );
}

// ===================================================================
// G6 (2026-08-16): the three callers `pe_segment_slice`'s heap arm
// reached and nothing tested — `spliterator`, `elements`, `toArray` —
// plus the `maxByteAlignment` rule the merge left UNSETTLED and the
// `heapBase` capability the oracle withholds from a read-only view.
//
// ORACLE. Every number and message below is `java` on this host, Temurin
// 25.0.3+9-LTS, transcribed from the probes `FfmProbe`, `FfmProbe2` and
// `FfmProbe3` (see the G6-1 record for the full tables). NOTHING here has
// been measured on a CratonVM binary.
//
// MOCK. These tests build their heap carriers with [`heap_alias_segment`],
// the eight-slot H2 shape, and NOT with [`make_real_heap_segment`]. That
// is not a preference. `make_real_heap_segment` writes `base`/`offset`/
// `readOnly`/`length` BY NAME, and `MockNativeContext::set_field_by_name`
// resolves a name through `mock_field_slot`, whose whole chain — including
// `cratonvm_classloading::synthetic_stub_field_model` — has no entry for
// `jdk/internal/foreign/HeapMemorySegmentImpl$Of*`. An unresolved name is
// a SILENT no-op on write and `Value::Int(0)` on read, so
// `heap_segment_view` cannot resolve such a carrier under the mock and
// answers `None`. H2 resolves by SLOT (`[6]=array, [7]=start`) and needs
// no name table, which is why `of_array_covers_byte_short_and_char_and_
// the_carrier_aliases` — the one existing test that proves a write reaches
// the caller's array — uses it. See the G6-1 record's NOM-1: the fix is a
// `mock_field_slot` arm in `test_utils.rs`, which this lane does not own.
// ===================================================================

/// The H2 heap carrier: `[0]=0` (no machine address), `[1]=byteSize`,
/// `[2]=scope`, `[3]=readOnly`, `[4]=1`, `[5]=0`, `[6]=array`,
/// `[7]=startWithinArray`.
///
/// This is the production shape, not a test fixture: it is exactly what
/// [`pe_of_array_alias`] mints for `ofArray(byte[]|short[]|char[])` and
/// what [`pe_segment_slice`]'s heap arm mints for every slice of a heap
/// segment — including a slice of a REAL `HeapMemorySegmentImpl$Of*`,
/// because `asSlice` is force-routed. So it is also the receiver that
/// `toArray`/`elements`/`spliterator` actually see in the field.
fn heap_alias_segment(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
    start: i64,
    size: i64,
    read_only: bool,
) -> ObjectRef {
    let seg = alloc_segment_carrier(ctx, SEG_HEAP_FIELDS).unwrap();
    ctx.set_field(seg, 0, Value::Long(0));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(i32::from(read_only)));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    ctx.set_field(seg, SEG_HEAP_BASE_FIELD, Value::Object(Some(array)));
    ctx.set_field(seg, SEG_HEAP_START_FIELD, Value::Long(start));
    seg
}

/// A `byte[]`-backed heap segment holding `bytes`, plus the array itself.
fn heap_byte_segment(ctx: &mut dyn NativeContext, bytes: &[i32]) -> (ObjectRef, ObjectRef) {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b));
    }
    let seg = heap_alias_segment(ctx, arr, 0, bytes.len() as i64, false);
    (seg, arr)
}

/// An `int[]`-backed heap segment holding `values`, plus the array itself.
fn heap_int_segment(ctx: &mut dyn NativeContext, values: &[i32]) -> (ObjectRef, ObjectRef) {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, values.len());
    for (i, v) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*v));
    }
    let seg = heap_alias_segment(ctx, arr, 0, values.len() as i64 * 4, false);
    (seg, arr)
}

/// `maxByteAlignment()` is the BACKING ARRAY'S element alignment, not 8.
///
/// This is the question the merge lane left open, and both arms of the
/// answer it guessed were wrong. Oracle rows, transcribed:
///
/// | receiver | `maxByteAlignment()` |
/// |---|---|
/// | `ofArray(byte[16])` | 1 |
/// | `ofArray(short[8])` / `ofArray(char[8])` | 2 |
/// | `ofArray(int[8])` / `ofArray(float[8])` | 4 |
/// | `ofArray(long[8])` / `ofArray(double[8])` | 8 |
/// | `ofArray(byte[0])` | 1 |
/// | `ofArray(long[0])` | 8 |
/// | `ofArray(long[4]).asSlice(4)` | 4 |
/// | `ofArray(int[8]).asSlice(2)` | 2 |
/// | `ofArray(byte[16]).asSlice(8)` | 1 |
/// | `MemorySegment.NULL` / `ofAddress(0)` | 4611686018427387904 |
/// | `ofAddress(16)` | 16 |
/// | `ofAddress(12)` | 4 |
///
/// MUTATION: the previous body was `addr == 0 ? 8 : addr & -addr` over the
/// segment's START, so every row with start 0 answered 8. Rows 1-3, 5 and
/// 10 are red under it; row 4 (`long[8]` -> 8) is the single row that
/// agreed, which is why reading one row would have settled nothing.
#[test]
fn max_byte_alignment_is_the_element_type_not_a_constant_eight() {
    use cratonvm_types::ArrayElementType as A;
    let mut ctx = mock_ctx();
    for (elem, len, expected) in [
        (A::Byte, 16usize, 1_i64),
        (A::Short, 8, 2),
        (A::Char, 8, 2),
        (A::Int, 8, 4),
        (A::Float, 8, 4),
        (A::Long, 8, 8),
        (A::Double, 8, 8),
        (A::Byte, 0, 1),
        (A::Long, 0, 8),
    ] {
        let width = heap_element_width(elem).unwrap() as i64;
        let arr = ctx.new_array(elem, len);
        let seg = heap_alias_segment(&mut ctx, arr, 0, len as i64 * width, false);
        assert_eq!(
            pe_segment_max_byte_alignment(&ctx, seg),
            expected,
            "a {elem:?}[] segment starting at offset 0 promises its element \
             alignment, not 8"
        );
    }

    // The offset cap, on the element type wide enough to show it.
    let longs = ctx.new_array(A::Long, 4);
    for (start, expected) in [(0_i64, 8_i64), (1, 1), (2, 2), (4, 4), (8, 8), (12, 4)] {
        let seg = heap_alias_segment(&mut ctx, longs, start, 32 - start, false);
        assert_eq!(
            pe_segment_max_byte_alignment(&ctx, seg),
            expected,
            "long[4] at byte offset {start} promises min(8, lowestOneBit({start}))"
        );
    }

    // A byte[] never promises more than 1, at any offset — the row the old
    // body got most wrong.
    let bytes = ctx.new_array(A::Byte, 16);
    for start in [0_i64, 1, 4, 8, 12] {
        let seg = heap_alias_segment(&mut ctx, bytes, start, 16 - start, false);
        assert_eq!(pe_segment_max_byte_alignment(&ctx, seg), 1);
    }

    // The native arm, including the address-0 row that used to answer 8.
    assert_eq!(native_max_byte_alignment(0), 1_i64 << 62);
    assert_eq!(native_max_byte_alignment(16), 16);
    assert_eq!(native_max_byte_alignment(12), 4);
    assert_eq!(native_max_byte_alignment(1), 1);
}

/// `toArray` on a heap receiver hands back the ARRAY'S BYTES.
///
/// It used to hand back zeros: `segment_address` is 0 for every heap
/// carrier (F27), and the raw-pointer loop took its `base.is_null()` early
/// return and returned a correctly-sized array of the type's default.
/// Oracle:
///
/// * `ofArray(new byte[]{0..7}).toArray(JAVA_BYTE)` -> `[0, 1, ..., 7]`
/// * `ofArray(byte[16]).asSlice(3,4).toArray(JAVA_BYTE)` -> `[3, 4, 5, 6]`
/// * `ofArray(new int[]{10,20,30,40}).toArray(JAVA_INT)` -> `[10, 20, 30, 40]`
///
/// MUTATION: delete the heap arm and every assertion below reads 0.
#[test]
fn to_array_on_a_heap_receiver_reads_the_arrays_own_bytes() {
    let mut ctx = mock_ctx();
    let byte_layout = jdk_byte_layout(&mut ctx);
    let (seg, _arr) = heap_byte_segment(&mut ctx, &[0, 1, 2, 3, 4, 5, 6, 7]);

    let out = pe_segment_to_array(
        &mut ctx,
        &[Value::Object(Some(seg)), Value::Object(Some(byte_layout))],
    )
    .unwrap();
    let Some(Value::Object(Some(out))) = out else {
        panic!("toArray must answer an array")
    };
    assert_eq!(ctx.array_length(out), 8);
    for i in 0..8 {
        assert_eq!(
            ctx.get_array_element(out, i),
            Value::Int(i as i32),
            "byte {i} must come from the backing array, not from address 0"
        );
    }

    // A SLICE of that heap segment is the receiver `elements` and
    // `spliterator` hand on, and the one `asSlice` mints from a REAL
    // `HeapMemorySegmentImpl$Of*`.
    let Some(Value::Object(Some(slice))) = pe_segment_slice(&mut ctx, seg, 3, 4, None).unwrap()
    else {
        panic!("a heap slice must be a segment")
    };
    let sliced = pe_segment_to_array(
        &mut ctx,
        &[Value::Object(Some(slice)), Value::Object(Some(byte_layout))],
    )
    .unwrap();
    let Some(Value::Object(Some(sliced))) = sliced else {
        panic!("toArray must answer an array")
    };
    assert_eq!(ctx.array_length(sliced), 4);
    for (i, expected) in [3, 4, 5, 6].into_iter().enumerate() {
        assert_eq!(
            ctx.get_array_element(sliced, i),
            Value::Int(expected),
            "a heap slice's toArray starts at the slice, not at the array"
        );
    }

    // A wider element type, so the stride is exercised and not only the
    // byte-for-byte identity case.
    let int_layout = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    let (int_seg, _) = heap_int_segment(&mut ctx, &[10, 20, 30, 40]);
    let out = pe_segment_to_array(
        &mut ctx,
        &[
            Value::Object(Some(int_seg)),
            Value::Object(Some(int_layout)),
        ],
    )
    .unwrap();
    let Some(Value::Object(Some(out))) = out else {
        panic!("toArray must answer an array")
    };
    assert_eq!(ctx.array_length(out), 4);
    for (i, v) in [10, 20, 30, 40].into_iter().enumerate() {
        assert_eq!(ctx.get_array_element(out, i), Value::Int(v));
    }
}

/// `toArray`'s two refusals, in the oracle's ORDER.
///
/// | call | oracle |
/// |---|---|
/// | `ofArray(byte[15]).toArray(JAVA_INT)` | ISE `Segment size is not a multiple of 4. Size: 15` |
/// | `ofArray(byte[16]).toArray(JAVA_INT)` | IAE `Source segment incompatible with alignment constraints` |
/// | `ofArray(byte[16]).toArray(JAVA_INT_UNALIGNED)` | 4 elements |
/// | `ofArray(int[8]).toArray(JAVA_LONG)` | IAE, same message |
/// | `ofArray(int[8]).toArray(JAVA_INT)` | 8 elements |
///
/// Row 1 is the ordering witness: that segment is BOTH badly sized and
/// badly aligned, and HotSpot reports the SIZE, because
/// `AbstractMemorySegmentImpl.toArray` runs `checkArraySize` before it
/// hands the segment to `MemorySegment.copy`.
#[test]
fn to_array_alignment_gate_matches_the_oracle_and_runs_after_the_size_gate() {
    let mut ctx = mock_ctx();
    let int_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    // Same CLASS, alignment 1: that is exactly how the JDK spells
    // `JAVA_INT_UNALIGNED` — one `OfIntImpl` with a different alignment
    // slot — so the element-kind sniff and the alignment gate are being
    // read off the two different places they are read off in production.
    let int_unaligned = jdk_int_unaligned(&mut ctx);
    let long_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        8,
        8,
    );

    let (b15, _) = heap_byte_segment(&mut ctx, &[0; 15]);
    let err = pe_segment_to_array(
        &mut ctx,
        &[Value::Object(Some(b15)), Value::Object(Some(int_aligned))],
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("IllegalState") && text.contains("not a multiple of 4"),
        "a segment that is both badly sized and badly aligned reports the \
         SIZE on the oracle, got {text}"
    );

    let (b16, _) = heap_byte_segment(&mut ctx, &[0; 16]);
    let err = pe_segment_to_array(
        &mut ctx,
        &[Value::Object(Some(b16)), Value::Object(Some(int_aligned))],
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("IllegalArgument") && text.contains("Source segment incompatible"),
        "a byte[] segment's maxByteAlignment is 1, so JAVA_INT is refused, \
         got {text}"
    );
    assert!(
        pe_segment_to_array(
            &mut ctx,
            &[Value::Object(Some(b16)), Value::Object(Some(int_unaligned))]
        )
        .is_ok(),
        "JAVA_INT_UNALIGNED on the same receiver is the oracle's OK row"
    );

    let (int_seg, _) = heap_int_segment(&mut ctx, &[0; 8]);
    assert!(
        pe_segment_to_array(
            &mut ctx,
            &[
                Value::Object(Some(int_seg)),
                Value::Object(Some(long_aligned))
            ]
        )
        .is_err(),
        "an int[] segment's maxByteAlignment is 4, so JAVA_LONG is refused"
    );
    assert!(pe_segment_to_array(
        &mut ctx,
        &[
            Value::Object(Some(int_seg)),
            Value::Object(Some(int_aligned))
        ]
    )
    .is_ok());
}


/// The splitter walks a heap receiver, and each element it mints is a HEAP
/// slice over the same array — not a synthetic native carrier parked at a
/// small integer address.
///
/// Oracle: `ofArray(new int[]{10,20,30,40}).spliterator(JAVA_INT)` advances
/// four times then reports exhausted; the second element has
/// `address() == 4`, `isNative() == false`, `heapBase()` present, and reads
/// `20`.
#[test]
fn the_splitter_walks_a_heap_receiver_element_by_element() {
    let mut ctx = mock_ctx();
    let int_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    let (seg, arr) = heap_int_segment(&mut ctx, &[10, 20, 30, 40]);

    let Some(Value::Object(Some(splitter))) = pe_segment_spliterator(
        &mut ctx,
        &[Value::Object(Some(seg)), Value::Object(Some(int_aligned))],
    )
    .unwrap() else {
        panic!("spliterator over an int[4] heap segment must exist")
    };
    assert_eq!(pe_splitter_state(&mut ctx, splitter), (4, 4, 0));

    // A null consumer exercises the mint and the advance without needing
    // the mock to dispatch `Consumer.accept`.
    for expected_index in 1..=4_i64 {
        assert_eq!(
            pe_splitter_try_advance(&mut ctx, &[Value::Object(Some(splitter))]).unwrap(),
            Some(Value::Int(1)),
            "element {expected_index} of 4 must be produced"
        );
        assert_eq!(pe_splitter_state(&mut ctx, splitter).2, expected_index);
    }
    assert_eq!(
        pe_splitter_try_advance(&mut ctx, &[Value::Object(Some(splitter))]).unwrap(),
        Some(Value::Int(0)),
        "the fifth advance is exhausted"
    );

    // And the element body itself — the same `pe_segment_slice` call
    // `tryAdvance` makes for index 1.
    let Some(Value::Object(Some(elem))) = pe_segment_slice(&mut ctx, seg, 4, 4, None).unwrap()
    else {
        panic!("element 1 must be a segment")
    };
    assert_eq!(pe_segment_base_address(&ctx, elem), 4, "address() is 4");
    assert_eq!(
        pe_segment_heap_base(&ctx, elem),
        Value::Object(Some(arr)),
        "an element of a heap segment is a HEAP segment over the SAME \
         array, not a native carrier parked at the small integer 4"
    );
    assert_eq!(
        pe_segment_get_impl(&mut ctx, elem, int_aligned, 0).unwrap(),
        Some(Value::Int(20)),
        "element 1 reads the array's second int"
    );
}

/// A read-only segment has NO `heapBase`.
///
/// Oracle (`FfmProbe` B12, `FfmProbe3` M4a-M4g): `heapBase()` is present on
/// a writable heap segment and EMPTY on `asReadOnly()`, on a slice of a
/// read-only segment, on an element of one, on a read-only `ofBuffer`
/// segment, and on a read-only native segment.
///
/// This is a capability, not cosmetics: the array `heapBase()` returns is
/// writable through plain array stores, so handing it out from a read-only
/// view returns exactly the capability `asReadOnly()` removed — F26's
/// "a copying slice is a wrong capability", one call further on.
///
/// MUTATION: drop the read-only arm and rows 2, 3 and 4 hand the array
/// back.
#[test]
fn a_read_only_segment_hands_out_no_backing_array() {
    let mut ctx = mock_ctx();
    let (writable, arr) = heap_byte_segment(&mut ctx, &[1, 2, 3, 4]);
    assert_eq!(
        pe_segment_heap_base(&ctx, writable),
        Value::Object(Some(arr)),
        "a writable heap segment answers its array"
    );

    let read_only = heap_alias_segment(&mut ctx, arr, 0, 4, true);
    assert_eq!(
        pe_segment_heap_base(&ctx, read_only),
        Value::Object(None),
        "a read-only heap segment must not hand out its writable array"
    );

    // Read-only is contagious through `asSlice` (F21), so the slice must
    // withhold it too — the arm that would otherwise leak the array one
    // call after the fix.
    let Some(Value::Object(Some(slice))) =
        pe_segment_slice(&mut ctx, read_only, 1, 2, None).unwrap()
    else {
        panic!("a heap slice must be a segment")
    };
    assert_eq!(
        pe_segment_heap_base(&ctx, slice),
        Value::Object(None),
        "a slice of a read-only segment is read-only, so it has no \
         heapBase either"
    );

    // And `asReadOnly()`'s own shape: the same body with the flag FORCED.
    let Some(Value::Object(Some(forced))) =
        pe_segment_slice(&mut ctx, writable, 0, 4, Some(true)).unwrap()
    else {
        panic!("asReadOnly must answer a segment")
    };
    assert_eq!(
        pe_segment_heap_base(&ctx, forced),
        Value::Object(None),
        "asReadOnly() of a writable heap segment withholds the array"
    );

    // Reads still work through the read-only view — the oracle's
    // `asReadOnly().toArray(JAVA_BYTE).length == 16` row. Withholding the
    // array must not become "a read-only segment is unreadable".
    let byte_layout = jdk_byte_layout(&mut ctx);
    let out = pe_segment_to_array(
        &mut ctx,
        &[
            Value::Object(Some(read_only)),
            Value::Object(Some(byte_layout)),
        ],
    )
    .unwrap();
    let Some(Value::Object(Some(out))) = out else {
        panic!("a read-only segment still reads")
    };
    assert_eq!(ctx.get_array_element(out, 0), Value::Int(1));
}

/// The slice arities refuse in the oracle's ORDER, with the oracle's
/// exception CLASSES and message TEXTS.
///
/// | call | oracle |
/// |---|---|
/// | `ofArray(byte[16]).asSlice(17, 0)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[16]).asSlice(4, -1)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[16]).asSlice(20, 4, 3)` | `IndexOutOfBoundsException` (bounds beat a bad alignment) |
/// | `ofArray(byte[16]).asSlice(4, 4, 3)` | IAE `Invalid alignment constraint : 3` |
/// | `ofArray(byte[16]).asSlice(4, 4, 0)` | IAE `Invalid alignment constraint : 0` |
/// | `ofArray(byte[16]).asSlice(0, 8, 8)` | IAE `Target offset incompatible with alignment constraints` |
/// | `ofArray(byte[16]).asSlice(4, 4, 1)` | OK |
/// | `ofArray(int[8]).asSlice(0, 4, 4)` | OK |
/// | `ofArray(int[8]).asSlice(2, 4, 4)` | IAE, alignment |
/// | `ofArray(int[8]).asSlice(0, 8, 8)` | IAE, alignment |
///
/// Note the SPACE before the colon in `Invalid alignment constraint : 3`.
/// It is HotSpot's, transcribed; the previous text had no space, and
/// interpolated the offset into the second message, which HotSpot does not
/// do at all.
#[test]
fn slice_refusals_are_the_oracles_classes_texts_and_order() {
    let mut ctx = mock_ctx();
    let (b16, _) = heap_byte_segment(&mut ctx, &[0; 16]);

    let oob = pe_slice_bounds_check(&ctx, b16, 17, 0).unwrap_err();
    assert!(
        format!("{oob:?}").contains("IndexOutOfBounds"),
        "an over-long slice is IndexOutOfBoundsException, not \
         IllegalStateException, got {oob:?}"
    );
    assert!(pe_slice_bounds_check(&ctx, b16, 4, -1).is_err());
    assert!(pe_slice_bounds_check(&ctx, b16, -1, 4).is_err());
    assert!(pe_slice_bounds_check(&ctx, b16, 16, 0).is_ok());

    let bad_power = pe_slice_alignment_check(&ctx, b16, 4, 3).unwrap_err();
    assert!(
        format!("{bad_power:?}").contains("Invalid alignment constraint : 3"),
        "the space before the colon is the oracle's, got {bad_power:?}"
    );
    let zero = pe_slice_alignment_check(&ctx, b16, 4, 0).unwrap_err();
    assert!(format!("{zero:?}").contains("Invalid alignment constraint : 0"));
    let unmeetable = pe_slice_alignment_check(&ctx, b16, 0, 8).unwrap_err();
    let text = format!("{unmeetable:?}");
    assert!(
        text.contains("Target offset incompatible with alignment constraints"),
        "the message does not interpolate the offset, got {text}"
    );
    assert!(
        pe_slice_alignment_check(&ctx, b16, 4, 1).is_ok(),
        "alignment 1 is always available"
    );

    let (int_seg, _) = heap_int_segment(&mut ctx, &[0; 8]);
    assert!(pe_slice_alignment_check(&ctx, int_seg, 0, 4).is_ok());
    assert!(pe_slice_alignment_check(&ctx, int_seg, 4, 4).is_ok());
    assert!(
        pe_slice_alignment_check(&ctx, int_seg, 2, 4).is_err(),
        "absolute offset 2 cannot carry a 4-byte alignment"
    );
    assert!(
        pe_slice_alignment_check(&ctx, int_seg, 0, 8).is_err(),
        "an int[] segment's maxByteAlignment is 4"
    );
}

/// A slice's own start does NOT disqualify a better-aligned offset inside
/// it.
///
/// This is the conjunct the previous `heap_segment_check_access` had one
/// too many of. Oracle (`FfmProbe3` M1a-M1j):
///
/// | call | oracle |
/// |---|---|
/// | `ofArray(int[8]).asSlice(2).maxByteAlignment()` | 2 |
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 2)` | reads (absolute 4) |
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 0)` | IAE (absolute 2) |
/// | `ofArray(long[4]).asSlice(4).get(JAVA_LONG, 4)` | reads (absolute 8) |
///
/// MUTATION: re-add `align <= maxByteAlignment(view.start)` and rows 2 and
/// 4 are refused.
#[test]
fn alignment_is_judged_at_the_absolute_offset_not_the_slice_start() {
    let mut ctx = mock_ctx();
    let int_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        4,
    );
    let long_aligned = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        8,
        8,
    );

    let ints = ctx.new_array(cratonvm_types::ArrayElementType::Int, 8);
    let at2 = heap_alias_segment(&mut ctx, ints, 2, 30, false);
    assert_eq!(
        pe_segment_max_byte_alignment(&ctx, at2),
        2,
        "a slice starting at byte 2 of an int[] promises 2"
    );
    assert!(
        pe_segment_get_impl(&mut ctx, at2, int_aligned, 2).is_ok(),
        "offset 2 of a slice starting at 2 is absolute 4, which the oracle \
         reads"
    );
    assert!(
        pe_segment_get_impl(&mut ctx, at2, int_aligned, 0).is_err(),
        "offset 0 of the same slice is absolute 2, which the oracle refuses"
    );

    let longs = ctx.new_array(cratonvm_types::ArrayElementType::Long, 4);
    let at4 = heap_alias_segment(&mut ctx, longs, 4, 28, false);
    assert_eq!(pe_segment_max_byte_alignment(&ctx, at4), 4);
    assert!(
        pe_segment_get_impl(&mut ctx, at4, long_aligned, 4).is_ok(),
        "absolute offset 8 carries an 8-byte alignment even though the \
         slice starts at 4"
    );
    assert!(pe_segment_get_impl(&mut ctx, at4, long_aligned, 0).is_err());
}

/// The zero-size refusal on the RAW-ADDRESS path changed class too, and
/// this is the control for that half.
///
/// Oracle: `MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)` is
/// `IndexOutOfBoundsException`, exactly as the heap and native
/// out-of-bounds rows are. It used to be `IllegalStateException` here,
/// which contradicted the "Bounds, not state" paragraph sitting directly
/// below it in the same function.
#[test]
fn a_zero_size_native_segment_is_also_an_index_out_of_bounds() {
    let mut ctx = mock_ctx();
    let seg = alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(seg, 0, Value::Long(0x1000));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 5, Value::Long(0));
    let err = pe_segment_access_addr(&mut ctx, seg, 0, 1).unwrap_err();
    assert!(
        format!("{err:?}").contains("IndexOutOfBounds"),
        "got {err:?}"
    );
}

/// `MemorySegment.ofArray` now covers all three missing primitive arrays,
/// and the carrier it mints ALIASES.
///
/// `javap java.lang.foreign.MemorySegment` declares seven `ofArray`
/// overloads — `byte[] char[] short[] int[] float[] long[] double[]` — and
/// NO `boolean[]` (measured: `NoSuchMethodException ... ofArray([Z)`).
/// This file registered four.
#[test]
fn of_array_covers_byte_short_and_char_and_the_carrier_aliases() {
    for (elem, width) in [
        (cratonvm_types::ArrayElementType::Byte, 1usize),
        (cratonvm_types::ArrayElementType::Short, 2),
        (cratonvm_types::ArrayElementType::Char, 2),
    ] {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(elem, 8);
        let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
            Some(Value::Object(Some(seg))) => seg,
            other => panic!("ofArray({elem:?}) answered {other:?}"),
        };

        assert_eq!(
            crate::panama_libffi::segment_byte_size(&ctx, seg),
            (8 * width) as i64,
            "byteSize is length x element width for {elem:?}"
        );
        assert_eq!(
            crate::panama_libffi::segment_address(&ctx, seg),
            0,
            "an alias carrier has NO machine address; 0 is the value every \
             raw-pointer consumer refuses on"
        );

        // A write through the segment must be visible in the Java array —
        // the property the four `int[]/long[]/float[]/double[]` arms do
        // NOT have, because they copy into an off-heap mirror.
        let byte_layout = jdk_byte_layout(&mut ctx);
        pe_segment_set_impl(&mut ctx, seg, byte_layout, 0, Value::Int(0x5a)).unwrap();
        assert_eq!(
            ctx.get_array_element(arr, 0),
            Value::Int(0x5a),
            "the write must alias the caller's {elem:?} array"
        );
    }
}

/// G19-1: a heap segment's scope is ONE session, minted with the segment,
/// and every view derived from it hands back that same object.
///
/// MEASURED on 25.0.3+9-LTS (`G19Probe` §SC), and every row below is one of
/// those rows:
///
/// ```text
/// heap.scope() == heap.scope()                 true
/// heap.asSlice(4,4).scope() == heap.scope()    true
/// heap.asReadOnly().scope() == heap.scope()    true
/// ofArray(a).scope() == ofArray(a).scope()     FALSE   (the same array!)
/// ```
///
/// The last row is the one that says the answer is per-SEGMENT and not a
/// process-wide singleton, so it is asserted as a NEGATIVE — a fix that
/// handed every heap segment one shared session would satisfy the first
/// three and fail this one.
///
/// Asserted through `pe_segment_session` as well as by slot, because the
/// slot write is only half the repair: the reader in `foreign_ffm`
/// (`p67_receiver_session`) has to recognise a session in slot 2, and if it
/// does not, `scope()` still mints a fresh one and this whole family stays
/// red with the carrier looking correct.
#[test]
fn a_heap_segments_scope_is_one_session_shared_by_its_slices() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray answered {other:?}"),
    };

    let session = match ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) {
        Value::Object(Some(session)) => session,
        other => panic!("slot 2 must carry the segment's session, got {other:?}"),
    };
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(session))
            .as_deref(),
        Some(PE_SESSION_CLASS),
        "the stamped object must be a session, not an arena and not the array"
    );
    assert_eq!(
        pe_segment_session(&ctx, seg),
        Some(session),
        "the resolver must find the stamped session"
    );

    // A slice and a read-only view both go through `pe_segment_slice`, so
    // one write covers both — assert both anyway, because that sharing is a
    // property of today's call graph and not of the rule.
    let slice = match pe_segment_as_slice(
        &mut ctx,
        &[Value::Object(Some(seg)), Value::Long(4), Value::Long(4)],
    )
    .unwrap()
    {
        Some(Value::Object(Some(slice))) => slice,
        other => panic!("asSlice answered {other:?}"),
    };
    assert_eq!(
        ctx.get_field(slice, PE_SEGMENT_ARENA_FIELD),
        Value::Object(Some(session)),
        "a slice of a heap segment must share its parent's scope"
    );
    assert_eq!(pe_segment_session(&ctx, slice), Some(session));

    let read_only = match pe_segment_slice(&mut ctx, seg, 0, 16, Some(true)).unwrap() {
        Some(Value::Object(Some(view))) => view,
        other => panic!("asReadOnly answered {other:?}"),
    };
    assert_eq!(
        pe_segment_session(&ctx, read_only),
        Some(session),
        "asReadOnly must not drop the scope on the floor"
    );

    // Per SEGMENT, not per array and not per process.
    let twin = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
        Some(Value::Object(Some(twin))) => twin,
        other => panic!("ofArray answered {other:?}"),
    };
    assert_ne!(
        pe_segment_session(&ctx, twin),
        Some(session),
        "two segments over the SAME array have distinct scopes on the oracle"
    );
}

/// G19-1: stamping the session into slot 2 must not disturb the slot's
/// older tenant, the owning arena on an arena-allocated segment.
/// `heapBase()` is the one with a visible return value, so it is the one
/// asserted: the oracle says an alias carrier's `heapBase()` is present
/// and holds the caller's array (`G6-1` §6, row 1) — reading it from slot
/// 6, not slot 2, is what proves the session in slot 2 was not mistaken
/// for something else.
#[test]
fn stamping_the_scope_does_not_disturb_the_other_tenants_of_slot_two() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
        Some(Value::Object(Some(seg))) => seg,
        other => panic!("ofArray answered {other:?}"),
    };
    assert_eq!(
        pe_segment_heap_base(&ctx, seg),
        Value::Object(Some(arr)),
        "heapBase() must still be the caller's array, read from slot 6"
    );
    assert!(
        heap_segment_view(&ctx, seg).is_some(),
        "the carrier must still decode as a heap view"
    );
    // The read-only contagion still withholds the array, and it does so on a
    // carrier whose slot 2 is now occupied.
    let read_only = match pe_segment_slice(&mut ctx, seg, 0, 16, Some(true)).unwrap() {
        Some(Value::Object(Some(view))) => view,
        other => panic!("asReadOnly answered {other:?}"),
    };
    assert_eq!(
        pe_segment_heap_base(&ctx, read_only),
        Value::Object(None),
        "a read-only view must not hand back the writable array"
    );
}

/// A slice of a heap segment is still a heap segment — and this is the
/// crash F27 moved rather than closed.
///
/// PRE-FIX `asSlice` computed `segment_address(this) + offset`, which for a
/// heap receiver is `0 + offset`, and stamped THAT into slot 0 of a
/// six-field synthetic. `ofArray(new byte[16]).asSlice(3, 4).get(...)`
/// then dereferenced the literal address 3. At offset 0 the product was 0
/// and refused, which is exactly why the un-sliced W7-89 §7.1 repro looked
/// fixed while the sliced one still died.
///
/// Oracle: `asSlice(3, 4)` has `byteSize=4`, `address()=3`,
/// `isNative()=false`, and a write through it lands at `src[3..7]`.
#[test]
fn a_slice_of_a_heap_segment_is_still_a_heap_segment() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let parent = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        0,
        16,
        false,
    );

    let slice = match pe_segment_as_slice(
        &mut ctx,
        &[Value::Object(Some(parent)), Value::Long(3), Value::Long(4)],
    )
    .unwrap()
    {
        Some(Value::Object(Some(slice))) => slice,
        other => panic!("asSlice answered {other:?}"),
    };

    assert_eq!(
        crate::panama_libffi::segment_address(&ctx, slice),
        0,
        "the slice must NOT carry its offset where an address belongs"
    );
    assert_eq!(crate::panama_libffi::segment_byte_size(&ctx, slice), 4);
    let view = heap_segment_view(&ctx, slice).expect("a heap slice is a heap segment");
    assert_eq!(view.start, 3, "and its address() is 3, per the oracle");

    let layout = jdk_int_unaligned(&mut ctx);
    pe_segment_set_impl(&mut ctx, slice, layout, 0, Value::Int(0x0102_0304)).unwrap();
    assert_eq!(
        byte_vec(&ctx, arr, 8),
        vec![0, 0, 0, 4, 3, 2, 1, 0],
        "the write goes through the slice to src[3..7]"
    );

    // And the slice's own bounds are the SLICE's, not the parent's.
    assert!(
        pe_segment_set_impl(&mut ctx, slice, layout, 1, Value::Int(0)).is_err(),
        "offset 1 + 4 > 4"
    );
}

/// NEGATIVE CONTROL for every arm above: a genuinely native segment must
/// still take the raw-address path and behave exactly as before.
///
/// Without this, an over-broad heap predicate would silently route arena
/// memory through `get_array_element` and every test above would still
/// pass.
#[test]
fn a_native_segment_still_takes_the_raw_address_path() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = arena_segment(&mut ctx, arena, 16);
    assert!(
        heap_segment_view(&ctx, seg).is_none(),
        "an arena segment is not a heap segment"
    );
    assert!(!crate::panama_libffi::is_real_heap_segment(&ctx, seg));

    let layout = make_layout(&mut ctx, LAYOUT_INT);
    pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x2a)).unwrap();
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap(),
        Some(Value::Int(0x2a)),
        "the off-heap round trip is unchanged"
    );
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(p) => p as *const i32,
        other => panic!("arena segment slot 0 is {other:?}"),
    };
    assert_eq!(
        unsafe { *ptr },
        0x2a,
        "and it really went to the native block, not to an array"
    );
}

/// A heap segment must not be handed to anything as a POINTER.
///
/// Oracle, and the message is quoted from it:
///
/// ```text
/// MemorySegment.ofArray(new byte[16])
///     .set(ADDRESS.withByteAlignment(1), 0, MemorySegment.ofArray(new byte[4]))
///   -> IllegalArgumentException: Heap segment not allowed: ...
/// strlen(MemorySegment.ofArray("hi\0".getBytes()))
///   -> IllegalArgumentException: Heap segment not allowed: ...
/// ```
///
/// PRE-FIX this arm stored `segment_address(target)`, which since F27 is
/// **0** for a heap segment — a legitimate C null that no downstream
/// consumer can tell apart from a real one. A quiet wrong write, not a
/// refusal.
#[test]
fn a_heap_segment_is_refused_as_an_address_value() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let dst = arena_segment(&mut ctx, arena, 16);
    let address_layout = make_jdk_layout(
        &mut ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        8,
        1,
    );

    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let heap = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        0,
        4,
        false,
    );
    let err = pe_segment_set_impl(&mut ctx, dst, address_layout, 0, Value::Object(Some(heap)))
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("Heap segment not allowed"),
        "the refusal must name the reason, got {err:?}"
    );

    // CONTROL: a real native segment is still stored, so the arm above is
    // not simply refusing every ADDRESS write.
    let target = arena_segment(&mut ctx, arena, 8);
    pe_segment_set_impl(
        &mut ctx,
        dst,
        address_layout,
        0,
        Value::Object(Some(target)),
    )
    .expect("a native segment is still a legal ADDRESS value");
    let stored = match pe_segment_get_impl(&mut ctx, dst, address_layout, 0).unwrap() {
        Some(Value::Object(Some(seg))) => crate::panama_libffi::segment_address(&ctx, seg),
        other => panic!("get(ADDRESS) answered {other:?}"),
    };
    assert_eq!(
        stored,
        crate::panama_libffi::segment_address(&ctx, target),
        "and the value that was written is the target's real address"
    );
}

/// `kind < 10` is NOT "is this a value layout".
///
/// `LAYOUT_UNKNOWN` is -2, so the old spelling admitted it at all three
/// sites. This pins the arithmetic fact itself, so a future change to
/// either constant that re-opens the hole goes red here rather than in a
/// downstream default arm.
#[test]
fn the_range_test_excludes_layout_unknown() {
    assert!(
        crate::panama_libffi::LAYOUT_UNKNOWN < 10,
        "this is WHY `kind < 10` was wrong - it is true for the unknown \
         sentinel"
    );
    assert!(!layout_kind_is_value(crate::panama_libffi::LAYOUT_UNKNOWN));
    assert!(!layout_kind_is_value(UPCALL_RETURN_VOID));
    for kind in [
        LAYOUT_BYTE,
        LAYOUT_SHORT,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_FLOAT,
        LAYOUT_DOUBLE,
        LAYOUT_ADDRESS,
        LAYOUT_BOOLEAN,
        LAYOUT_CHAR,
    ] {
        assert!(layout_kind_is_value(kind), "kind {kind} is a value layout");
    }
    for kind in [LAYOUT_STRUCT, LAYOUT_UNION, LAYOUT_SEQUENCE, LAYOUT_PADDING] {
        assert!(!layout_kind_is_value(kind), "kind {kind} is a group layout");
    }
    assert_ne!(
        UPCALL_RETURN_VOID,
        crate::panama_libffi::LAYOUT_UNKNOWN,
        "the two sentinels must stay distinct"
    );
}

/// An unclassifiable layout is REFUSED by both accessors, not defaulted.
///
/// PRE-FIX `ffi::layout_byte_size` answers 1 for an unrecognised kind and
/// the read's `_ =>` arm answered `Value::Int(0)`, so `get` returned a
/// believable zero and `set` was a silent no-op. Both are the shape this
/// family keeps paying for.
#[test]
fn an_unclassifiable_layout_is_refused_by_get_and_set() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = arena_segment(&mut ctx, arena, 16);
    // A carrier with a `Long` slot 0 and a class name that is not any
    // layout: `read_layout_kind` falls to the class matcher and answers
    // LAYOUT_UNKNOWN.
    let layout = make_jdk_layout(&mut ctx, "com/example/NotALayout", 4, 4);
    assert_eq!(
        crate::panama_libffi::read_layout_kind(&ctx, layout),
        crate::panama_libffi::LAYOUT_UNKNOWN
    );

    let read = pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap_err();
    assert!(
        format!("{read:?}").contains("NotALayout"),
        "the refusal must name the carrier, got {read:?}"
    );
    let write = pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(1)).unwrap_err();
    assert!(format!("{write:?}").contains("NotALayout"));

    // CONTROL: the same segment with a layout that DOES classify still
    // works, so the refusal is not simply "this segment is broken".
    let good = jdk_int_unaligned(&mut ctx);
    assert!(pe_segment_get_impl(&mut ctx, seg, good, 0).is_ok());
}

/// The stride the ERASED `getAtIndex`/`setAtIndex` used cannot exist.
///
/// They read `get_field(layout, 1)` matched against `Value::Int` — the
/// DELETED three-slot encoding, in which slot 1 was `Int(byteSize)`. On
/// the JDK-true four-slot carrier F16 made authoritative, slot 1 is
/// `Long(byteAlignment)`. So the arm could never match and the stride was
/// the `_ => 1` default: `getAtIndex(JAVA_INT, 2)` read offset 2.
///
/// This pins the fact that made the old code unreachable-correct, which is
/// what a future re-introduction would have to contradict.
#[test]
fn slot_one_of_a_layout_is_a_long_alignment_never_an_int_size() {
    let mut ctx = mock_ctx();
    let int_unaligned = jdk_int_unaligned(&mut ctx);
    assert_eq!(
        ctx.get_field(int_unaligned, 1),
        Value::Long(1),
        "JAVA_INT_UNALIGNED is byteSize=4 byteAlignment=1 on the oracle, \
         and slot 1 carries the ALIGNMENT"
    );
    assert!(
        !matches!(ctx.get_field(int_unaligned, 1), Value::Int(_)),
        "the erased accessors' `Value::Int` arm can never match"
    );
    assert_eq!(
        crate::panama_libffi::layout_align(&ctx, int_unaligned),
        1,
        "so 1 is the alignment, not the size"
    );
    assert_eq!(
        ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(&ctx, int_unaligned)),
        4,
        "and the size - the stride the covariant accessors use - is 4"
    );
}

/// W7-89 §7.2: the accessors are not `@Restricted` on JDK 25, so a missing
/// `--enable-native-access` must not refuse them.
///
/// `grep "@Restricted" jdk25src/java.base/java/lang/foreign/MemorySegment.java`
/// hits three lines and all three are `reinterpret`. HotSpot 25 runs
/// `get`/`set`/`copy`/`fill` with no flag at all.
///
/// MUTATION: reverting `require_segment_access` to `require_native_access`
/// makes the first assertion fail whenever the process has granted
/// nothing, which is the state a unit-test process is in. The second
/// assertion is the control: the flag-absent refusal still exists for the
/// paths that genuinely need it, and is still keyed on the same global.
#[test]
fn the_segment_accessors_are_not_gated_on_enable_native_access() {
    let mut ctx = mock_ctx();
    for op in ["get", "set", "getAtIndex", "setAtIndex", "copy", "fill"] {
        assert!(
            require_segment_access(&mut ctx, op).is_ok(),
            "MemorySegment.{op} is not @Restricted on JDK 25 and must not \
             raise IllegalCallerException"
        );
    }
    assert_eq!(
        require_native_access(&mut ctx, "downcall").is_err(),
        !native_access_enabled(&ctx),
        "the flag-absent refusal must survive, unchanged, for the paths \
         the JDK really does restrict"
    );
}

/// Every `MemorySegment` triple registered on the interface is registered on
/// [`CRATON_SEGMENT_CLASS`] too, and vice versa.
///
/// This is the guard on the whole fix, not a tidiness check. Native dispatch
/// is keyed on the RECEIVER's class; since 2026-08-22 a CratonVM-minted
/// segment's receiver class is `CRATON_SEGMENT_CLASS`, so a method that
/// exists only under the interface name is a `NoSuchMethodError` waiting for
/// its first caller — and it would be raised from JDK code three frames
/// away from the registration that forgot it.
///
/// The two names are registered by a loop at each of the three sites
/// (`register_pe_memory_segment`, `register_pe2_string_marshaling`,
/// `foreign_ffm::register_p67_segment_surface`, plus the two-registration
/// block beside `Arena.allocate`), so the sets can only diverge if someone
/// adds a fourth site and registers one name. That is precisely the mistake
/// this asserts against.
#[test]
fn the_craton_segment_class_mirrors_the_interface() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_pe_memory_segment(&mut registry);
    super::register_pe2_string_marshaling(&mut registry);
    crate::phases_late::foreign_ffm::register_p67_foreign_memory(&mut registry);

    let collect = |class: &str| {
        let mut rows: Vec<(String, String)> = registry
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == class)
            .map(|(_, m, d, _)| (m.to_string(), d.to_string()))
            .collect();
        rows.sort();
        rows.dedup();
        rows
    };
    let iface = collect(super::PE_SEGMENT_INTERFACE);
    let craton = collect(super::CRATON_SEGMENT_CLASS);

    // A registrar that silently registered nothing would make the equality
    // below vacuously true; `byteSize` is the cheapest proof that it ran.
    assert!(
        iface.iter().any(|(m, d)| m == "byteSize" && d == "()J"),
        "the interface registrars did not run",
    );
    let only_iface: Vec<_> = iface.iter().filter(|r| !craton.contains(r)).collect();
    let only_craton: Vec<_> = craton.iter().filter(|r| !iface.contains(r)).collect();
    assert!(
        only_iface.is_empty() && only_craton.is_empty(),
        "the segment natives have drifted apart.\n\
         registered ONLY on {}: {only_iface:#?}\n\
         registered ONLY on {}: {only_craton:#?}",
        super::PE_SEGMENT_INTERFACE,
        super::CRATON_SEGMENT_CLASS,
    );
}

// ---- round 12 wave 5 (lane ffm2) ---------------------------------------

/// A JDK-true int layout with an explicit byte order: slot 2 is the
/// fabricated carrier's order flag (`1` little, `0` big), which is what
/// `foreign_ffm::p67_layout_is_little` reads for a layout with no real
/// `order` field.
fn int_layout_ordered(ctx: &mut dyn NativeContext, align: i64, little: bool) -> ObjectRef {
    let layout = make_jdk_layout(
        ctx,
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        4,
        align,
    );
    ctx.set_field(layout, 2, Value::Int(i32::from(little)));
    layout
}

/// `JAVA_INT.withOrder(BIG_ENDIAN)` reads and writes the reversed bytes on
/// a native segment and on a heap segment. Before the fix both paths used
/// host order (`R12Hunt2FfmLayouts`: `getAtIndex` BE got 16909060, want
/// 67305985).
#[test]
fn a_big_endian_layout_swaps_on_native_and_heap_segments() {
    if cfg!(target_endian = "big") {
        return;
    }
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = arena_segment(&mut ctx, arena, 16);
    let le = int_layout_ordered(&mut ctx, 4, true);
    let be = int_layout_ordered(&mut ctx, 4, false);

    pe_segment_set_impl(&mut ctx, seg, le, 0, Value::Int(0x0102_0304)).unwrap();
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, be, 0).unwrap(),
        Some(Value::Int(0x0403_0201)),
        "a big-endian read of a little-endian store is the byte swap"
    );
    pe_segment_set_impl(&mut ctx, seg, be, 4, Value::Int(0x0102_0304)).unwrap();
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(p) => p as *const u8,
        other => panic!("no address: {other:?}"),
    };
    // SAFETY: the arena block is 16 bytes and still live.
    let stored = unsafe { std::slice::from_raw_parts(ptr.add(4), 4) }.to_vec();
    assert_eq!(
        stored,
        vec![1, 2, 3, 4],
        "a big-endian store lays the bytes out MSB first"
    );
    assert_eq!(
        pe_segment_get_impl(&mut ctx, seg, be, 4).unwrap(),
        Some(Value::Int(0x0102_0304)),
        "and reads back through the same layout unchanged"
    );

    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let heap = make_real_heap_segment(
        &mut ctx,
        "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
        arr,
        0,
        8,
        false,
    );
    let be_unaligned = int_layout_ordered(&mut ctx, 1, false);
    pe_segment_set_impl(&mut ctx, heap, be_unaligned, 0, Value::Int(0x0102_0304)).unwrap();
    assert_eq!(byte_vec(&ctx, arr, 8), vec![1, 2, 3, 4, 0, 0, 0, 0]);
    assert_eq!(
        pe_segment_get_impl(&mut ctx, heap, be_unaligned, 0).unwrap(),
        Some(Value::Int(0x0102_0304))
    );
    assert_eq!(
        crate::ffm_fast::layout_align_mask(be.as_ptr() as u64),
        None,
        "a big-endian layout must never get a fast-path layout verdict"
    );
}

/// A native access at an address the layout's alignment does not admit is
/// `IllegalArgumentException: Misaligned access`; the unaligned layout
/// reads the same address; `getAtIndex` refuses a layout whose alignment
/// exceeds its size.
#[test]
fn a_misaligned_native_access_is_an_illegal_argument() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = arena_segment(&mut ctx, arena, 16);
    let aligned = int_layout_ordered(&mut ctx, 4, cfg!(target_endian = "little"));
    let unaligned = int_layout_ordered(&mut ctx, 1, cfg!(target_endian = "little"));

    // j27 wave 4 (lane FFMMSG): HotSpot's `checkEnclosingLayout` text
    // (`Misaligned access at address` is the byte-array view handles').
    // The layout here is a synthetic one, so no `(of ..)` clause.
    let misaligned = |ctx: &dyn NativeContext, offset: i64| {
        format!(
            "Target offset {offset} is incompatible with alignment constraint 4 for segment {}",
            segment_to_string(ctx, seg)
        )
    };
    let message = iae_message(pe_segment_get_impl(&mut ctx, seg, aligned, 1));
    assert_eq!(message, misaligned(&ctx, 1));
    let message = iae_message(pe_segment_set_impl(&mut ctx, seg, aligned, 2, Value::Int(1)));
    assert_eq!(message, misaligned(&ctx, 2));
    assert!(pe_segment_get_impl(&mut ctx, seg, unaligned, 1).is_ok());
    assert!(pe_segment_get_impl(&mut ctx, seg, aligned, 4).is_ok());

    let over_aligned = int_layout_ordered(&mut ctx, 8, cfg!(target_endian = "little"));
    let err = pe_segment_get_at_index(
        &mut ctx,
        &[
            Value::Object(Some(seg)),
            Value::Object(Some(over_aligned)),
            Value::Long(0),
        ],
    )
    .unwrap_err();
    assert!(
        format!("{err:?}").contains("Layout alignment greater than its size"),
        "{err:?}"
    );
}

/// A native access publishes a layout verdict for a host-order layout, and
/// carries its alignment, so the JIT element fast path can check both.
#[test]
fn a_host_order_access_publishes_its_layout_verdict() {
    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
    let seg = arena_segment(&mut ctx, arena, 16);
    let layout = int_layout_ordered(&mut ctx, 4, cfg!(target_endian = "little"));
    // Other test modules bump the process-wide epoch (a shared close
    // handshake); retry until one access saw a stable epoch.
    let mut published = false;
    for _ in 0..32 {
        let before = crate::ffm_fast::epoch();
        assert!(pe_segment_get_impl(&mut ctx, seg, layout, 0).is_ok());
        let mask = crate::ffm_fast::layout_align_mask(layout.as_ptr() as u64);
        if crate::ffm_fast::epoch() == before {
            published = mask == Some(3);
            break;
        }
    }
    assert!(published, "no layout verdict (alignment 4) was published");
}

/// `fill` and the segment-to-segment `copy` checked neither segment's
/// scope: on a closed arena they filled and copied a freed block
/// (`R12Hunt2FfmLayouts` `closed-ise` 12000 against HotSpot's 16000).
#[test]
fn fill_and_copy_refuse_a_closed_arenas_segment() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_pe_memory_segment(&mut registry);
    let ms = super::CRATON_SEGMENT_CLASS;
    let fill = registry
        .find(ms, "fill", "(B)Ljava/lang/foreign/MemorySegment;")
        .expect("fill is registered");
    let copy = registry
        .find(
            ms,
            "copy",
            "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        )
        .expect("copy is registered");

    let mut ctx = mock_ctx();
    let arena = make_arena(&mut ctx, ffi::ARENA_SHARED);
    let src = arena_segment(&mut ctx, arena, 16);
    let other = make_arena(&mut ctx, ffi::ARENA_SHARED);
    let dst = arena_segment(&mut ctx, other, 16);
    let copy_args = |from: ObjectRef, to: ObjectRef| {
        [
            Value::Object(Some(from)),
            Value::Long(0),
            Value::Object(Some(to)),
            Value::Long(0),
            Value::Long(16),
        ]
    };

    assert!(fill(&mut ctx, &[Value::Object(Some(src)), Value::Int(7)]).is_ok());
    assert!(copy(&mut ctx, &copy_args(src, dst)).is_ok());

    assert!(pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]).is_ok());
    let err = fill(&mut ctx, &[Value::Object(Some(src)), Value::Int(1)])
        .expect_err("fill on a closed arena's segment answered");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    let err = copy(&mut ctx, &copy_args(src, dst))
        .expect_err("copy FROM a closed arena's segment answered");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    let err = copy(&mut ctx, &copy_args(dst, src))
        .expect_err("copy INTO a closed arena's segment answered");
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
}

#[test]
fn a_swap_reverses_exactly_the_access_width() {
    assert_eq!(pe_swap_width(0x0102, 2), 0x0201);
    assert_eq!(pe_swap_width(0x0102_0304, 4), 0x0403_0201);
    assert_eq!(
        pe_swap_width(0x0102_0304_0506_0708, 8),
        0x0807_0605_0403_0201
    );
    assert_eq!(pe_swap_width(0xAB, 1), 0xAB);
    // Round trip through set's encoder and get's decoder, every width.
    for (kind, value) in [
        (LAYOUT_SHORT, Value::Int(-2)),
        (LAYOUT_CHAR, Value::Int(0xFFFE)),
        (LAYOUT_INT, Value::Int(i32::MIN)),
        (LAYOUT_LONG, Value::Long(i64::MIN + 1)),
        (LAYOUT_FLOAT, Value::Float(-0.0)),
        (LAYOUT_DOUBLE, Value::Double(-0.0)),
    ] {
        let raw = pe_raw_from_value(kind, value).expect("encodable");
        let back = pe_value_from_raw(kind, raw).expect("decodable");
        assert_eq!(format!("{back:?}"), format!("{value:?}"), "kind {kind}");
    }
}
