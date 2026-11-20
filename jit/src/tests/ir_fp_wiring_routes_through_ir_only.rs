#[allow(unused_imports)]
use super::*;

/// inc 30 (FP value tier): a method that uses `float`/`double` internally
/// (with an FP-free `int` signature) routes through the IR pipeline ONLY when
/// `ir_emit_fp` is on. Single-pass also compiles the method, so result
/// equality alone would not prove the IR path ran — `IR_LOWER_COMPILES`
/// proves it (==1 with the flag, ==0 without → vacuous single-pass fallback).
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
pub(in crate::tests) fn ir_fp_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(int a) { return (int)((float)a + 2.0f); }`
    //   iload_0; i2f; fconst_2; fadd; f2i; ireturn
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/F"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from([0x1a, 0x86, 0x0d, 0x62, 0x8b, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 4,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes), and a pure FP-arithmetic method calls no helper.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    // ir_emit_fp = true → the FP method takes the IR pipeline.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, true,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(with.is_some(), "FP method must compile with ir_emit_fp");
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "an FP method must route through the IR pipeline when ir_emit_fp is on"
    );

    // ir_emit_fp = false → method_uses_fp bails the IR path → single-pass.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without ir_emit_fp, an FP method must NOT take the IR pipeline"
    );
}

/// inc 26 (Gap B) + jit-inlining-and-ir-calls: a resolved `invokevirtual`
/// routes through the IR pipeline.
///
/// **The polarity of this test inverted on 2026-07-26.** It used to assert
/// "…ONLY when `ir_emit_virtual_calls` is on", because the IR lowered
/// virtual calls through the generic dispatch helper with no inline cache
/// and admitting them was a throughput regression versus single-pass.
/// `ir_lower::emit_inline_cache_call` now emits the MIC + 4-way-PIC
/// cascade, so the capability became opt-OUT: the caller's parameter can
/// still force it ON, but only the diagnostic
/// `CRATONVM_JIT_IR_CALL_VIRTUAL=0` (here, its thread-local test override)
/// turns it off.
///
/// Guards against a vacuous validation: single-pass ALSO dispatches
/// invokevirtual, so result-equality alone would not prove the IR path ran.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
pub(in crate::tests) fn ir_virtual_call_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(Obj o, int n) { return o.g(n); }`  (g virtual → invokevirtual)
    //   aload_0; iload_1; invokevirtual #2; ireturn
    // Modelled as a `static` caller (receiver `o` is param 0) exactly like the
    // invokespecial wiring test, so the local/param counts line up.
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Caller"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(Lpkg/Obj;I)I"),
        source_file: None,
        code: Arc::from([0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes the body), so the baked `invoke_dispatch` is not called.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Obj".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };

    // ir_emit_virtual_calls = true (calls/special OFF) → invokevirtual lowers
    // to Op::Call → IR pipeline. Crossing the flags proves VIRTUAL is the gate.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls (invokestatic) OFF
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long
        true,  // ir_emit_virtual_calls ON
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(
        with.is_some(),
        "invokevirtual must compile with ir_emit_virtual_calls"
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "invokevirtual must route through the IR pipeline when ir_emit_virtual_calls is on"
    );
    assert!(
        with.as_ref().unwrap().needs_context(),
        "an Op::Call method must be needs_context"
    );

    // The VM's centrally parsed default is true, so the IR pipeline takes
    // the method without any compiler-local environment lookup.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let by_default = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls OFF
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long
        true,  // ir_emit_virtual_calls: VM's default-ON policy
        false, // ir_emit_fp OFF
        None,
    );
    assert!(by_default.is_some());
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "invokevirtual must take the IR pipeline BY DEFAULT now that ir_lower \
         emits MIC/PIC inline caches — the capability is opt-out, not opt-in"
    );

    // Explicit opt-out, modelled by the thread-local override, wins over
    // the caller's true policy and routes the method to single-pass.
    __set_ir_virtual_calls_override(Some(false));
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        true,  // ir_emit_calls ON
        true,  // ir_emit_special_calls ON
        false, // ir_emit_long
        true,  // central policy ON; test override forces it OFF
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    let took_ir = IR_LOWER_COMPILES.with(|c| c.get());
    __set_ir_virtual_calls_override(None);
    assert_eq!(
        took_ir, 0,
        "with CRATONVM_JIT_IR_CALL_VIRTUAL=0, invokevirtual must NOT take the IR pipeline"
    );
}

// ── Stage A.4 (precise oop maps) — param oop mask ──────────────
//
// `compute_param_oop_mask` must mark exactly the JVM local slots that hold a
// reference parameter on entry, using the SAME slot walk as
// `compute_param_jvm_slots` (category-2 J/D consume two slots; arrays and
// `L…;` are references; primitives are not; instance `this` is slot 0). A
// wrong bit here would, under the eventual moving path, either rewrite a
// primitive (false positive) or miss an oop (false negative) — so pin it.
#[test]
pub(in crate::tests) fn test_compute_param_oop_mask() {
    // static, all primitive → no oop slots.
    assert_eq!(compute_param_oop_mask("(II)I", true), 0b00);
    // instance, primitive params → only `this` (slot 0).
    assert_eq!(compute_param_oop_mask("(II)I", false), 0b1);
    // static, one reference param at slot 0.
    assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;I)V", true), 0b1);
    // instance, one reference param → this (0) + param (1).
    assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;)V", false), 0b11);
    // static, long (slots 0,1; non-oop) then reference at slot 2.
    assert_eq!(
        compute_param_oop_mask("(JLjava/lang/Object;)V", true),
        0b100
    );
    // static, array ref (slot 0), long (slots 1,2), array-of-ref (slot 3).
    assert_eq!(
        compute_param_oop_mask("([IJ[Ljava/lang/String;)V", true),
        0b1001
    );
    // bt18's `static Node make(int)` — the live oop is the LOCAL `n`, not a
    // param, so the param mask is empty (the dataflow seeds it as `astore`d).
    assert_eq!(compute_param_oop_mask("(I)Lpkg/Node;", true), 0b0);
    // instance, double param (slots 1,2; non-oop) → only `this`.
    assert_eq!(compute_param_oop_mask("(D)V", false), 0b1);
    // no params: static → 0; instance → this only.
    assert_eq!(compute_param_oop_mask("()V", true), 0b0);
    assert_eq!(compute_param_oop_mask("()V", false), 0b1);
}

// ── JitMICSlot layout tests (CRIT-8 prerequisite) ──────────────
//
// The JIT codegen in `jit/src/x64.rs` emits raw `MOV` instructions
// against a `JitMICSlot*` using the `CACHED_*_OFFSET` constants.
// If the struct layout drifts (e.g. someone reorders a field,
// changes padding, or `parking_lot::Mutex` grows), those MOVs will
// silently read the wrong bytes. Pin the offsets here.
//
// We don't use `core::mem::offset_of!` because the workspace MSRV
// (see `Cargo.toml: rust-version`) is 1.75; the macro stabilized
// in 1.77.

#[test]
pub(in crate::tests) fn test_jit_mic_slot_offsets() {
    let slot = JitMICSlot::new();
    let base = &slot as *const JitMICSlot as usize;
    let off_class_id = (&slot.cached_class_id as *const _ as usize) - base;
    let off_entry_ptr = (&slot.cached_entry_word as *const _ as usize) - base;
    let off_hits = (&slot.hits as *const _ as usize) - base;
    assert_eq!(
        off_class_id,
        JitMICSlot::CACHED_CLASS_ID_OFFSET,
        "cached_class_id offset drift: expected {}, got {}",
        JitMICSlot::CACHED_CLASS_ID_OFFSET,
        off_class_id
    );
    assert_eq!(
        off_entry_ptr,
        JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
        "cached_entry_word offset drift: expected {}, got {}",
        JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
        off_entry_ptr
    );
    // The needs-context byte moved into the entry word; its 8 reserved
    // bytes keep the counters where they were.
    assert_eq!(off_hits, 24, "hits offset drift: got {off_hits}");
}

/// The re-run pin flag took the first reserved byte after the entry word; the
/// hot offsets and the counters must not move, and the flag is sticky across
/// the entry being withdrawn (the site can publish the table callee again).
#[test]
pub(in crate::tests) fn mic_callee_rerun_pins_flag_is_sticky_and_moves_no_offset() {
    let slot = JitMICSlot::new();
    assert!(
        !slot.callee_rerun_pins(),
        "a fresh slot has published nothing"
    );
    slot.mark_callee_rerun_pins();
    assert!(slot.callee_rerun_pins());
    slot.clear_compiled_entry();
    assert!(
        slot.callee_rerun_pins(),
        "withdrawing the entry keeps the site flag"
    );
    let base = &slot as *const JitMICSlot as usize;
    assert_eq!((&slot.cached_entry_word as *const _ as usize) - base, 8);
    assert_eq!((&slot.hits as *const _ as usize) - base, 24);
}

// ── JitPICSlot layout tests (CRIT-8 prerequisite) ──────────────
//
// The JIT codegen emits raw `MOV` instructions against a
// `JitPICSlot*` using the `CLASS_ID_OFFSETS` / `ENTRY_PTR_OFFSETS`
// constants for inline 4-way PIC dispatch. Pin the offsets here so any layout drift (field
// reorder, padding change, `parking_lot::Mutex` resize) fails
// loudly instead of silently misreading bytes.

#[test]
pub(in crate::tests) fn test_jit_pic_slot_offsets() {
    let slot = JitPICSlot::new();
    let base = &slot as *const JitPICSlot as usize;
    for i in 0..JIT_PIC_ENTRIES {
        let actual = (&slot.class_ids[i] as *const _ as usize) - base;
        assert_eq!(
            actual,
            JitPICSlot::CLASS_ID_OFFSETS[i],
            "CLASS_ID_OFFSETS[{i}] drift: {actual} vs {}",
            JitPICSlot::CLASS_ID_OFFSETS[i]
        );
        let actual = (&slot.entry_words[i] as *const _ as usize) - base;
        assert_eq!(
            actual,
            JitPICSlot::ENTRY_PTR_OFFSETS[i],
            "ENTRY_PTR_OFFSETS[{i}] drift: {actual} vs {}",
            JitPICSlot::ENTRY_PTR_OFFSETS[i]
        );
    }
    // The per-way needs-context bytes moved into the entry words; the 8
    // reserved bytes keep the counters (and the hashed table after them)
    // where generated code expects them.
    assert_eq!((&slot.hits as *const _ as usize) - base, 56);
    assert_eq!((&slot.misses as *const _ as usize) - base, 88);
}

// ── JitCodeRegion tests ─────────────────────────────────────────

#[test]
pub(in crate::tests) fn test_jit_code_region_register_and_contains() {
    let mut region = JitCodeRegion::new();
    let fake_ptr = 0x1000 as *const u8;
    assert!(!region.contains(fake_ptr));
    region.register(fake_ptr, 256);
    assert!(region.contains(fake_ptr));
    // Middle of region
    assert!(region.contains(0x1080 as *const u8));
    // One past end should NOT be contained
    assert!(!region.contains(0x1100 as *const u8));
}

#[test]
pub(in crate::tests) fn test_jit_code_region_deregister() {
    let mut region = JitCodeRegion::new();
    let fake_ptr = 0x2000 as *const u8;
    region.register(fake_ptr, 128);
    assert!(region.contains(fake_ptr));
    region.deregister(fake_ptr);
    assert!(!region.contains(fake_ptr));
}

/// The registry is split into a shared `base` and a small `recent` half so a
/// registration does not copy the whole table. Every answer must still be
/// the answer one sorted table gives, across merges and across removals
/// from either half.
#[test]
pub(in crate::tests) fn code_ranges_answer_like_one_sorted_table_across_merges_and_removals() {
    // A band no real mapping uses, so parallel tests cannot collide with it.
    const BAND: usize = 0x7f3c_0000_0000;
    const N: usize = 300;
    let in_band = |start: usize| (BAND..BAND + N * 0x100).contains(&start);
    let entry_of = |i: usize| BAND + i * 0x100;
    // Register out of order, so both halves see interleaved inserts.
    for i in (0..N).rev().step_by(2).chain((0..N).step_by(2)) {
        register_jit_code_range(entry_of(i), 0x80, entry_of(i) + 1);
    }
    for i in 0..N {
        assert_eq!(
            lookup_jit_code_range(entry_of(i) + 0x7c),
            Some(entry_of(i) + 1)
        );
        assert_eq!(
            lookup_jit_code_range(entry_of(i) + 0x80),
            None,
            "gap after {i}"
        );
    }
    let band: Vec<(usize, usize)> = jit_code_ranges_snapshot()
        .into_iter()
        .filter(|(start, _)| in_band(*start))
        .collect();
    assert_eq!(band.len(), N);
    assert!(
        band.windows(2).all(|w| w[0].0 < w[1].0),
        "snapshot must be sorted"
    );
    let mut buf = Vec::new();
    snapshot_code_ranges_into(&mut buf);
    assert!(
        buf.windows(2).all(|w| w[0].0 <= w[1].0),
        "copy must be sorted"
    );

    // Remove every third range: some live in `base`, the latest in `recent`.
    for i in (0..N).step_by(3) {
        unregister_jit_code_range(entry_of(i));
        // Removing an absent range is a no-op, not a panic or a duplicate.
        unregister_jit_code_range(entry_of(i));
    }
    for i in 0..N {
        let expect = (i % 3 != 0).then_some(entry_of(i) + 1);
        assert_eq!(lookup_jit_code_range(entry_of(i)), expect, "range {i}");
    }
    for i in 0..N {
        unregister_jit_code_range(entry_of(i));
    }
    assert!(jit_code_ranges_snapshot()
        .into_iter()
        .all(|(start, _)| !in_band(start)));
}

/// Compile ids are recycled, but only once the retirement generation stamped
/// at release has been graced. A mirror still holding a released id must
/// not resolve it to a different body. A full table is refused, not wrapped.
#[test]
pub(in crate::tests) fn compile_ids_are_recycled_only_after_grace() {
    let mut ids = CompileIdAllocator::new(4);
    assert_eq!(ids.reserve(|_| true), Some(1), "0 is never issued");
    assert_eq!(ids.reserve(|_| true), Some(2));
    ids.release(1, 10);
    // Not graced yet: a fresh id is issued instead.
    assert_eq!(ids.reserve(|stamp| stamp < 10), Some(3));
    // Graced: the released id comes back, oldest first.
    assert_eq!(ids.reserve(|stamp| stamp <= 10), Some(1));
    ids.release(2, 11);
    // The table is full and nothing is graced: refused.
    assert_eq!(ids.reserve(|_| false), None);
    assert_eq!(ids.reserve(|_| true), Some(2));
    assert_eq!(ids.reserve(|_| true), None);

    // The process table: reserve, bind, lock-free lookup, release.
    let id = reserve_compile_id();
    assert_ne!(id, 0);
    assert_eq!(lookup_compile_id(id), None, "reserved but unbound");
    let fake_cm = 0x7f3d_0000_1000usize;
    bind_compile_id(id, fake_cm);
    assert_eq!(lookup_compile_id(id), Some(fake_cm));
    release_compile_id(id);
    // (A parallel test may already have been reissued the id once graced,
    // so the assertion is only that it no longer names this body.)
    assert_ne!(lookup_compile_id(id), Some(fake_cm));
}

/// A reservation owns its id until it is handed off. Handed off, the id
/// stays reserved when the reservation drops (the artifact releases it);
/// an empty reservation releases nothing. The drop-before-hand-off path is
/// `release_compile_id`, pinned above; asserting it here would race a
/// parallel test reissuing the id once graced.
#[test]
pub(in crate::tests) fn a_handed_off_compile_id_outlives_its_reservation() {
    use std::sync::atomic::Ordering::Acquire;
    let mut reservation = CompileIdReservation::reserve();
    let id = reservation.hand_off();
    assert_ne!(id, 0, "a fresh process table has ids to issue");
    assert_eq!(reservation.id(), 0, "hand_off leaves nothing to release");
    drop(reservation);
    let slot = compile_id_slot(id).expect("a reserved id has a slot");
    assert_eq!(
        slot.load(Acquire),
        COMPILE_ID_RESERVED,
        "dropping a handed-off reservation must not release the artifact's id"
    );
    release_compile_id(id);
    drop(CompileIdReservation::none());
}

#[test]
pub(in crate::tests) fn test_jit_code_region_multiple_regions() {
    let mut region = JitCodeRegion::new();
    region.register(0x1000 as *const u8, 256);
    region.register(0x3000 as *const u8, 512);
    assert!(region.contains(0x1080 as *const u8));
    assert!(region.contains(0x3100 as *const u8));
    assert!(!region.contains(0x2000 as *const u8));
}

// ── validate_code_ptr tests ─────────────────────────────────────

/// The memo must not change any ANSWER, only the cost of reaching it.
///
/// Both arms are exercised in one process on purpose: routing this through
/// `CRATONVM_JIT_NO_CODE_PTR_MEMO` would be a `set_var` race against every
/// other test in this binary (see
/// `reference_set_var_in_a_parallel_test_suite_is_a_data_race`), so the
/// test drives the two implementations directly instead.
#[test]
pub(in crate::tests) fn code_ptr_memo_agrees_with_the_locked_lookup_on_every_probe() {
    // A real region, so both arms have something to find.
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let base = buf.as_ptr() as usize;
    let probes = [
        base,
        base + 4,
        base + 4092,
        base + 4096, // one past the end
        base.wrapping_sub(4),
        0x1000,
    ];
    for p in probes {
        let ptr = p as *const u8;
        let memo_answer = validate_code_ptr(ptr).is_ok();
        let locked_answer = {
            let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
            !ptr.is_null() && (p % 4 == 0) && regions.contains(ptr)
        };
        assert_eq!(
            memo_answer,
            locked_answer,
            "memo and locked lookup disagree at {p:#x} (region {base:#x}..{:#x})",
            base + 4096
        );
    }
    // A second pass, now that the memo is warm, must give the same answers:
    // a warm memo that starts admitting addresses outside its region is the
    // failure this whole design has to exclude.
    for p in probes {
        let ptr = p as *const u8;
        let memo_answer = validate_code_ptr(ptr).is_ok();
        let locked_answer = {
            let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
            !ptr.is_null() && (p % 4 == 0) && regions.contains(ptr)
        };
        assert_eq!(memo_answer, locked_answer, "warm memo disagrees at {p:#x}");    }}

