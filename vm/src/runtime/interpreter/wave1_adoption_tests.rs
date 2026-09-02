// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Coverage for the `arch-2026-07-26` adoption edits in this file:
//! O(1) quickened dispatch (`quickened-dispatch-o1.md`), the frame-pointer
//! hoists (`frame-arena.md` §6.1) and the constant-triple native memo cells
//! (`native-dispatch-memoization.md` §3 Step 1).

use super::*;
use crate::runtime::frame::FrameStack;
use std::sync::Arc as StdArc;

/// A tiny, fully decodable method body: `iconst_0; istore_0; iload_0;
/// ifeq +3 (back-edge target); return`, padded with the two trailing zero
/// bytes every `Frame` code array carries.
fn sample_code() -> StdArc<[u8]> {
    StdArc::from(
        [
            0x03u8, // iconst_0        @0
            0x3b,   // istore_0        @1
            0x1a,   // iload_0         @2
            0x99, 0x00, 0x03, // ifeq +3   @3
            0xb1, // return          @6
            0x00, 0x00, // padding
        ]
        .as_slice(),
    )
}

/// The dispatch loop swapped `index_of_pc(pc, hint)` + `op(idx)` +
/// `next_pc(idx)` for the single `resolve(pc)`. Pin that the replacement is
/// exactly equivalent at every pc — including the misses that must still
/// fall through to `Instruction::decode`, and regardless of what the
/// now-deleted `quick_hint` would have been.
#[test]
fn resolve_matches_the_index_of_pc_triple_it_replaced() {
    let code = sample_code();
    let quick =
        cratonvm_reader::quickened::intern(&code).expect("a fully decodable body must quicken");

    for pc in 0..code.len() + 4 {
        let expected = quick
            .index_of_pc_direct(pc)
            .map(|idx| (quick.op(idx), quick.next_pc(idx)));
        let actual = quick.resolve(pc);

        match (expected, actual) {
            (None, None) => {}
            (Some((eop, enext)), Some((aop, anext))) => {
                assert!(
                    std::ptr::eq(eop, aop),
                    "resolve({pc}) must hand back the same interned record"
                );
                assert_eq!(enext, anext, "resolve({pc}) next_pc diverged");
            }
            (e, a) => panic!(
                "resolve({pc}) disagreed: {:?} vs {:?}",
                e.is_some(),
                a.is_some()
            ),
        }

        // The deleted hint was never load-bearing: every hint value, valid
        // or nonsensical, must agree with the hint-free form.
        for hint in [0usize, 1, 2, 5, usize::MAX] {
            assert_eq!(
                quick.index_of_pc(pc, hint),
                quick.index_of_pc_direct(pc),
                "hint {hint} changed the answer at pc {pc}"
            );
        }
    }
}

fn sample_frame() -> Frame {
    Frame::new(
        ClassId::new(1),
        "com/example/Sample".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        sample_code()[..7].to_vec(),
        Vec::new(),
        2,
        2,
        &[],
    )
}

/// The preamble and dispatch hoists read `pc`, `last_instr_pc` and `code`
/// through a raw `*mut Frame` instead of
/// re-indexing. Pin the two properties that makes sound: the pointer
/// addresses the same frame indexing would, and a region that performs no
/// push cannot relocate it (`reloc_epoch` unchanged).
#[test]
fn hoisted_frame_pointer_addresses_the_same_frame_as_indexing() {
    let mut frames = FrameStack::new();
    frames.push(sample_frame());
    frames.push(sample_frame());
    let frame_idx = frames.len() - 1;

    let epoch_before = frames.reloc_epoch();
    let fp = frames.frame_ptr(frame_idx);
    assert!(!fp.is_null(), "an in-range index must not yield null");

    // Everything the hoisted region reads must match indexing.
    // SAFETY: `fp` came from `frame_ptr(frame_idx)` with `frame_idx` in
    // range and was asserted non-null above, and no push has happened
    // since, so it still addresses the same live frame.
    unsafe {
        assert_eq!((*fp).pc, frames[frame_idx].pc);
        // Explicit `&` — see the note at the `padded_code_len` read: an
        // implicit autoref through a raw pointer is denied by
        // `dangerous_implicit_autorefs`.
        assert_eq!((&(*fp).code).len(), frames[frame_idx].code.len());
        assert!(std::ptr::eq(
            (&(*fp).code).as_ptr(),
            frames[frame_idx].code.as_ptr()
        ));
    }

    // A write through the pointer is observable through the index, and the
    // no-push region did not relocate anything.
    let fp = frames.frame_ptr(frame_idx);
    // SAFETY: freshly re-derived from the same in-range index, and no push
    // has occurred, so the pointer is valid and uniquely held here.
    unsafe {
        (*fp).pc = 6;
        (*fp).last_instr_pc = 3;
    }
    assert_eq!(frames[frame_idx].pc, 6);
    assert_eq!(frames[frame_idx].last_instr_pc, 3);
    assert_eq!(
        epoch_before,
        frames.reloc_epoch(),
        "a region with no push must not bump the relocation epoch"
    );

    // Rule 4: out of range is null, which is what routes to `VmError`
    // instead of the panic the old `thread.frames[frame_idx]` would raise.
    assert!(frames.frame_ptr(frames.len()).is_null());
}

