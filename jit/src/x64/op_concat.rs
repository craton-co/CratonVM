// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The inline string concatenation of a bridged `invokedynamic`
//! (`StringConcatFactory`) site in the single-pass backend: JIT round 19
//! proposal wave 4, lane strgap4, STRGAP19-3 (`OptimizeStringConcat` for
//! indy; `CRATONVM_JIT_CONCAT_INLINE`, default ON since the round-19 close, `=0` off).
//!
//! # Shape
//!
//! The `0xba` arm stages every operand into its frame block exactly as before
//! (the bridge's argument buffer), then, for a site the VM's plan admits
//! ([`crate::concat_inline_plan`]), emits ahead of the unchanged bridge call:
//!
//! ```text
//!   compact strings on?                                  (else decline)
//!   R8 = literal bytes
//!   per operand, from its staged slot:                   (length pass)
//!     String: null -> 4 ; exact String, LATIN1, value a byte[] -> +value.length
//!     int/long: sign + decimal digit count ; char <= 0xFF -> 1 ; boolean 4/5
//!   R8 != 0 ; R8 <=u inline cap ; ECX = R8               (else decline)
//!   one TLAB bump: byte[ECX] + String (the STR16-P1 tail, shared)
//!   fill pass: literals as immediate stores, operands in recipe order
//!   commit ; RAX = the String ; JMP join
//! decline:
//!   the bridge call, unchanged, over the same staged operands
//! join:
//! ```
//!
//! Every decline precedes the first store (the shared tail's contract), and
//! the fast path makes no call: it cannot collect, trap or run Java code, so a
//! declined evaluation repeats no side effect and the bridge answers exactly
//! as it did before this switch. Nothing an operand's `toString()` could do
//! is reachable here: only `String`-typed operands are read as objects, and
//! a `String`'s rendering is its own bytes (javac 25 turns every other object
//! operand into a `String` with `String.valueOf` before the `invokedynamic`).
//!
//! # HotSpot identity
//!
//! `null` renders `"null"`, `int`/`long` their `Integer`/`Long.toString`
//! decimal spelling (`MIN_VALUE` exact: the magnitude is unsigned), `char` its
//! one unit, `boolean` `"true"`/`"false"`; the result is a FRESH LATIN1
//! `String` with `hash` 0 and `hashIsZero` false, as `StringConcatHelper`'s
//! `newString` builds. A result that would be UTF16 (a UTF16 operand, a
//! `char` above `0xFF`) or empty, and every run with compact strings off,
//! declines to the bridge.

use super::bytecode_walk::*;
use super::concat_emit::{self as ce, ConcatPassShape};
use super::op_invoke::{
    sb_to_string_gc_arm, string_alloc_plan, SbToStringArm, SbToStringGc, SbToStringGcInputs,
    SbToStringPlan,
};
use super::*;

/// The compile-time facts of one inline concat site, gathered before any byte
/// is emitted. The two passes and the renderers are `concat_emit`'s (JIT round
/// 19 proposal wave 5, lane concat5: shared with the optimizing tier's twin,
/// STRGAP4-1); the allocation is this backend's STR16-P1 tail.
struct ConcatSite {
    shape: ConcatPassShape,
    alloc: SbToStringPlan,
    gc: SbToStringGc,
}