/// A dropped buffer must stop validating, memo or no memo.
///
/// This is the one thing an address-keyed cache can get wrong, and the only
/// reason [`REGIONS_EPOCH`] exists. Without the epoch the memo would keep
/// admitting a pointer into a region that has been unmapped -- and the
/// caller's next act is `transmute` and `call`.
#[test]
pub(in crate::tests) fn dropping_a_region_invalidates_a_warm_memo() {
    let (base, len) = {
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let base = buf.as_ptr() as usize;
        // Warm the memo on this region.
        assert!(
            validate_code_ptr(base as *const u8).is_ok(),
            "a live region must validate"
        );
        (base, 4096usize)
    };
    // `buf` is dropped: `deregister` ran and bumped the epoch.
    let _ = len;
    assert!(
        validate_code_ptr(base as *const u8).is_err(),
        "a pointer into an unmapped region must stop validating even though \
         the memo was warm for it"
    );
}

/// The epoch is what the memo trusts, so it must actually move.
#[test]
pub(in crate::tests) fn registering_and_deregistering_move_the_regions_epoch() {
    let before = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let after_register = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        after_register > before,
        "register must bump the epoch ({before} -> {after_register})"
    );
    drop(buf);
    let after_drop = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        after_drop > after_register,
        "deregister must bump the epoch ({after_register} -> {after_drop})"
    );
}


#[test]
pub(in crate::tests) fn test_validate_code_ptr_null() {
    assert_eq!(
        validate_code_ptr(std::ptr::null()),
        Err("null JIT code pointer")
    );
}

#[test]
pub(in crate::tests) fn test_validate_code_ptr_misaligned() {
    assert_eq!(
        validate_code_ptr(0x1001 as *const u8),
        Err("misaligned JIT code pointer")
    );
}

// ── ExecutableBuffer tests ──────────────────────────────────────

#[test]
pub(in crate::tests) fn test_executable_buffer_new_and_emit() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    assert_eq!(buf.pos(), 0);
    buf.emit(&[0x90, 0x90, 0x90]); // NOP NOP NOP
    assert_eq!(buf.pos(), 3);
    assert_eq!(buf.as_slice(), &[0x90, 0x90, 0x90]);
}

#[test]
pub(in crate::tests) fn test_executable_buffer_emit_byte() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit_byte(0xCC);
    buf.emit_byte(0xC3);
    assert_eq!(buf.pos(), 2);
    assert_eq!(buf.as_slice(), &[0xCC, 0xC3]);
}

/// An erased range must be JUMPED over, not merely filled with NOPs.
///
/// This is the failure mode that has to be pinned by a byte assertion
/// rather than by behaviour: a range of `0x90` is perfectly *correct*, it
/// just executes. The IR backend shipped exactly that for the ~46-byte
/// shadow-stack thread fetch, and `CratonBench fib` — entered 2.27e9
/// times — ran ~2x slower than on the single-pass body. Nothing failed;
/// it was only slow. So assert the opcode.
#[test]
pub(in crate::tests) fn erase_range_with_jump_over_emits_a_jump_not_a_nop_sled() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    // A 46-byte span, the size the IR thread fetch actually occupies,
    // between a leading and a trailing sentinel.
    buf.emit_byte(0xCC);
    let start = buf.pos();
    buf.emit(&[0xAA; 46]);
    let end = buf.pos();
    buf.emit_byte(0xC3);

    buf.erase_range_with_jump_over(start, end);

    let code = buf.as_slice();
    assert_eq!(code[0], 0xCC, "byte before the range must not move");
    assert_eq!(code[start], 0xEB, "range must begin with JMP rel8");
    assert_eq!(
        code[start + 1],
        44,
        "displacement must land exactly on the first byte after the range"
    );
    assert!(
        code[start + 2..end].iter().all(|&b| b == 0x90),
        "the skipped remainder should be NOP-filled"
    );
    assert_eq!(code[end], 0xC3, "byte after the range must not move");
    assert!(
        !buf.overflowed(),
        "a 46-byte erase must not bail the buffer"
    );
}

/// A span too short to hold `JMP rel8`, or too long for its displacement,
/// still has to be erased — just without the jump.
#[test]
pub(in crate::tests) fn erase_range_with_jump_over_falls_back_to_nops_when_a_jump_will_not_fit() {
    // 1 byte: no room for the 2-byte JMP.
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xAA, 0xC3]);
    buf.erase_range_with_jump_over(0, 1);
    assert_eq!(buf.as_slice()[0], 0x90);
    assert_eq!(buf.as_slice()[1], 0xC3, "the range end is exclusive");

    // 130 bytes: `len - 2` exceeds the i8 displacement range.
    let mut big = ExecutableBuffer::new(256).expect("alloc failed");
    big.emit(&[0xAA; 130]);
    big.erase_range_with_jump_over(0, 130);
    assert!(
        big.as_slice()[..130].iter().all(|&b| b == 0x90),
        "an over-long span must fall back to a full NOP fill, never a truncated rel8"
    );
    assert!(!big.overflowed(), "the fallback must not bail the compile");
}

/// An empty or inverted range is a no-op, not a panic or a stray patch.
#[test]
pub(in crate::tests) fn erase_range_with_jump_over_ignores_an_empty_range() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xAA, 0xBB]);
    buf.erase_range_with_jump_over(1, 1);
    buf.erase_range_with_jump_over(2, 1);
    assert_eq!(buf.as_slice(), &[0xAA, 0xBB]);
    assert!(!buf.overflowed());
}

#[test]
pub(in crate::tests) fn test_executable_buffer_emit_checked_success() {
    let mut buf = ExecutableBuffer::new(8).expect("alloc failed");
    assert!(buf.emit_checked(&[1, 2, 3, 4]));
    assert_eq!(buf.pos(), 4);
}

#[test]
pub(in crate::tests) fn test_executable_buffer_emit_checked_overflow() {
    let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
    buf.emit(&[1, 2, 3]);
    // Only 1 byte left, trying to emit 2 should fail
    assert!(!buf.emit_checked(&[4, 5]));
    // Position should be unchanged
    assert_eq!(buf.pos(), 3);
}

#[test]
pub(in crate::tests) fn test_executable_buffer_patch_and_read_i32() {
    let mut buf = ExecutableBuffer::new(32).expect("alloc failed");
    buf.emit(&[0; 8]); // 8 zero bytes
    buf.try_patch_i32(0, 0x12345678).expect("in-bounds patch");
    assert_eq!(buf.read_i32(0), 0x12345678);
    buf.try_patch_i32(4, -42).expect("in-bounds patch");
    assert_eq!(buf.read_i32(4), -42);
}

#[test]
pub(in crate::tests) fn test_executable_buffer_emit_overflow_marks_flag() {
    // An emit past capacity must NOT panic: it records `overflowed` and
    // skips the write so the compile driver can bail gracefully.
    let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
    assert!(!buf.overflowed());
    buf.emit(&[1, 2, 3, 4, 5]); // 5 bytes into 4-capacity buffer
    assert!(buf.overflowed());
    assert_eq!(buf.pos(), 0, "overflowing emit must not advance len");
    buf.emit_byte(0xCC);
    assert_eq!(buf.pos(), 0, "overflowing emit_byte must not advance len");
}

#[test]
pub(in crate::tests) fn test_try_patch_i32_out_of_bounds_returns_err_does_not_panic() {
    // Task #20: a patch site pointing past `len` must return
    // `Err(PatchFailed)`, mark the buffer overflowed, and never panic.
    // The compile driver relies on the overflow flag to bail to the
    // interpreter when codegen has slipped past its size estimate.
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0; 8]); // only 8 bytes emitted, so offset 8..=11 is OOB
    let result = buf.try_patch_i32(8, 0xDEAD_BEEFu32 as i32);
    assert!(matches!(
        result,
        Err(CompileError::PatchFailed {
            kind: "i32",
            offset: 8
        })
    ));
    assert!(
        buf.overflowed(),
        "OOB patch must set the sticky overflow flag for the compile driver"
    );

    // Same contract for try_patch_byte.
    let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
    buf2.emit(&[0; 2]);
    let result2 = buf2.try_patch_byte(99, 0xCC);
    assert!(matches!(
        result2,
        Err(CompileError::PatchFailed {
            kind: "byte",
            offset: 99
        })
    ));
    assert!(buf2.overflowed());
}

#[test]
pub(in crate::tests) fn test_try_call_nine_args_returns_too_many_args_err_no_panic() {
    // Task #20: invoking a compiled method with more arguments than
    // the JIT's hand-rolled call thunks support must return
    // `Err(TooManyArgs)` rather than silently returning 0 or
    // panicking. Both `try_call` and `try_call_with_context` honor
    // this contract.
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — just a valid landing pad
    let cm = CompiledMethod::new(buf);

    // One word past the ceiling of `try_call` (sixteen on x86-64 since
    // round 12 wave 2, eight elsewhere).
    let cap = if cfg!(target_arch = "x86_64") {
        crate::TRY_CALL_MAX_WORDS
    } else {
        8
    };
    let over = vec![0i64; cap + 1];
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r = unsafe { cm.try_call(&over) };
    assert!(
        matches!(r, Err(CompileError::TooManyArgs(n)) if n == cap + 1),
        "expected TooManyArgs({}), got {r:?}",
        cap + 1
    );

    // `cap` args exceed the ceiling of `try_call_with_context` (one word is
    // consumed by the implicit vm_ptr).
    let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
    buf2.emit(&[0xC3]);
    let cm2 = CompiledMethod::new_with_context(buf2);
    let full = vec![0i64; cap];
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r2 = unsafe { cm2.try_call_with_context(0, &full) };
    assert!(
        matches!(r2, Err(CompileError::TooManyArgs(n)) if n == cap),
        "expected TooManyArgs({cap}) for context call, got {r2:?}"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn r12w2_try_call_passes_nine_to_sixteen_words_through_the_padded_thunk() {
    // MOV RAX, 42; RET — reads no argument, so any padded arity is sound.
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00, 0xC3]);
    let cm = CompiledMethod::new(buf);
    for n in 9..=crate::TRY_CALL_MAX_WORDS {
        let args = vec![7i64; n];
        // SAFETY: the body reads no argument and returns a constant; the
        // caller-cleanup C ABI makes any word count sound for it.
        let r = unsafe { cm.try_call(&args) };
        assert!(matches!(r, Ok(42)), "{n} words: {r:?}");
    }
    let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
    buf2.emit(&[0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00, 0xC3]);
    let cm2 = CompiledMethod::new_with_context(buf2);
    for n in 8..crate::TRY_CALL_MAX_WORDS {
        let args = vec![7i64; n];
        // SAFETY: as above.
        let r = unsafe { cm2.try_call_with_context(0, &args) };
        assert!(matches!(r, Ok(42)), "context + {n} words: {r:?}");
    }
}

#[test]
pub(in crate::tests) fn test_try_call_returns_ok_for_in_bounds_zero_arg_method() {
    // Task #44: positive test. After removing the panicking `call`
    // wrapper, `try_call` is the canonical happy-path entry point.
    // A minimal compiled method that returns 0 (XOR EAX,EAX; RET)
    // must produce `Ok(0)` when invoked with no arguments.
    // Encoded as: 31 C0 (xor eax, eax) C3 (ret).
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0x31, 0xC0, 0xC3]);
    let cm = CompiledMethod::new(buf);
    // SAFETY: the emitted code is a well-formed x86-64 leaf (XOR
    // EAX,EAX; RET) using the platform C ABI for a no-arg
    // `extern "C" fn() -> i64`. CompiledMethod::new finalized the
    // buffer, so the page is executable.
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let r = unsafe { cm.try_call(&[]) };
        assert_eq!(r, Ok(0), "try_call({{}}) should return Ok(0)");
    }
    // On non-x86_64 targets the emitted bytes are not valid, so
    // suppress the call but still exercise the type-level
    // try_call contract (TooManyArgs path) to keep the test
    // platform-independent.
    #[cfg(not(target_arch = "x86_64"))]
    {
        let big = [0i64; 9];
        let r = unsafe { cm.try_call(&big) };
        assert!(matches!(r, Err(CompileError::TooManyArgs(9))));
    }
}

#[test]
pub(in crate::tests) fn test_try_call_invalid_code_ptr_returns_err_not_silent_zero() {
    // Task #44 acceptance criterion 5: demonstrate that a JIT
    // "compile-failed" / runtime-invalid-pointer result propagates
    // as `Err(CompileError::InvalidCodePtr(_))` instead of being
    // silently downgraded to a `0` return value (which is what the
    // now-removed `call` wrapper did via `tracing::warn!` +
    // return 0).
    //
    // We synthesize the invalid-pointer condition by constructing
    // a `CompiledMethod` whose `entry` field has been overwritten
    // with a value outside any known JIT code region. The pointer
    // is also misaligned (1 byte) so `validate_code_ptr` would
    // reject it on alignment alone even if the region check ever
    // changes.
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]);
    let mut cm = CompiledMethod::new(buf);
    // Stomp on the entry pointer: misaligned + outside any region.
    cm.entry = 0x1 as *const u8;

    // SAFETY: the call is gated by `validate_code_ptr` inside
    // `try_call`, which rejects the synthetic pointer before any
    // transmute-to-fn-pointer happens. No real code is executed.
    let r = unsafe { cm.try_call(&[]) };
    assert!(
        matches!(r, Err(CompileError::InvalidCodePtr(_))),
        "expected InvalidCodePtr, got {r:?} (silent-zero would be Ok(0) — that contract is gone)"
    );

    // Equivalent contract for the context-needing variant: must
    // also surface InvalidCodePtr rather than swallowing it.
    let mut buf2 = ExecutableBuffer::new(16).expect("alloc failed");
    buf2.emit(&[0xC3]);
    let mut cm2 = CompiledMethod::new_with_context(buf2);
    cm2.entry = 0x3 as *const u8;
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r2 = unsafe { cm2.try_call_with_context(0, &[]) };
    assert!(
        matches!(r2, Err(CompileError::InvalidCodePtr(_))),
        "expected InvalidCodePtr for context call, got {r2:?}"
    );
}

/// Both sides of the `CRATONVM_JIT_OSR_DEAD_LOCALS` kill switch, through
/// the pure `can_osr_enter_with` so one process can pin both. The
/// no-native-offset refusal must hold either way: that one is not a
/// dead-local question at all.
#[test]
pub(in crate::tests) fn test_can_osr_enter_dead_masked_entry_both_sides() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![-1, 0, 4]);
    cm.osr_dead_mask = Some(vec![0, 0x80, 0]);

    for allow in [false, true] {
        assert!(
            !cm.can_osr_enter_with(0, allow),
            "negative native offset must bail regardless of the dead-local knob (allow={allow})"
        );
        assert!(
            cm.can_osr_enter_with(2, allow),
            "zero dead mask remains OSR-eligible (allow={allow})"
        );
    }
    assert!(
        !cm.can_osr_enter_with(1, false),
        "CRATONVM_JIT_OSR_DEAD_LOCALS=0 restores the blanket refusal"
    );
    assert!(
        cm.can_osr_enter_with(1, true),
        "default: the trampoline skips the masked locals, so the entry is admitted"
    );
}

/// The public entry point agrees with the pure one under this process's
/// (latched) flag value, whichever way the environment set it.
#[test]
pub(in crate::tests) fn test_can_osr_enter_matches_the_flagged_variant() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![0]);
    cm.osr_dead_mask = Some(vec![0x80]);

    let allow = osr_dead_local_entry_allowed();
    assert_eq!(cm.can_osr_enter(0), cm.can_osr_enter_with(0, allow));
}

/// `osr_enter`'s dead-mask short-circuit, pinned on the one side a unit
/// test can pin.
///
/// Only the REFUSING side is assertable here. Admitting means actually
/// running `osr_trampoline`, which builds a frame (pushes rbp, subtracts
/// `osr_frame_size`, spills the callee-saved set at `osr_callee_saved_base`)
/// and jumps to the target — so a stub body has to be a real compiled
/// method with matching frame metadata, not a bare `RET`. Entering a `RET`
/// stub returns into the middle of the trampoline's own prologue and
/// access-violates; an earlier version of this test did exactly that and
/// took the whole `cratonvm-jit` binary down with 0xC0000005.
///
/// The admitted path's coverage is `probes/OsrDeadLocalProbe.java`, which
/// executes eight real coalescing shapes end to end and checksums every
/// result against HotSpot. The decision itself is pinned above by
/// `test_can_osr_enter_dead_masked_entry_both_sides`.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn test_osr_enter_refuses_dead_mask_under_the_kill_switch() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — never entered on this path.
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![0]);
    cm.osr_dead_mask = Some(vec![0x80]);

    if osr_dead_local_entry_allowed() {
        // Default: the refusal is gone, so there is nothing to assert that
        // does not require entering the stub. Assert the decision instead.
        assert!(cm.can_osr_enter_with(0, true));
    } else {
        assert_eq!(
            // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
            unsafe { cm.osr_enter(0, &[], 0, 0) },
            None,
            "CRATONVM_JIT_OSR_DEAD_LOCALS=0 must still bail before the trampoline"
        );
    }
}

// ── Validated OSR entry (C2 review P1, "Add on-stack replacement") ──
//
// The loop these tests model, with javac's bcis:
//
//     0: iconst_0                  // int i = 0
//     1: istore_1
//     2: goto 8
//     5: <body>                    // sum += a[i]; i++
//     8: iload_1                   // <-- LOOP HEADER = the OSR entry bci
//     9: … if_icmplt 5             // the back-edge targets bci 8
//
// Locals: 0 = `int[] a` (ref), 1 = `int i`, 2 = `int sum`.
//
// Every assertion below is on the *pure* validator and on the pure
// post-exit resume decision. Neither enters compiled code: these fixtures
// carry OSR metadata over a bare `RET` body, and actually entering such a
// stub through the trampoline access-violates (see
// `test_osr_enter_refuses_dead_mask_under_the_kill_switch` for the history).

/// The loop-header bci used by every fixture in this group.
pub(in crate::tests) const OSR_T_HEADER: usize = 8;

/// An artifact that publishes exactly one OSR entry, at [`OSR_T_HEADER`],
/// with `num_locals` memory-homed locals and no deopt metadata (the
/// production shape: `deopt_points` is empty unless `CRATONVM_DEOPT_REAL`
/// was on at compile time).
pub(in crate::tests) fn osr_t_artifact(num_locals: usize) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — metadata-only fixture, never executed.
    let mut cm = CompiledMethod::new(buf);
    cm.method_label = "craton/probe/OsrEntry.sum([I)I".to_string();
    let mut table = vec![-1i32; OSR_T_HEADER + 2];
    table[OSR_T_HEADER] = 0;
    cm.osr_pc_to_native = Some(table);
    cm.osr_num_locals = num_locals;
    cm.osr_local_assignments = Some(vec![None; num_locals]);
    cm.osr_xmm_assignments = Some(vec![None; num_locals]);
    cm
}

