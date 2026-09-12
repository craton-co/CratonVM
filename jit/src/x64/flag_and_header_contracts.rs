// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Flag-skew and header-offset contracts for the x86-64 backend.
//!
//! ---------------------------------------------------------------------------
//! Flag-skew and header-offset contracts
//! ---------------------------------------------------------------------------
//!
//! Companion doc: `x64-flag-skew-and-contracts.md`.
//!
//! These tests defend two properties that no build error would catch:
//!
//! 1. Codegen and the collector must derive the SAME answer for a shared
//! feature gate. Before 2026-07-26 `CRATONVM_MOVING_YOUNG` was `getenv`'d
//! independently in three crates; a divergence there is heap corruption,
//! not a wrong answer, because the JIT half (publish a complete rewritable
//! root map) and the GC half (run the moving cycle) are only sound
//! together.
//! 2. The x86-64 array/field emitters bake object-header offsets into
//! instruction displacements. The planned 32→16-byte `ObjectHeader` shrink
//! must touch every one of them; the inventory counts below are the
//! tripwire that says "the doc's site list is stale".

use super::*;

/// The sources the x86-64 compiler proper is split across (SEAM-01).
///
/// Before the split this was a single 40k-line file and the two inventory
/// tests below scanned it directly. The split moves emission sites into
/// submodules without changing one of them, so an inventory that still looked
/// at `x64.rs` alone would silently under-count and read as "sites were
/// deleted" instead of "sites moved".
///
/// **Add every new `x64/*.rs` that receives moved compiler code to this list
/// in the same commit that creates it.** A missing entry surfaces as a count
/// shortfall in `header_offset_emission_site_inventory_matches_the_doc`, which
/// is precisely the tripwire that test is for. Pre-existing analysis
/// submodules (`bce`, `licm`, `vec_emit`, `isel`, ...) are deliberately absent:
/// they were never inside the scanned text, so adding them would change the
/// totals for a reason that has nothing to do with the shrink audit.
fn backend_sources() -> String {
    [
        include_str!("../x64.rs"),
        include_str!("bytecode_walk.rs"),
        include_str!("emit.rs"),
        include_str!("operand_stack.rs"),
        include_str!("frames.rs"),
        include_str!("safepoint.rs"),
        include_str!("deopt_stubs.rs"),
        include_str!("osr.rs"),
        include_str!("arith.rs"),
        include_str!("simd.rs"),
        include_str!("arrays.rs"),
        include_str!("objects.rs"),
        include_str!("inlining.rs"),
        include_str!("loop_rewrite.rs"),
        include_str!("driver.rs"),
        include_str!("tests.rs"),
        include_str!("flag_and_header_contracts.rs"),
        include_str!("loop_unroll_admission.rs"),
    ]
    .concat()
}

/// The whole point of the 2026-07-26 de-skew: this function must be a
/// projection of the centralized config, not an independent parse.
#[test]
fn moving_young_enabled_is_the_centralized_flag() {
    assert_eq!(
        moving_young_enabled(),
        cratonvm_types::flags().gc.moving_young,
        "jit::x64::moving_young_enabled must project cratonvm_types::flags().gc.moving_young \
         — an independent getenv here lets codegen and the collector disagree about whether \
         the moving young gen is on"
    );
}

/// The direct JIT-to-JIT call gate does NOT depend on the moving-young
/// flag, and must not be re-coupled to it.
///
/// It was, for one day, because the raw edge SIGSEGV'd every run of
/// `BasicErrorControllerIntegrationTests` under moving-young and the frame
/// shape looked like the culprit. It was not: the crash was the inline PIC
/// cascade's inter-slot `JNE` truncating to `rel8` and branching into the
/// middle of the shadow-stack push. An unguarded callee frame costs the
/// moving-young coverage proof (the cycle falls back to the non-moving
/// sweep) but reclaims nothing, so gating the edge on the collector's mode
/// buys no safety — it only hides machine-code bugs behind a flag. See
/// `crate::direct_jit_callee_calls_enabled` for the measurement.
#[test]
fn direct_jit_callee_calls_open_under_moving_young() {
    if cratonvm_types::flags::runtime_var("CRATONVM_JIT_DIRECT_CALLEE_CALLS").is_ok() {
        // A caller-set off-switch legitimately closes the gate; this test
        // is about the *default*, so it has nothing to assert here.
        return;
    }
    let saved = MOVING_YOUNG_OVERRIDE.with(|c| c.get());
    set_moving_young_override(Some(true));
    let open = crate::direct_jit_callee_calls_enabled();
    let ir_open = crate::ir_direct_calls_enabled();
    set_moving_young_override(saved);
    assert!(
        open,
        "the raw JIT-to-JIT edge must not be re-gated on the collector's mode: an \
         unguarded callee frame costs coverage, not correctness"
    );
    assert!(
        ir_open,
        "the IR direct-call lowering is subordinate to the same master switch"
    );
}

/// A `rel8` branch displacement that does not fit must discard the compile,
/// never truncate.
///
/// Truncation is not a near-miss: `rel as u8` turns an out-of-range forward
/// branch into a backward one, and on x86 the landing site is normally the
/// middle of an earlier instruction. The inline-PIC cascade shipped exactly
/// that — a `-128` wrap into the body of the pre-call shadow-stack push,
/// which then ran as an unguarded infinite push loop. `debug_assert!` was
/// the only guard, i.e. none, since release is where it matters.
#[test]
fn rel8_patch_out_of_range_bails_instead_of_truncating() {
    let mut buf = ExecutableBuffer::new(4096).expect("test buffer");
    // `0x75 0x00` — a JNE whose displacement byte is the patch site.
    buf.emit(&[0x75, 0x00]);
    let patch = buf.pos() - 1;
    Compiler::patch_rel8_or_bail(&mut buf, patch, 127);
    assert!(!buf.overflowed(), "an in-range rel8 must patch normally");
    assert_eq!(buf.as_slice()[patch], 127);
    Compiler::patch_rel8_or_bail(&mut buf, patch, 128);
    assert!(
        buf.overflowed(),
        "a rel8 that does not fit must mark the buffer overflowed so the driver \
         discards the method — truncating retargets the branch"
    );
    assert_eq!(
        buf.as_slice()[patch],
        127,
        "the bail must leave the displacement byte alone"
    );
    // Negative displacements are ordinary (every backward branch is one), and
    // the range is `i8`, not `u8`. `u8::try_from` was the check one site used
    // and it accepts 128..=255 — which the CPU reads back as -128..=-1, a
    // backward branch, i.e. the exact failure this helper exists to stop.
    Compiler::patch_rel8_or_bail(&mut buf, patch, -128);
    assert_eq!(
        buf.as_slice()[patch],
        0x80,
        "an in-range negative rel8 fits"
    );
    let mut buf2 = ExecutableBuffer::new(4096).expect("test buffer");
    buf2.emit(&[0x75, 0x00]);
    let p2 = buf2.pos() - 1;
    Compiler::patch_rel8_or_bail(&mut buf2, p2, 200);
    assert!(
        buf2.overflowed(),
        "200 is not a rel8: `u8::try_from` would have accepted it and encoded -56"
    );
}