impl Compiler {
    /// The inline concat of the bridged indy site `bridge_site` (STRGAP19-3).
    ///
    /// `pre_pop_spill` / `arg_slots` locate the staged operand block the 0xba
    /// arm just wrote (operand `i` at `pre_pop_spill + (arg_slots - 1 - i) *
    /// 8`). Returns an empty list with NOTHING emitted when the site is not
    /// lowered; otherwise the patches of the fast paths' jumps to the join
    /// (RAX = the String there: one, or two with a CONCAT21-1 coder-promoted
    /// UTF16 build), with every decline edge pushed onto `decline` for the
    /// caller to land on the bridge call.
    pub(super) fn try_emit_concat_inline(
        &mut self,
        pc: usize,
        bridge_site: usize,
        pre_pop_spill: i32,
        arg_slots: usize,
        ret_type: u8,
        decline: &mut Vec<usize>,
    ) -> Vec<usize> {
        if !crate::concat_inline_plan::concat_inline_enabled() {
            return Vec::new();
        }
        let site = match self.concat_inline_site(bridge_site, pre_pop_spill, arg_slots, ret_type) {
            Ok(site) => site,
            Err(why) => {
                if dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] concat-inline DECLINED {} pc={pc}: {why}",
                        self.method_label
                    );
                }
                return Vec::new();
            }
        };
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] concat-inline EMITTED {} pc={pc} args={} literal={} segments={}",
                self.method_label,
                site.shape.plan.arg_count,
                site.shape.plan.literal_len,
                site.shape.plan.segments.len()
            );
        }
        self.slot_mirror = None;
        // CONCAT21-4 (`CRATONVM_JIT_CONCAT_VALUE_OF_FUSION`): the passes read
        // `Integer.value` at offsets baked now, so a layout replaced since the
        // compile declines (the unbox prefix's guard), before anything else.
        if site.shape.boxed_int.is_some() && jit_sp_field_layout_guard_enabled() {
            decline.extend(self.emit_layout_epoch_guard());
        }
        // CONCAT21-3 (`CRATONVM_JIT_CONCAT_FP_INLINE`): an FP plan renders its
        // float / double operands into an RSP-relative scratch area first.
        // From here every decline leaves with RSP moved, so they are
        // collected in `inner` and land on a stub that restores RSP before
        // the bridge call. Without FP operands `inner` is `decline` itself,
        // byte for byte.
        let fp = site.shape.plan.has_fp();
        let mut inner: Vec<usize> = Vec::new();
        ce::emit_fp_prologue(&mut self.buf, &site.shape, &mut inner);
        // JIT round 21 wave 4, lane concat (CONCAT21-1,
        // `CRATONVM_JIT_CONCAT_CODER_PROMOTE`, default OFF): a LATIN1 site's
        // run-time UTF16 operands land on its wide twin's passes and the
        // tail's UTF16 arm, emitted after the LATIN1 build, instead of the
        // bridge. Off (or no twin), the bytes are unchanged.
        let twin = ce::coder_promotion_twin(site.shape.plan);
        let mut promote: Vec<usize> = Vec::new();
        if twin.is_some() {
            ce::emit_length_pass_promoting(&mut self.buf, &site.shape, &mut inner, &mut promote);
        } else {
            ce::emit_length_pass(&mut self.buf, &site.shape, &mut inner);
        }
        let mut joins = vec![self.emit_concat_inline_build(&site, &site.shape, &mut inner)];
        if let Some(twin) = twin {
            if !promote.is_empty() {
                if dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] concat-inline coder-promote {} pc={pc} edges={}",
                        self.method_label,
                        promote.len()
                    );
                }
                for p in promote {
                    self.patch_rel32_to_here(p);
                }
                let wide_shape = ConcatPassShape {
                    plan: twin,
                    ..site.shape.clone()
                };
                ce::emit_length_pass(&mut self.buf, &wide_shape, &mut inner);
                joins.push(self.emit_concat_inline_build(&site, &wide_shape, &mut inner));
            }
        }
        if fp {
            // The restore stub: every inner decline, RSP back, then the
            // bridge call (the caller's decline target).
            for p in inner {
                self.patch_rel32_to_here(p);
            }
            ce::emit_fp_restore(&mut self.buf, &site.shape);
            decline.push(self.emit_jmp_rel32_patch());
        } else {
            decline.extend(inner);
        }
        self.slot_mirror = None;
        joins
    }

    /// The allocation, fill and commit of one inline concat build over
    /// `shape` (the site's own plan, or its CONCAT21-1 wide twin), after its
    /// length pass; answers the patch of the jump to the join (RAX = the
    /// String there).
    fn emit_concat_inline_build(
        &mut self,
        site: &ConcatSite,
        shape: &ConcatPassShape,
        decline: &mut Vec<usize>,
    ) -> usize {
        // STRGAP4-4 (`CRATONVM_JIT_CONCAT_UTF16`): a wide plan's String is
        // UTF16, the tail's UTF16 arm (the validated coder slot; the length
        // pass left ECX = 2 * units, the byte count that arm expects).
        let arm = if shape.plan.wide {
            SbToStringArm {
                utf16_coder: site.alloc.utf16_coder,
                narrow: false,
                taken_row: crate::metrics::SB_TS_TAKEN_UTF16,
            }
        } else {
            SbToStringArm {
                utf16_coder: None,
                narrow: false,
                taken_row: crate::metrics::SB_TS_TAKEN_LATIN1,
            }
        };
        let fill = |c: &mut Compiler| ce::emit_fill_pass(&mut c.buf, shape);
        self.emit_sb_to_string_tail(&site.alloc, site.gc, arm, false, decline, Some(&fill));
        // CONCAT21-3: RSP back before the join (RAX = the String is kept).
        ce::emit_fp_restore(&mut self.buf, shape);
        self.slot_mirror = None;
        self.emit_jmp_rel32_patch()
    }

    /// Every compile-time screen of the inline concat; `Err` names the refusal.
    fn concat_inline_site(
        &self,
        bridge_site: usize,
        pre_pop_spill: i32,
        arg_slots: usize,
        ret_type: u8,
    ) -> Result<ConcatSite, &'static str> {
        if ret_type != b'L' {
            return Err("not a reference result");
        }
        let plan = self
            .direct_helpers
            .concat_inline_plan(bridge_site)
            .ok_or("no plan (shape not admitted, or reader not wired)")?;
        if plan.wide && !crate::concat_inline_plan::concat_utf16_enabled() {
            return Err("UTF16 literal (wide plan) without CRATONVM_JIT_CONCAT_UTF16");
        }
        // CONCAT21-3: a float / double operand only under its switch (off:
        // the bridge call these sites always had).
        if plan.has_fp() && !crate::concat_inline_plan::concat_fp_inline_enabled() {
            return Err("float/double operand without CRATONVM_JIT_CONCAT_FP_INLINE");
        }
        // JIT round 19 wave 11 (lane hoist11;
        // `r19-unary-indy-concat-returns-a-fresh-string-20261008`): the
        // optimizing tier's screen (`concat_emit::concat_ir_site`) already
        // declines `"" + b`, whose HotSpot answer is the interned literal the
        // bridge's renderer now returns; the single-pass tier built a fresh
        // String for it. Unconditional (parity; the decline is the bridge
        // call the site always had).
        if super::concat_emit::is_unary_boolean(plan) {
            return Err("unary boolean (`\"\" + b`): HotSpot answers the interned literal");
        }
        if plan.arg_count != arg_slots {
            return Err("operand count disagrees with the staged block");
        }
        let outer = self.string_layout.ok_or("no String layout")?;
        if !outer.has_coder || outer.string_class_id == 0 {
            return Err("String layout without coder");
        }
        let alloc = string_alloc_plan(&outer)?;
        // REVIEW6-3 (JIT round 19 close, unconditional, fail-closed): the
        // length pass reads `coder` at the outer layout's compact offset;
        // only `string_alloc_plan` validates that slot against the registry
        // row (width, bounds, disp8). Admit the site only when both name the
        // same slot, so a String layout replaced between the two reads can
        // never make the pass read a neighbour.
        if alloc.utf16_coder != Some((outer.coder_compact_offset, outer.coder_compact_is_byte)) {
            return Err("coder slot not validated by the allocation plan");
        }
        let gc_inputs = SbToStringGcInputs {
            zgc_registration: cratonvm_types::jit_tlab_registration_required(),
            zgc_start_bit_requested: super::objects::zgc_inline_start_bit_requested(),
            zgc_array_announce: crate::x64::zgc_inline_array_announce_enabled(),
            g1_table_live: g1_barrier_table_live(self.helpers.g1_barrier_addr),
            generational_bounds_live: region_bounds_are_live(self.helpers.region_bounds_addr),
        };
        let gc = sb_to_string_gc_arm(gc_inputs)?;
        if self.jit_thread_slot_off == 0 && jit_thread_tls_disp() == 0 {
            return Err("no thread source that is not a call");
        }
        if self.deferred_alloc_blind_spill || self.deferred_alloc_shadow_push {
            return Err("withheld allocation spill");
        }
        if self.helpers.tlab_cursor_offset_in_thread == self.helpers.tlab_end_offset_in_thread {
            return Err("no TLAB offsets");
        }
        let compact_flag_addr = self
            .direct_helpers
            .compact_strings_flag_addr()
            .ok_or("no compact-strings flag address")?;
        let arg_offsets = (0..arg_slots)
            .map(|i| {
                i32::try_from(arg_slots - 1 - i)
                    .ok()
                    .and_then(|k| k.checked_mul(8))
                    .and_then(|d| pre_pop_spill.checked_add(d))
            })
            .collect::<Option<Vec<i32>>>()
            .ok_or("staged offset overflow")?;
        // Cast: a small header constant.
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;
        let mut shape = ConcatPassShape::new(
            plan,
            outer,
            &alloc,
            compact_flag_addr,
            arg_offsets,
            class_id_off,
        );
        // CONCAT21-4: a String operand may carry a fused `valueOf`'s exact
        // Integer (only this tier's prefix produces one).
        if concat_value_of_fusion_enabled() {
            shape.boxed_int = concat_boxed_int(&self.direct_helpers);
        }
        Ok(ConcatSite { shape, alloc, gc })
    }

    /// STRGAP19-4 (JIT round 19 proposal wave 4, lane strgap4;
    /// `CRATONVM_JIT_STRING_VALUE_OF_THIN`, default OFF): a guarded prefix of
    /// a direct `String.valueOf(Object)` call. JDK 25's body is `obj == null ?
    /// "null" : obj.toString()`, and `String.toString()` is `return this`, so
    /// for an exact `java/lang/String` argument the answer is the argument
    /// itself, with no Java code run. Returns the hit edge's `JMP rel32`
    /// patch (RAX = the argument); every other argument -- `null` (whose
    /// answer is the interned `"null"` literal, a movable heap object this
    /// code cannot name), any other class (its `toString()` runs, with its
    /// side effects and exceptions) -- falls into the unchanged call.
    ///
    /// ```text
    ///   RAX = arg ; TEST RAX,RAX ; JZ call
    ///   CMP DWORD [RAX+class_id], String ; JNE call
    ///   JMP join                          ; RAX = arg
    /// call: <the unchanged direct call>
    /// ```
    ///
    /// No allocation, no call, no store: the hit owes no safepoint map and no
    /// sentinel check (the `VALUE_OF_INLINE_BOX` prefix's contract). Declines
    /// at compile time like that prefix for an operand in an XMM or scratch
    /// register, and without a String layout.
    ///
    /// CONCAT21-4 (JIT round 21 wave 5, lane concat;
    /// `CRATONVM_JIT_CONCAT_VALUE_OF_FUSION`, opt-in): when the call's result
    /// provably feeds ONE `String` operand of a following inline-planned
    /// `StringConcatFactory` indy and nothing else
    /// ([`value_of_result_concat_operand`]), an exact `java/lang/Integer`
    /// argument is answered by ITSELF: its `toString()` is deferred to the
    /// concat, which renders the `int` in its own passes (the bridge, the
    /// interpreter after a deopt, and the Latin-1 fast path's decline all
    /// render an `Integer` operand by class as `Integer.toString`). Sound
    /// because `Integer` is final and immutable and `Integer.toString` has no
    /// side effect a program can observe between the two bcis; the
    /// intermediate String is never observable (a stack temporary whose only
    /// reader is the indy). This arm needs no String layout.
    pub(super) fn try_emit_string_value_of_prefix(
        &mut self,
        pc: usize,
        value: StackSlot,
        code: &[u8],
    ) -> Option<usize> {
        if self.failed {
            return None;
        }
        let thin = string_value_of_thin_enabled();
        let fused = if concat_value_of_fusion_enabled() {
            self.value_of_fusion_box(pc, code)
        } else {
            None
        };
        if !thin && fused.is_none() {
            return None;
        }
        if matches!(value, StackSlot::Xmm(..))
            || self
                .stack
                .iter()
                .any(|slot| matches!(slot, StackSlot::Scratch(..)))
        {
            return None;
        }
        let cid = if thin {
            self.string_layout
                .map(|l| l.string_class_id)
                .filter(|&c| c != 0)
        } else {
            None
        };
        if cid.is_none() && fused.is_none() {
            return None;
        }
        // STRGAP4-2 (JIT round 19 proposal wave 5, lane concat5;
        // `CRATONVM_JIT_STRING_LITERAL_SLOTS`, default OFF): this VM's rooted
        // `"null"` literal slot, which answers the `null` argument too.
        let null_slot = if cid.is_some() && string_literal_slots_enabled() {
            self.direct_helpers.string_literal_slot(false)
        } else {
            None
        };
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] string-valueof-thin {} @pc={pc} string_class_id={cid:?} null_slot={} fused_integer={}",
                self.method_label,
                null_slot.is_some(),
                fused.is_some()
            );
        }
        self.load_slot_to_reg(RAX, value);
        self.emit_test_r64_r64(RAX);
        let is_null = self.emit_jcc_rel32_patch(0x84); // JZ null arm (or call)

        // Cast: a small header constant.
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;
        // (position of the hit JMP, its patch)
        let mut hit: Option<(usize, usize)> = None;
        let mut to_call: Vec<usize> = Vec::new();
        if let Some(cid) = cid {
            self.buf.emit(&[0x81, 0xB8]); // CMP DWORD [RAX+disp32], imm32
            self.buf.emit(&class_id_off.to_le_bytes());
            // Cast: a ClassId fits in 32 bits.
            self.buf.emit(&(cid as i32).to_le_bytes());
            let other = self.emit_jcc_rel32_patch(0x85); // JNE: the next arm
            let hit_at = self.buf.pos();
            hit = Some((hit_at, self.emit_jmp_rel32_patch()));
            // r21 review (rev-x64): a non-String goes to the fused arm when
            // there is one, else to the call -- never into the null arm
            // below (which would answer the "null" literal for it).
            if fused.is_some() {
                self.patch_rel32_to_here(other);
            } else {
                to_call.push(other);
            }
        }
        if let Some(b) = fused {
            // A plain object (an `Integer[]` carries the same id), then the
            // exact class: RAX = the Integer itself, to the join.
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
                cratonvm_types::KIND_TAG_BYTE_MASK,
            );
            to_call.push(self.emit_jcc_rel32_patch(0x85)); // JNZ call
            self.buf.emit(&[0x81, 0xB8]); // CMP DWORD [RAX+disp32], imm32
            self.buf.emit(&class_id_off.to_le_bytes());
            self.buf.emit(&b.class_id.to_le_bytes());
            to_call.push(self.emit_jcc_rel32_patch(0x85)); // JNE call
            match hit {
                Some((hit_at, _)) => {
                    let back = self.emit_jmp_rel32_patch();
                    let rel = hit_at as i32 - (back as i32 + 4); // Cast: rel32 displacement
                    self.buf.try_patch_i32(back, rel).ok(); // on Err the buffer is marked overflowed; compile bails
                }
                None => {
                    let at = self.buf.pos();
                    hit = Some((at, self.emit_jmp_rel32_patch()));
                }
            }
        }
        let (hit_at, hit) = hit?;
        self.patch_rel32_to_here(is_null);
        if let Some(slot) = null_slot {
            // null: RAX = the interned "null" (JDK 25: `obj == null ? "null"
            // : ...`, the `ldc` answer), read raw from the GC-maintained slot
            // as `emit_ldc_slot_probe` reads an `ldc` slot; a word still 0
            // (never, once published) takes the call with RAX = 0 = the
            // argument, as the slot-less prefix does.
            // Cast: the VM's slot address, baked.
            self.emit_mov_imm64_full(RAX, slot as i64);
            self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX,[RAX]
            self.emit_test_r64_r64(RAX);
            let back = self.emit_jcc_rel32_patch(0x85); // JNZ hit (the JMP join)
            let rel = hit_at as i32 - (back as i32 + 4); // Cast: rel32 displacement
            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        }
        for p in to_call {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// CONCAT21-4: the boxed-`Integer` shape the prefix at `pc` may pass
    /// through, or `None`: no `Integer` layout, or the call's result is not
    /// provably the sole input of one `String` operand of a following
    /// inline-planned concat indy ([`value_of_result_concat_operand`]).
    fn value_of_fusion_box(&self, pc: usize, code: &[u8]) -> Option<ce::ConcatBoxedInt> {
        let boxed = concat_boxed_int(&self.direct_helpers)?;
        let is_value_of_at = |p: usize| {
            self.invoke_info_idx
                .get(&p)
                .map(|&i| self.invoke_info[i].1)
                .is_some_and(|ip| {
                    if ip.is_null() {
                        return false;
                    }
                    // SAFETY: `invoke_info` owns every pointer it hands out
                    // for the life of this compile.
                    let info: &crate::JitInvokeInfo = unsafe { &*ip };
                    is_string_value_of_object(info)
                })
        };
        let indy_args_at = |p: usize| {
            self.indy_info_idx
                .get(&p)
                .map(|&i| (self.indy_info[i].3.len(), self.indy_info[i].4))
        };
        let (indy_pc, operand, site) =
            value_of_result_concat_operand(code, pc + 3, is_value_of_at, indy_args_at)?;
        let plan = self.direct_helpers.concat_inline_plan(site)?;
        let feeds_string = plan.segments.iter().any(|s| {
            matches!(
                s,
                crate::concat_inline_plan::ConcatSegment::Arg {
                    index,
                    kind: crate::concat_inline_plan::ConcatArgKind::String,
                } if *index == operand
            )
        });
        if !feeds_string {
            return None;
        }
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] concat-valueof-fusion {} valueOf@pc={pc} -> indy@pc={indy_pc} operand={operand}",
                self.method_label
            );
        }
        Some(boxed)
    }
}