/// An `OsrExit`-tagged deopt point at `bci` — the loop-boundary snapshot
/// that doubles as the *entry* contract at the same bci.
///
/// `ResumeSemantics::for_reason(OsrExit)` is `REEXECUTE`, which is what a
/// loop-header snapshot means: the header iteration has not run.
pub(in crate::tests) fn osr_t_exit_point(
    bci: u32,
    locals: Vec<deopt::FrameValue>,
    stack: Vec<deopt::FrameValue>,
) -> deopt::DeoptimizationPoint {
    deopt::DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: deopt::DeoptReason::OsrExit,
        action: deopt::DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: deopt::FrameState {
            class_id: 0,
            method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
            bci,
            locals,
            stack,
            monitors: Vec::new(),
            caller: None,
        },
        semantics: deopt::ResumeSemantics::for_reason(deopt::DeoptReason::OsrExit),
    }
}

/// The three-slot contract every fixture here uses: `[ref, int, int]`.
pub(in crate::tests) fn osr_t_contract_locals() -> Vec<deopt::FrameValue> {
    vec![
        deopt::FrameValue::StackSlotRef(-16),
        deopt::FrameValue::Register(0),
        deopt::FrameValue::Register(1),
    ]
}

/// The interpreter tags for `[int[] a, int i, int sum]`.
pub(in crate::tests) const OSR_T_TAGS: [u8; 3] = [
    cratonvm_types::VTAG_OBJECT,
    cratonvm_types::VTAG_INT,
    cratonvm_types::VTAG_INT,
];

/// The refusal tag carried by a bailout, or `None` if it is not an OSR
/// refusal at all.
pub(in crate::tests) fn osr_t_tag(b: &bailout::Bailout) -> Option<&'static str> {
    match &b.reason {
        bailout::BailoutReason::UnsupportedShape(tag) => Some(*tag),
        _ => None,
    }
}

/// The taxonomy is a closed set with no duplicates, and every tag routes to
/// the `unsupported_shape` bailout counter (never a panic, never a bare
/// `None`).
#[test]
pub(in crate::tests) fn osr_refusal_taxonomy_is_closed_and_counted() {
    let mut seen: Vec<&str> = Vec::new();
    for tag in OSR_REFUSAL_TAGS {
        assert!(!seen.contains(&tag), "duplicate OSR refusal tag {tag}");
        assert!(
            tag.starts_with("osr-entry-") || tag.starts_with("osr-exit-"),
            "{tag} does not name its phase"
        );
        seen.push(tag);
    }
    let before = bailout::bailout_count("unsupported_shape").expect("registered category");
    let b = osr_refusal(OSR_REFUSE_SLOT_TYPE, "probe");
    assert_eq!(b.category(), "unsupported_shape");
    assert!(b.to_string().contains(OSR_REFUSE_SLOT_TYPE), "{b}");
    assert!(
        bailout::bailout_count("unsupported_shape").unwrap() > before,
        "every OSR refusal must be counted"
    );
    // Permanence splits the taxonomy: artifact-only answers may be memoed
    // through `mark_osr_entry_rejected`; state-dependent ones may not.
    for tag in OSR_PERMANENT_REFUSAL_TAGS {
        assert!(
            OSR_REFUSAL_TAGS.contains(&tag),
            "{tag} is permanent but not in the taxonomy"
        );
        assert!(osr_refusal_is_permanent(&osr_refusal(tag, "probe")));
    }
    assert!(!osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_SLOT_TYPE,
        "probe"
    )));
    assert!(!osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_OPERAND_STACK,
        "probe"
    )));
    // A non-OSR bailout is never an OSR memo candidate.
    assert!(!osr_refusal_is_permanent(&bailout::Bailout::new(
        bailout::BailoutReason::RegisterPressure
    )));
}

// ── osr-01 item 4: the two views of one frame ────────────────────
//
// `osr_entry_frame_state` (the precise contract `validate_osr_entry`
// type-checks against) and the register homes (`osr_local_assignments` /
// `osr_xmm_assignments`, which is what `osr_trampoline` seeds through) are
// two views of the same compile. Until now nothing compared them, so an
// artifact could validate one entry and perform another.
//
// The fixtures below build the disagreement directly rather than trying to
// provoke it out of a real compile — a compiler bug that has never been
// observed cannot be reproduced, and the point of the check is that it
// would be caught if it ever happened.

/// An artifact with a precise contract at the header: `[ref, int, int]`.
pub(in crate::tests) fn osr_t_precise(num_locals: usize) -> CompiledMethod {
    let mut cm = osr_t_artifact(num_locals);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        osr_t_contract_locals(),
        Vec::new(),
    )];
    cm
}

/// The baseline the three refusals below are perturbations of: a precise
/// contract over memory-homed locals agrees with the register-home view
/// vacuously, because a memory-homed slot admits every type.
///
/// Stated as its own test so a check that refused *everything* could not
/// pass the refusal tests below and look correct.
#[test]
pub(in crate::tests) fn a_memory_homed_precise_contract_has_no_home_disagreement() {
    let cm = osr_t_precise(3);
    let locals = [0x1234_5678i64, 200, 4950];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .is_ok());
}

/// The corruption this check exists for, in its most direct form: the
/// contract says local 1 is a `double`, but its only home is a GPR. The
/// trampoline moves the double's bits into a general-purpose register the
/// compiled body reads as an integer, and the frame-slot store is elided
/// because a home exists — so nothing downstream can notice.
#[test]
pub(in crate::tests) fn a_wide_fp_local_homed_only_in_a_gpr_refuses_the_entry() {
    let mut cm = osr_t_precise(3);
    cm.deopt_points[0].frame_state.locals[1] = deopt::FrameValue::XmmDouble(2);
    cm.osr_local_assignments = Some(vec![None, Some(3), None]);
    let locals = [0x1234_5678i64, 0x4008_0000_0000_0000u64 as i64, 4950];
    let tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_DOUBLE,
        cratonvm_types::VTAG_INT,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &tags))
        .expect_err("a double with only a GPR home is a compiler bug, not an entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_CONTRACT_DISAGREEMENT));
    let msg = err.to_string();
    assert!(msg.contains("local 1"), "{msg}");
    assert!(msg.contains("double"), "{msg}");
}

/// The other direction, and the more dangerous one: a reference whose only
/// home is an XMM register is seeded into the FP file with the frame-slot
/// store elided, so the GC's precise map has nothing to find and the body
/// reads a stale word.
#[test]
pub(in crate::tests) fn a_reference_homed_only_in_an_xmm_refuses_the_entry() {
    let mut cm = osr_t_precise(3);
    cm.osr_xmm_assignments = Some(vec![Some(1), None, None]);
    let locals = [0x1234_5678i64, 200, 4950];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a reference with only an XMM home must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_CONTRACT_DISAGREEMENT));
    assert!(err.to_string().contains("local 0"), "{err}");
}

/// Over-refusal check, and the reason `osr_homes_admit` answers with a
/// *set*. A JVM slot index is legally reused by locals of different types
/// in disjoint live ranges, so the allocator can give one slot BOTH a GPR
/// and an XMM home — `DualPivotQuicksort.mixedInsertionSort` does exactly
/// this. The trampoline seeds both, so neither home contradicts the other.
#[test]
pub(in crate::tests) fn a_slot_with_both_register_homes_is_not_a_disagreement() {
    let mut cm = osr_t_precise(3);
    cm.osr_local_assignments = Some(vec![None, Some(3), None]);
    cm.osr_xmm_assignments = Some(vec![None, Some(1), None]);
    let locals = [0x1234_5678i64, 200, 4950];
    assert!(
        cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "a slot reused across disjoint live ranges may carry both homes"
    );
}

/// A masked-dead slot is not seeded at all, so its homes describe nothing
/// that happens at this entry and must not be cross-checked. Without this
/// the check would refuse entries the dead mask exists to make safe.
#[test]
pub(in crate::tests) fn a_masked_dead_slot_is_not_cross_checked() {
    let mut cm = osr_t_precise(3);
    cm.osr_xmm_assignments = Some(vec![Some(1), None, None]);
    let mut mask = vec![0u64; OSR_T_HEADER + 2];
    mask[OSR_T_HEADER] = 0b1; // local 0 is dead here
    cm.osr_dead_mask = Some(mask);
    let locals = [0x1234_5678i64, 200, 4950];
    // The kill switch turns a non-zero mask into a blanket refusal, which
    // would make this pass for the wrong reason.
    if !osr_dead_local_entry_allowed() {
        return;
    }
    assert!(
        cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "a slot the trampoline skips carries no home claim"
    );
}

/// A snapshot shorter than the compiled frame describes a prefix; the tail
/// is simply untyped, not contradicted. (`validate_osr_entry` already falls
/// back to the register-home expectation for those slots.)
#[test]
pub(in crate::tests) fn a_short_snapshot_does_not_manufacture_a_disagreement() {
    let mut cm = osr_t_precise(3);
    cm.deopt_points[0].frame_state.locals.truncate(1);
    cm.osr_xmm_assignments = Some(vec![None, None, Some(3)]);
    let locals = [0x1234_5678i64, 200, 4950];
    let tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_DOUBLE,
    ];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &tags))
        .is_ok());
}

/// The verdict is a pure function of the artifact and the entry pc — it
/// does not read the offered locals — so it may be memoed. Asserted here as
/// well as in the taxonomy test because memoing a *state-dependent* refusal
/// is the same bug in the other direction: a silent, permanent loss of OSR.
#[test]
pub(in crate::tests) fn the_home_disagreement_refusal_is_memoable() {
    assert!(osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_CONTRACT_DISAGREEMENT,
        "probe"
    )));
}

/// A hot loop's OSR request must name the loop header, because the entry
/// bci is what the compiled artifact publishes a native offset for — and
/// the loop, which keeps running while the compile is queued, must not
/// enqueue a task per iteration.
///
/// This used to drive `TieredCompilationManager::on_backedge` across its
/// `osr_threshold`. That door had no production caller (the interpreter's
/// per-frame back-edge schedule is the throttle, and it calls
/// `request_osr`) and was deleted.
#[test]
pub(in crate::tests) fn a_long_running_loop_requests_osr_at_the_backedge() {
    use tiered::{CompilationTier, MethodKey, TieredCompilationManager};
    // A manager of our own, and a method key no other test names: OSR
    // denials and per-method state are per manager, so nothing here is
    // shared with another test.
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("craton/probe/OsrEntry", "sum", "([I)I");
    let bci = OSR_T_HEADER as u32;

    let task = mgr
        .request_osr(&key, bci)
        .expect("a hot back-edge must request an OSR compile");
    assert_eq!(
        task.osr_bci,
        Some(bci),
        "the request must name the loop header, not the method entry"
    );
    assert_eq!(task.target_tier, CompilationTier::C2);
    assert_eq!(task.method_key, key);
    for i in 0..63 {
        assert!(
            mgr.request_osr(&key, bci).is_none(),
            "back-edge {i}: an already-queued OSR request must not be re-enqueued"
        );
    }
    assert_eq!(mgr.queue_size(), 1);
}

/// The JIT key hash XOR-folded per-component hashes, so equal components
/// cancelled and swapped components collided. The negative memos key on it
/// without the cache's full-key check, so a collision there refused an
/// innocent method.
#[test]
pub(in crate::tests) fn jit_key_hash_does_not_collide_on_equal_or_swapped_components() {
    let id = cratonvm_types::ClassId::new(0);
    assert_ne!(
        compute_jit_key_hash("Foo", "Foo", "()V", id),
        compute_jit_key_hash("Bar", "Bar", "()V", id),
        "equal class and method names must not cancel out"
    );
    assert_ne!(
        compute_jit_key_hash("a/A", "b", "()V", id),
        compute_jit_key_hash("b", "a/A", "()V", id),
        "swapped components must not collide"
    );
    assert_ne!(
        compute_jit_key_hash("ab", "c", "()V", id),
        compute_jit_key_hash("a", "bc", "()V", id),
        "a component boundary is part of the key"
    );
    assert_ne!(
        compute_jit_key_hash("a/A", "m", "()V", id),
        compute_jit_key_hash("a/A", "m", "()V", cratonvm_types::ClassId::new(3)),
        "the declaring class id is part of the key"
    );
}

/// Verdicts go with their class, and a verdict recorded against older
/// bytecode (redefine epoch) or older compile state (install epoch) stops
/// counting.
#[test]
pub(in crate::tests) fn jit_verdicts_are_cleared_with_their_class_and_expire_with_their_epoch() {
    let class = "craton/test/VerdictClearing";
    let id = cratonvm_types::ClassId::new(0);
    mark_jit_bail_listed(id, class, "bailed", "()V");
    record_compile_refusal(id, class, "bailed", "()V", "test-refusal");
    mark_osr_entry_rejected(id, class, "loop", "()V", 12);
    assert!(is_jit_bail_listed(id, class, "bailed", "()V"));
    assert!(jit_bail_reason_for(id, class, "bailed", "()V").is_some());
    assert!(is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(
        !is_osr_entry_rejected(id, class, "loop", "()V", 13),
        "per pc"
    );
    assert!(
        !is_jit_bail_listed(id, class, "notBailed", "()V"),
        "a verdict is looked up by its exact names"
    );

    assert_eq!(forget_jit_verdicts_for_class(id, class), 2);
    assert!(!is_jit_bail_listed(id, class, "bailed", "()V"));
    assert!(jit_bail_reason_for(id, class, "bailed", "()V").is_none());
    assert!(!is_osr_entry_rejected(id, class, "loop", "()V", 12));

    // Epoch expiry, checked on the stamp itself: bumping the real epochs
    // would expire every other test's verdicts in this binary.
    let about_bytecode = VerdictStamp {
        redefine_epoch: 3,
        install_epoch: None,
    };
    assert!(about_bytecode.is_current_at(3, 100));
    assert!(
        about_bytecode.is_current_at(3, 101),
        "a bytecode verdict survives a code-cache flush"
    );
    assert!(
        !about_bytecode.is_current_at(4, 100),
        "but not a redefinition"
    );
    let about_compile_state = VerdictStamp {
        redefine_epoch: 3,
        install_epoch: Some(100),
    };
    assert!(about_compile_state.is_current_at(3, 100));
    assert!(
        !about_compile_state.is_current_at(3, 101),
        "a compile-state verdict expires with the install epoch"
    );
}

/// A refusal site recorded for a method is cleared once a compile of that method succeeds.
#[test]
pub(in crate::tests) fn jit_bail_reason_is_cleared_on_successful_compile() {
    let class = "craton/test/BailClear";
    let id = cratonvm_types::ClassId::new(0);
    record_compile_refusal(id, class, "method", "()V", "code-buffer-estimate-too-small");
    assert!(jit_bail_reason_for(id, class, "method", "()V").is_some());

    jit_clear_bail_site(id, class, "method", "()V");
    assert!(jit_bail_reason_for(id, class, "method", "()V").is_none());
}

/// Two same-named classes in different loaders keep separate verdicts, and
/// forgetting one class's verdicts leaves the other's alone.
#[test]
pub(in crate::tests) fn same_named_classes_with_different_ids_keep_separate_verdicts() {
    let class = "craton/test/VerdictPerLoader";
    // Ids far above anything a unit test's fixtures allocate, so a forget
    // by id cannot reach another test's entries.
    let app = cratonvm_types::ClassId::new(0x7A00_0001);
    let plugin = cratonvm_types::ClassId::new(0x7A00_0002);
    let no_id = cratonvm_types::ClassId::new(0);

    mark_jit_bail_listed(app, class, "m", "()V");
    record_compile_refusal(app, class, "m", "()V", "test-refusal");
    mark_osr_entry_rejected(app, class, "loop", "()V", 7);
    assert!(is_jit_bail_listed(app, class, "m", "()V"));
    assert!(jit_bail_reason_for(app, class, "m", "()V").is_some());
    assert!(is_osr_entry_rejected(app, class, "loop", "()V", 7));
    assert!(
        !is_jit_bail_listed(plugin, class, "m", "()V"),
        "one loader's bail-list verdict must not refuse the other loader's class"
    );
    assert!(jit_bail_reason_for(plugin, class, "m", "()V").is_none());
    assert!(!is_osr_entry_rejected(plugin, class, "loop", "()V", 7));
    assert!(
        !is_jit_bail_listed(no_id, class, "m", "()V"),
        "an id-less lookup does not see an identified class's verdict"
    );

    mark_jit_bail_listed(plugin, class, "other", "()V");
    mark_osr_entry_rejected(plugin, class, "loop", "()V", 9);

    // Forgetting the app's class drops its two methods' entries only.
    assert_eq!(forget_jit_verdicts_for_class(app, class), 2);
    assert!(!is_jit_bail_listed(app, class, "m", "()V"));
    assert!(jit_bail_reason_for(app, class, "m", "()V").is_none());
    assert!(!is_osr_entry_rejected(app, class, "loop", "()V", 7));
    assert!(
        is_jit_bail_listed(plugin, class, "other", "()V"),
        "forgetting one loader's class must leave the other's verdicts"
    );
    assert!(is_osr_entry_rejected(plugin, class, "loop", "()V", 9));

    // An id-less forget falls back to the name and errs towards forgetting.
    assert_eq!(forget_jit_verdicts_for_class(no_id, class), 2);
    assert!(!is_jit_bail_listed(plugin, class, "other", "()V"));
    assert!(!is_osr_entry_rejected(plugin, class, "loop", "()V", 9));
}

/// The IR refusal memo is per declaring class identity as well as per
/// redefine epoch.
#[test]
pub(in crate::tests) fn the_ir_refusal_memo_key_separates_same_named_classes() {
    let hash = ir_method_memo_hash("craton/test/IrMemoPerLoader", "m", "()V");
    let app = cratonvm_types::ClassId::new(11);
    let plugin = cratonvm_types::ClassId::new(12);
    assert_ne!(
        ir_refusal_memo_key(hash, app, 0),
        ir_refusal_memo_key(hash, plugin, 0),
        "the class id is part of the key"
    );
    assert_ne!(
        ir_refusal_memo_key(hash, app, 0),
        ir_refusal_memo_key(hash, app, 1),
        "the redefine epoch is part of the key"
    );
    assert_eq!(
        ir_refusal_memo_key(hash, app, 3),
        ir_refusal_memo_key(hash, app, 3)
    );
}

#[test]
pub(in crate::tests) fn compile_state_dependent_osr_refusals_are_memoable_and_classified() {
    for tag in OSR_COMPILE_STATE_REFUSAL_TAGS {
        assert!(
            OSR_PERMANENT_REFUSAL_TAGS.contains(&tag),
            "{tag} must be memoable in the first place"
        );
        assert!(osr_refusal_depends_on_compile_state(&osr_refusal(
            tag, "probe"
        )));
    }
    assert!(!osr_refusal_depends_on_compile_state(&osr_refusal(
        OSR_REFUSE_PC_NOT_AN_ENTRY,
        "probe"
    )));
}

/// The happy path, on the production artifact shape (no deopt metadata):
/// the plan's resume bci IS the entry bci, which is the interpreter's own
/// pc — so a refusal repeats no work.
#[test]
pub(in crate::tests) fn a_validated_osr_entry_resumes_at_the_interpreters_own_pc() {
    let cm = osr_t_artifact(3);
    let locals = [0x1234_5678i64, 200, 4950];
    let state = OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS);

    let plan = cm.validate_osr_entry(&state).expect("clean entry");
    assert_eq!(plan.entry_pc, OSR_T_HEADER);
    assert_eq!(
        plan.resume_bci, state.pc,
        "the fallback resume point must be the pc the interpreter is already at"
    );
    assert_eq!(plan.native_offset, 0);
    assert_eq!(plan.contract, OsrContractSource::RegisterHomes);
    assert_eq!(
        plan.exit_policy,
        OsrExitPolicy::PropagateOnly,
        "an artifact with no deopt points has no frame-deopt exit to transfer through"
    );
    assert_eq!(plan.expected_locals.len(), 3);
    assert!(plan
        .expected_locals
        .iter()
        .all(|e| *e == OsrSlotExpectation::Unconstrained));

    // A pc the artifact published no entry for is refused, not guessed at.
    let at_body = OsrEntryState::at(5, &locals, &OSR_T_TAGS);
    assert_eq!(
        osr_t_tag(&cm.validate_osr_entry(&at_body).unwrap_err()),
        Some(OSR_REFUSE_PC_NOT_AN_ENTRY)
    );
}