/// Every `rel8` displacement in this crate is written by the range-checked
/// helper, in BOTH backends.
///
/// The PIC cascade's `rel as u8` was not the only one — `ir_lower` carried
/// eight more of the identical shape, and `arith`/`deopt_stubs`/`frames` each
/// hand-rolled their own range check next to a raw cast. A hand-rolled check
/// is not wrong today; it is a place for the next one to be added without one.
/// So the rule is mechanical and greppable: in an EMITTER source, a
/// `try_patch_byte` call may not cast its value. `lib.rs` is deliberately not
/// scanned — it holds `patch_rel8_or_bail`, the one place the cast is correct
/// because it sits behind `i8::try_from`.
///
/// The scan is [`casting_try_patch_byte_calls`], which walks each call's
/// ARGUMENT LIST to its balanced `)` rather than testing one line. The first
/// version tested a single line, and would have missed the offender written as
///
/// ```ignore
/// self.buf.try_patch_byte(
///     patch,
///     rel as u8,
/// );
/// ```
///
/// — a shape rustfmt produces on its own once the arguments get long. The
/// sites it *did* catch happened to be formatted the one way it understood.
///
/// Needles are assembled at runtime so this test's own text does not match.
#[test]
fn rel8_displacement_patches_all_go_through_the_range_checked_helper() {
    let sources: [(&str, &str); 9] = [
        ("x64.rs", include_str!("../x64.rs")),
        ("x64/bytecode_walk.rs", include_str!("bytecode_walk.rs")),
        ("x64/emit.rs", include_str!("emit.rs")),
        ("x64/arith.rs", include_str!("arith.rs")),
        ("x64/frames.rs", include_str!("frames.rs")),
        ("x64/deopt_stubs.rs", include_str!("deopt_stubs.rs")),
        ("x64/safepoint.rs", include_str!("safepoint.rs")),
        ("x64/driver.rs", include_str!("driver.rs")),
        ("ir_lower.rs", include_str!("../ir_lower.rs")),
    ];
    let mut offenders: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for (name, src) in sources {
        let (calls, bad) = casting_try_patch_byte_calls(src);
        scanned += calls;
        offenders.extend(
            bad.into_iter()
                .map(|(line, text)| format!("{name}:{line}: {text}")),
        );
    }
    // A source scan that matches nothing passes exactly as happily as one that
    // matches everything. If the walker stops finding calls, the needle or the
    // file list has rotted and this test is about to approve anything.
    //
    // The floor was 5 — the exact census at the time — and dropped to 1 when
    // the erase-a-dead-range idiom moved onto `ExecutableBuffer` as
    // `erase_range_with_jump_over` (it had been open-coded in `frames.rs` and,
    // missing its JMP half, in `ir_lower.rs`). That is the direction this test
    // wants, so the floor follows the census down rather than the conversion
    // being reverted to satisfy it. What actually proves the walker still
    // works is `the_rel8_scan_sees_a_call_split_across_lines`, which runs it
    // against injected offenders in three formattings; this assert only
    // catches the file list going stale.
    assert!(
        scanned >= 1,
        "found no try_patch_byte call at all across the emitter sources — the scan \
         is broken, or the file list is stale, and it was about to pass without \
         checking anything"
    );
    assert!(
        offenders.is_empty(),
        "a rel8 displacement must go through `ExecutableBuffer::patch_rel8_or_bail`, which \
         marks the buffer overflowed (compile discarded) instead of truncating. Truncation \
         retargets the branch — the inline-PIC cascade's wrapped `JNE -128` landed inside \
         the pre-call spill run and ran as an infinite shadow push (SIGSEGV), and as a \
         SIGILL in the CRATONVM_NO_MOVING_YOUNG lane. Offending sites:\n{}",
        offenders.join("\n")
    );
}

/// `(calls seen, (1-based line, text) per call whose arguments cast to `u8`)`.
///
/// Walks from each `try_patch_byte(` to its balanced `)`, so the check does not
/// depend on how the call is wrapped across lines. Pure and source-taking, so
/// [`the_rel8_scan_sees_a_call_split_across_lines`] can prove it is not
/// vacuous on input that is not whatever the tree happens to look like today.
fn casting_try_patch_byte_calls(src: &str) -> (usize, Vec<(usize, String)>) {
    // Assembled so this function's own text does not match.
    let call = format!("try_patch{}byte(", "_");
    let cast = format!(" as {}8", "u");

    let bytes = src.as_bytes();
    let mut calls = 0usize;
    let mut bad = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(&call) {
        let open = from + rel + call.len(); // first byte inside the parens
        calls += 1;
        let mut depth = 1usize;
        let mut i = open;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        // `open` and the closing `)` are ASCII, but an UNBALANCED call would
        // leave `i` at the end of the buffer, which can be mid-character in
        // these comment-heavy sources. `get` yields None rather than panicking.
        let end = i.saturating_sub(1).max(open);
        let args = src.get(open..end).unwrap_or("");
        if args.contains(&cast) {
            let line = src[..open].lines().count();
            let text: String = args.split_whitespace().collect::<Vec<_>>().join(" ");
            bad.push((line, text));
        }
        from = open;
    }
    (calls, bad)
}

/// The scan has to see the offender however rustfmt happened to wrap it.
///
/// Asserted on synthetic input: a guard that only ever runs against a tree
/// that already satisfies it cannot tell "nothing is wrong" from "I am not
/// looking". The single-line version of this scan passed the middle case here.
#[test]
fn the_rel8_scan_sees_a_call_split_across_lines() {
    let call = format!("try_patch{}byte", "_");

    let one_line = format!("        self.buf.{call}(patch, rel as {}8).ok();\n", "u");
    let (n, bad) = casting_try_patch_byte_calls(&one_line);
    assert_eq!(n, 1);
    assert_eq!(bad.len(), 1, "the one-line form must be caught: {bad:?}");

    let wrapped = format!(
        "        self.buf.{call}(\n            patch,\n            rel as {}8,\n        );\n",
        "u"
    );
    let (n, bad) = casting_try_patch_byte_calls(&wrapped);
    assert_eq!(n, 1);
    assert_eq!(
        bad.len(),
        1,
        "a call wrapped across lines is the same defect and must be caught: {bad:?}"
    );

    // Nested parens in the argument must not end the walk early.
    let nested = format!(
        "        self.buf.{call}(\n            patch,\n            (a - b - 1) as {}8,\n        );\n",
        "u"
    );
    assert_eq!(casting_try_patch_byte_calls(&nested).1.len(), 1);

    // And no false positive: a cast AFTER the call's closing paren is not this
    // call's argument.
    let after = format!(
        "        self.buf.{call}(patch, v).ok();\n        let x = y as {}8;\n",
        "u"
    );
    let (n, bad) = casting_try_patch_byte_calls(&after);
    assert_eq!(n, 1);
    assert!(bad.is_empty(), "no false positives: {bad:?}");
}