/// CONCAT21-4: the bytecode scan behind the fusion. From `after` (the
/// instruction after the `String.valueOf(Object)` call), walk forward over
/// instructions that only PUSH one value each (constants, `ldc*`, local
/// loads) or are another `String.valueOf(Object)` call on a value above ours,
/// to the first `invokedynamic`; answer `(its pc, the operand index our value
/// is, its bridge site)` when that indy consumes our value. Anything else --
/// a branch, a store, a `dup`, any other call or instruction, a `wide`
/// prefix, a valueOf of OUR value, more than 64 instructions -- answers
/// `None`. Straight-line by construction, so on the compiled path our value
/// is read by nothing before the indy; a deopt in between hands the
/// interpreter the Integer, whose concat renders it by class.
///
/// `is_value_of_at(pc)`: is the `invokestatic` at `pc` `String.valueOf(Object)`;
/// `indy_args_at(pc)`: `(operand count, bridge site)` of the indy at `pc`.
pub(super) fn value_of_result_concat_operand(
    code: &[u8],
    after: usize,
    is_value_of_at: impl Fn(usize) -> bool,
    indy_args_at: impl Fn(usize) -> Option<(usize, usize)>,
) -> Option<(usize, usize, usize)> {
    // Values pushed above ours and still on the stack.
    let mut above = 0usize;
    let mut p = after;
    for _ in 0..64 {
        let op = *code.get(p)?;
        let len = match op {
            // aconst_null, iconst_*, lconst_*, fconst_*, dconst_*.
            0x01..=0x0f => 1,
            0x10 => 2,        // bipush
            0x11 => 3,        // sipush
            0x12 => 2,        // ldc
            0x13 | 0x14 => 3, // ldc_w, ldc2_w
            0x15..=0x19 => 2, // iload..aload idx
            0x1a..=0x2d => 1, // iload_0..aload_3
            0xb8 => {
                // valueOf of a value ABOVE ours: pops one, pushes one.
                if above >= 1 && is_value_of_at(p) {
                    p += 3;
                    continue;
                }
                return None;
            }
            0xba => {
                let (n, site) = indy_args_at(p)?;
                return (above < n).then(|| (p, n - 1 - above, site));
            }
            _ => return None,
        };
        above += 1;
        p += len;
    }
    None
}