/// A type mismatch in an incoming slot refuses with a named reason, under
/// both contract strengths.
///
/// Inferred (register-home) contract: an XMM-homed local can only hold FP,
/// so an incoming `int` there would be seeded into an XMM register and read
/// back as a double — a silent miscompile, which is what this check exists
/// to turn into a refusal.
///
/// Precise (`FrameState`) contract: the exact JVM type is known, so an
/// `int` offered where the compiled body reads a reference is caught even
/// though both are GPR-homed 64-bit words.
#[test]
pub(in crate::tests) fn an_incoming_slot_type_mismatch_refuses_with_a_named_reason() {
    // --- inferred contract ---
    let mut cm = osr_t_artifact(3);
    // Local 2 is XMM-homed: the compiled body treats it as a double.
    cm.osr_xmm_assignments = Some(vec![None, None, Some(3)]);
    let locals = [0x1234_5678i64, 200, 4950];

    let ok_tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_DOUBLE,
    ];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &ok_tags))
        .is_ok());

    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an int offered to an xmm-homed slot must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_SLOT_TYPE));
    let msg = err.to_string();
    assert!(msg.contains("local 2"), "{msg}");
    assert!(msg.contains("xmm-homed"), "{msg}");
    assert!(msg.contains("int"), "{msg}");

    // --- precise contract ---
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .is_ok());

    let ref_as_int = [
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &ref_as_int))
        .expect_err("an int offered where the body reads a reference must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_SLOT_TYPE));
    assert!(err.to_string().contains("local 0"), "{err}");

    // A `jsr` return address has no JIT representation at all.
    let retaddr = [
        cratonvm_types::VTAG_RETADDR,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    assert_eq!(
        osr_t_tag(
            &cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &retaddr))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_RETURNADDRESS)
    );
}

/// An OSR entry whose contract contains a slot the compiled frame cannot
/// describe refuses — it never guesses a value.
///
/// `MaterializationRequired` is the case that matters: it means an
/// optimization deleted a value that WAS live and left no rebuild recipe.
/// Reconstructing it as `Undefined`/zero would be a silent
/// wrong-reconstruction, which is precisely why that variant exists as a
/// separate marker from `Undefined`.
#[test]
pub(in crate::tests) fn an_undescribable_slot_refuses_the_osr_entry() {
    let locals = [0x1234_5678i64, 200, 4950];

    for (slot_value, what) in [
        (
            deopt::FrameValue::MaterializationRequired(deopt::EliminatedValue::allocation(
                17,
                42,
                deopt::EliminationCause::ScalarReplacedObject,
            )),
            "MaterializationRequired",
        ),
        (deopt::FrameValue::Unsupported, "Unsupported"),
        (
            deopt::FrameValue::VirtualObjectRef(0),
            "VirtualObjectRef (needs a heap allocation to rebuild)",
        ),
    ] {
        let mut cm = osr_t_artifact(3);
        cm.deopt_points = vec![osr_t_exit_point(
            OSR_T_HEADER as u32,
            vec![
                slot_value,
                deopt::FrameValue::Register(0),
                deopt::FrameValue::Register(1),
            ],
            Vec::new(),
        )];
        let result = cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS));
        let err = match result {
            Ok(plan) => panic!("{what} must refuse the entry, got {plan:?}"),
            Err(e) => e,
        };
        assert_eq!(
            osr_t_tag(&err),
            Some(OSR_REFUSE_UNDESCRIBABLE_SLOT),
            "{what} refused with the wrong reason: {err}"
        );
    }
}

/// The refusal names the offending slot, is permanent (so the caller may
/// memo it), and is distinct from the whole-artifact "some other exit is
/// unresumable" answer.
#[test]
pub(in crate::tests) fn an_undescribable_slot_names_the_slot_it_refused() {
    let locals = [0x1234_5678i64, 200, 4950];
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::MaterializationRequired(deopt::EliminatedValue::allocation(
                17,
                42,
                deopt::EliminationCause::ScalarReplacedObject,
            )),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a MaterializationRequired slot in the entry contract must refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNDESCRIBABLE_SLOT));
    assert!(err.to_string().contains("local 0"), "{err}");
    assert!(osr_refusal_is_permanent(&err));

    // An unresumable slot at some OTHER deopt point is a different, still
    // pre-entry, refusal: entering would commit iterations that the bail
    // could then only discard.
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![
        osr_t_exit_point(
            OSR_T_HEADER as u32,
            vec![
                deopt::FrameValue::StackSlotRef(-16),
                deopt::FrameValue::Register(0),
                deopt::FrameValue::Register(1),
            ],
            Vec::new(),
        ),
        osr_t_exit_point(5, vec![deopt::FrameValue::Unsupported], Vec::new()),
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an unresumable exit anywhere in the body must refuse the entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
}

/// The trampoline seeds locals only, so a live operand stack is refused
/// rather than silently dropped — and an inlined scope, which
/// `FrameState::caller` still cannot describe, is refused too.
#[test]
pub(in crate::tests) fn unrepresentable_entry_shapes_are_refused() {
    let cm = osr_t_artifact(3);
    let locals = [0x1234_5678i64, 200, 4950];
    let stack = [7i64];
    let stack_tags = [cratonvm_types::VTAG_INT];
    let with_stack = OsrEntryState {
        pc: OSR_T_HEADER,
        locals: &locals,
        local_tags: &OSR_T_TAGS,
        stack: &stack,
        stack_tags: &stack_tags,
    };
    assert_eq!(
        osr_t_tag(&cm.validate_osr_entry(&with_stack).unwrap_err()),
        Some(OSR_REFUSE_OPERAND_STACK)
    );

    // Local count must match the compiled frame exactly.
    let short = [0x1234_5678i64, 200];
    assert_eq!(
        osr_t_tag(
            &cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &short, &OSR_T_TAGS[..2]))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_LOCAL_COUNT)
    );

    // Inlined scope: `FrameState::caller` is `None` at every producer, so an
    // artifact that inlined something and can also take a frame-deopt exit
    // cannot tell an outer-scope bail from one inside the callee.
    let mut inlined = osr_t_artifact(3);
    // The refusal is the OPTIMIZING tier's now (round 15 wave 1, lane calls):
    // a single-pass artifact's splice postcondition makes its chainless
    // points outer-scope ones -- see the single-pass twin below.
    inlined.used_ir_backend = true;
    inlined.inlined_methods = vec![(
        "craton/probe/OsrEntry".to_string(),
        "helper".to_string(),
        "(I)I".to_string(),
    )];
    inlined.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let err = inlined
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an inlined region the deopt metadata cannot describe must refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_INLINED_SCOPE));
    assert!(osr_refusal_is_permanent(&err));

    // The single-pass twin: same inlined method, same chainless header exit
    // point, admitted (`r15w1-calls-osr-entry-single-pass-splice-patch`).
    let mut sp_inlined = osr_t_artifact(3);
    sp_inlined.inlined_methods = vec![(
        "craton/probe/OsrEntry".to_string(),
        "helper".to_string(),
        "(I)I".to_string(),
    )];
    sp_inlined.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        osr_t_contract_locals(),
        Vec::new(),
    )];
    sp_inlined
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a single-pass artifact's chainless points are outer-scope ones");

    // An artifact with an unconditional uncommon trap is never worth
    // entering: it bails on every execution reaching that site.
    let mut trapping = osr_t_artifact(3);
    trapping.has_indy_trap = true;
    assert_eq!(
        osr_t_tag(
            &trapping
                .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_UNCONDITIONAL_TRAP)
    );
}

/// Entry, then a forced deopt at the loop boundary: the resume lands on the
/// SAME bci the entry used, carrying the same local layout — with the
/// JIT-advanced values, not the pre-entry ones.
#[test]
pub(in crate::tests) fn a_forced_deopt_returns_to_the_same_bci_with_the_same_locals() {
    let mut cm = osr_t_artifact(3);
    let contract_locals = vec![
        deopt::FrameValue::StackSlotRef(-16),
        deopt::FrameValue::Register(0),
        deopt::FrameValue::Register(1),
    ];
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        contract_locals.clone(),
        Vec::new(),
    )];

    let array = 0x1234_5678i64;
    let entry_locals = [array, 200, 4950];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &entry_locals, &OSR_T_TAGS))
        .expect("clean entry");
    assert_eq!(plan.contract, OsrContractSource::PreciseFrameState);
    assert_eq!(
        plan.exit_policy,
        OsrExitPolicy::ExactTransfer,
        "every exit of this artifact reconstructs a resumable frame"
    );

    // The forced deopt: the OSR'd body ran to the loop boundary again with
    // i = 700, sum = 244_650, and reconstructed its frame there.
    let rframe = deopt::ReconstructedFrame {
        class_id: 0,
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(array as u64),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(244_650),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };
    let resume = plan
        .resume_after_exit(&cm, &rframe)
        .expect("a loop-boundary exit must resume exactly");
    assert_eq!(
        resume, OSR_T_HEADER,
        "the resume bci is the loop header the entry used"
    );
    assert_eq!(resume, plan.entry_pc);

    // Same slot layout, same per-slot types as the entry contract…
    assert_eq!(rframe.locals.len(), plan.expected_locals.len());
    for (i, want) in contract_locals.iter().enumerate() {
        assert_eq!(
            OsrSlotType::from_frame_value(&rframe.locals[i]),
            OsrSlotType::from_frame_value(want),
            "local {i} changed JVM type across the OSR transition"
        );
    }
    // …but the induction variable carries the JIT's advanced value.
    assert_eq!(rframe.locals[1], deopt::FrameValue::Int(700));
    assert_ne!(
        rframe.locals[1],
        deopt::FrameValue::Int(entry_locals[1] as i64)
    );
}

/// No iteration executes twice across the transition.
///
/// The recorded defect ("an OSR bail re-runs loop iterations") is: compiled
/// code commits N iterations, then bails, and the interpreter resumes at the
/// *entry* bci from its own stale locals — replaying all N. This test
/// executes the whole trip through the API and asserts every iteration index
/// is executed exactly once, in order.
///
/// The structural guarantee behind it: `resume_bci` (== the entry bci) is
/// only reachable when `validate_osr_entry` FAILED, i.e. before compiled
/// code ran; after entry the only sanctioned resume point is
/// `resume_after_exit`, which returns the reconstructed frame's own bci or
/// refuses.
#[test]
pub(in crate::tests) fn no_iteration_executes_twice_across_the_osr_transition() {
    const TRIP: usize = 1000;
    let mut executed: Vec<usize> = Vec::with_capacity(TRIP);

    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];

    // Phase 1 — interpreted warm-up. `i` is the interpreter's local 1.
    let mut i = 0usize;
    while i < 200 {
        executed.push(i);
        i += 1;
    }

    // A refused entry must leave the interpreter exactly where it was: it
    // resumes at `resume_bci` == its own pc, having executed nothing. Model
    // that with a deliberately mistyped slot.
    let bad_tags = [
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    let locals = [0x1234_5678i64, i as i64, 0];
    let refused = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &bad_tags))
        .unwrap_err();
    assert_eq!(osr_t_tag(&refused), Some(OSR_REFUSE_SLOT_TYPE));
    assert_eq!(executed.len(), 200, "a refusal must execute nothing");

    // Phase 2 — the entry actually validates.
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("clean entry");
    assert_eq!(plan.resume_bci, OSR_T_HEADER);

    // Phase 3 — compiled code runs iterations [200, 700) and is then forced
    // to deopt at the loop boundary.
    let jit_start = i;
    while i < 700 {
        executed.push(i);
        i += 1;
    }
    assert!(i > jit_start, "the JIT must have committed real iterations");
    let rframe = deopt::ReconstructedFrame {
        class_id: 0,
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(i as i64),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };

    // Phase 4 — the resume. The interpreter's induction variable comes from
    // the RECONSTRUCTED frame, never from its own pre-entry copy.
    let resume_bci = plan
        .resume_after_exit(&cm, &rframe)
        .expect("a loop-boundary exit must resume exactly");
    assert_eq!(resume_bci, OSR_T_HEADER);
    let resumed_i = match rframe.locals[1] {
        deopt::FrameValue::Int(v) => v as usize,
        ref other => panic!("induction variable came back as {other:?}"),
    };
    assert_eq!(
        resumed_i, i,
        "the resumed frame must carry the JIT's advanced counter, not the pre-entry one"
    );
    i = resumed_i;
    while i < TRIP {
        executed.push(i);
        i += 1;
    }

    assert_eq!(
        executed,
        (0..TRIP).collect::<Vec<_>>(),
        "every iteration must execute exactly once, in order"
    );
}

/// A bail the artifact cannot describe must NOT hand back a resume point.
///
/// This is the other half of the no-replay guarantee: when the frame is
/// unresumable the API refuses, so the caller has no sanctioned way to fall
/// back to the entry bci (which would replay every committed iteration).
#[test]
pub(in crate::tests) fn an_undescribable_exit_refuses_to_name_a_resume_point() {
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let locals = [0x1234_5678i64, 200, 4950];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("clean entry");

    let base = deopt::ReconstructedFrame {
        class_id: 0,
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };

    // A deleted value in a local: the live frame's stale word is genuinely
    // wrong, not merely unread, so it cannot be left in place.
    let mut materialize = base.clone();
    materialize.locals[1] = deopt::FrameValue::MaterializationRequired(
        deopt::EliminatedValue::unknown(deopt::EliminationCause::EliminatedStore),
    );
    let err = plan.resume_after_exit(&cm, &materialize).unwrap_err();
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_EXIT_REPLAY));
    assert!(err.to_string().contains("local 1"), "{err}");

    // `Unsupported` in a LOCAL is the documented exception: refusing would
    // replay the exit's committed side effects from pre-entry state, so the
    // live frame's current value stands and the resume goes ahead.
    let mut unsupported_local = base.clone();
    unsupported_local.locals[2] = deopt::FrameValue::Unsupported;
    assert_eq!(
        plan.resume_after_exit(&cm, &unsupported_local).unwrap(),
        OSR_T_HEADER
    );

    // `Unsupported` on the operand STACK has no such argument.
    let mut unsupported_stack = base.clone();
    unsupported_stack.stack = vec![deopt::FrameValue::Unsupported];
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &unsupported_stack).unwrap_err()),
        Some(OSR_REFUSE_EXIT_REPLAY)
    );

    // An inlined caller chain: describable or not, the VM's in-place
    // OSR-exit transfer is single-frame, so it cannot be resumed here.
    let mut inlined = base.clone();
    inlined.caller_frames = vec![base.clone()];
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &inlined).unwrap_err()),
        Some(OSR_REFUSE_INLINED_SCOPE)
    );

    // A bci this artifact never recorded a deopt point for is a mis-routed
    // stash, not a resume point.
    let mut stray = base.clone();
    stray.bci = 5;
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &stray).unwrap_err()),
        Some(OSR_REFUSE_EXIT_REPLAY)
    );
}