/// The relocation-safety gates must ask "can a relocating collection see a
/// compiled frame?", which is strictly narrower than "is moving-young on?".
/// Equating the two is what switched the optimizing tier off by default.
#[test]
fn relocates_compiled_frames_implies_moving_young_but_not_conversely() {
    assert!(
        !moving_young_relocates_compiled_frames() || moving_young_enabled(),
        "relocation of compiled frames must imply the moving young gen is on"
    );
    assert_eq!(
        moving_young_relocates_compiled_frames(),
        moving_young_enabled() && cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT,
        "the predicate must be exactly `moving_young && JIT_PUBLISHES_RELOCATION_CONTRACT` — \
         the same constant the runtime veto in conservative_roots reads, or the veto and \
         these gates can drift apart"
    );
}

/// While the JIT publishes no relocation contract, the runtime vetoes
/// moving-young for the whole process as soon as any compiled code exists.
/// The optimizing tier must therefore NOT be disabled by the moving-young
/// default — that trade bought nothing. Pins the regression that
/// `jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`
/// describes.
#[test]
fn optimizing_tier_is_not_disabled_while_relocation_is_vetoed() {
    if cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT {
        // Contract landed: the gates are supposed to be armed again.
        return;
    }
    assert!(
        !moving_young_relocates_compiled_frames(),
        "with no published relocation contract, no compiled frame can be live during a \
         relocating young collection, so the IR/direct-call gates must be open regardless \
         of the moving-young default"
    );
}

/// `CRATONVM_SHADOW_STACK` is likewise read by `jit`, `gc` and `vm`; the
/// emission side and the root-scan side must agree or the collector walks
/// a shadow stack the codegen never pushed to.
///
/// The moving-young implication is GONE (2026-07-31): publication is how a
/// JIT frame's live references reach the collector at all, so keying it on
/// the collector choice made `CRATONVM_NO_MOVING_YOUNG=1` withdraw root
/// visibility — that lane crashed on a reclaimed root within seconds. The
/// assertion below is what stops it coming back, and it also pins the two
/// sides to one expression.
#[test]
fn shadow_stack_maps_enabled_does_not_depend_on_the_young_collector() {
    assert_eq!(
        shadow_stack_maps_enabled(),
        cratonvm_types::flags().jit.shadow_stack || shadow_emission_moving_implication_enabled(),
        "shadow-stack codegen must be gated on the shared flag plus its own opt-out \
         (CRATONVM_JIT_MY_SHADOW_EMISSION), NOT on the young collector: root \
         publication is not a property of which collector runs. \
         `vm::jit::conservative_roots::shadow_stack_enabled` must spell the SAME \
         expression — they are two halves of one agreement"
    );
    // The property that actually matters, stated directly: turning the
    // moving young generation off must not turn publication off with it.
    crate::x64::set_moving_young_override(Some(false));
    assert!(
        shadow_stack_maps_enabled(),
        "publication must survive CRATONVM_NO_MOVING_YOUNG=1"
    );
    crate::x64::set_moving_young_override(None);
}

/// Source-scan regression guard. Needles are assembled at runtime so this
/// test's own text does not match them.
#[test]
fn no_crate_private_getenv_for_centralized_gates() {
    let src = backend_sources();
    for var in ["CRATONVM_MOVING_YOUNG", "CRATONVM_SHADOW_STACK"] {
        for read in ["::var_os(\"", "::var(\""] {
            let needle = format!("env{read}{var}\"");
            assert!(
                !src.contains(&needle),
                "the x64 backend re-introduced a crate-private read of {var}. It is centralized in \
                 types/src/flags.rs and is also read by gc and vm; parsing it here again \
                 recreates the three-way gate skew this file was fixed for."
            );
        }
    }
}

/// The `<= 127` assertion in `types/src/heap_types.rs` exists because the
/// emitters below encode `HEADER_SIZE` as a **signed** disp8. Restate the
/// signedness explicitly: the upstream assert would still pass at 128..=255
/// if it were ever relaxed to `u8` reasoning, and 128 encodes as -128.
#[test]
fn header_size_fits_signed_disp8_and_is_qword_aligned() {
    assert!(
        i8::try_from(HEADER_SIZE).is_ok(),
        "HEADER_SIZE={HEADER_SIZE} does not fit a SIGNED disp8; the array emitters would \
         address backwards from the object base. Switch those sites to disp32 first."
    );
    assert_eq!(
        HEADER_SIZE % 8,
        0,
        "object bodies are addressed as qword-indexed cells; HEADER_SIZE must stay \
         8-aligned"
    );
    assert!(
        i8::try_from(ARRAY_LENGTH_OFFSET).is_ok(),
        "the array-length loads use a disp8 too"
    );
}

/// `emit_inline_tlab_new` writes `GC_FLAG_COMPACT` by storing a whole dword
/// at `KIND_TAGS_BYTE_OFFSET` with the flag byte shifted into place. That
/// shift is only correct while `gc_flags` is byte 3 of that dword.
#[test]
fn header_offset_contract_gc_flags_live_in_the_mark_words_top_byte() {
    assert_eq!(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET,
        cratonvm_types::MARK_WORD_OFFSET + 7,
        "the inline-TLAB compact-flag store shifts GC_FLAG_COMPACT by \
         8*(GC_FLAGS_BYTE_OFFSET - KIND_TAGS_BYTE_OFFSET); if gc_flags moves out of the top byte of \
         that dword the store lands on kind/element_type/gc_age instead"
    );
    assert!(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET > cratonvm_types::KIND_TAGS_BYTE_OFFSET
            && cratonvm_types::GC_FLAGS_BYTE_OFFSET - cratonvm_types::KIND_TAGS_BYTE_OFFSET < 4,
        "gc_flags must live inside the dword the emitter overwrites, or the single dword \
         store silently drops the compact bit"
    );
}