/// `Frame::new` now interns padded bytecode per method identity. The
/// uncached invoke path in `execute` adopted the same call. Pin both halves
/// of the contract `local_liveness.rs` depends on: same method shares one
/// allocation, distinct methods never do.
#[test]
fn frames_of_one_method_share_bytecode_but_distinct_methods_do_not() {
    let a = sample_frame();
    let b = sample_frame();
    assert!(
        std::ptr::eq(a.code.as_ptr(), b.code.as_ptr()),
        "two frames of the same method must share one padded code allocation"
    );

    let other = Frame::new(
        ClassId::new(1),
        "com/example/Sample".to_string(),
        "runOther".to_string(),
        "()V".to_string(),
        None,
        sample_code()[..7].to_vec(),
        Vec::new(),
        2,
        2,
        &[],
    );
    assert!(
        !std::ptr::eq(a.code.as_ptr(), other.code.as_ptr()),
        "byte-identical bodies of DIFFERENT methods must keep distinct Arcs — \
         local_liveness.rs keys its handler-edge-dependent table on this pointer"
    );
}

fn noop_native(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// Every constant-triple site converted in this file (B1-B7) replaced
/// `registry.find(c, m, d)` with `CELL.callback(&registry, c, m, d)`. Pin
/// that substitution — on a hit, on a miss, and across the late
/// registration that the old `OnceLock`-shaped memo could not survive.
#[test]
fn constant_triple_memo_cell_is_substitutable_for_find() {
    let cell = cratonvm_native_api::NativeCallSite::new();
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();

    // Miss before registration must agree, and must not be memoized as a
    // permanent negative.
    assert!(registry
        .find(
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;"
        )
        .is_none());
    assert!(cell
        .callback(
            &registry,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .is_none());

    registry.register(
        "java/lang/Class",
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        noop_native,
    );

    let via_find = registry
        .find(
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .expect("registered native must be findable");
    let via_cell = cell
        .callback(
            &registry,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .expect("the memo cell must self-heal past the earlier negative");
    assert_eq!(
        via_find as usize, via_cell as usize,
        "the memo cell must resolve the same callback `find` does"
    );

    // Warm path: a second read agrees with itself and with `find`.
    let warm = cell
        .callback(
            &registry,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )
        .expect("warm read");
    assert_eq!(warm as usize, via_find as usize);
}

/// ONE STATIC, ONE TRIPLE — the invariant every `NCS_*` cell in this file
/// relies on, pinned here so the failure mode is visible next to the call
/// sites rather than only in `native-api`.
///
/// A `NativeCallSite` memo is `(generation << 32) | slot` and is validated
/// against the registry generation *only*: the triple is deliberately not
/// re-checked on a warm hit, because re-hashing three strings is the exact
/// cost the cell exists to remove. So a cell reached with two different
/// triples will hand the second one whatever the first memoized — and when
/// the first was a miss, that is a silent `None` for a native that is
/// registered and dispatches fine through `find`. No panic, no log.
#[test]
fn sharing_one_memo_cell_across_two_triples_silently_mis_answers() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    registry.register("java/lang/Sample", "b", "()V", noop_native);

    // A cell that first sees triple A (unregistered) memoizes a negative
    // for this generation, then wrongly serves it to triple B.
    let shared_cell = cratonvm_native_api::NativeCallSite::new();
    assert!(shared_cell
        .callback(&registry, "java/lang/Sample", "a", "()V")
        .is_none());
    let leaked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared_cell.callback(&registry, "java/lang/Sample", "b", "()V")
    }));
    if cfg!(debug_assertions) {
        assert!(
            leaked.is_err(),
            "debug builds must reject cross-triple memo-cell reuse"
        );
    } else {
        assert!(
            leaked.unwrap().is_none(),
            "release builds demonstrate the footgun: a shared cell \
             redeems triple A's negative for triple B"
        );
    }

    // `find` proves the native really is registered and resolvable — the
    // shared cell was simply wrong.
    assert!(registry.find("java/lang/Sample", "b", "()V").is_some());

    // One cell per triple is correct. Every `NCS_*` static in this file is
    // reached from exactly one call site with a literal/const triple.
    let cell_b = cratonvm_native_api::NativeCallSite::new();
    assert!(cell_b
        .callback(&registry, "java/lang/Sample", "b", "()V")
        .is_some());
}