/// The lane's "what to refuse", and the two halves of getting it right:
/// an ambiguous resume bci refuses the ENTRY, and copies that agree do not.
///
/// > An OSR exit whose resume bci has more than one possible native image,
/// > or none.
///
/// The refusal is at ADMISSION on purpose. Refusing at exit is not the
/// mirror image: by then the body has committed iterations and the caller's
/// only remaining move is the safe reject, which re-runs all of them — the
/// exact defect this lane exists for. So the test asserts the refusal
/// happens where nothing has run, and that it is memoable.
///
/// The two NON-refusing cases carry as much weight as the refusing one, and
/// one of them is here because a measurement put it here: refusing a
/// `reason`-only disagreement took 10 of CratonBench's 11 OSR refusals and
/// cost `matrixKernel(I)I` its OSR permanently.
#[test]
pub(in crate::tests) fn an_ambiguous_resume_bci_refuses_the_entry_and_a_copy_does_not() {
    let locals = [0x1234_5678i64, 200, 4950];

    // Two images of the loop header that disagree on the SEMANTICS.
    //
    // This used to be spelled `REEXECUTE` vs `RETHROW`, on the reasoning
    // that a `RESUME` point is refused by the per-point rule before this
    // check sees it while a `RETHROW` point is explicitly allowed to exist.
    // Both halves of that were true and the conclusion was wrong: a
    // `RETHROW` point is not a competing resume IMAGE — the same sentence
    // that admits it says such points are "stashed separately and never
    // routed to a resume" — so a bci carrying one plus one `REEXECUTE`
    // point has exactly one image and no ambiguity.
    //
    // Refusing it was not academic. After the RBC.6b lift (2026-08-17) a
    // `try { foo(x); } catch (...)` loop puts a speculative-dispatch guard
    // and a `PendingException` frame on the same invoke bci, which is the
    // ORDINARY shape of the population that lift admits;
    // `probes/OsrExcTableProbe.java` reported `osr_entered=0
    // osr_entry_refused_ambiguous_image=15` with every correctness arm
    // green. `osr_exit::resume_image` now skips rethrow points, and
    // `a_rethrow_point_is_not_a_competing_resume_image` is that case.
    //
    // What remains here is the genuine disagreement between two things that
    // both claim to be resume points, which is the wrong-code half.
    let mut cm = osr_t_artifact(3);
    let mut other = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    other.native_offset = 0x90;
    other.semantics = deopt::ResumeSemantics::RESUME;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        other,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .unwrap_err();
    // The per-point `RESUME` rule fires first (it walks the list before the
    // ambiguity scan), so THAT is what this pair reports. Assert the
    // ambiguity scan itself directly, where nothing else can shadow it.
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
    assert!(
        osr_refusal_is_permanent(&err),
        "the point list is a pure function of the artifact, so the refusal is memoable"
    );
    assert!(
        osr_exit::first_ambiguous_resume_bci(&cm.deopt_points).is_some(),
        "two points that both claim to be resume points and disagree are ambiguous"
    );

    // And the rethrow pairing, which must NOT be ambiguous — the correction
    // above, asserted at the same level as the refusal it replaced.
    let mut rethrow_pair = osr_t_artifact(3);
    let mut exc = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    exc.native_offset = 0x90;
    exc.reason = deopt::DeoptReason::PendingException;
    exc.semantics = deopt::ResumeSemantics::for_reason(deopt::DeoptReason::PendingException);
    assert_eq!(exc.semantics, deopt::ResumeSemantics::RETHROW);
    rethrow_pair.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        exc,
    ];
    let plan = rethrow_pair
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a RETHROW point sharing a bci must not refuse the entry");
    assert_eq!(plan.exit_policy, OsrExitPolicy::ExactTransfer);

    // Over-refusal guard 1: several native images of ONE bytecode is
    // exactly what a loop transform produces, and they agree on everything
    // a by-bci lookup reads, so the pick cannot be wrong.
    let mut copies = osr_t_artifact(3);
    let mut second = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    second.native_offset = 0x90;
    let mut third = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    third.native_offset = 0xE0;
    copies.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        second,
        third,
    ];
    let plan = copies
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("agreeing copies must still admit the entry");
    assert_eq!(plan.exit_policy, OsrExitPolicy::ExactTransfer);

    // Over-refusal guard 2, the measured one: the loop-boundary exit map
    // and the speculative-BCE range guard on the same header bci. Different
    // reasons, same `REEXECUTE` semantics — the ordinary shape of a
    // compiled counted loop, and the resume bci is not in doubt.
    let mut counted = osr_t_artifact(3);
    let mut guard = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    guard.native_offset = 0x3c1;
    guard.reason = deopt::DeoptReason::BoundsCheck;
    assert_eq!(
        guard.semantics,
        deopt::ResumeSemantics::REEXECUTE,
        "a reason disagreement never implies a semantics one"
    );
    counted.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        guard,
    ];
    assert!(
        counted
            .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "refusing this cost CratonBench.matrixKernel its OSR, permanently"
    );
}

/// `osr_exit_points` is cross-checked against where exits are actually
/// taken — the lane's step 4, which had no consumer at all before.
///
/// The first two are both legitimate; the last two are disagreements and
/// must read zero. The `invokedynamic` trap is the case that makes set
/// membership alone insufficient: it shares the exit-map machinery, so it
/// lands in `osr_exit_points` exactly like a loop boundary and is told
/// apart only by its recorded reason.
#[test]
pub(in crate::tests) fn the_exit_site_classification_names_every_outcome() {
    use osr_exit::OsrExitSite;

    let mut cm = osr_t_artifact(3);
    let mut trap = osr_t_exit_point(30, osr_t_contract_locals(), Vec::new());
    trap.native_offset = 0x90;
    trap.reason = deopt::DeoptReason::UnreachedCode;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        trap,
    ];
    // Both bcis are in the set — that is what the shared emitter does.
    cm.osr_exit_points = vec![OSR_T_HEADER, 30];

    assert_eq!(
        cm.classify_osr_exit_site(OSR_T_HEADER as u32),
        OsrExitSite::LoopBoundary
    );
    assert_eq!(
        cm.classify_osr_exit_site(30),
        OsrExitSite::OffLoopBoundary(deopt::DeoptReason::UnreachedCode),
        "membership alone would have called the indy trap a loop boundary"
    );
    assert_eq!(cm.classify_osr_exit_site(99), OsrExitSite::Unrecorded);

    // The cross-check: drop the header from the set while its `OsrExit`
    // point stays. One function writes both, so this cannot happen — and
    // nothing could have said so before.
    cm.osr_exit_points = vec![30];
    assert_eq!(
        cm.classify_osr_exit_site(OSR_T_HEADER as u32),
        OsrExitSite::ExitMapMissing
    );
}

/// `ResumeSemantics` decides whether a snapshot bci is a place the
/// interpreter may be *parked*, and only `REEXECUTE` is.
///
/// A `RESUME` point means the bytecode at `bci` already took effect, so
/// parking there re-executes it — the same double-execution defect as
/// replaying a loop iteration, one bytecode at a time. Its successor bci
/// needs the method's bytecode, which this crate does not have, so such a
/// point disqualifies the OSR entry up front and is refused again on the
/// way out.
#[test]
pub(in crate::tests) fn only_reexecute_semantics_yield_an_exact_resume_point() {
    // Sanity: the convention this rests on.
    assert_eq!(
        deopt::ResumeSemantics::for_reason(deopt::DeoptReason::OsrExit),
        deopt::ResumeSemantics::REEXECUTE
    );

    let locals = [0x1234_5678i64, 200, 4950];

    // A `RESUME`-semantics point anywhere in the artifact refuses the ENTRY
    // — before any iteration is committed.
    let mut cm = osr_t_artifact(3);
    let mut resume_point =
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    resume_point.semantics = deopt::ResumeSemantics::RESUME;
    cm.deopt_points = vec![resume_point];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a RESUME-semantics exit must refuse the entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
    assert!(err.to_string().contains("RESUME"), "{err}");

    // A `RETHROW` point may EXIST — it is stashed separately and never
    // routed to a resume — so it must not disqualify the entry…
    let mut cm = osr_t_artifact(3);
    let mut rethrow = osr_t_exit_point(5, osr_t_contract_locals(), Vec::new());
    rethrow.reason = deopt::DeoptReason::PendingException;
    rethrow.semantics = deopt::ResumeSemantics::RETHROW;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        rethrow,
    ];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a RETHROW point elsewhere in the body must not refuse the entry");

    // …but a frame that arrives naming it is refused: its bci is a throwing
    // instruction, not a resume point.
    let rframe = deopt::ReconstructedFrame {
        class_id: 0,
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: 5,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    };
    let err = plan.resume_after_exit(&cm, &rframe).unwrap_err();
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_EXIT_REPLAY));
    assert!(err.to_string().contains("REEXECUTE"), "{err}");
    // …and it says WHY, rather than "not a recorded deopt point": since
    // `resume_image` stopped counting rethrow points as resume images, a
    // bci whose only point is one answers `None` there, and the generic
    // "not recorded" wording would send the reader looking for a
    // mis-routed stash that does not exist.
    assert!(err.to_string().contains("RETHROW"), "{err}");
}

/// A crash handler must be able to tell "no compiled body covers this
/// address" from "I could not read the registry". Collapsing the two into
/// one `None` is what made the 2026-07-28 wild-jump report unreadable.
#[test]
pub(in crate::tests) fn jit_name_lookup_separates_absence_from_an_unreadable_registry() {
    // Addresses far outside anything this process maps, so the assertion
    // cannot be perturbed by a concurrently-running test's registrations.
    let base = 0x5EED_0000_0000usize;
    register_jit_method_name(base, 0x100, "Probe.absent()V".to_string());
    // …but it CAN be perturbed by a concurrent test holding the registry
    // lock, because this accessor is a `try_lock` and `Locked` is a third
    // legitimate answer — which is the whole point of the type this test is
    // about. Retry past it rather than reading it as a wrong answer. The
    // address argument above is about registrations; this is about the
    // lock, and only the sibling test below had noticed the difference.
    assert!(
        eventually(|| matches!(
            lookup_jit_method_name_detailed(base + 0x10),
            JitNameLookup::Found(ref n) if n == "Probe.absent()V"
        )),
        "a registered range must resolve to its name"
    );
    assert!(
        eventually(|| matches!(
            lookup_jit_method_name_detailed(base + 0x100),
            JitNameLookup::NotFound
        )),
        "one past the end of the only registered range is an absence"
    );

    let _held = jit_name_ranges().lock().expect("registry lock");
    assert!(matches!(
        lookup_jit_method_name_detailed(base + 0x10),
        JitNameLookup::Locked
    ));
}

/// Every executable buffer registers a live region, including the ones no
/// `JitCache::put` ever names. A crash handler uses that to separate "this
/// address is a buffer we still hold" from "this address is nothing".
#[test]
pub(in crate::tests) fn live_code_region_covers_a_buffer_the_name_registry_never_saw() {
    let buf = ExecutableBuffer::new(4096).expect("alloc failed");
    let base = buf.as_ptr() as usize;
    // `Locked` is a legitimate answer from this `try_lock` accessor, not a
    // miss — retry past any concurrent registration rather than reading it
    // as "the region is absent".
    assert!(
        eventually(|| matches!(
            jit_code_region_covering(base),
            JitRegionLookup::Found(b, cap) if b == base && cap == buf.mapped_len()
        )),
        "the live buffer must be covered at its base"
    );
    assert!(
        eventually(|| matches!(
            jit_code_region_covering(base + buf.mapped_len() - 1),
            JitRegionLookup::Found(..)
        )),
        "the live buffer must be covered at its last byte (the not-entrant tail included)"
    );
    // One past the end is outside THIS buffer. Phrased as "not this base"
    // rather than "NotFound" because a concurrently-created buffer may
    // legitimately be mapped immediately after it.
    assert!(!matches!(
        jit_code_region_covering(base + buf.mapped_len()),
        JitRegionLookup::Found(b, _) if b == base
    ));
    drop(buf);
    assert!(
        eventually(|| !matches!(
            jit_code_region_covering(base),
            JitRegionLookup::Found(b, _) if b == base
        )),
        "dropping the buffer must unregister its region"
    );
}

#[test]
pub(in crate::tests) fn test_executable_buffer_finalize_and_make_writable() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    buf.finalize();
    // make_writable and finalize again should not crash
    buf.make_writable();
    buf.finalize();
}

// ── CompiledMethod tests ────────────────────────────────────────

#[test]
pub(in crate::tests) fn test_compiled_method_new_pure() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new(buf);
    assert!(!cm.needs_context());
    assert!(!cm.needs_heap());
    assert!(!cm.entry_ptr().is_null());
}

#[test]
pub(in crate::tests) fn test_compiled_method_new_with_context() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new_with_context(buf);
    assert!(cm.needs_context());
    assert!(cm.needs_heap());
}

// ── JitMICSlot tests ────────────────────────────────────────────

#[test]
pub(in crate::tests) fn test_jit_mic_slot_default() {
    let slot = JitMICSlot::new();
    // A fresh slot holds the empty sentinel, not 0: class id 0 is a real
    // receiver (`java/lang/Object`). Round 9, mic-empty-slot-class-zero-window.
    assert_eq!(
        slot.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        JitMICSlot::EMPTY_CLASS_ID
    );
    assert!(slot.cached_class_name.lock().is_none());
}

#[test]
pub(in crate::tests) fn test_jit_mic_slot_prepopulate() {
    let slot = JitMICSlot::new();
    slot.prepopulate(42);
    assert_eq!(
        slot.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        42
    );
}

// ── JitPICSlot tests ────────────────────────────────────────────

#[test]
pub(in crate::tests) fn test_jit_pic_slot_empty_misses() {
    let pic = JitPICSlot::new();
    assert!(pic.lookup(42).is_none());
    assert_eq!(pic.entries_used(), 0);
    assert_eq!(pic.misses.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
pub(in crate::tests) fn test_jit_pic_slot_install_and_hit() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, true, false);
    pic.install(3, "C", 0x3000, false, false);
    assert_eq!(pic.entries_used(), 3);
    assert_eq!(pic.lookup(1), Some((0x1000, false)));
    assert_eq!(pic.lookup(2), Some((0x2000, true)));
    assert_eq!(pic.lookup(3), Some((0x3000, false)));
    assert!(pic.lookup(99).is_none());
}

#[test]
pub(in crate::tests) fn test_jit_pic_slot_clear_entries_drops_compiled_targets() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, true, false);
    pic.install(3, "C", 0x3000, false, false);
    assert_eq!(pic.entries_used(), 3);

    pic.clear_entries();

    assert_eq!(pic.entries_used(), 0);
    assert!(pic.lookup(1).is_none());
    assert!(pic.lookup(2).is_none());
    assert!(pic.lookup(3).is_none());
    for i in 0..JIT_PIC_ENTRIES {
        let (class_id, entry, needs_context) = pic.way(i).expect("way exists");
        assert_eq!(entry, 0);
        assert!(!needs_context);
        assert!(
            class_id == 0 || class_id == JitPICSlot::RETIRED_WAY_CLASS_ID,
            "a cleared way must not keep a guard a receiver can match (way {i}: {class_id})"
        );
        assert!(pic.class_names[i].lock().is_none());
    }
}

/// A fifth receiver must NOT displace a live way. Generated code may be
/// between a way's class compare and its entry-word load, so overwriting
/// the way would pair the old receiver's guard with the new receiver's
/// body. The overflow receiver is served by the hashed table instead.
#[test]
pub(in crate::tests) fn test_jit_pic_slot_full_ways_are_never_evicted() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, false, false);
    pic.install(3, "C", 0x3000, false, false);
    pic.install(4, "D", 0x4000, false, false);
    for _ in 0..10 {
        pic.lookup(1);
    }
    pic.install(5, "E", 0x5000, true, false);
    assert_eq!(pic.lookup(1), Some((0x1000, false)));
    assert_eq!(pic.lookup(2), Some((0x2000, false)));
    assert_eq!(pic.lookup(3), Some((0x3000, false)));
    assert_eq!(pic.lookup(4), Some((0x4000, false)));
    assert!(
        pic.lookup(5).is_none(),
        "no live way may be evicted for class 5"
    );
    assert_eq!(pic.lookup_megamorphic(5), Some((0x5000, true)));
}

#[test]
pub(in crate::tests) fn test_jit_pic_slot_seed_from_mic() {
    let mic = JitMICSlot::new();
    mic.update(7, "Seven", 0x7000, true, false);
    mic.record_hit();
    mic.record_hit();
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
    let name = pic.class_names[0].lock();
    assert_eq!(name.as_deref(), Some("Seven"));
}

#[test]
pub(in crate::tests) fn test_jit_pic_slot_seed_empty_mic_noop() {
    let mic = JitMICSlot::new();
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 0);
}

/// A recompiled target for a cached receiver is never written over the
/// old way. The old way is retired (guard first, then word) and the new
/// target goes into a different way, in both the inline ways and the
/// hashed table.
#[test]
pub(in crate::tests) fn test_jit_pic_slot_recompiled_target_retires_the_stale_way() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(1, "A2", 0x2000, true, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(1), Some((0x2000, true)));
    assert_eq!(
        pic.way(0),
        Some((JitPICSlot::RETIRED_WAY_CLASS_ID, 0, false)),
        "the stale way must be retired, not retargeted"
    );
    assert_eq!(pic.lookup_megamorphic(1), Some((0x2000, true)));
}

#[test]
pub(in crate::tests) fn test_jit_mega_offsets_match_generated_stub_contract() {
    let pic = JitPICSlot::new();
    let base = &pic as *const JitPICSlot as usize;
    assert_eq!(
        &pic.mega_class_ids as *const _ as usize - base,
        JitPICSlot::MEGA_CLASS_IDS_OFFSET
    );
    assert_eq!(
        &pic.mega_entry_words as *const _ as usize - base,
        JitPICSlot::MEGA_ENTRY_PTRS_OFFSET
    );
}

#[test]
pub(in crate::tests) fn test_jit_pic_secondary_cache_serves_overflow_and_clears() {
    let pic = JitPICSlot::new();
    // Entries are even: bit 0 of a word is the context tag.
    for class_id in 1..=5 {
        pic.install(
            class_id,
            &format!("C{class_id}"),
            0x1000 + (u64::from(class_id) << 4),
            class_id % 2 == 0,
            false,
        );
    }
    assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);
    for class_id in 1..=5 {
        assert_eq!(
            pic.lookup_megamorphic(class_id),
            Some((0x1000 + (u64::from(class_id) << 4), class_id % 2 == 0))
        );
    }
    pic.clear_entries();
    for class_id in 1..=5 {
        assert!(pic.lookup_megamorphic(class_id).is_none());
    }
}

#[test]
pub(in crate::tests) fn test_jit_pic_slot_thresholds_are_sensible() {
    assert!(MIC_TO_PIC_THRESHOLD >= 2);
    assert!(JIT_PIC_ENTRIES >= 2);
}

// ── T17.Β.1 — PIC 4-way probe semantics ────────────────────────