/// `CRATONVM_JIT_CONCAT_VALUE_OF_FUSION` (JIT round 21 wave 5, lane concat,
/// CONCAT21-4; opt-in, default OFF): the single-pass `String.valueOf(Object)`
/// prefix passes an exact `Integer` through to the inline concat it feeds,
/// which renders the `int`. Read once per site of a compile, no process
/// global.
pub(super) fn concat_value_of_fusion_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_CONCAT_VALUE_OF_FUSION")
}

/// CONCAT21-4: this VM's `Integer` class id and `value` offsets, as the
/// single-pass unbox prefix reads them, or `None`.
pub(super) fn concat_boxed_int(helpers: &crate::DirectHelperTable) -> Option<ce::ConcatBoxedInt> {
    let class_id = helpers.integer_class_id()?;
    let l = crate::AtomicIntFieldLayout::new(0, class_id)?;
    Some(ce::ConcatBoxedInt {
        class_id,
        compact_off: l.value_compact_offset,
        legacy_off: l.value_legacy_offset,
    })
}

/// `CRATONVM_JIT_STRING_VALUE_OF_THIN` (JIT round 19 proposal wave 4, lane
/// strgap4, STRGAP19-4; default OFF): the single-pass `String.valueOf(Object)`
/// prefix above. Read once per site of a compile, no process global.
fn string_value_of_thin_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_STRING_VALUE_OF_THIN")
}