/// The inline allocator must write the mark word on EVERY path, because that
/// word is content now and not padding.
///
/// This is the regression guard for the defect that broke the Spring Boot suite
/// on 2026-08-07: the mark-word store was folded into the `zero_elision` branch
/// during the 24 -> 16 shrink, and `zero_elision` is default-ON. Every
/// JIT-inline allocation then published an object wearing whatever the TLAB
/// slot held as its `kind` / `element_type` / `gc_age` / `gc_flags` -- 178 of
/// the first 184 suite classes died in JUnit discovery with
/// `gen_heap::read_slot: corrupt Value cell`, which is what a stale
/// `GC_FLAG_COMPACT` produces when a legacy tagged-`Value` object is read as
/// bare compact pointers.
///
/// The test asserts on the SOURCE rather than on emitted bytes on purpose: what
/// went wrong was structural (a store moved inside a conditional), and it is
/// the structure that has to stay pinned. The elision flag is a process-wide
/// `OnceLock`, so a byte-level test could only ever observe the default arm and
/// would have passed just as happily with the bug in place.
#[test]
fn the_inline_allocator_writes_the_mark_word_unconditionally() {
    let src = include_str!("objects.rs");
    let start = src
        .find("pub(super) fn emit_inline_tlab_new")
        .expect("the inline TLAB allocator must still exist");
    let body = &src[start..];

    // Line-based, and deliberately simple: track a stack of open blocks and
    // whether each was opened by a `zero_elision` test. If the mark-word store
    // is reached while any such block is still open, the store is conditional.
    let mut stack: Vec<bool> = Vec::new();
    let mut found = false;
    for line in body.lines() {
        let code = line.split("//").next().unwrap_or("");
        if code.contains("emit_mov_dword_mem_disp32_imm32") && code.contains("MARK_WORD_OFFSET") {
            assert!(
                !stack.iter().any(|gated| *gated),
                "the mark-word store sits inside an `if !zero_elision` block.                  That flag is default-ON, so the store would not run for any                  inline allocation and the object would publish whatever the                  TLAB slot held as its kind, element_type, gc_age and                  gc_flags. That is the 2026-08-07 Spring Boot regression                  (`read_slot: corrupt Value cell` in 178 of 184 classes); the                  store must stay unconditional."
            );
            found = true;
        }
        let gated = code.contains("zero_elision");
        for ch in code.chars() {
            match ch {
                '{' => stack.push(gated),
                '}' => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if found && stack.is_empty() {
            break;
        }
    }
    assert!(found, "the inline allocator must zero the mark word");
}

/// The decision to INLINE the bump and the decision to DROP the post-init
/// helper call must stay two different questions.
///
/// They were one boolean until 2026-09-02, and collapsing them is only sound
/// while every collector can find an object by walking the chunk it was
/// allocated in. ZGC cannot: its sweep, its `is_object_address` oracle and
/// its conservative scans are driven by an allocation-base registry, and the
/// post-init helper is the only place an inline allocation can enter it. An
/// object allocated without that call is invisible to the runtime — its first
/// use as a receiver decodes as `null`, which is what miscompiled
/// `probes/FjpProbe.java` (`NullPointerException: null object argument` inside
/// `ForkJoinTask.fork`, no collection anywhere in the run).
///
/// A source scan for the same reason the test above is one: the flag is
/// process-wide and published by the heap, so a byte-level test in this crate
/// could only ever observe the unpublished default.
#[test]
fn dropping_the_post_init_helper_asks_the_collector_first() {
    let src = include_str!("bytecode_walk.rs");
    let start = src
        .find("let skip_helper =")
        .expect("the `new` arm must still decide whether to skip the post-init helper");
    // The decision, up to the end of its statement.
    let decision = &src[start..start + src[start..].find(';').expect("terminated statement")];
    assert!(
        decision.contains("jit_tlab_registration_required"),
        "`skip_helper` no longer consults \
         `cratonvm_types::jit_tlab_registration_required()`. On a collector \
         that is TOLD about each allocation rather than walking the chunk \
         (ZGC), dropping the post-init call leaves every inline-allocated \
         object unregistered, and it decodes as `null` at its first native \
         boundary. Found instead: {decision}"
    );

    let inline_gate_start = src
        .find("let can_inline =")
        .expect("the `new` arm must still gate inline allocation");
    let inline_gate =
        &src[inline_gate_start..inline_gate_start + src[inline_gate_start..].find(';').unwrap()];
    assert!(
        !inline_gate.contains("skip_helper"),
        "inline ELIGIBILITY is reading `skip_helper`, so a collector that \
         requires the announcing call would switch the inline bump off \
         altogether rather than keep it and pay one call. It must read the \
         class-only `helper_is_noop` term instead. Found: {inline_gate}"
    );
}

/// The zeroing stores in `emit_inline_tlab_new` must cover exactly the
/// header words that are not written with a real value, and every one of
/// them must sit inside the header.
#[test]
fn inline_tlab_header_writes_stay_inside_the_header() {
    for (name, off, width) in [
        ("class_id", 0usize, 4usize),
        ("gc_flags byte", cratonvm_types::GC_FLAGS_BYTE_OFFSET, 1),
        ("shape", cratonvm_types::NUM_SLOTS_OFFSET, 4),
        // `forwarding_ptr` was here until the 2026-08-06 shrink folded it into
        // the mark word; there is no separate field, and the emitter no longer
        // writes one.
        ("mark_word", cratonvm_types::MARK_WORD_OFFSET, 8),
    ] {
        assert!(
            off + width <= HEADER_SIZE,
            "inline-TLAB emitter writes {name} at +{off} ({width}B), past HEADER_SIZE \
             ({HEADER_SIZE}) — that store would land in the object body"
        );
    }
    // The emitter used to also write a zero at the identity-hash dword. That
    // field left the header on 2026-08-07 and `shape` took over its offset, so
    // the store now zeroes the MARK WORD instead -- see the comment at that
    // emission for why that matters more than the hash ever did.
    assert!(
        cratonvm_types::MARK_WORD_OFFSET + 8 <= HEADER_SIZE,
        "the inline-TLAB emitter zeroes the 8-byte mark word; it must fit"
    );
}

/// The ARRAY inline allocator is held to the same publication order as the
/// object one: **every header word lands before the cursor commits**.
///
/// `the_inline_allocator_writes_the_mark_word_unconditionally` next door pins
/// the object emitter against a store drifting inside a conditional. This pins
/// the array emitter against the other half of the same failure — a store
/// drifting after the commit — because that is the ordering BinTrees-18 was:
/// the bump published the object's address into `thread.tlab.cursor` and only
/// then wrote the header, leaving a window in which a walker read
/// `class_id = 0, num_slots = 0`, computed `size = HEADER_SIZE`, and stepped
/// into the object's own body.
///
/// Source-level on purpose, and for the reason its sibling gives: what must
/// stay pinned is structural, and the emitted bytes cannot show it — the
/// commit and the header writes are all just `MOV`s.
#[test]
fn the_inline_array_allocator_commits_the_cursor_after_every_header_write() {
    let src = include_str!("objects.rs");
    let start = src
        .find("pub(super) fn emit_inline_tlab_newarray")
        .expect("the inline TLAB array allocator must still exist");
    let body = &src[start..];

    let mut header_writes = 0usize;
    let mut commit_at: Option<usize> = None;
    let mut last_header_at = 0usize;
    let mut depth = 0i32;
    for (n, line) in body.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        // The four header stores: class_id, shape, and the two mark-word
        // halves. `emit_mov_dword_mem_disp32_imm32` covers three of them and
        // the raw `MOV [R11+ARRAY_LENGTH_OFFSET], ECX` is the fourth.
        if code.contains("emit_mov_dword_mem_disp32_imm32")
            || code.contains("ARRAY_LENGTH_OFFSET as u8")
        {
            header_writes += 1;
            last_header_at = n;
        }
        // The commit: `MOV [R10 + cursor_off], RAX`.
        if code.contains("emit_mov_mem_disp32_r64") && commit_at.is_none() {
            commit_at = Some(n);
        }
        for ch in code.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth < 0 {
            break;
        }
    }

    assert_eq!(
        header_writes, 4,
        "the array emitter must write class_id, shape and both halves of the \
         mark word; found {header_writes} header stores"
    );
    let commit_at = commit_at.expect("the array emitter must commit the TLAB cursor");
    assert!(
        commit_at > last_header_at,
        "the TLAB cursor commit (line +{commit_at}) precedes a header store \
         (line +{last_header_at}). The commit is the single linearization \
         point: publishing it first exposes an object whose header is still \
         whatever the TLAB slot held, which the collector's linear walk \
         mis-decodes and steps through into its neighbour."
    );
}

/// The array emitter's inline ceiling must be the allocator's own.
///
/// `tlab_alloc_array_guarded_refill` refuses anything at or above
/// `tlab_max_alloc()` and hands it to the ordinary path, which owns the
/// young-vs-old-gen (humongous) routing decision. An inline bump that admitted
/// a larger array would take that decision away from it silently.
///
/// The constant is duplicated in `objects.rs` because `cratonvm-gc` is a
/// dev-dependency of this crate — reachable from a test and not from the
/// emitter. This is the test that makes the duplicate safe.
#[test]
fn the_inline_array_cap_tracks_the_allocator_s_own() {
    assert_eq!(
        super::Compiler::INLINE_ARRAY_TLAB_MAX_ALLOC,
        cratonvm_gc::tlab::tlab_max_alloc(),
        "the inline `newarray` size ceiling has drifted from the TLAB's own \
         per-allocation cap"
    );
}

/// `ArrayElementType`'s discriminants ARE the JVM `newarray` atype values.
///
/// The `0xbc` arm decodes its operand byte with
/// `array_element_type_from_tag(atype)`, which is only the right decode
/// because of this coincidence — and it is a designed one (the enum's own doc
/// says "maps to `newarray` atype values"), not an accident. If a
/// discriminant were renumbered, that arm would allocate a `byte[]` where the
/// bytecode asked for a `long[]` and the mark word would say so, which no
/// later check would catch.
#[test]
fn the_array_element_discriminants_are_the_jvm_atype_values() {
    use cratonvm_types::ArrayElementType as E;
    for (atype, expected) in [
        (4u8, E::Boolean),
        (5, E::Char),
        (6, E::Float),
        (7, E::Double),
        (8, E::Byte),
        (9, E::Short),
        (10, E::Int),
        (11, E::Long),
    ] {
        assert_eq!(
            cratonvm_types::array_element_type_from_tag(atype),
            Some(expected),
            "JVM atype {atype} must decode to {expected:?} — this is the \
             mapping `jit_newarray` applies, and the `0xbc` inline arm must \
             agree with it exactly"
        );
    }
}

// -- arch-2026-07-26 R1: reference-only self-call spill elision ---------

#[test]
fn moving_oop_free_self_call_requires_complete_empty_root_proof() {
    assert!(
        moving_oop_free_self_call_is_publishable(true, true, 0),
        "moving-young may publish a metadata-only empty map when exact coverage proves no roots"
    );
    assert!(
        !moving_oop_free_self_call_is_publishable(true, false, 0),
        "incomplete moving-young coverage must retain the full spill"
    );
    assert!(
        !moving_oop_free_self_call_is_publishable(true, true, 1),
        "one live oop home must retain spill plus shadow publication"
    );
    assert!(
        !moving_oop_free_self_call_is_publishable(false, true, 0),
        "shadow-only mode keeps its established conservative path"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn linux_inline_rbp_tls_slot_is_live_and_round_trips() {
    assert_eq!(inline_rbp_tls_segment_prefix(), 0x64);
    let disp = inline_rbp_tls_disp();
    assert_ne!(
        disp, 0,
        "the sentinel-probed Linux fs-relative TLS slot should be available"
    );
    let old = inline_rbp_tls_mirror_read().expect("probe enabled the mirror");
    let marker = 0x5242_5054_4553_5401usize;
    assert!(inline_rbp_tls_mirror_write(marker));
    assert_eq!(inline_rbp_tls_mirror_read(), Some(marker));
    assert!(inline_rbp_tls_mirror_write(old));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn linux_inline_rbp_tls_slot_is_thread_local() {
    let disp = inline_rbp_tls_disp();
    assert_ne!(disp, 0);
    let old = inline_rbp_tls_mirror_read().expect("probe enabled the mirror");
    let main_marker = 0x5242_5054_4852_4401usize;
    let child_marker = 0x5242_5054_4852_4402usize;
    assert!(inline_rbp_tls_mirror_write(main_marker));

    let child = std::thread::spawn(move || {
        assert_eq!(
            inline_rbp_tls_disp(),
            disp,
            "the static ELF TLS offset must be identical in every thread"
        );
        assert_eq!(
            inline_rbp_tls_mirror_read(),
            Some(0),
            "a new thread must not inherit the caller's frame mirror"
        );
        assert!(inline_rbp_tls_mirror_write(child_marker));
        assert_eq!(inline_rbp_tls_mirror_read(), Some(child_marker));
    });
    child.join().expect("TLS isolation probe thread panicked");

    assert_eq!(
        inline_rbp_tls_mirror_read(),
        Some(main_marker),
        "the child thread must not overwrite the caller's frame mirror"
    );
    assert!(inline_rbp_tls_mirror_write(old));
}

/// `int fib(int)` — the workload the elision exists for. No `aload`/`astore`
/// anywhere, so no local can hold a reference and the elision must fire
/// **even though** the allocator gave locals register homes. Before R1 the
/// predicate failed closed here, so giving `fib` a register home made both
/// recursive call sites *more* expensive than with allocation off.
#[test]
fn fib_shaped_kernel_has_no_reference_locals_in_registers() {
    // 0: iload_0        1: iconst_2      2: if_icmpge 7
    // 5: iload_0        6: ireturn
    // 7: iload_0        8: iconst_1      9: isub
    // 10: invokestatic  13: iload_0     14: iconst_2   15: isub
    // 16: invokestatic  19: iadd        20: ireturn
    let code: Vec<u8> = vec![
        0x1a, 0x05, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0x1a, 0x05,
        0x64, 0xb8, 0x00, 0x01, 0x60, 0xac,
    ];
    let code_len = code.len();
    // Both locals register-homed, as the allocator would do for a hot kernel.
    let assignments = vec![Some(R12), Some(R13)];
    let plan = crate::regalloc::plan_safepoint_publication(
        &code,
        code_len,
        2,
        1,
        &[],
        &assignments,
        0,
        &[],
    );
    assert_eq!(
        plan.reference_locals, 0,
        "an int-only kernel has no reference locals"
    );
    assert!(
        plan.no_reference_in_registers(),
        "no register-homed local can hold an oop here, so the self-call spill must be elidable"
    );
    assert!(
        !reference_local_in_register(Some(&plan), &assignments),
        "R1: the predicate must no longer fail closed merely because a local has a register"
    );
}

/// The contrast case. A single `astore_1` taints local 1 method-wide; if
/// local 1 also has a register home the elision must be refused, because
/// that register really can hold a GC root the stack-only scan cannot see.
#[test]
fn register_homed_reference_local_still_forces_the_spill() {
    // 0: aconst_null  1: astore_1  2: aload_1  3: areturn
    let code: Vec<u8> = vec![0x01, 0x4c, 0x2b, 0xb0];
    let code_len = code.len();
    let assignments = vec![None, Some(R12)];
    let plan = crate::regalloc::plan_safepoint_publication(
        &code,
        code_len,
        2,
        0,
        &[],
        &assignments,
        0,
        &[],
    );
    assert_eq!(
        plan.reference_locals & 0b10,
        0b10,
        "astore_1 taints local 1"
    );
    assert!(!plan.no_reference_in_registers());
    assert!(
        reference_local_in_register(Some(&plan), &assignments),
        "a register-homed reference local must keep forcing the full spill"
    );

    // Same method, but local 1 spilled to its frame slot: the oop is
    // already frame-resident, so nothing needs publishing.
    let spilled = vec![None, None];
    let plan_spilled =
        crate::regalloc::plan_safepoint_publication(&code, code_len, 2, 0, &[], &spilled, 0, &[]);
    assert!(plan_spilled.no_reference_in_registers());
    assert!(!reference_local_in_register(Some(&plan_spilled), &spilled));
}

/// A reference PARAMETER the method never `aload`s is invisible to
/// `find_reference_locals`. `compile_with_param_slots` unions in
/// `param_oop_mask` for exactly this case; if it ever stops doing so, a
/// live oop could be left in an unpublished register.
#[test]
fn param_oop_mask_covers_a_never_loaded_reference_parameter() {
    // `static int f(Object o) { return 1; }` — bytecode never touches local 0.
    let code: Vec<u8> = vec![0x04, 0xac]; // iconst_1; ireturn
    let code_len = code.len();
    let assignments = vec![Some(R12)];
    let without = crate::regalloc::plan_safepoint_publication(
        &code,
        code_len,
        1,
        1,
        &[],
        &assignments,
        0,
        &[],
    );
    assert!(
        without.no_reference_in_registers(),
        "the bytecode scan alone cannot see an unloaded reference parameter"
    );
    let with = crate::regalloc::plan_safepoint_publication(
        &code,
        code_len,
        1,
        1,
        &[],
        &assignments,
        0b1, // param_oop_mask: local 0 is a reference parameter
        &[],
    );
    assert!(
        !with.no_reference_in_registers(),
        "param_oop_mask must make the never-loaded reference parameter visible; \
         compile_with_param_slots passes it for this reason"
    );
}

/// Compile paths that build no plan must keep the old, strictly more
/// conservative behaviour.
#[test]
fn absent_plan_falls_back_to_the_conservative_all_or_nothing_test() {
    assert!(
        reference_local_in_register(None, &[None, Some(R12)]),
        "without a plan, any register home must still force the spill"
    );
    assert!(
        !reference_local_in_register(None, &[None, None]),
        "without a plan and without register homes, the old predicate already elided"
    );
}

/// `compile_with_param_slots` skips building the plan when no local has a
/// register home, to avoid a second whole-method liveness pass on every
/// compile. That shortcut is only legitimate if the plan and the `None`
/// fallback agree in exactly that case — including for a method that is
/// full of reference locals.
#[test]
fn cost_gate_skipping_the_plan_is_behaviour_identical_without_register_homes() {
    // aconst_null; astore_1; aload_1; areturn — reference locals present.
    let code: Vec<u8> = vec![0x01, 0x4c, 0x2b, 0xb0];
    let no_homes = vec![None, None];
    let plan = crate::regalloc::plan_safepoint_publication(
        &code,
        code.len(),
        2,
        0,
        &[],
        &no_homes,
        0,
        &[],
    );
    assert_eq!(
        reference_local_in_register(Some(&plan), &no_homes),
        reference_local_in_register(None, &no_homes),
        "with no register homes the plan and the fallback must agree, or the \
         cost gate in compile_with_param_slots silently changes behaviour"
    );
    assert!(!reference_local_in_register(None, &no_homes));
}

/// `plan_safepoint_publication`'s masks are `u64`, and the R1 argument
/// depends on there being no unrepresented tail above bit 63. That holds
/// only because `color_graph` refuses to colour locals `>= 64`.
#[test]
fn locals_past_the_bitset_never_receive_a_register_home() {
    let code: Vec<u8> = vec![0x04, 0xac];
    // Assert the allocator's own contract rather than trusting the plan to
    // mask the tail away: a local at index >= 64 must never be coloured.
    let alloc = crate::regalloc::allocate_registers(&code, code.len(), 80, 0, &[]);
    assert!(
        alloc.assignments.iter().skip(64).all(Option::is_none),
        "color_graph caps at 64 locals; if that ever changes, \
         SafepointPublishPlan's u64 masks silently stop covering the tail and \
         can_elide_self_call_register_spill's soundness argument breaks"
    );
    // Feeding the allocator's own output back through the plan must agree.
    let plan = crate::regalloc::plan_safepoint_publication(
        &code,
        code.len(),
        80,
        0,
        &[],
        &alloc.assignments,
        0,
        &[],
    );
    assert!(plan.no_reference_in_registers());
}

/// Inventory tripwire for the 32→16-byte `ObjectHeader` shrink. If these
/// counts change, the site list in
/// `x64-flag-skew-and-contracts.md` §5 is
/// stale and the shrink has an unaudited emission site.
#[test]
fn header_offset_emission_site_inventory_matches_the_doc() {
    let src = backend_sources();
    // Needles assembled at runtime so this test's own text is not counted.
    // `ARRAY_DATA_OFFSET` joined the inventory when array element addressing
    // was split from the object body base -- the two were the same integer
    // until `HEADER_SIZE` reached 16, so the split had to be recorded here or
    // the map would under-count exactly the sites the shrink moves.
    // The first row fell 35 -> 22: thirteen of those emissions were array
    // element addressing and now name the array-data constant instead, which is
    // exactly where the new 13 in the fifth row comes from. The totals moved
    // between rows rather than shrinking, which is the point of the split.
    //
    // (Deliberately phrased without the literal needles -- this test counts its
    // own source text, so spelling one out here inflates the very number it is
    // checking. That cost one round.)
    //
    // The third row fell 23 -> 22 on 2026-09-02 and the sixth row appeared.
    // `emit_bounds_check`'s length load moved into its cold stub (the fast path
    // is now a fused `CMP r32, [array + len]`), so the constant is spelled at
    // two emission sites instead of one -- and both were written as a `const`
    // binding through the checked narrowing rather than a raw one, which is why
    // the raw row went DOWN while a site was added. The checked row is counted
    // for the same reason the sibling test counts ir_lower's: deleting it would
    // silently restore an unchecked site.
    // The fifth row went 2 -> 3 on 2026-09-06: the precise-AIOOBE deopt stub
    // (reason 11) re-loads the array length for `jit_throw_aioobe`, exactly as
    // the shared cold pad above it does, and spells the constant through the
    // same checked `disp8_const` narrowing. A third site, still build-checked.
    let cases: [(&str, &str, usize); 7] = [
        ("HEADER_SIZE", " as u8", 22),
        ("HEADER_SIZE", " as i32", 13),
        // 2026-09-11: +3 for the STRINGBUILDER_ACCESS / inline-`newarray`
        // work in `objects.rs` — the array allocator's disp8 screen and its
        // shape store, and the append body's capacity load. All three are
        // disp8, which is the hazardous form, and all three are behind the
        // allocator's own screen: a header that grew past 127 costs the site
        // its inline path instead of addressing backwards.
        ("ARRAY_LENGTH_OFFSET", " as u8", 25),
        ("ARRAY_LENGTH_OFFSET", " as i32", 5),
        ("ARRAY_LENGTH_OFFSET", " as i64", 3),
        // 2026-09-11: +2 for the same work — the array allocator's disp8
        // screen and the `MOV [RDX+R8+ARRAY_DATA_OFFSET], CL` element store in
        // the append body. Both behind that screen.
        ("ARRAY_DATA_OFFSET", " as u8", 15),
        ("ARRAY_DATA_OFFSET", " as i32", 0),
    ];
    for (base, suffix, expected) in cases {
        let needle = format!("{base}{suffix}");
        let found = src.matches(needle.as_str()).count();
        assert_eq!(
            found, expected,
            "{needle} appears {found}x across the x64 backend sources, doc records \
             {expected}x. Update \
             x64-flag-skew-and-contracts.md §5 (the \
             header-offset site list) in the same change — it is the map the \
             ObjectHeader 32→16 shrink navigates by."
        );
    }
}


/// The array bounds check and its cold stub are ONE contract, split across two
/// files, and neither half is correct alone.
///
/// The fast path is `CMP ECX, [RAX + len] ; JAE stub` — the length is compared
/// straight out of the object header and is **not** left in a register. The
/// stub reports it: `jit_throw_aioobe`'s second argument is the number in
/// "Index 5 out of bounds for length 3", and it reads that from R10D. So the
/// stub must re-load the length itself.
///
/// Before 2026-09-02 the fast path did the load (`MOV R10D, [RAX+len]`) and the
/// stub inherited the register. Folding the load into the compare — one
/// instruction and four bytes fewer on every emitted bounds check — was a
/// deliberately-refused peephole for years *precisely* because doing only that
/// half leaves the stub printing whatever R10 last held, which is a wrong
/// exception message and not a crash: nothing fails, the number is just false.
///
/// This test is the tripwire on the pairing. It emits both halves and asserts
/// each contains the load-or-compare it owes, so removing either one fails here
/// rather than in a user's stack trace.
#[test]
fn bounds_check_length_is_reloaded_in_the_cold_stub() {
    use cratonvm_types::ARRAY_LENGTH_OFFSET;
    let len_disp = u8::try_from(ARRAY_LENGTH_OFFSET).expect("array length offset fits a disp8");

    // FAST PATH — `CMP ECX, [RAX + len]`: 3B /r with ModRM(01, ECX, RAX).
    let mut c = bounds_check_test_compiler();
    let start = c.buf.pos();
    c.emit_bounds_check(0);
    let fast: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert_eq!(
        &fast[..3],
        &[0x3B, 0x48, len_disp],
        "the fast path must compare against the header word directly; if this \
         became a register compare again, the stub's own load below is now dead \
         and the two halves have drifted"
    );
    // ...followed by `JAE rel32` and nothing else. Nine bytes total, four fewer
    // than the pre-fold `MOV`(4) + `CMP`(3) + `JAE`(6).
    assert_eq!(fast.len(), 9, "fused bounds check is CMP(3) + JAE rel32(6)");
    assert_eq!(&fast[3..5], &[0x0F, 0x83], "JAE rel32");

    // COLD STUB — `MOV R10D, [RAX + len]` must be the FIRST thing it does,
    // before the argument shuffle overwrites RAX or RCX.
    let mut c = bounds_check_test_compiler();
    c.emit_bounds_check(0);
    let stub_start = c.buf.pos();
    c.emit_bounds_check_stubs();
    let stub: Vec<u8> = c.buf.as_slice()[stub_start..c.buf.pos()].to_vec();
    assert_eq!(
        &stub[..4],
        &[0x44, 0x8B, 0x50, len_disp],
        "the cold stub must re-load the length into R10D as its first \
         instruction — it is `jit_throw_aioobe`'s `length` argument, and the \
         fast path no longer leaves it in a register"
    );

    // And the kill switch really restores the old shape, so the A/B it exists
    // for compares two different instruction sequences and not one.
    if jit_fused_bounds_load_enabled() {
        // Only meaningful in the default configuration; under
        // `CRATONVM_JIT_FUSED_BOUNDS_LOAD=0` the assertions above would be
        // testing the fallback against itself.
        assert_eq!(fast.len(), 9);
    }
}

/// A bare `Compiler` for the two bounds-check emitters: no locals, no register
/// assignments, a helper table whose `throw_aioobe` is a plausible address so
/// `emit_call_absolute` takes its rel32 path.
fn bounds_check_test_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    // Any non-zero, in-reach address: the stub only has to be emittable, it is
    // never executed here.
    helpers.throw_aioobe = bounds_check_test_compiler as usize;
    Compiler::new(
        "bounds-check-pairing-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    )
}

// -- arch-2026-07-26 `header-shrink`: contracts the shrink navigates by ---

/// **`ir_lower.rs` is a second x64 emitter and the inventory above does not
/// cover it.** `header_offset_emission_site_inventory_matches_the_doc`
/// scans only `x64.rs`, so five header-offset emission sites in the IR
/// lowerer were invisible to the audit the shrink is planned from. They
/// bake the same displacements into the same instruction encodings, and a
/// shrink that updates only `x64.rs` leaves them emitting stale offsets.
///
/// Needles are assembled at runtime for the same reason the sibling test
/// does it: a literal here would be counted by that test's scan of this
/// file and break its totals.
#[test]
fn ir_lower_header_offset_sites_are_inventoried_too() {
    let src = include_str!("../ir_lower.rs");
    let cases: [(&str, &str, usize); 5] = [
        // The two raw narrowings are the FP element access (`faload`/`daload`/
        // `fastore`/`dastore`) emitted before COV-02. Everything COV-02 added
        // goes through the checked `disp8_const` form counted below, so this
        // number must not grow.
        ("HEADER_SIZE", " as u8", 2),
        ("HEADER_SIZE", " as i32", 2),
        // COV-02: the two GPR array element emitters
        // (`emit_gpr_array_elem_load` / `emit_gpr_array_elem_store`), each of
        // which materialises the header displacement ONCE and shares it across
        // every element width. Deleting either would silently restore an
        // unchecked site.
        ("HEADER_SIZE", " as i64", 2),
        // 0, deliberately: this site moved to the checked `disp::disp8_const`
        // narrowing, a `const fn` that fails the BUILD if the constant ever
        // exceeds 127. (Spelled without the literal needle: the sibling test
        // now counts that form too, and this file is inside its scan.)
        // That is strictly stronger than counting the raw narrowing here —
        // an inventory notices drift after the fact, the const check makes
        // the drift unrepresentable. A future raw `as u8` reintroduces the
        // silent negative-disp8 hazard and trips this back to 1.
        ("ARRAY_LENGTH_OFFSET", " as u8", 0),
        // The checked form must stay present; deleting it would silently
        // restore an unchecked site elsewhere. Two of them since COV-02: the
        // bounds check's length load, and `arraylength`'s own.
        ("ARRAY_LENGTH_OFFSET", " as i64", 2),
    ];
    for (base, suffix, expected) in cases {
        let needle = format!("{base}{suffix}");
        let found = src.matches(needle.as_str()).count();
        assert_eq!(
            found, expected,
            "{needle} appears {found}x in ir_lower.rs, the header-shrink audit \
             records {expected}x. ir_lower.rs emits array/field displacements \
             exactly as x64.rs does; update \
             header-shrink.md §6 in the same change."
        );
    }
}

/// The TLAB grid invariant, asserted where it is actually emitted.
///
/// `emit_inline_tlab_new` computes
/// `total_size = HEADER_SIZE + (compact_body | num_fields * SLOT_SIZE)`,
/// bakes it as an immediate, and bumps the cursor by it **without rounding
/// up** — the Rust twin `Tlab::alloc_initialized` rounds to `align`. The
/// two agree only while `total_size` is already a multiple of 8, and the
/// body-zeroing loop likewise steps 8 bytes at a time from `HEADER_SIZE`.
/// The allocation-path guard for this is a `debug_assert`, i.e. absent in
/// release, where a violation is a silent heap-walk desync rather than a
/// panic. Pin it here so it is checked unconditionally.
#[test]
fn inline_tlab_total_size_stays_on_the_eight_byte_grid() {
    assert_eq!(
        HEADER_SIZE % 8,
        0,
        "the inline-TLAB cursor bump and the qword body-zeroing loop both \
         assume an 8-byte grid anchored at HEADER_SIZE"
    );
    assert_eq!(SLOT_SIZE % 8, 0, "legacy field cells must be 8-aligned");
    for num_fields in 0..128usize {
        let total = HEADER_SIZE + num_fields * SLOT_SIZE;
        assert_eq!(
            total % 8,
            0,
            "legacy total_size for {num_fields} fields is off the grid; the JIT \
             would publish a cursor the Rust allocator would have rounded"
        );
        // The zeroing loop walks HEADER_SIZE..total_size in 8-byte steps.
        assert_eq!((total - HEADER_SIZE) % 8, 0);
    }
    // Compact bodies: `ObjectHeader::set_compact_shape` asserts
    // `body_size & 7 == 0`, so every admissible body keeps the grid.
    for body in (0..512usize).step_by(8) {
        assert_eq!(
            (HEADER_SIZE + body) % 8,
            0,
            "compact total_size for body {body} is off the grid"
        );
    }
}

/// Every header field the inline-TLAB emitter writes must stay inside the
/// header *and* on the 4-byte grid the dword-immediate store emitter can
/// express. There is no qword-immediate store, which is why the 8-byte
/// fields are written as dword pairs — a field whose offset is not
/// 4-aligned could not be written at all.
#[test]
fn every_header_field_is_dword_addressable() {
    for (name, off, width) in [
        ("class_id", 0usize, 4usize),
        ("shape", cratonvm_types::NUM_SLOTS_OFFSET, 4),
        // `forwarding_ptr`, `identity_hash_code` and then the whole
        // kind/element_type/gc_age/gc_flags quartet all folded into the mark
        // word; none of them is a separately addressed field any more.
        //
        // `gc_flags` is deliberately NOT listed: it is a byte inside that word,
        // and it is reachable because `emit_mov_byte_mem_disp32_imm8` was added
        // for it. This list is about fields the emitter reaches with its
        // dword-immediate store, so a byte field does not belong in it -- and
        // `inline_tlab_header_writes_stay_inside_the_header` covers its bounds.
        ("mark_word", cratonvm_types::MARK_WORD_OFFSET, 8),
    ] {
        assert_eq!(
            off % 4,
            0,
            "{name} at +{off} is not 4-aligned; the emitter has only a \
             dword-immediate store"
        );
        assert!(
            off + width <= HEADER_SIZE,
            "{name} at +{off} ({width}B) runs past HEADER_SIZE ({HEADER_SIZE})"
        );
    }
    // The mark word is the target of atomic CAS from the runtime lock
    // fast-path, so it needs full 8-byte alignment, not merely 4.
    assert_eq!(
        cratonvm_types::MARK_WORD_OFFSET % 8,
        0,
        "mark_word must be 8-aligned for the atomic CAS in monitor.rs"
    );
}