/// Fill every `(class_id, entry_ptr)` entry and issue a
/// lookup for each. The pure-Rust [`JitPICSlot::lookup`] mirrors
/// what the x64 emission does with `CMP RAX, [RBX+off]; JE
/// entry_i` for each slot. All entries must hit and the
/// miss counter must stay at 0.
#[test]
pub(in crate::tests) fn t17_b_pic_all_entries_hit() {
    let pic = JitPICSlot::new();
    pic.install(11, "A", 0xAAAA_0000, false, false);
    pic.install(22, "B", 0xBBBB_0000, true, false);
    pic.install(33, "C", 0xCCCC_0000, false, false);
    pic.install(44, "D", 0xDDDD_0000, true, false);
    assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);

    // Each receiver class_id is one of the 3 probed slots.
    assert_eq!(pic.lookup(11), Some((0xAAAA_0000, false)));
    assert_eq!(pic.lookup(22), Some((0xBBBB_0000, true)));
    assert_eq!(pic.lookup(33), Some((0xCCCC_0000, false)));
    assert_eq!(pic.lookup(44), Some((0xDDDD_0000, true)));

    // Miss counter stays at 0 — no probe fell through.
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "miss counter should not advance on 3 PIC hits"
    );

    // Per-slot hit counters all incremented exactly once.
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
            1,
            "slot {i} should have exactly 1 hit"
        );
    }
}

/// A receiver whose class_id is not in any slot must
/// fall through to the generic-helper path. In the pure-Rust
/// mirror, that surfaces as `lookup` returning `None` and the
/// miss counter incrementing.
#[test]
pub(in crate::tests) fn t17_b_pic_miss_falls_through() {
    let pic = JitPICSlot::new();
    pic.install(11, "A", 0xAAAA_0000, false, false);
    pic.install(22, "B", 0xBBBB_0000, false, false);
    pic.install(33, "C", 0xCCCC_0000, false, false);
    pic.install(44, "D", 0xDDDD_0000, false, false);

    // 5th receiver type — no slot matches.
    let res = pic.lookup(55);
    assert!(
        res.is_none(),
        "miss on an unknown class_id must return None"
    );
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "miss counter must increment on a fall-through"
    );

    // Successive misses also increment the counter.
    pic.lookup(66);
    pic.lookup(77);
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "miss counter must track every fall-through"
    );

    // No hit counters were touched on misses.
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
            0,
            "slot {i} must not record a hit on a miss"
        );
    }
}

/// A PIC seeded from its MIC keeps the seeded receiver in way 0, and a
/// later receiver lands in a fresh way rather than over it.
#[test]
pub(in crate::tests) fn t17_b_seeded_way_is_not_overwritten_by_a_new_receiver() {
    let mic = JitMICSlot::new();
    mic.update(7, "Seven", 0x7000, true, false);
    let pic = JitPICSlot::new_at(mic.bci);
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));

    pic.install(8, "Eight", 0x8000, false, false);
    assert_eq!(pic.entries_used(), 2);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
    assert_eq!(pic.lookup(8), Some((0x8000, false)));
}

// ── DescriptorParamIter tests ───────────────────────────────────

#[test]
pub(in crate::tests) fn test_descriptor_param_iter_simple() {
    let params: Vec<u8> = DescriptorParamIter::new("(II)I").collect();
    assert_eq!(params, vec![b'I', b'I']);
}

#[test]
pub(in crate::tests) fn test_descriptor_param_iter_mixed_types() {
    let params: Vec<u8> = DescriptorParamIter::new("(IJLjava/lang/String;[I)V").collect();
    assert_eq!(params, vec![b'I', b'J', b'L', b'[']);
}

#[test]
pub(in crate::tests) fn test_descriptor_param_iter_no_params() {
    let params: Vec<u8> = DescriptorParamIter::new("()V").collect();
    assert!(params.is_empty());
}

#[test]
pub(in crate::tests) fn test_descriptor_param_iter_array_of_objects() {
    let params: Vec<u8> = DescriptorParamIter::new("([Ljava/lang/Object;)V").collect();
    assert_eq!(params, vec![b'[']);
}

// ── JitCache tests ──────────────────────────────────────────────

#[test]
pub(in crate::tests) fn test_scalar_selfrec_ir_structural_admission() {
    let fib = [
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // fib(n - 1)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // fib(n - 2)
        0x60, 0xac, // iadd; ireturn
    ];
    assert!(scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)I"));
    assert!(!scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)J"));
    assert!(!scalar_selfrec_ir_would_engage(&[0x1a, 0xac], 2, "(I)I"));
}

#[test]
pub(in crate::tests) fn test_jit_cache_new_is_empty() {
    let cache = JitCache::new();
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
}

#[test]
pub(in crate::tests) fn test_jit_cache_put_and_get() {
    let mut cache = JitCache::new();
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new(buf);

    let class: Arc<str> = Arc::from("TestClass");
    let method: Arc<str> = Arc::from("testMethod");
    let desc: Arc<str> = Arc::from("(II)I");

    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cratonvm_types::ClassId::new(1),
        cm,
    );
    assert_eq!(cache.len(), 1);
    assert!(!cache.is_empty());

    let result = cache.get(&class, &method, &desc, cratonvm_types::ClassId::new(1));
    assert!(result.is_some());
}

/// **Every publication advances `jit_cache_generation()`** — first-time
/// insertion, replacement, and OSR body alike.
///
/// This is the load-bearing step of the recycled-`JitSiteKey` argument, and
/// until now it rested on a comment plus four hand-written `fetch_add`
/// calls. See
/// `site-alias-diagnostic-and-the-recycled-jitsitekey-question-CLOSED-20260806.md`.
///
/// The chain it closes: a `JitInvokeInfo` box is owned by its
/// `CompiledMethod` (`_jit_invoke_infos`), so its ADDRESS — which is what a
/// `JitSiteKey` is — is freed and re-issued. Every per-thread memo keyed on
/// one is cleared by `flush_raw_entry_dispatch_caches`, which fires on a
/// generation change and runs before any memo probe. That only closes the
/// hole because a recycled address cannot become *dispatchable* without a
/// publication: compiled code referencing the new `JitInvokeInfo` has to be
/// published before it can run. So if a publication could land without
/// advancing the generation, the flush would not fire and one call site
/// would serve another's dispatch — the defect `383e7f5cf` fixed, in a new
/// disguise.
///
/// **Non-vacuous:** each step asserts the body is actually REACHABLE
/// afterwards, not just that a counter moved. A `put` that silently
/// declined (stale publication epoch, `prepare_for_publication` refusal)
/// publishes nothing and correctly does not bump — so a test that only
/// watched the counter could pass while proving nothing about publication.
///
/// **Strictly greater, not `+ 1`:** `JIT_CACHE_GENERATION` is a process
/// global and the test binary runs tests in parallel, so a concurrent
/// publication in another test may also bump it. `>` is the assertion that
/// is both true and stable; `== before + 1` would be a flake.
///
/// **Verified as a negative control.** Deleting the `fetch_add` at the
/// first-time publication site turns this red on the first arm, with the
/// message it was written to produce — so it is pinning the bump, not
/// riding on a counter something else moves.
#[test]
pub(in crate::tests) fn every_publication_advances_the_jit_cache_generation() {
    fn ret_body(osr: bool) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let mut cm = CompiledMethod::new(buf);
        cm.compiled_via_osr = osr;
        cm
    }

    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("GenerationClass");
    let method: Arc<str> = Arc::from("hot");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);

    // 1. First-time insertion. The comment at the bump site calls this out
    //    specifically ("bump on EVERY publication, not just replacements"),
    //    because it is the one a replacement-only bump would miss.
    let before = jit_cache_generation();
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false),
    );
    let first = cache
        .get(&class, &method, &desc, cid)
        .expect("first publication must be reachable");
    assert!(
        jit_cache_generation() > before,
        "a first-time publication did not advance the JIT cache generation; \
         a recycled JitInvokeInfo address could then inherit the previous \
         site's memoized dispatch"
    );

    // 2. Replacement. The superseded artifact's `JitInvokeInfo` boxes are
    //    what get freed and re-issued, so this is the case the whole
    //    argument is about.
    let before = jit_cache_generation();
    let first_entry = first.entry_ptr();
    drop(first);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false),
    );
    let replaced = cache
        .get(&class, &method, &desc, cid)
        .expect("replacement must be reachable");
    assert_ne!(
        replaced.entry_ptr(),
        first_entry,
        "the second put must have superseded the first, or this arm proves nothing"
    );
    assert!(
        jit_cache_generation() > before,
        "a replacement publication did not advance the JIT cache generation"
    );

    // 3. OSR body. A separate entry point with its own bump; it publishes
    //    alongside the method-entry body rather than superseding it.
    let before = jit_cache_generation();
    cache.put_osr(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(true),
    );
    assert!(
        cache.get_osr(&class, &method, &desc, cid).is_some(),
        "OSR publication must be reachable"
    );
    assert!(
        jit_cache_generation() > before,
        "an OSR publication did not advance the JIT cache generation"
    );
}
/// Freeing a body that owns `JitInvokeInfo`s advances the RETIRE half of
/// `jit_cache_generation()` before its boxes are freed (rt proposal W8-1).
/// `>` rather than `+ 1` for the reason `every_publication_advances_the_jit_cache_generation`
/// gives: other tests move the same word concurrently, and only upward.
#[test]
pub(in crate::tests) fn freeing_a_body_that_owns_invoke_infos_advances_the_retire_half() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm._jit_invoke_infos.push(Box::new(JitInvokeInfo {
        class_name: "RetireHalfClass",
        method_name: "callee",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let before = jit_cache_generation() >> JIT_CACHE_RETIRE_SHIFT;
    drop(cm);
    assert!(
        jit_cache_generation() >> JIT_CACHE_RETIRE_SHIFT > before,
        "a freed JitInvokeInfo address can be re-issued without a retire bump"
    );
}

/// A withdrawal moves the retire half. A replacement does not have to (round 11
/// wave 11): it marks the body it displaced `superseded`, which the two
/// body-holding dispatch memos check at every hit, and leaves it NOT retired,
/// because six readers take `retired` to mean "withdrawn by an invalidation".
#[test]
pub(in crate::tests) fn replacement_marks_superseded_and_withdrawal_advances_the_retire_half() {
    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }
    let retire = || jit_cache_generation() >> JIT_CACHE_RETIRE_SHIFT;
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RetireHalfPut");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    let first = cache
        .get(&class, &method, &desc, cid)
        .expect("first put published");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    assert!(
        first.superseded.load(std::sync::atomic::Ordering::Acquire),
        "a replacement did not mark the body it displaced"
    );
    assert!(
        !first.retired.load(std::sync::atomic::Ordering::Acquire),
        "a replacement must not look like an invalidation"
    );
    drop(first);
    let before = retire();
    cache.remove(&class, &method, &desc, cid);
    assert!(
        cache.get(&class, &method, &desc, cid).is_none(),
        "the body was withdrawn"
    );
    assert!(
        retire() > before,
        "a withdrawal did not move the retire half"
    );
}

#[test]
pub(in crate::tests) fn r12w2_a_single_pass_put_keeps_a_live_optimizing_body_only_when_asked() {
    fn ret_body(ir: bool, keep: bool) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let mut cm = CompiledMethod::new(buf);
        cm.used_ir_backend = ir;
        cm.keep_live_optimizing_body = keep;
        cm
    }
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("R12KeepOptimizing");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(true, false),
    );
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false, true),
    );
    let live = cache.get(&class, &method, &desc, cid).expect("published");
    assert!(
        live.used_ir_backend,
        "a single-pass body asked to keep replaced the optimizing one"
    );
    assert!(!live.superseded.load(std::sync::atomic::Ordering::Acquire));
    drop(live);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false, false),
    );
    let live = cache.get(&class, &method, &desc, cid).expect("published");
    assert!(!live.used_ir_backend, "an ordinary put publishes as before");
    drop(live);
    // Over a single-pass body the request changes nothing.
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false, true),
    );
    assert!(cache.get(&class, &method, &desc, cid).is_some());
}

#[test]
pub(in crate::tests) fn test_jit_cache_osr_and_method_entry_bodies_coexist() {
    let mut cache = JitCache::new();
    let class: Arc<str> = Arc::from("LoopClass");
    let method: Arc<str> = Arc::from("hotLoop");
    let desc: Arc<str> = Arc::from("(I)I");

    let cid = cratonvm_types::ClassId::new(1);
    let mut entry_buf = ExecutableBuffer::new(64).expect("alloc failed");
    entry_buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(entry_buf),
    );

    let mut osr_buf = ExecutableBuffer::new(64).expect("alloc failed");
    osr_buf.emit(&[0xC3]);
    let mut osr = CompiledMethod::new(osr_buf);
    osr.compiled_via_osr = true;
    cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

    let entry = cache.get(&class, &method, &desc, cid).expect("entry body");
    let osr = cache
        .get_osr(&class, &method, &desc, cid)
        .expect("osr body");
    assert_ne!(entry.entry_ptr(), osr.entry_ptr());
    assert!(!entry.compiled_via_osr);
    assert!(osr.compiled_via_osr);
    assert_eq!(cache.len(), 2);

    let osr_entry = osr.entry_ptr();
    let mut c2_buf = ExecutableBuffer::new(64).expect("alloc failed");
    c2_buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(c2_buf),
    );
    assert_eq!(
        cache
            .get_osr(&class, &method, &desc, cid)
            .expect("osr survives method-entry supersede")
            .entry_ptr(),
        osr_entry
    );
    assert_eq!(cache.len(), 2);
}

#[test]
pub(in crate::tests) fn test_jit_cache_put_replacement_reclaims_after_last_reader() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("ReplaceClass");
    let method: Arc<str> = Arc::from("replaceMethod");
    let desc: Arc<str> = Arc::from("()V");

    let cid = cratonvm_types::ClassId::new(1);
    let mut old_buf = ExecutableBuffer::new(64).expect("alloc failed");
    old_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(old_buf),
    );
    let old = cache
        .get(&class, &method, &desc, cid)
        .expect("old compiled method");
    let old_entry = old.entry_ptr() as usize;
    register_jit_code_range(old_entry, old.code_len(), Arc::as_ptr(&old) as usize);
    assert!(lookup_jit_code_range(old_entry).is_some());

    let mut new_buf = ExecutableBuffer::new(64).expect("alloc failed");
    new_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(new_buf),
    );

    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "an outstanding lock-free reader must own the replaced artifact"
    );
    drop(old);
    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "the replaced artifact must unregister after its last Arc is released"
    );
    if let Some(new_cm) = cache.get(&class, &method, &desc, cid) {
        unregister_jit_code_range(new_cm.entry_ptr() as usize);
    }
}

/// A thread executing compiled code holds NO `Arc` — only a return address
/// on its stack. So a tier-up `put` that displaces the body it is running
/// would drop the last strong reference and `munmap` the mapping out from
/// under it (`SIGSEGV at pc=X, addr=X`, the PC landing in a hole between
/// core-dump segments, ~10% of runs under load). `put` therefore hands the
/// superseded artifact to [`defer_jit_owner`], which holds it until
/// `ACTIVE_JIT_EXECUTIONS` reaches zero.
#[test]
pub(in crate::tests) fn replaced_body_survives_until_jit_execution_is_quiescent() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RetireWhileRunningClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);

    let mut old_buf = ExecutableBuffer::new(64).expect("alloc failed");
    old_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(old_buf),
    );
    let old = cache
        .get(&class, &method, &desc, cid)
        .expect("old compiled method");
    let old_entry = old.entry_ptr() as usize;
    register_jit_code_range(old_entry, old.code_len(), Arc::as_ptr(&old) as usize);
    // Release the reader: the shard snapshot is now the ONLY strong owner,
    // exactly as it is for a body that is merely being executed.
    drop(old);
    assert!(lookup_jit_code_range(old_entry).is_some());

    let execution = jit_execution_enter();
    let mut new_buf = ExecutableBuffer::new(64).expect("alloc failed");
    new_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(new_buf),
    );
    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "a tier-up must not release the body a thread is executing"
    );
    jit_execution_leave(execution);

    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "the retired body must be released once JIT execution is quiescent"
    );
    if let Some(new_cm) = cache.get(&class, &method, &desc, cid) {
        unregister_jit_code_range(new_cm.entry_ptr() as usize);
    }
}

/// Round 11 (lane `rt`): `clear_all` retires every body of a shard in ONE
/// retirement-queue trip (`retire_withdrawn_bodies`) instead of one trip per
/// body. The batch must keep the single form's contract for every member: no
/// body is released while a thread is inside compiled code, and every body is
/// released once execution is quiescent.
#[test]
pub(in crate::tests) fn clear_all_batch_retirement_keeps_every_body_until_quiescent() {
    let cache = JitCache::new();
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let mut entries = Vec::new();
    for name in ["r11BatchA", "r11BatchB", "r11BatchC"] {
        let class: Arc<str> = Arc::from("R11BatchRetireClass");
        let method: Arc<str> = Arc::from(name);
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache
            .get(&class, &method, &desc, cid)
            .expect("published compiled method");
        let entry = cm.entry_ptr() as usize;
        register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
        drop(cm);
        entries.push(entry);
    }
    for &entry in &entries {
        assert!(lookup_jit_code_range(entry).is_some());
    }

    let execution = jit_execution_enter();
    assert_eq!(cache.clear_all(), 3);
    for &entry in &entries {
        assert!(
            lookup_jit_code_range(entry).is_some(),
            "a flushed body must survive while a thread is in compiled code"
        );
    }
    jit_execution_leave(execution);

    for &entry in &entries {
        assert!(
            eventually_drained(|| lookup_jit_code_range(entry).is_none()),
            "every member of a batch retirement must be released once quiescent"
        );
    }
}

/// A whole-cache flush taken while a thread is inside compiled code (the
/// ordinary state of a `retransformClasses` call, which a compiled caller
/// makes) runs ONE retirement-queue drain, not one per body. Every body the
/// flush withdraws reaches the queue by its own door -- the shard map's
/// counted `RetainedCode` handles, each body's inline caches, the per-shard
/// batch -- and with nothing releasable each drain partitioned the whole
/// growing queue: a flush of the cache Hibernate's
/// `DefaultCatalogAndSchemaTest` leaves behind stopped the VM for ~120 s inside
/// Mockito's first `retransformClasses` (`hibernate-orm-single-jvm-post-heavy-class-slowdown-FIXED-20261027`).
/// The queue's contract is kept: nothing is released until quiescence.
#[test]
pub(in crate::tests) fn clear_all_while_in_compiled_code_drains_the_retire_queue_once() {
    const BODIES: usize = 1500;
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("HibSlowFlushClass");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let mut entries = Vec::with_capacity(BODIES);
    for i in 0..BODIES {
        let method: Arc<str> = Arc::from(format!("m{i}"));
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache
            .get(&class, &method, &desc, cid)
            .expect("published compiled method");
        let entry = cm.entry_ptr() as usize;
        register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
        entries.push(entry);
    }

    let execution = jit_execution_enter();
    let before = jit_code_reclamation_stats().drains;
    assert_eq!(cache.clear_all(), BODIES);
    let drains = jit_code_reclamation_stats().drains - before;
    for &entry in &entries {
        assert!(
            lookup_jit_code_range(entry).is_some(),
            "a flushed body must survive while a thread is in compiled code"
        );
    }
    jit_execution_leave(execution);
    // One drain is the flush's own; the bound leaves room for other tests of
    // this process draining in the same window, and is far below one per body.
    assert!(
        drains < (BODIES / 10) as u64,
        "a whole-cache flush of {BODIES} bodies ran {drains} retire-queue drains"
    );
    for &entry in &entries {
        assert!(
            eventually_drained(|| lookup_jit_code_range(entry).is_none()),
            "every flushed body must be released once quiescent"
        );
    }
}