/// `CRATONVM_JIT_STRING_LITERAL_SLOTS` (JIT round 19 proposal wave 5, lane
/// concat5, STRGAP4-2; default OFF): compiled code reads this VM's rooted
/// `"null"` / `""` literal slots (`DirectHelperTable::string_literal_slot`)
/// where JDK 25 would `ldc` the literal in a callee it does not run: the
/// `null` argument of the STRGAP19-4 `String.valueOf(Object)` prefix (itself
/// behind `CRATONVM_JIT_STRING_VALUE_OF_THIN`) and the empty builder of the
/// inline `StringBuilder.toString()` (`CRATONVM_JIT_SB_TOSTRING_INLINE`).
/// Read once per site of a compile, no process global.
pub(super) fn string_literal_slots_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_STRING_LITERAL_SLOTS")
}

/// Is this call site `java/lang/String.valueOf(Ljava/lang/Object;)` (the
/// static the STRGAP19-4 prefix serves)?
pub(super) fn is_string_value_of_object(info: &crate::JitInvokeInfo) -> bool {
    info.invoke_kind == 3
        && info.class_name == "java/lang/String"
        && info.method_name == "valueOf"
        && info.descriptor == "(Ljava/lang/Object;)Ljava/lang/String;"
}

/// STRGAP19-3's machine-code pieces, executed: the decimal renderer (every
/// `Integer`/`Long.toString` edge), the digit count, the literal stores (never
/// past the literal) and the operand copy (never past its count).
#[cfg(all(test, target_arch = "x86_64"))]
mod r19_strgap4_concat_inline_tests {
    use super::*;