fn cached_entry(class: &str, method: &str, descriptor: &str) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(4242),
        class_name: StdArc::from(class),
        method_name: StdArc::from(method),
        method_descriptor: StdArc::from(descriptor),
        source_file: None,
        code: StdArc::from(vec![0xB1u8].as_slice()),
        exception_table: StdArc::from(vec![].as_slice()),
        max_stack: 0,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: false,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// Sites A1-A3 (`native-dispatch-memoization.md` §3 Step 2) all read the
/// SAME per-entry `NativeCallSite`. That is only safe because all three
/// hand it the same triple — and each spells the arguments differently:
///
/// * A1 (`intercept_force_registered_native_cached`) binds
///   `let class_name = cached.class_name.as_ref();` at the top of the
///   function and passes the bindings;
/// * A2 (the vtable-hit force-native gate) passes `cached.*.as_ref()`;
/// * A3 (the instance tier-up gate) passes `&cached.*`.
///
/// Three spellings of one triple. If any of them ever drifts to a
/// *different* triple, the shared cell silently serves that site whatever
/// the others memoized (see
/// `sharing_one_memo_cell_across_two_triples_silently_mis_answers`). Pin
/// that the three spellings are interchangeable through the shared cell.
#[test]
fn sites_a1_a2_a3_share_one_cell_because_they_share_one_triple() {
    let cached = cached_entry("java/lang/Sample", "run", "()V");
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    registry.register("java/lang/Sample", "run", "()V", noop_native);

    // A1's spelling.
    let class_name = cached.class_name.as_ref();
    let method_name = cached.method_name.as_ref();
    let method_descriptor = cached.method_descriptor.as_ref();
    let a1 = cached
        .native_call_site()
        .callback(&registry, class_name, method_name, method_descriptor)
        .expect("A1 must resolve the registered native");

    // A2's spelling, through the now-warm shared cell.
    let a2 = cached
        .native_call_site()
        .resolve(
            &registry,
            cached.class_name.as_ref(),
            cached.method_name.as_ref(),
            cached.method_descriptor.as_ref(),
        )
        .expect("A2 must resolve the same native through the warm cell");

    // A3's spelling.
    let a3 = cached
        .native_call_site()
        .resolve(
            &registry,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
        .expect("A3 must resolve the same native through the warm cell");

    assert_eq!(a2, a3, "A2 and A3 must resolve the identical native id");
    assert_eq!(
        registry.callback_of(a2).map(|cb| cb as usize),
        Some(a1 as usize),
        "all three sites must agree with each other and with `find`"
    );
    assert_eq!(
        registry
            .find("java/lang/Sample", "run", "()V")
            .map(|cb| cb as usize),
        Some(a1 as usize),
        "and with the `find` they replaced"
    );
}

/// The latent bug the A1-A3 conversion fixes. The `OnceLock` these sites
/// used sealed a *negative* for the life of the process, so a native
/// registered by `alias_class` or a lazy `register_*` pass — both of which
/// run after the first bytecode executes — stayed invisible here. At A3
/// that is not merely a slow path: a missed native lets tier-up compile the
/// method's real bytecode, which permanently bypasses the override.
#[test]
fn a1_a3_see_a_native_registered_after_the_site_first_ran() {
    let cached = cached_entry("java/lang/Sample", "late", "()V");
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();

    // Cold pass: no native yet. A `OnceLock` would seal this `None`.
    assert!(cached
        .native_call_site()
        .callback(
            &registry,
            cached.class_name.as_ref(),
            cached.method_name.as_ref(),
            cached.method_descriptor.as_ref(),
        )
        .is_none());

    registry.register("java/lang/Sample", "late", "()V", noop_native);

    assert!(
        cached
            .native_call_site()
            .resolve(
                &registry,
                cached.class_name.as_ref(),
                cached.method_name.as_ref(),
                cached.method_descriptor.as_ref(),
            )
            .is_some(),
        "the tier-up gate must observe a late registration, or it will \
         compile real bytecode over a registered native override"
    );
}