/// Round 11 (lane `rt`): unregistering a range that has been merged into the
/// registry's `base` tombstones it instead of rebuilding `base`. A tombstoned
/// range must be invisible to every reader, and a NEW range registered at the
/// reused address must be visible despite the tombstone at the same start.
#[test]
pub(in crate::tests) fn code_range_tombstones_hide_the_dead_range_and_not_its_successor() {
    // Synthetic, far from any real mapping; the registry is process-global.
    const BASE: usize = 0x7ff0_1100_0000;
    const STEP: usize = 0x1000;
    const LEN: usize = 0x100;
    // More than `recent_code_range_capacity`'s floor of 32, so the first
    // ranges are merged into `base` by the later registrations.
    let starts: Vec<usize> = (0..40).map(|k| BASE + k * STEP).collect();
    for (k, &s) in starts.iter().enumerate() {
        register_jit_code_range(s, LEN, 0x10_0000 + k * 8);
    }
    let victim = starts[0];
    assert_eq!(lookup_jit_code_range(victim + 1), Some(0x10_0000));

    unregister_jit_code_range(victim);
    assert_eq!(lookup_jit_code_range(victim), None);
    assert_eq!(lookup_jit_code_range(victim + LEN - 1), None);
    let mut snap = Vec::new();
    snapshot_code_ranges_into(&mut snap);
    assert!(!snap.iter().any(|&(s, _)| s == victim));
    // Non-decreasing, not strictly increasing: the registry is process-global,
    // and it admits the same body registered twice (a harness does, beside
    // `put`), so a test running in parallel can put two equal starts in this
    // snapshot. Every reader searches it with `partition_point`, which only
    // needs the order.
    assert!(
        snap.windows(2).all(|w| w[0].0 <= w[1].0),
        "the snapshot must stay sorted by start"
    );
    // A neighbour is untouched.
    assert_eq!(lookup_jit_code_range(starts[1]), Some(0x10_0000 + 8));

    // The allocator hands the address out again, with a different length.
    register_jit_code_range(victim, LEN / 2, 0xABC0);
    assert_eq!(lookup_jit_code_range(victim + 1), Some(0xABC0));
    assert_eq!(
        lookup_jit_code_range(victim + LEN / 2),
        None,
        "past the successor's end is inside only the DEAD range"
    );
    snapshot_code_ranges_into(&mut snap);
    assert_eq!(
        snap.iter().filter(|&&(s, _)| s == victim).count(),
        1,
        "exactly one live range starts at the reused address"
    );

    unregister_jit_code_range(victim);
    assert_eq!(lookup_jit_code_range(victim + 1), None);
    for &s in &starts[1..] {
        unregister_jit_code_range(s);
        assert_eq!(lookup_jit_code_range(s), None);
    }
}

/// Drive a published body down to a single owner and hand that owner to
/// the caller, together with its entry address.
///
/// `ACTIVE_JIT_EXECUTIONS` is process-global and this suite runs in
/// parallel, so "the cache released its reference" is not observable on the
/// first try — a sibling test's execution epoch can hold the drain off.
/// Pump the quiescence check until the count settles.
pub(in crate::tests) fn sole_owner_of_a_published_body(
    cache: &JitCache,
    class: &Arc<str>,
    method: &Arc<str>,
    desc: &Arc<str>,
    cid: cratonvm_types::ClassId,
) -> (Arc<CompiledMethod>, usize) {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let cm = cache.get(class, method, desc, cid).expect("published body");
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
    cache.remove(class, method, desc, cid);
    assert!(
        eventually_drained(|| Arc::strong_count(&cm) == 1),
        "the retirement queue should have released the cache's reference \
         (strong count is {})",
        Arc::strong_count(&cm),
    );
    (cm, entry)
}

/// The last owner of a published body is regularly a **per-thread dispatch
/// cache entry**, and such a cache is evicted by the very thread that
/// dispatches through it — a generation flush, a staleness eviction, a
/// replacement. So the eviction can run while a frame of that body is on
/// that thread's own stack, and a bare `Arc` drop there unmaps executable
/// memory with no quiescence proof behind it.
///
/// [`RetainedCode`] is what makes the release structural instead of a rule
/// every container has to remember. Measured on
/// `BasicErrorControllerIntegrationTests` before it existed: 50 published
/// bodies per run released outside the queue, seven of them with
/// `active_jit_executions` at 1, 2 or 3 — the crash signature of
/// `jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`.
#[test]
pub(in crate::tests) fn retained_code_releases_a_published_body_through_the_queue() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RetainedCodeQueueClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);

    let held = RetainedCode::new(cm);
    let execution = jit_execution_enter();
    drop(held);
    assert!(
        lookup_jit_code_range(entry).is_some(),
        "the last owner must not unmap a published body while a thread is inside compiled code"
    );
    jit_execution_leave(execution);

    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "and must release it once JIT execution is quiescent"
    );
}

/// The counter behind [`published_code_free_audit`] is the *enforcement* of
/// the rule above, and it has to be able to fail: a plain `Arc` drop that
/// happens to be the last reference to a published body must be recorded.
///
/// Both counters are process-global and monotone, so a sibling test can only
/// push them higher — never hide the increment this test causes itself.
#[test]
pub(in crate::tests) fn the_free_audit_records_a_release_that_skipped_the_queue() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("UnqueuedReleaseAuditClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);

    let (published_before, unqueued_before) = published_code_free_audit();
    // A bare `Arc`, dropped by hand: exactly the shape the two thread-local
    // dispatch caches used to have.
    let own = Arc::as_ptr(&cm) as usize;
    drop(cm);
    let (_, unqueued_after) = published_code_free_audit();

    // The unmap itself is now DEFERRED whenever a sibling test is inside JIT
    // execution: `ExecutableBuffer::drop` rescues an unqueued published
    // mapping into the retirement queue rather than unmapping it on the spot.
    // So the release is counted once the queue gets to it, not necessarily by
    // the time `drop` returns.
    assert!(
        eventually_drained(|| published_code_free_audit().0 > published_before),
        "the release of a published body must be counted"
    );
    assert!(
        unqueued_after > unqueued_before,
        "and a release that never reached the retirement queue must be counted as one"
    );
    // `is_none()` would be wrong, and flaked about 1 run in 20 at
    // `--test-threads=32`: `mmap` regularly hands the page this body just
    // released straight to a concurrently-running test, which registers its
    // own range at the same address. What this test owns is that the range
    // no longer binds `entry` to OUR artifact — a `Some(other)` is somebody
    // else's business, and is exactly the reading
    // `clear_all_unregisters_every_code_range` already spells out.
    assert_ne!(
        lookup_jit_code_range(entry),
        Some(own),
        "the unqueued release really did unmap it — which is why it is counted"
    );
}

/// The live executable buffer covering `addr`, retrying the crash-safe
/// `try_lock` lookup instead of reading `Locked` as an answer.
pub(in crate::tests) fn live_region_base(addr: usize) -> Option<usize> {
    for _ in 0..1000 {
        match jit_code_region_covering(addr) {
            JitRegionLookup::Found(base, _) => return Some(base),
            JitRegionLookup::NotFound => return None,
            JitRegionLookup::Locked => std::thread::yield_now(),
        }
    }
    panic!("the code-region registry stayed locked");
}

/// The fix for the published-code use-after-free that the audit above only
/// COUNTED (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
///
/// The shape is the one a C2 compile task's `superseded_body` local had: the
/// cache withdraws a published body, the retirement queue releases ITS
/// reference, and a leftover `Arc` somewhere becomes the last owner. Before,
/// that drop unmapped the body on the spot, with no question asked about
/// whether a thread was executing inside it — and in a Spring suite run one
/// was. Now the mapping is rescued into the retirement queue and stays mapped,
/// bytes intact, until the queue's proof covers it.
#[test]
pub(in crate::tests) fn an_unqueued_last_drop_under_running_jit_code_is_rescued_not_unmapped() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RescuedOrphanMappingClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);
    let base = live_region_base(entry).expect("the published body is mapped");

    // A thread is inside compiled code — this one, standing in for the thread
    // that was executing the superseded body.
    let token = jit_execution_enter();
    let (_, unqueued_before) = published_code_free_audit();
    drop(cm);
    assert!(
        published_code_free_audit().1 > unqueued_before,
        "the stray last drop is still counted: it names a holder that should \
         have released through the queue"
    );
    // While this thread is inside compiled code the mapping is ours alone, so
    // its address cannot have been handed to anyone else: covering it at the
    // same base means it was NOT unmapped.
    assert_eq!(
        live_region_base(entry),
        Some(base),
        "a published body whose last owner dropped outside the queue must not \
         be unmapped while a thread is inside compiled code"
    );
    jit_execution_leave(token);

    // And it is released once the queue can prove quiescence. After that the
    // page may be reused by a concurrently running test, so "released" is
    // either no live buffer at `base` or an AUTHORISED free recorded for it.
    assert!(
        eventually_drained(|| {
            live_region_base(entry) != Some(base)
                || matches!(
                    recent_code_free_covering(entry),
                    Some((b, _, _, flags))
                        if b == base
                            && flags & CODE_FREE_AUTHORISED != 0
                            && flags & CODE_FREE_PUBLISHED != 0
                )
        }),
        "the rescued mapping must be released by the queue once JIT execution \
         is quiescent"
    );
}

#[test]
pub(in crate::tests) fn test_jit_cache_remove_reclaims_code_range() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RemoveClass");
    let method: Arc<str> = Arc::from("removeMethod");
    let desc: Arc<str> = Arc::from("()V");

    let cid = cratonvm_types::ClassId::new(1);
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let cm = cache
        .get(&class, &method, &desc, cid)
        .expect("compiled method");
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
    drop(cm);

    cache.remove(&class, &method, &desc, cid);

    assert!(cache.get(&class, &method, &desc, cid).is_none());
    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "removed code must unregister when no caller or reader owns it"
    );
}

#[test]
pub(in crate::tests) fn test_replaced_callee_is_owned_by_baked_direct_caller() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(7);
    let callee_class: Arc<str> = Arc::from("OwnedCallee");
    let callee_method: Arc<str> = Arc::from("target");
    let desc: Arc<str> = Arc::from("()V");

    let mut callee_buf = ExecutableBuffer::new(64).expect("alloc callee");
    callee_buf.emit(&[0xC3]);
    cache.put(
        callee_class.clone(),
        callee_method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(callee_buf),
    );
    let old_entry = cache
        .get(&callee_class, &callee_method, &desc, cid)
        .expect("callee")
        .entry_ptr() as usize;

    let mut caller_buf = ExecutableBuffer::new(64).expect("alloc caller");
    caller_buf.emit(&[0xC3]);
    let mut caller = CompiledMethod::new(caller_buf);
    caller._direct_callee_entries.push(old_entry);
    let caller_class: Arc<str> = Arc::from("OwningCaller");
    let caller_method: Arc<str> = Arc::from("call");
    cache.put(
        caller_class.clone(),
        caller_method.clone(),
        desc.clone(),
        cid,
        caller,
    );

    let mut replacement_buf = ExecutableBuffer::new(64).expect("alloc replacement");
    replacement_buf.emit(&[0xC3]);
    cache.put(
        callee_class.clone(),
        callee_method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(replacement_buf),
    );
    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "the caller's baked edge must pin its superseded callee"
    );

    cache.remove(&caller_class, &caller_method, &desc, cid);
    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "dropping the final direct caller must reclaim the old body"
    );
}

/// A body whose baked direct-call target cannot be pinned must NOT reach
/// the cache.
///
/// `prepare_for_publication` upgrades every `_direct_callee_entries` address
/// into a strong `Arc` so the emitted `call` keeps its callee mapped. When
/// one no longer resolves — the callee was superseded between the compile
/// driver baking its address and this publication — the body carries a call
/// to an address nothing owns. Publishing it anyway (the historical
/// default-OFF `CRATONVM_JIT_STRICT_CALLEE_ROOTS` behaviour) is the
/// retired `jit-wild-jump-page-aligned-pc-20260728` crash: the callee is
/// retired on an ordinary tier-up while NO thread is inside compiled code —
/// a legal, quiescent retirement that `defer_jit_owner` cannot help with,
/// because a baked address holds no `Arc` — and the caller's next
/// invocation jumps into the unmapped page (`SIGSEGV`, `pc == addr`, at the
/// callee's page-aligned entry).
#[test]
pub(in crate::tests) fn a_body_with_an_unpinnable_baked_callee_is_not_published() {
    // The scenario needs an entry no `put` ever registered an owner for,
    // whose buffer is already gone: exactly what `resolve_jit_entry_owner`
    // cannot pin. That address is recyclable, so a concurrently allocating
    // test can adopt it at any point -- including between establishing it
    // and publishing the caller. Retry the whole scenario when that
    // happens, and tell the two outcomes apart by re-reading ownership at
    // the assertion: a publish while the orphan is STILL unowned is the
    // real defect and fails, exactly as it always did.
    let class: Arc<str> = Arc::from("DanglingCaller");
    let method: Arc<str> = Arc::from("call");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4713);

    let mut attempts = 0;
    loop {
        attempts += 1;
        assert!(
            attempts < 100,
            "a concurrently allocating test kept adopting the orphan address"
        );

        let cache = JitCache::new();
        let orphan_entry = {
            let mut buf = ExecutableBuffer::new(64).expect("alloc orphan callee");
            buf.emit(&[0xC3]);
            let cm = CompiledMethod::new(buf);
            let entry = cm.entry_ptr() as usize;
            drop(cm);
            entry
        };
        if resolve_jit_entry_owner(orphan_entry).is_some() {
            continue; // adopted before we could use it
        }

        let mut caller_buf = ExecutableBuffer::new(64).expect("alloc caller");
        caller_buf.emit(&[0xC3]);
        let mut caller = CompiledMethod::new(caller_buf);
        caller._direct_callee_entries.push(orphan_entry);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, caller);

        if cache.get(&class, &method, &desc, cid).is_none() {
            break; // the body was refused, which is the invariant
        }

        // It WAS published. Legitimate only if someone adopted the orphan
        // between the check above and the `put`, making the callee
        // genuinely pinnable; otherwise this is the defect.
        assert!(
            resolve_jit_entry_owner(orphan_entry).is_some(),
            "a body whose baked `call` target is unowned must not be published; \
             it stays interpreted and recompiles once the callee is live again"
        );
    }
}

/// An inline cache that publishes a compiled entry MUST retain the
/// artifact owning it: generated code (`jit/src/x64.rs`'s MIC/PIC cascade)
/// loads the raw pointer into R11 and `CALL R11`s it with no validation,
/// so an entry whose last `Arc` has dropped is a call into an unmapped —
/// or recycled — executable mapping. The virtual dispatch helper used to
/// publish the MIC's entry with a bare atomic store on its
/// "class cached, target unresolved" path, taking no owner at all; that is
/// the NodeConnectionsServiceTests SIGSEGV.
#[test]
pub(in crate::tests) fn mic_update_retains_the_callee_artifact() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("OwnedTarget");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4711);
    let mut buf = ExecutableBuffer::new(64).expect("alloc owned target");
    buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let published = cache
        .get(&class, &method, &desc, cid)
        .expect("target published");
    let entry = published.entry_ptr() as usize;
    // Identity, not address: once this artifact is unmapped its address can
    // be handed straight back to a concurrently-allocating test, and
    // `resolve_jit_entry_owner` would then correctly report SOMEONE at
    // `entry`. A `Weak` to our own body cannot be confused that way.
    let artifact = Arc::downgrade(&published);
    drop(published);

    let slot = JitMICSlot::new();
    slot.update(cid.as_u32(), &class, entry as u64, false, false);
    assert_eq!(
        slot.cached_entry().0 as usize,
        entry,
        "a live entry must be published"
    );
    assert!(
        slot.compiled_owner.lock().is_some(),
        "publishing an entry must take a strong owner for its artifact"
    );

    // Withdrawing the cache's own reference must not release the artifact
    // while the slot still points at it.
    cache.remove(&class, &method, &desc, cid);
    assert!(
        resolve_jit_entry_owner(entry).is_some(),
        "the slot's owner must keep the callee artifact alive"
    );

    slot.clear_compiled_entry();
    assert!(
        eventually_drained(|| artifact.strong_count() == 0),
        "clearing the last holder must release the artifact"
    );
}

/// `JitCache::clear_inline_caches` (interpreter round i1 wave 9, the JVMTI
/// interpreter-only flush) empties every published body's MIC and PIC and
/// retires nothing: the body stays published and callable.
#[test]
pub(in crate::tests) fn clearing_inline_caches_empties_mic_and_pic_and_retires_nothing() {
    // An unregistered target, like a native: publishable without an owner.
    const TARGET: u64 = 0x0CAFE_3000;
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("IcFlushCaller");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4713);
    let mut buf = ExecutableBuffer::new(64).expect("alloc caller");
    buf.emit(&[0xC3]);
    let mut body = CompiledMethod::new(buf);
    body._jit_mic_slots.push(Box::new(JitMICSlot::new()));
    body._jit_pic_slots.push(Box::new(JitPICSlot::new()));
    cache.put(class.clone(), method.clone(), desc.clone(), cid, body);
    let published = cache
        .get(&class, &method, &desc, cid)
        .expect("caller published");
    published._jit_mic_slots[0].update(31, "IcFlushReceiver", TARGET, false, false);
    published._jit_pic_slots[0].install(32, "IcFlushOther", TARGET, false, false);
    assert_eq!(published._jit_mic_slots[0].cached_entry().0, TARGET);
    assert_eq!(
        published._jit_pic_slots[0].lookup(32),
        Some((TARGET, false))
    );

    assert_eq!(cache.clear_inline_caches(), 1);

    assert_eq!(published._jit_mic_slots[0].cached_entry(), (0, false));
    assert_eq!(published._jit_pic_slots[0].lookup(32), None);
    assert!(
        !published.retired.load(std::sync::atomic::Ordering::SeqCst),
        "a flush is not a retirement"
    );
    assert!(
        cache.get(&class, &method, &desc, cid).is_some(),
        "the body stays published"
    );
}