    /// Run `body` as a leaf `extern "C" fn(a0, a1, a2) -> i64` snippet. On
    /// entry RAX = `a0`, R8 = `a1`, RDX = ECX = `a2`; the body leaves its
    /// answer in RAX.
    fn run(body: impl Fn(&mut Compiler), a0: i64, a1: i64, a2: i64) -> i64 {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        // Arguments first into registers no other argument lives in.
        c.emit_mov_r64_r64(R11, ARG_REGS[2]);
        c.emit_mov_r64_r64(R10, ARG_REGS[1]);
        c.emit_mov_r64_r64(RAX, ARG_REGS[0]);
        c.emit_mov_r64_r64(R8, R10);
        c.emit_mov_r64_r64(RDX, R11);
        c.emit_mov_r64_r64(RCX, R11);
        body(&mut c);
        c.buf.emit(&[0xC3]); // ret
        assert!(!c.buf.overflowed());
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is a byte offset inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: a leaf snippet that touches only the caller's buffers named
        // by its arguments (alive across the call) and caller-saved registers.
        let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
        let r = f(a0, a1, a2);
        drop(c);
        r
    }

    /// The decimal text the emitted renderer writes for `v` (`int_kind`:
    /// sign-extend the low 32 bits first, as the `int` operand does).
    fn render(v: i64, int_kind: bool) -> String {
        let mut buf = [0xAAu8; 40];
        let base = buf.as_mut_ptr();
        // Cast: the buffer's address as the snippet's i64 argument.
        let end = run(
            move |c| {
                if int_kind {
                    c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                }
                ce::emit_decimal(&mut c.buf);
                c.emit_mov_r64_r64(RAX, R8);
            },
            v,
            base as i64,
            0,
        );
        // Cast: the end pointer back to an offset into `buf`.
        let n = (end as usize) - (base as usize);
        assert!(n <= 20, "{v}: {n} bytes");
        assert!(
            buf[n..].iter().all(|&b| b == 0xAA),
            "{v}: wrote past its digits"
        );
        String::from_utf8(buf[..n].to_vec()).expect("ASCII")
    }

    #[test]
    fn longs_render_as_long_to_string() {
        let mut values = vec![
            0i64,
            1,
            -1,
            9,
            10,
            11,
            99,
            100,
            -100,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
            999_999_999_999_999_999,
            1_000_000_000_000_000_000,
            -1_000_000_000_000_000_000,
            4_294_967_296,
            -2_147_483_649,
        ];
        let mut p = 1i64;
        for _ in 0..18 {
            p *= 10;
            values.extend([p - 1, p, p + 1, -p]);
        }
        for v in values {
            assert_eq!(render(v, false), v.to_string());
        }
    }

    #[test]
    fn ints_render_as_integer_to_string_from_the_low_word() {
        for v in [
            0i32,
            7,
            -7,
            10,
            -10,
            i32::MAX,
            i32::MIN,
            i32::MIN + 1,
            999_999_999,
            1_000_000_000,
            -1_000_000_000,
            123_456_789,
        ] {
            // Garbage in the high half: the operand is the low 32 bits.
            // Cast: the int's bits, zero-extended under a garbage high word.
            let raw = (0x5A5A_5A5Ai64 << 32) | i64::from(v as u32);
            assert_eq!(render(raw, true), v.to_string());
        }
    }

    #[test]
    fn the_length_pass_digit_count_matches_the_rendering() {
        for v in [
            0u64,
            9,
            10,
            99,
            100,
            1 << 31,
            u64::from(u32::MAX),
            1 << 63,
            9_999_999_999_999_999_999,
        ] {
            // R8 starts at 5: the count is added to what is there.
            // Cast: the magnitude's bits as the i64 argument.
            let got = run(
                |c| {
                    c.emit_mov_imm32_sx(R8, 5);
                    ce::emit_digit_count(&mut c.buf, R8);
                    c.emit_mov_r64_r64(RAX, R8);
                },
                v as i64,
                0,
                0,
            );
            // Cast: a digit count.
            assert_eq!(got, 5 + v.to_string().len() as i64, "{v}");
        }
    }