/// The structural half of the same fix: if any caller hands an inline
/// cache an entry whose artifact is already gone, the cache must refuse it
/// instead of handing it to generated code. A *registered* address whose
/// `Weak` no longer upgrades is exactly that case, and is distinguishable
/// from a native target, which was never registered and stays publishable.
#[test]
pub(in crate::tests) fn inline_caches_refuse_a_dead_artifact_entry_but_allow_unregistered_ones() {
    const DEAD_ENTRY: usize = 0x0DEAD_1000;
    const NATIVE_ENTRY: u64 = 0x0CAFE_2000;
    {
        let mut buf = ExecutableBuffer::new(64).expect("alloc dead target");
        buf.emit(&[0xC3]);
        let arc = Arc::new(CompiledMethod::new(buf));
        jit_entry_owners()
            .lock()
            .insert(DEAD_ENTRY, Arc::downgrade(&arc));
        drop(arc);
    }

    let slot = JitMICSlot::new();
    slot.update(21, "DeadOwner", DEAD_ENTRY as u64, false, false);
    assert_eq!(
        slot.cached_entry_word
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "a dead artifact's entry must never be published to generated code"
    );

    let native_slot = JitMICSlot::new();
    native_slot.update(22, "NativeOwner", NATIVE_ENTRY, false, false);
    assert_eq!(
        native_slot.cached_entry().0,
        NATIVE_ENTRY,
        "non-artifact targets (natives, trampolines) stay publishable"
    );

    let pic = JitPICSlot::new();
    pic.install(21, "DeadOwner", DEAD_ENTRY as u64, false, false);
    assert!(
        pic.lookup_megamorphic(21).is_none(),
        "the hashed overflow table must refuse a dead entry too"
    );
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.entry_words[i].load(std::sync::atomic::Ordering::Acquire),
            0,
            "no PIC way may hold a dead entry"
        );
    }
}

#[test]
pub(in crate::tests) fn test_inline_cache_reclamation_waits_for_jit_quiescence() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("DeferredTarget");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(19);
    let mut buf = ExecutableBuffer::new(64).expect("alloc deferred target");
    buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let entry = cache
        .get(&class, &method, &desc, cid)
        .expect("target")
        .entry_ptr() as usize;
    let slot = JitMICSlot::new();
    slot.update(cid.as_u32(), &class, entry as u64, false, false);

    let execution = jit_execution_enter();
    slot.clear_compiled_entry();
    cache.remove(&class, &method, &desc, cid);
    assert!(
        lookup_jit_code_range(entry).is_some(),
        "a raw cache reader may still be between load and call"
    );
    jit_execution_leave(execution);
    // `eventually_drained`, not a bare assertion: `ACTIVE_JIT_EXECUTIONS`
    // is process-global, so this thread's `leave` is the final transition
    // only if no sibling test is inside its own execution epoch. What is
    // being checked is that the deferred owner is released once JIT
    // execution IS quiescent — and the pump is what makes that observable
    // here rather than whenever some unrelated test happens to leave.
    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "the quiescent transition must drain deferred code owners"
    );
}

/// A thread parked inside compiled code — a pool worker blocked in
/// `LinkedBlockingQueue.take()` — used to hold `ACTIVE_JIT_EXECUTIONS` up
/// for as long as it stayed parked, so nothing retired after it parked was
/// ever released and the code cache filled to its cap. While blocked, the
/// thread can only return into what its stack names: a retired body it
/// does not name must be reclaimed without waiting for it to wake, and one
/// it does name must be kept until it leaves.
#[test]
pub(in crate::tests) fn a_thread_parked_in_compiled_code_holds_only_what_its_stack_names() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("ParkedInJitClass");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4723);
    let publish = |name: &str| {
        let method: Arc<str> = Arc::from(name);
        let mut buf = ExecutableBuffer::new(64).expect("alloc body");
        buf.emit(&[0xC3]);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache.get(&class, &method, &desc, cid).expect("published");
        let entry = cm.entry_ptr() as usize;
        (method, Arc::downgrade(&cm), entry)
    };
    let (unrelated_method, unrelated, _) = publish("unrelated");
    let (held_method, held, held_entry) = publish("held");

    let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    let parked = std::thread::spawn(move || {
        let execution = jit_execution_enter();
        // This thread's stack band while parked: a return address into
        // `held`, and nothing that points into `unrelated`.
        let band: [usize; 8] = [0, 0x10, held_entry + 4, 0, 0, 0, 0, 0];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        // SAFETY: `band` is a live local array on this thread's stack for
        // the whole blocked window.
        unsafe { jit_thread_blocked_enter(lo, hi) };
        parked_tx.send(()).expect("the test waits for the park");
        wake_rx.recv().expect("the test wakes the thread");
        jit_thread_blocked_leave();
        std::hint::black_box(&band);
        jit_execution_leave(execution);
    });
    parked_rx.recv().expect("the thread parks");

    cache.remove(&class, &unrelated_method, &desc, cid);
    cache.remove(&class, &held_method, &desc, cid);
    assert!(
        eventually_drained(|| unrelated.strong_count() == 0),
        "a retired body no parked thread can return into must be reclaimed \
         while that thread is still parked inside compiled code"
    );
    assert_ne!(
        held.strong_count(),
        0,
        "a retired body the parked thread's stack names must be kept"
    );

    wake_tx.send(()).expect("the parked thread is waiting");
    parked.join().expect("the parked thread finishes");
    assert!(
        eventually_drained(|| held.strong_count() == 0),
        "and released once the thread has left compiled code"
    );
}

#[test]
pub(in crate::tests) fn test_sharded_cache_supports_concurrent_lock_free_reads_and_publication() {
    let cache = Arc::new(JitCache::new());
    let mut workers = Vec::new();
    for worker in 0..4u32 {
        let cache = cache.clone();
        workers.push(std::thread::spawn(move || {
            for method_index in 0..32u32 {
                let class: Arc<str> = Arc::from(format!("Concurrent{worker}"));
                let method: Arc<str> = Arc::from(format!("m{method_index}"));
                let desc: Arc<str> = Arc::from("()V");
                let cid = cratonvm_types::ClassId::new(worker * 32 + method_index + 1);
                let mut buf = ExecutableBuffer::new(16).expect("alloc concurrent body");
                buf.emit(&[0xC3]);
                cache.put(
                    class.clone(),
                    method.clone(),
                    desc.clone(),
                    cid,
                    CompiledMethod::new(buf),
                );
                assert!(cache.get(&class, &method, &desc, cid).is_some());
            }
        }));
    }
    for worker in workers {
        worker.join().expect("cache worker");
    }
    assert_eq!(cache.len(), 128);
    assert_eq!(cache.clear_all(), 128);
    assert!(cache.is_empty());
}

#[test]
pub(in crate::tests) fn test_code_range_snapshot_stays_searchable_during_publication() {
    let base = 0x7f00_0000_0000usize;
    for i in 0..128usize {
        register_jit_code_range(base + i * 0x1000, 0x100, i + 1);
    }
    let readers = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    for i in 0..128usize {
                        assert_eq!(lookup_jit_code_range(base + i * 0x1000 + 0x40), Some(i + 1));
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    for reader in readers {
        reader.join().expect("range reader");
    }
    for i in 0..128usize {
        unregister_jit_code_range(base + i * 0x1000);
    }
}

// `clear_all` must evict every entry and drop every published body —
// `CompiledMethod::drop` is what returns the executable mapping AND
// withdraws that body's `JIT_CODE_RANGES` registration.
//
// The post-conditions below are deliberately phrased against state this
// test OWNS rather than against the process-global range table. This lib
// test binary runs its tests on a thread pool and `JIT_CODE_RANGES` is
// process-global, so the old `lookup_jit_code_range(entry).is_none()`
// assertion was order-dependent: `clear_all` unmaps our code pages, a
// concurrently-running test then gets one of those very addresses back
// from `ExecutableBuffer::new` and registers ITS range over it, and the
// lookup correctly reports `Some(<that other test's body>)`. Holding a
// `Weak` to each body keeps its `Arc` allocation (not the body) alive, so
// `own_*` can never be recycled underneath the comparison and
// `strong_count() == 0` proves *our* `Drop` ran.
// T2.2 epoch invalidation for the interpreter's Bytecode invoke cache
//
// These tests guard the contract the interpreter's cached-invoke `Bytecode`
// arms now rely on: instead of calling `JitCache::get` (three string hashes
// + three string compares) on every interpreted call just to notice that a
// background compile published a body, they memoize
// `jit_cache_generation()` in `CachedBytecodeMethod::jit_probe_generation`
// and only re-probe when the live generation differs.
//
// Every assertion below is written to be robust under a parallel `cargo
// test`: the generation is a process-global, so other tests can advance it
// concurrently. That can only push two sampled values FURTHER apart, so
// "must differ" assertions are race-free; no test here asserts that the
// generation stayed equal across a window in which another test could run.

pub(in crate::tests) fn probe_test_method(
    class: &str,
    method: &str,
    desc: &str,
    cid: cratonvm_types::ClassId,
) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cid,
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Arc::from(desc),
        source_file: None,
        code: Arc::from([0xb1u8].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

pub(in crate::tests) fn probe_ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

/// THE staleness test. Replays the interpreter's exact epoch protocol
/// end-to-end and proves that a **first-time** publication (not a
/// replacement) forces the memoized call site back onto the slow path,
/// where it picks up the newly published body.
///
/// This is the case that used to be broken: `JitCache::put` advanced the
/// generation only when it *replaced* an existing entry, so a call site that
/// had already memoized "no compiled body for this method" would have stayed
/// pinned to the interpreter forever after the background worker published
/// its first body.
#[test]
pub(in crate::tests) fn jit_probe_epoch_forces_reprobe_after_first_publication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4101);
    let class: Arc<str> = Arc::from("ProbeEpochA");
    let method: Arc<str> = Arc::from("hot");
    let desc: Arc<str> = Arc::from("()V");
    let cached = probe_test_method(&class, &method, &desc, cid);

    // 1. Cold: the call site probes, misses, and memoizes the miss --
    //    exactly what the `Bytecode` arm does on its first interpreted call.
    let gen_at_probe = jit_cache_generation();
    assert!(
        cache.get(&class, &method, &desc, cid).is_none(),
        "nothing published yet"
    );
    cached.record_jit_probe_miss(gen_at_probe);
    assert!(
        cached.jit_probe_is_current(gen_at_probe),
        "the memo must suppress a re-probe while the generation is unchanged"
    );

    // 2. A background compile publishes this method's FIRST body.
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );

    // 3. The memo must now report stale, so the interpreter re-probes...
    let gen_after_publish = jit_cache_generation();
    assert!(
        !cached.jit_probe_is_current(gen_after_publish),
        "a first-time publication MUST advance the JIT cache generation, or an \
         interpreted call site that already memoized a probe miss would never \
         notice the new body"
    );
    // ...and the re-probe finds the freshly published body.
    assert!(
        cache.get(&class, &method, &desc, cid).is_some(),
        "the re-probe must pick up the published body"
    );
}

/// An OSR publication lands in a separate map (`osr_methods`) but reaches the
/// same `Bytecode` call sites, so it must advance the generation too. Same
/// first-publication (non-replacement) shape as above.
#[test]
pub(in crate::tests) fn jit_probe_epoch_forces_reprobe_after_first_osr_publication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4102);
    let class: Arc<str> = Arc::from("ProbeEpochB");
    let method: Arc<str> = Arc::from("loopy");
    let desc: Arc<str> = Arc::from("(I)I");
    let cached = probe_test_method(&class, &method, &desc, cid);

    let gen_at_probe = jit_cache_generation();
    cached.record_jit_probe_miss(gen_at_probe);

    let mut osr = probe_ret_body();
    osr.compiled_via_osr = true;
    cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "put_osr must advance the generation on a first publication"
    );
    assert!(cache.get_osr(&class, &method, &desc, cid).is_some());
}

/// A C1->C2 supersede republishes under an existing key. That path already
/// bumped (it is a replacement), but it is part of the audited contract, so
/// pin it: a call site that had flipped to `Jit` and then been downgraded
/// back to `Bytecode` must still re-probe and find the C2 body.
#[test]
pub(in crate::tests) fn jit_probe_epoch_forces_reprobe_after_supersede_republication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4103);
    let class: Arc<str> = Arc::from("ProbeEpochC");
    let method: Arc<str> = Arc::from("tiered");
    let desc: Arc<str> = Arc::from("()J");
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );
    let c1_entry = cache
        .get(&class, &method, &desc, cid)
        .expect("C1 body")
        .entry_ptr() as usize;

    let cached = probe_test_method(&class, &method, &desc, cid);
    let gen_at_probe = jit_cache_generation();
    cached.record_jit_probe_miss(gen_at_probe);

    // C2 replaces the C1 entry under the same key.
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );

    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "a superseding republication must advance the generation"
    );
    let c2_entry = cache
        .get(&class, &method, &desc, cid)
        .expect("C2 body")
        .entry_ptr() as usize;
    assert_ne!(c1_entry, c2_entry, "C2 must have replaced the C1 artifact");
}

/// Invalidation/eviction must advance the generation as well. It is not a
/// staleness hazard for the negative memo (a removal cannot make "there is
/// no body" wrong), but the interpreter's `Jit` entries are re-derived
/// through this same probe, so pin the behaviour rather than leave it to
/// chance.
#[test]
pub(in crate::tests) fn jit_probe_epoch_advances_on_invalidation() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4104);
    let class: Arc<str> = Arc::from("ProbeEpochD");
    let method: Arc<str> = Arc::from("gone");
    let desc: Arc<str> = Arc::from("()V");
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );
    assert!(cache.get(&class, &method, &desc, cid).is_some());

    let cached = probe_test_method(&class, &method, &desc, cid);
    let gen_before_remove = jit_cache_generation();
    cached.record_jit_probe_miss(gen_before_remove);

    cache.remove(&class, &method, &desc, cid);
    assert!(cache.get(&class, &method, &desc, cid).is_none());
    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "an eviction must advance the generation"
    );
}

/// The memoized invocation-counter key must be exactly the canonical
/// `invoc_key_parts` key every other door computes -- the key indexes
/// `ProfileStore`'s per-method warmup counters, so a door with a different
/// value would count a method's warmup in a counter nobody else reads.
#[test]
pub(in crate::tests) fn memoized_invoc_key_matches_the_open_coded_hash() {
    let cid = cratonvm_types::ClassId::new(7);
    let cached = probe_test_method("pkg/Hash", "someMethod", "(Ljava/lang/String;I)Z", cid);
    let expected = cratonvm_jit_api::invoc_key_parts(
        cid.as_u32(),
        &cached.method_name,
        &cached.method_descriptor,
    );
    assert_eq!(cached.invoc_key(), expected);
    // Memoized: stable across calls.
    assert_eq!(cached.invoc_key(), expected);
    // And it survives a clone (the manual `Clone` impl).
    assert_eq!(cached.clone().invoc_key(), expected);
}

#[test]
pub(in crate::tests) fn test_jit_cache_clear_all_evicts_entries() {
    let cache = JitCache::new();
    let class_a: Arc<str> = Arc::from("TestClassA");
    let method_a: Arc<str> = Arc::from("testA");
    let desc_a: Arc<str> = Arc::from("()V");
    let class_b: Arc<str> = Arc::from("TestClassB");
    let method_b: Arc<str> = Arc::from("testB");
    let desc_b: Arc<str> = Arc::from("(I)I");

    let cid_a = cratonvm_types::ClassId::new(1);
    let cid_b = cratonvm_types::ClassId::new(2);
    let mut buf_a = ExecutableBuffer::new(16).expect("alloc failed");
    buf_a.emit(&[0xC3]); // RET
    cache.put(
        class_a.clone(),
        method_a.clone(),
        desc_a.clone(),
        cid_a,
        CompiledMethod::new(buf_a),
    );

    let mut buf_b = ExecutableBuffer::new(16).expect("alloc failed");
    buf_b.emit(&[0xC3]); // RET
    cache.put(
        class_b.clone(),
        method_b.clone(),
        desc_b.clone(),
        cid_b,
        CompiledMethod::new(buf_b),
    );

    let cm_a = cache
        .get(&class_a, &method_a, &desc_a, cid_a)
        .expect("compiled A");
    let cm_b = cache
        .get(&class_b, &method_b, &desc_b, cid_b)
        .expect("compiled B");
    let entry_a = cm_a.entry_ptr() as usize;
    let entry_b = cm_b.entry_ptr() as usize;
    // The `cm_ptr` each body registered in `JIT_CODE_RANGES` (see
    // `JitCache::put`), used below to tell our own registration apart from a
    // recycled-address one belonging to another test.
    let own_a = Arc::as_ptr(&cm_a) as usize;
    let own_b = Arc::as_ptr(&cm_b) as usize;
    let weak_a = Arc::downgrade(&cm_a);
    let weak_b = Arc::downgrade(&cm_b);
    drop(cm_a);
    drop(cm_b);

    assert_eq!(cache.len(), 2);
    assert_eq!(cache.clear_all(), 2);
    assert!(cache.is_empty());
    assert!(cache.get(&class_a, &method_a, &desc_a, cid_a).is_none());
    assert!(cache.get(&class_b, &method_b, &desc_b, cid_b).is_none());

    // The cache held the last strong reference, so eviction must have run
    // `CompiledMethod::drop` — which unmaps the code and unregisters the
    // range — for both bodies.
    assert!(
        eventually_drained(|| weak_a.strong_count() == 0),
        "clear_all must release the last owner of body A"
    );
    assert!(
        eventually_drained(|| weak_b.strong_count() == 0),
        "clear_all must release the last owner of body B"
    );
    // And no code range still binds our entry addresses to OUR bodies. A
    // `Some(other)` here just means a concurrently-running test has already
    // been handed the freed page back; that is not this cache's business.
    assert_ne!(
        lookup_jit_code_range(entry_a),
        Some(own_a),
        "clear_all must withdraw body A's own code-range registration"
    );
    assert_ne!(
        lookup_jit_code_range(entry_b),
        Some(own_b),
        "clear_all must withdraw body B's own code-range registration"
    );
}