    #[test]
    fn literals_are_stored_exactly() {
        for len in 0..=40usize {
            // Cast: `i % 26` fits a byte.
            let text: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
            let mut buf = [0xAAu8; 64];
            let base = buf.as_mut_ptr();
            let t = text.clone();
            // Cast: the buffer's address as the snippet's i64 argument.
            let end = run(
                move |c| {
                    ce::emit_literal(&mut c.buf, &t);
                    c.emit_mov_r64_r64(RAX, R8);
                },
                0,
                base as i64,
                0,
            );
            // Cast: the end pointer back to an offset into `buf`.
            assert_eq!((end as usize) - (base as usize), len);
            assert_eq!(&buf[..len], text.as_slice());
            assert!(
                buf[len..].iter().all(|&b| b == 0xAA),
                "len {len}: wrote past"
            );
        }
    }

    #[test]
    fn the_copy_moves_exactly_count_bytes() {
        let src: Vec<u8> = (0..64u8)
            .map(|b| b.wrapping_mul(7).wrapping_add(1))
            .collect();
        for count in 0..=40usize {
            let mut dst = [0xAAu8; 64];
            let d = dst.as_mut_ptr();
            let s = src.as_ptr();
            // RDX = src, ECX = count, R8 = dst.
            // Cast: addresses and the count as the snippet's i64 arguments.
            let end = run(
                move |c| {
                    c.emit_mov_imm64_full(RDX, s as i64);
                    ce::emit_copy(&mut c.buf);
                    c.emit_mov_r64_r64(RAX, R8);
                },
                0,
                d as i64,
                count as i64,
            );
            // Cast: the end pointer back to an offset into `dst`.
            assert_eq!((end as usize) - (d as usize), count);
            assert_eq!(&dst[..count], &src[..count]);
            assert!(
                dst[count..].iter().all(|&b| b == 0xAA),
                "count {count}: wrote past"
            );
        }
    }

    /// A fake well-known reader: for the null literal
    /// slot: the context word IS the slot's address (the test puts the fake
    /// literal there), any other query answers 0.
    unsafe extern "C" fn fake_well_known(ctx: i64, which: i64) -> i64 {
        if which == crate::direct_helpers::WELL_KNOWN_NULL_LITERAL_SLOT {
            ctx
        } else {
            0
        }
    }

    /// r21 review (rev-x64) regression: the `String.valueOf(Object)` prefix
    /// with `CRATONVM_JIT_STRING_VALUE_OF_THIN=1` and
    /// `CRATONVM_JIT_STRING_LITERAL_SLOTS=1` (fusion off), executed. An exact
    /// String answers itself and `null` the literal slot's object (both on the
    /// hit edge); any other class -- an Integer here -- must reach the CALL
    /// (marker `-1`), never the null arm. The CONCAT21-4 rewrite let the
    /// String arm's `JNE` fall into the null arm: `String.valueOf(42)` was
    /// `"null"`.
    #[test]
    fn r21_the_value_of_prefix_sends_a_non_string_to_the_call_not_the_null_arm() {
        const STRING_CID: u32 = 77;
        const INTEGER_CID: u32 = 55;
        let string_obj: Vec<u64> = vec![u64::from(STRING_CID), 0, 0, 0];
        let integer_obj: Vec<u64> = vec![u64::from(INTEGER_CID), 42, 0, 0];
        let fake_null_literal: u64 = 0x5151_0000;
        let slot = Box::new(fake_null_literal);
        // Cast: the slot's address as the reader's context word.
        let slot_addr = &*slot as *const u64 as usize;
        let layout = crate::StringFieldLayout {
            value_field_index: 0,
            value_compact_offset: 16,
            value_compact_is_narrow: false,
            value_legacy_offset: 40,
            hash_field_index: 1,
            hash_compact_offset: 28,
            hash_legacy_offset: 56,
            has_coder: true,
            coder_field_index: 2,
            coder_compact_offset: 24,
            coder_legacy_offset: 48,
            coder_compact_is_byte: true,
            string_class_id: STRING_CID,
            builder: None,
            array_list: None,
            hash_is_zero_compact_offset: None,
        };
        let answer = |arg: u64| -> i64 {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_STRING_VALUE_OF_THIN", Some("1")),
                    ("CRATONVM_JIT_STRING_LITERAL_SLOTS", Some("1")),
                    ("CRATONVM_JIT_CONCAT_VALUE_OF_FUSION", None),
                ],
                || {
                    run(
                        |c| {
                            c.string_layout = Some(layout);
                            c.helpers.class_id_offset_in_obj = 0;
                            // Cast: the fake reader's address, as the table holds it.
                            c.direct_helpers.well_known_class_id = fake_well_known
                                as unsafe extern "C" fn(i64, i64) -> i64
                                as usize;
                            c.direct_helpers.static_base_resolver_ctx = slot_addr;
                            c.buf.emit(&[0x41, 0x54]); // PUSH R12
                            c.buf.emit(&[0x49, 0x89, 0xC4]); // MOV R12,RAX
                            let hit = c
                                .try_emit_string_value_of_prefix(0, StackSlot::CalleeSaved(R12), &[])
                                .expect("the prefix is emitted");
                            // The call edge: a marker.
                            c.emit_mov_imm32_sx(RAX, -1);
                            c.patch_rel32_to_here(hit);
                            c.buf.emit(&[0x41, 0x5C]); // POP R12
                        },
                        // Cast: the argument's bits.
                        arg as i64,
                        0,
                        0,
                    )
                },
            )
        };
        // Cast: addresses as the snippet's i64 arguments / answers.
        let s = string_obj.as_ptr() as u64;
        let i = integer_obj.as_ptr() as u64;
        assert_eq!(answer(s), s as i64, "an exact String answers itself");
        assert_eq!(answer(0), fake_null_literal as i64, "null answers the literal");
        assert_eq!(answer(i), -1, "an Integer takes the call (String.valueOf(42) is \"42\")");
        drop(slot);
    }

    /// CONCAT21-4: the fusion scan finds the concat operand a `valueOf`
    /// result is, across loads, constants and later `valueOf`s, and refuses
    /// every shape where anything else could read it.
    #[test]
    fn r21_the_value_of_scan_finds_the_concat_operand_and_nothing_else() {
        // `"" + a + i + b`: aload_1 valueOf(@0) | iload_2 aload_3 valueOf(@5) indy(@8)
        // The scan starts after the first valueOf (pc 3).
        let code = [
            0x2b, // 0: aload_1 (before the call; not scanned)
            0x00, 0x00, // filler for the call's operands
            0x1c, // 3: iload_2
            0x2d, // 4: aload_3
            0xb8, 0x00, 0x09, // 5: invokestatic valueOf
            0xba, 0x00, 0x07, 0x00, 0x00, // 8: invokedynamic
        ];
        let is_value_of = |p: usize| p == 5;
        let indy = |p: usize| (p == 8).then_some((3usize, 0xABCusize));
        assert_eq!(
            value_of_result_concat_operand(&code, 3, is_value_of, indy),
            Some((8, 0, 0xABC))
        );
        // The second valueOf's own result: the last operand.
        assert_eq!(
            value_of_result_concat_operand(&code, 8, is_value_of, indy),
            Some((8, 2, 0xABC))
        );
        // A valueOf of OUR value (nothing above it) consumes it: refused.
        let own = [0xb8, 0x00, 0x09, 0xba, 0x00, 0x07, 0x00, 0x00];
        assert_eq!(
            value_of_result_concat_operand(&own, 0, |p| p == 0, |p| (p == 3).then_some((1, 1))),
            None
        );
        // dup / astore / a branch / any other call / wide: refused.
        for op in [0x59u8, 0x3a, 0xa7, 0xb6, 0xc4, 0x60] {
            let c = [op, 0x00, 0x00, 0xba, 0x00, 0x07, 0x00, 0x00];
            assert_eq!(
                value_of_result_concat_operand(&c, 0, |_| false, |p| (p == 3).then_some((2, 1))),
                None,
                "opcode {op:#x}"
            );
        }
        // Pushed past the indy's operands: not ours.
        let c = [0x1a, 0x1b, 0xba, 0x00, 0x07, 0x00, 0x00];
        assert_eq!(
            value_of_result_concat_operand(&c, 0, |_| false, |p| (p == 2).then_some((2, 1))),
            None
        );
        // An indy the bridge does not describe: refused.
        assert_eq!(value_of_result_concat_operand(&c, 0, |_| false, |_| None), None);
        // Wide constants and ldc forms are one value each.
        let c = [0x14, 0x00, 0x01, 0x12, 0x02, 0x11, 0x00, 0x05, 0xba, 0, 7, 0, 0];
        assert_eq!(
            value_of_result_concat_operand(&c, 0, |_| false, |p| (p == 8).then_some((4, 9))),
            Some((8, 0, 9))
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_VALUE_OF_FUSION", None)],
            || assert!(!concat_value_of_fusion_enabled()),
        );
    }

    #[test]
    fn string_value_of_object_is_recognised_by_its_exact_triple() {
        let info = |class: &'static str, desc: &'static str, kind: u8| crate::JitInvokeInfo {
            class_name: class,
            method_name: "valueOf",
            descriptor: desc,
            num_jit_args: 1,
            return_type: b'L',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let want = "(Ljava/lang/Object;)Ljava/lang/String;";
        assert!(is_string_value_of_object(&info(
            "java/lang/String",
            want,
            3
        )));
        assert!(!is_string_value_of_object(&info(
            "java/lang/String",
            "(I)Ljava/lang/String;",
            3
        )));
        assert!(!is_string_value_of_object(&info(
            "java/lang/String",
            "([C)Ljava/lang/String;",
            3
        )));
        assert!(!is_string_value_of_object(&info(
            "java/lang/Strings",
            want,
            3
        )));
        assert!(!is_string_value_of_object(&info(
            "java/lang/String",
            want,
            0
        )));
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_STRING_VALUE_OF_THIN", None)],
            || assert!(!string_value_of_thin_enabled()),
        );
    }
}
