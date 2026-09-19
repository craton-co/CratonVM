// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime-sensitive x86-64 lowering shared by the baseline and optimizing JITs.
//!
//! The two tiers deliberately retain different graph/bytecode construction and
//! optimization policies.  They must not, however, grow independent encodings
//! for calls, allocation, locking, barriers, or dispatch.  This module is the
//! common machine-lowering seam for those operations.
//!
//! # Baked addresses and what keeps each one alive
//!
//! Every function here writes at least one raw address into machine code that
//! outlives this call.  The audit is in `docs/jit/code-cache-lifetime.md`; the
//! summary belongs at the emission site:
//!
//! | Baked | By | Kept alive by |
//! |---|---|---|
//! | `target` (runtime helper) | `emit_new_object_stub`, `emit_new_object_cp_stub`, `emit_monitor_stub` | `JitRuntimeHelpers` function pointers — VM-lifetime `extern "C"` items, never unmapped |
//! | `frame_record` | `emit_post_call_frame_republish` | same |
//! | `service_helper` | `emit_callee_deopt_check` | same |
//! | `info_ptr` (`*const JitInvokeInfo`) | `emit_callee_deopt_check` | the compiling artifact's `CompiledMethod::_jit_invoke_infos` — a `Vec<Box<..>>`, so growth moves the handles, never the pointees |
//! | `pic` (`*const JitPICSlot`) | `emit_hashed_vtable_stub` | the compiling artifact's `CompiledMethod::_jit_pic_slots`, same shape |
//! | `mega_entry_words[i]` (loaded once, tag stripped, then `CALL R11`) | `emit_hashed_vtable_stub` | **not baked** — read from the slot at dispatch time, and retained by that slot's `mega_compiled_owners[i]` `Arc<CompiledMethod>` |
//!
//! The one that is not a constant is the interesting one: the megamorphic way's
//! entry pointer is *loaded* rather than baked, so it can be withdrawn.  It is
//! only ever installed through `JitPICSlot::install_megamorphic`, which refuses
//! any address `jit_entry_publishable` cannot retain, and the emitted probe
//! tests the loaded pointer for zero before calling it — so a withdrawal
//! (`invalidate_targets` / `clear_entries` zero the entry, then release the
//! owner) degrades to a helper call rather than to a wild jump.
//!
//! Both slot pointers are single-artifact-scoped: a slot dies exactly when the
//! code that names it dies, which is why no separate keep-alive is needed for
//! the imm64 itself.  A replicated loop body shares ONE slot across its copies
//! (`x64/licm.rs`), which does not change that — all copies live in the same
//! artifact.

//! # Call-site shape assertions
//!
//! Every stub below loads N argument registers by hand and jumps to a bare
//! address. Nothing in the type system connected that hand-written setup to
//! the callee's declared arity, so appending a parameter to a helper *and* its
//! `helper_fn_slots!` row compiled clean on both sides and the stub passed an
//! uninitialised register as the new argument. `HELPER_FN_SIGS` was built to
//! describe exactly that hazard and had no consumer anywhere in the workspace.
//!
//! The `assert_helper_call_shape!` / `assert_direct_helper_call_shape!`
//! invocations beside each `emit_call_absolute` are that consumer: the count
//! the emitter actually writes is a literal there, and the declared arity comes
//! from the signature table, which is derived from the `HelperFn*` alias rather
//! than transcribed. The two cannot disagree and still build.

use cratonvm_jit_api::assert_helper_call_shape;

use crate::{assert_direct_helper_call_shape, ExecutableBuffer, JitPICSlot};

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const R10: u8 = 10;
const R11: u8 = 11;

#[cfg(target_os = "windows")]
const ENTRY_ABI_REGS: &[u8] = &[1, 2, 8, 9];
#[cfg(not(target_os = "windows"))]
const ENTRY_ABI_REGS: &[u8] = &[7, 6, 2, 1, 8, 9];

#[inline]
fn rex_w(buf: &mut ExecutableBuffer, reg: u8, index: u8, base: u8) {
    let mut rex = 0x48;
    if reg >= 8 {
        rex |= 0x04;
    }
    if index >= 8 {
        rex |= 0x02;
    }
    if base >= 8 {
        rex |= 0x01;
    }
    buf.emit_byte(rex);
}

#[inline]
fn emit_mov_imm64(buf: &mut ExecutableBuffer, reg: u8, value: u64) {
    buf.emit_byte(0x48 | u8::from(reg >= 8));
    buf.emit_byte(0xB8 + (reg & 7));
    buf.emit(&value.to_le_bytes());
}

#[inline]
fn emit_load_frame(buf: &mut ExecutableBuffer, reg: u8, offset: i32) {
    rex_w(buf, reg, 0, 5);
    buf.emit_byte(0x8B);
    buf.emit_byte(0x80 | ((reg & 7) << 3) | 5);
    buf.emit(&(-offset).to_le_bytes());
}

#[inline]
fn emit_jcc(buf: &mut ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
fn emit_jmp(buf: &mut ExecutableBuffer) -> usize {
    buf.emit_byte(0xE9);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
pub(crate) fn patch_rel32_to_here(buf: &mut ExecutableBuffer, patch: usize) {
    let displacement = (buf.pos() as i64) - (patch as i64 + 4);
    if let Ok(displacement) = i32::try_from(displacement) {
        let _ = buf.try_patch_i32(patch, displacement);
    } else {
        buf.mark_codegen_unencodable("rel32-displacement-out-of-range");
    }
}

fn emit_marshal(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    arg_offsets: &[i32],
    needs_context: bool,
) {
    let shift = usize::from(needs_context);
    if needs_context {
        emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    }
    for (index, &offset) in arg_offsets.iter().enumerate() {
        emit_load_frame(buf, ENTRY_ABI_REGS[index + shift], offset);
    }
}

#[inline]
fn emit_mov_reg(buf: &mut ExecutableBuffer, dst: u8, src: u8) {
    rex_w(buf, src, 0, dst);
    buf.emit_byte(0x89);
    buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
}

#[inline]
fn emit_call_absolute(buf: &mut ExecutableBuffer, target: usize) {
    emit_mov_imm64(buf, RAX, target as u64);
    buf.emit(&[0xFF, 0xD0]);
}

pub(crate) fn emit_post_call_frame_republish(buf: &mut ExecutableBuffer, frame_record: usize) {
    // `jit_frame_record(rbp)` — one argument, no result. A result register
    // reserved here would read whatever the callee left in RAX, and the
    // fallback arm below deliberately PUSHes RAX around the call for exactly
    // that reason.
    assert_helper_call_shape!("frame_record", int_args = 1, returns_value = false);
    if frame_record == 0 {
        return;
    }
    // Prefer the inline TLS store: the prologue publishes RBP with one
    // segment-relative MOV into this exact slot, and this is the value-identical
    // inverse. It costs 9 bytes and no call, versus a PUSH/SUB/CALL/ADD/POP
    // sequence — which matters because every raw JIT-to-JIT call site now pays
    // it, and an inline-cache site emits up to five of them.
    let disp = crate::x64::inline_rbp_tls_disp();
    if disp != 0 {
        buf.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        // MOV <seg>:[disp32], RBP — REX.W + 89 /r + ModRM(00,RBP,SIB) + SIB(abs).
        buf.emit(&[0x48, 0x89, 0x2C, 0x25]);
        buf.emit(&(disp as u32).to_le_bytes());
        // …and INVALIDATE the identity half, which this site cannot restore.
        //
        // The two backends' own republish paths rewrite the identity with the
        // caller's compile id, because the method being compiled knows it. This
        // one is shared code emitted into many callers and does not, and the
        // call it follows can have been to a COMPILED callee — `CALL R11` in
        // the hashed megamorphic dispatch below, or a helper that ran arbitrary
        // Java. Such a callee published ITS id on entry. Restoring the caller's
        // RBP while leaving that id would pair this frame with another method's
        // identity, and `innermost_frame_method` would then read oop maps off
        // the wrong method — a confidently WRONG answer, which is far worse
        // than the absent one it replaced, and undetectable downstream because
        // both halves still read consistently out of the mirrors.
        //
        // Zero is the honest value: `lookup_compile_id(0)` is `None`, so the
        // scan falls back to decoding the call exactly as it did before the
        // identity existed. Writing a constant needs no scratch register and no
        // plumbing of the caller's id through five stub emitters.
        let cm_disp = crate::x64::inline_cm_tls_disp();
        if cm_disp != 0 {
            buf.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
            // MOV dword <seg>:[disp32], 0 — C7 /0 + ModRM(00,/0,SIB) + SIB(abs).
            buf.emit(&[0xC7, 0x04, 0x25]);
            buf.emit(&(cm_disp as u32).to_le_bytes());
            buf.emit(&0u32.to_le_bytes());
        }
        return;
    }
    buf.emit_byte(0x50); // PUSH RAX (preserve Java return)
    #[cfg(target_os = "windows")]
    const RESERVE: u8 = 40; // shadow space + alignment after PUSH
    #[cfg(not(target_os = "windows"))]
    const RESERVE: u8 = 8; // restore 16-byte call-site alignment
    buf.emit(&[0x48, 0x83, 0xEC, RESERVE]); // SUB RSP,reserve
    emit_mov_reg(buf, ENTRY_ABI_REGS[0], 5); // ARG0 = RBP
    emit_call_absolute(buf, frame_record);
    buf.emit(&[0x48, 0x83, 0xC4, RESERVE]); // ADD RSP,reserve
    buf.emit_byte(0x58); // POP RAX
}

/// Emit the canonical object-allocation runtime stub used by both JIT tiers.
///
/// The runtime helper owns class initialization, compact-layout sizing, the
/// per-thread TLAB fast path, GC retry, and the zero-on-failure convention.
/// Keeping this ABI sequence here prevents the baseline and optimizing
/// backends from drifting on argument order or post-call frame publication.
pub(crate) fn emit_new_object_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    class_id: u32,
    num_fields: usize,
    frame_record: usize,
) {
    // `jit_new_object(vm_ptr, class_id, num_fields) -> obj | 0`.
    assert_helper_call_shape!("new_object", int_args = 3, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(class_id));
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], num_fields as u64);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// `Op::New` sites that received the inline bump, and those that declined and
/// kept the stub.
///
/// Both, always. A matching checksum on a workload whose allocations all took
/// the stub proves nothing about the bump, and this path is unreachable under a
/// default configuration (`c2_alloc_upgrade_enabled` is opt-in) -- so a zero on
/// the left is the EXPECTED reading, and it has to be distinguishable from
/// "emitted and refused".
static INLINE_TLAB_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STUB_ONLY_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Why the IR inline-TLAB bump last declined, and how often each reason fired.
static INLINE_TLAB_DECLINES: std::sync::Mutex<Option<Vec<(&'static str, u64)>>> =
    std::sync::Mutex::new(None);

fn note_inline_tlab_decline(why: &'static str) {
    let Ok(mut g) = INLINE_TLAB_DECLINES.lock() else {
        return;
    };
    let v = g.get_or_insert_with(Vec::new);
    match v.iter_mut().find(|(k, _)| *k == why) {
        Some((_, n)) => *n += 1,
        None => v.push((why, 1)),
    }
}

/// `(reason, count)` for every inline-TLAB decline this process has seen.
pub fn inline_tlab_declines() -> Vec<(&'static str, u64)> {
    INLINE_TLAB_DECLINES
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

pub(crate) fn note_stub_only_alloc() {
    STUB_ONLY_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(inline bump, stub only)` counts of compiled `Op::New` sites.
pub fn ir_alloc_site_counts() -> (u64, u64) {
    (
        INLINE_TLAB_SITES.load(std::sync::atomic::Ordering::Relaxed),
        STUB_ONLY_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// What [`emit_inline_tlab_new_ir`] needs from its caller, grouped so the call
/// site reads as a contract rather than as nine positional arguments.
pub(crate) struct InlineTlabPlan {
    /// Frame offset of the cached `*mut JvmThread` that `fetch_current_thread`
    /// wrote in the prologue. `0` declines: a thread nobody fetched must not be
    /// dereferenced, which is the same verdict every shadow-stack site reaches
    /// through its own null guard.
    pub thread_slot_off: i32,
    /// `Tlab::cursor` and `Tlab::end` as byte offsets from `&JvmThread`.
    pub cursor_off: i32,
    pub end_off: i32,
    /// `jit_post_tlab_init(vm, obj, class_id, num_fields) -> obj`. `0` declines.
    pub post_init: usize,
    /// Frame offset of the VM context pointer, for that call.
    pub context_off: i32,
    pub class_id: u32,
    pub num_fields: usize,
    /// `zgc_note_tlab_object(vm, obj, size_bytes) -> obj | 0`, called INSTEAD
    /// of `post_init` when non-zero: the class is a post-init no-op (no
    /// primitive default, no finalizer) and the collector requires
    /// registration. `0` keeps `post_init`. Mirrors
    /// `x64::objects::tlab_post_init_mode`'s `ZgcAnnounce` (round 9 wave 8b,
    /// irl8 request 3; the caller only sets it under
    /// `CRATONVM_JIT_IR_ZGC_ANNOUNCE`).
    pub announce: usize,
}

/// Inline TLAB bump allocation for the **optimizing tier**, with
/// [`emit_new_object_stub`] as its slow path.
///
/// Returns `false` without emitting anything when the shape is not admitted, in
/// which case the caller emits the stub alone — exactly the previous behaviour.
///
/// # Why this exists
///
/// `emit_new_object_stub` is three register loads and a `CALL` into
/// `jit_new_object`, which walks the allocator's own path and may collect. The
/// single-pass backend has `x64::objects::emit_inline_tlab_new` and pays no
/// call on the common path, so an escaping allocation compiled *worse* after
/// escape analysis had run on it. That is one of the two independent causes of
/// the July 2026 Binary Trees 4x regression, and the reason
/// `IR_MAX_ALLOCATIONS` is 16 while every neighbouring cap is 64.
///
/// # The size contract, which is the whole difficulty
///
/// `jit_post_tlab_init` derives the object's `shape` and total size from
/// `class_layout(class_id)` **itself**. A caller that sizes the allocation as
/// `HEADER_SIZE + num_fields * SLOT_SIZE` while the class carries a registered
/// compact layout therefore hands the helper a size mismatch and corrupts the
/// heap. The snapshot is taken here from the same two functions the single-pass
/// emitter and the helper both use — `class_layout` and `layout_replace_guard`
/// — so all three agree by construction rather than by inspection.
///
/// A layout can also be **replaced** between compile and execution, which is
/// what the guard emitted first is for: it compares the live field count
/// against the one this compile baked and diverts to the helper on a mismatch,
/// before any state exists to unwind.
///
/// # Ordering: every header write lands BEFORE the cursor commits
///
/// Publishing the cursor first exposes an object whose header is still whatever
/// the TLAB slot held — `class_id = 0` to the GC walker, which then mis-decodes
/// it and steps into its neighbour. The mark word is written
/// **unconditionally**: it stopped being padding when the 24 -> 16 shrink folded
/// `kind`, `element_type`, `gc_age` and `gc_flags` into bits 48..63, and
/// skipping it published whatever the slot held as those four fields — the
/// 2026-08-07 Spring Boot regression (`read_slot: corrupt Value cell` in 178 of
/// 184 classes).
///
/// The cursor is aligned up to 8 before use, because an interleaved array
/// allocation can leave it unaligned and the walker assumes 8-aligned headers.
///
/// # This is a second implementation, and the tests know it
///
/// One shared sequence would be better. The obstacle is that the header-write
/// contract is policed by source scans of `emit_inline_tlab_new`'s own body, so
/// moving that body means rewriting the oracle in the same change as the code it
/// polices.
///
/// **This body is under its own scan**, not the `backend_sources()` one (adding
/// this file there would move the pinned site inventory):
/// `x64::flag_and_header_contracts::ir_inline_tlab_writes_the_whole_header_before_the_commit`
/// requires the class_id, shape, mark-word and gc_flags stores to be
/// unconditional and to precede the cursor commit, the flags byte to follow the
/// mark word, and `GC_FLAG_HEADER` on every arm of the flags value. A divergence
/// from `emit_inline_tlab_new` on any of those fails it.
pub(crate) fn emit_inline_tlab_new_ir(
    buf: &mut ExecutableBuffer,
    plan: &InlineTlabPlan,
    stub_target: usize,
    frame_record: usize,
) -> bool {
    // Name the refusal. This bump is DEFAULT ON since round 9 wave 4
    // (`CRATONVM_JIT_IR_INLINE_TLAB=0` is the kill switch, read by
    // `ir_lower::ir_inline_tlab_enabled`, whose doc has the measurement). It
    // was opt-in from 2026-09-02 for a SIGSEGV that has not reproduced in 90+
    // engaged runs since, and a census across all five collectors once found
    // it emitting ZERO sites in every one of them, with the allocation lowering
    // through the stub instead. A silent `false` is how a feature gets held
    // responsible for blocking another one while never executing.
    let declined = if plan.thread_slot_off <= 0 {
        Some("no frame thread slot")
    } else if plan.post_init == 0 {
        Some("helpers.tlab_post_init is null")
    } else if stub_target == 0 {
        Some("helpers.new_object is null")
    } else if plan.cursor_off == plan.end_off {
        Some("TLAB cursor and end share an offset")
    } else {
        None
    };
    if let Some(why) = declined {
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!("[cratonvm-jitc] ir inline-TLAB bump DECLINED: {why}");
        }
        note_inline_tlab_decline(why);
        return false;
    }

    // The layout snapshot, from the same source the helper reads. `None` means
    // this class allocates with uniform 16-byte cells.
    let compact: Option<(usize, *const u32, u32)> = if cratonvm_types::compact_ref_fields_enabled()
    {
        cratonvm_types::class_layout(plan.class_id)
            .filter(|l| l.field_count() == plan.num_fields)
            .map(|l| {
                let (addr, expected) = cratonvm_types::layout_replace_guard(plan.class_id);
                (l.body_size as usize, addr, expected)
            })
    } else {
        None
    };

    let body = match compact {
        Some((body, _, _)) => body,
        None => match plan.num_fields.checked_mul(cratonvm_types::SLOT_SIZE) {
            Some(b) => b,
            None => return false,
        },
    };
    let Some(total) = body.checked_add(cratonvm_types::HEADER_SIZE) else {
        return false;
    };
    // The allocator hands out 8-aligned runs and the walker steps by them.
    if total % 8 != 0 {
        return false;
    }
    let Ok(total) = i32::try_from(total) else {
        return false;
    };
    // Every header displacement below is encoded as a disp8 — the shape word
    // at `NUM_SLOTS_OFFSET` included, which this test used to leave out.
    if cratonvm_types::GC_FLAGS_BYTE_OFFSET > 127
        || cratonvm_types::MARK_WORD_OFFSET > 127
        || cratonvm_types::NUM_SLOTS_OFFSET > 127
    {
        return false;
    }

    let mut slow: Vec<usize> = Vec::new();

    // Step 0 — layout-replace guard, before any state exists to unwind.
    if let Some((_, count_addr, expected)) = compact {
        emit_mov_imm64(buf, R11, count_addr as u64);
        buf.emit(&[0x41, 0x8B, 0x03]); // MOV EAX, [R11]
        buf.emit_byte(0x3D); // CMP EAX, imm32
        buf.emit(&expected.to_le_bytes());
        slow.push(emit_jcc(buf, 0x85)); // JNE .slow
    }

    // Step 1 — the cached thread. Null means "untracked": take the stub rather
    // than dereference a pointer nobody fetched.
    emit_load_frame(buf, R10, plan.thread_slot_off);
    buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    slow.push(emit_jcc(buf, 0x84)); // JE .slow

    // Step 2 — cursor, aligned up to 8.
    buf.emit(&[0x4D, 0x8B, 0x9A]); // MOV R11, [R10 + disp32]
    buf.emit(&plan.cursor_off.to_le_bytes());
    buf.emit(&[0x49, 0x83, 0xC3, 0x07]); // ADD R11, 7
    buf.emit(&[0x49, 0x83, 0xE3, 0xF8]); // AND R11, -8

    // Step 3 — bump, and refuse if it passes the TLAB end.
    buf.emit(&[0x49, 0x8D, 0x83]); // LEA RAX, [R11 + disp32]
    buf.emit(&total.to_le_bytes());
    buf.emit(&[0x49, 0x3B, 0x82]); // CMP RAX, [R10 + disp32]
    buf.emit(&plan.end_off.to_le_bytes());
    slow.push(emit_jcc(buf, 0x87)); // JA .slow

    // Step 4 — the header, all of it, before the commit below.
    // class_id at offset 0.
    buf.emit(&[0x41, 0xC7, 0x43, 0x00]); // MOV DWORD [R11 + 0], imm32
    buf.emit(&plan.class_id.to_le_bytes());
    // shape at NUM_SLOTS_OFFSET — `num_fields` in BOTH layouts, matching
    // `jit_post_tlab_init`, which writes the same value again idempotently.
    // Cast: a field count is bounded by the class file's own u16 limits.
    let shape = plan.num_fields as u32;
    buf.emit(&[0x41, 0xC7, 0x43, cratonvm_types::NUM_SLOTS_OFFSET as u8]);
    buf.emit(&shape.to_le_bytes());
    // mark_word at MARK_WORD_OFFSET — UNCONDITIONAL. See the doc comment.
    buf.emit(&[0x49, 0xC7, 0x43, cratonvm_types::MARK_WORD_OFFSET as u8]);
    buf.emit(&0i32.to_le_bytes());
    // The gc_flags byte, as a BYTE and AFTER the mark word that would erase it.
    // `gc_age` shares this byte and is 0 at allocation, so writing the whole
    // byte is safe; a dword store here would run past a 16-byte header.
    //
    // UNCONDITIONAL, unlike the compact bit it used to carry alone.
    // `GC_FLAG_HEADER` is what separates a published object header from zeroed
    // arena space, and a JIT-inline `new Object()` — `ClassId(0)`, `shape = 0`,
    // mark word zeroed by the store above — is otherwise sixteen zero bytes
    // that the young non-moving sweep cannot parse and never reclaims. See
    // `cratonvm_types::GC_FLAG_HEADER`. It cannot ride in the mark-word store
    // above the way it does in the single-pass backend's paired dword stores:
    // that store is `MOV QWORD [R11+disp8], imm32`, sign-extended, and bit 59
    // is not encodable as an imm32.
    buf.emit(&[
        0x41,
        0xC6,
        0x43,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
        if compact.is_some() {
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER
        } else {
            cratonvm_types::GC_FLAG_HEADER
        },
    ]);

    // Step 5 — commit, now that the header is walker-coherent.
    buf.emit(&[0x49, 0x89, 0x82]); // MOV [R10 + disp32], RAX
    buf.emit(&plan.cursor_off.to_le_bytes());

    // Step 6 — the cold header work the helper owns: identity hash, primitive
    // defaults, finalizer registration. It returns the object in RAX, which is
    // where both arms converge.
    emit_load_frame(buf, ENTRY_ABI_REGS[0], plan.context_off);
    emit_mov_reg(buf, ENTRY_ABI_REGS[1], R11);
    if plan.announce != 0 {
        // The thin announce helper takes the footprint, which is this
        // function's own bump displacement (`total`, the LEA above) -- the
        // same `HEADER_SIZE + body` `jit_post_tlab_init` would hand
        // `note_tlab_object`.
        emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[2], total);
    } else {
        // Casts: a class id is a u32; a field count is bounded by the class file.
        emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[2], plan.class_id as i32);
        emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[3], plan.num_fields as i32);
    }
    assert_helper_call_shape!("tlab_post_init", int_args = 4, returns_value = true);
    // Round 9 wave 4 (shared contract with lane vm4): the ZGC TLAB-object note
    // the single-pass inline `new` calls after its bump,
    // `zgc_note_tlab_object(vm_ptr, obj_ptr, size_bytes) -> obj_ptr | 0`
    // (`x64/objects.rs`). Pinned beside its sibling so a change to the
    // helper's declared shape fails the build here too.
    assert_helper_call_shape!("zgc_note_tlab_object", int_args = 3, returns_value = true);
    emit_call_absolute(
        buf,
        if plan.announce != 0 {
            plan.announce
        } else {
            plan.post_init
        },
    );
    emit_post_call_frame_republish(buf, frame_record);
    let done = emit_jmp(buf);

    // ── slow path: the shared stub, unchanged ────────────────────────
    for p in slow {
        patch_rel32_to_here(buf, p);
    }
    emit_new_object_stub(
        buf,
        plan.context_off,
        stub_target,
        plan.class_id,
        plan.num_fields,
        frame_record,
    );
    patch_rel32_to_here(buf, done);
    INLINE_TLAB_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// cov-06: emit the array-allocation runtime stub shared by both JIT tiers.
///
/// Same ABI shape as [`emit_new_object_stub`] with one difference: the THIRD
/// argument (the length) is a RUNTIME value read from a frame slot, not an
/// immediate — an array's element count is a JVM operand-stack value, unlike
/// `new`'s field count, which is fixed by the class. `element_type_or_class_id`
/// is the immediate second argument and is a compile-time constant either
/// way: the JVM `newarray` atype tag for a primitive array, or the loaded
/// component class id for an `anewarray`. Matches `jit_newarray(vm, atype,
/// length)` / `jit_anewarray_object(vm, component_class_id, length)`, and the
/// same zero-on-failure convention (`0` = pending exception stashed) as
/// `emit_new_object_stub`'s target.
pub(crate) fn emit_new_array_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    element_type_or_class_id: u32,
    length_offset: i32,
    frame_record: usize,
) {
    // `target` is `jit_newarray(vm, atype, length)` for a primitive array and
    // `jit_anewarray_object(vm, component_class_id, length)` for a reference
    // one. Both are asserted: the site is one sequence serving two slots, so a
    // divergence between them is exactly as dangerous as a change in either.
    assert_helper_call_shape!("newarray", int_args = 3, returns_value = true);
    assert_helper_call_shape!("anewarray_object", int_args = 3, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(element_type_or_class_id));
    emit_load_frame(buf, ENTRY_ABI_REGS[2], length_offset);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// Emit the CONSTANT-POOL-INDEXED object-allocation stub.
///
/// Identical ABI shape to [`emit_new_object_stub`], but the immediates name a
/// `new` site rather than a class: `(vm_ptr, holder_class_id, cp_idx)`. Used
/// when the target class was not loaded at compile time, so the helper must
/// resolve + initialise it on first execution — see
/// `jit_api::JitRuntimeHelpers::new_object_cp` for why compiling the site this
/// way is the fix for the "hot method containing a cold `new` never compiles"
/// gap. Same zero-on-failure convention (`0` = pending exception stashed), so
/// the caller's existing post-alloc sentinel check covers a failed class
/// resolution as well as OOM.
pub(crate) fn emit_new_object_cp_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    holder_class_id: u32,
    cp_idx: u16,
    frame_record: usize,
) {
    emit_cp_indexed_call(
        buf,
        context_offset,
        target,
        holder_class_id,
        cp_idx,
        frame_record,
    );
}

/// Emit the constant-pool-indexed `ldc <Class>` call: `(vm, holder_class_id,
/// cp_idx) -> mirror ObjectRef`, `0` after publishing a pending exception.
///
/// Shares [`emit_cp_indexed_call`] with the deferred-`new` stub because the
/// two helpers deliberately have the same ABI — both defer a class resolution
/// that must not run inside the compiler — but they stay separate entry points
/// so each call site names the helper it actually calls.
pub(crate) fn emit_ldc_class_cp_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    holder_class_id: u32,
    cp_idx: u16,
    frame_record: usize,
) {
    emit_cp_indexed_call(
        buf,
        context_offset,
        target,
        holder_class_id,
        cp_idx,
        frame_record,
    );
}

/// `(vm_ptr, holder_class_id, cp_idx)` in the entry ABI's first three argument
/// registers, an absolute `CALL`, then the post-call frame republish every
/// helper that can run Java (and therefore GC) needs.
fn emit_cp_indexed_call(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    holder_class_id: u32,
    cp_idx: u16,
    frame_record: usize,
) {
    // The two helpers this one sequence serves — `jit_new_object_cp` and
    // `jit_ldc_class_cp` — are deliberately the same shape, which is the whole
    // reason they share an emitter. Assert BOTH, so "deliberately the same"
    // stops being a comment and becomes a build failure when it stops holding.
    assert_helper_call_shape!("new_object_cp", int_args = 3, returns_value = true);
    assert_helper_call_shape!("ldc_class_cp", int_args = 3, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(holder_class_id));
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], u64::from(cp_idx));
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// Emit a VM/object runtime call used for monitor enter/exit.
///
/// Both operands are canonical frame slots, so the call remains valid after
/// register allocation and at GC safepoints. The helper returns a non-sentinel
/// value on success and `i64::MIN` after publishing a pending Java exception.
///
/// # What the return value is, and what it is NOT
///
/// The two tiers disagreed about this, and the disagreement was visible in
/// their comments rather than in anything that could fail. `ir_lower.rs`
/// sentinel-checks RAX **and** stores it back into the receiver's home slot;
/// `x64/op_object.rs` discards RAX entirely and relies on
/// `emit_post_invoke_exception_check`. The `helpers_abi` row asserts the
/// return "is not advisory: a contended acquire can move the object while the
/// thread is parked", which reads as if one of the two tiers were losing a
/// relocation. Written down once, here, because this is the shared seam:
///
/// 1. **The sentinel IS load-bearing, on both tiers.** `jit_monitor_enter`
///    returns `i64::MIN` after publishing a pending Java exception — a null
///    receiver takes the NPE path — and `jit_monitor_exit` does the same for
///    `IllegalMonitorStateException`. The IR tier reads that out of RAX; the
///    single-pass tier reads the same fact out of the thread's pending-signal
///    word via `emit_post_invoke_exception_check`. Two mechanisms, one fact,
///    both sound.
///
/// 2. **The store-back is idempotent, not load-bearing** — *given* that the
///    receiver's live home is named by the map this site publishes.
///    `jit_monitor_enter` answers with `monitor_enter_blocking(..)`'s object,
///    i.e. the post-move address; the collector's
///    `conservative_roots::remap_one_jit_frame` rewrites published slots keyed
///    on their current value and arrives at the same address. So the IR tier's
///    store writes a value the slot already holds. It is worth keeping for the
///    same reason a redundant bounds check is: it is the only arm that does not
///    depend on the publication being right.
///
/// 3. **`monitor_exit`'s return is NOT an object** — it is `1` on success.
///    Storing RAX back after an exit put the address `1` into the receiver's
///    home, which a second `synchronized (this)` in the same method then read
///    as `this` and the oop map published. That is why any store-back must be
///    ENTER-only, and why this shared emitter does not do it today: it serves
///    both opcodes through one `target` and cannot tell them apart.
///
/// The residual this paragraph used to describe is CLOSED (`NOTES-runtime.md`
/// RT-4 item 2). `x64/op_object.rs` used to copy a register-resident receiver
/// into a slot reserved from `SpillReason::HelperArgs` *after*
/// `emit_pre_safepoint_spill()` had taken `pending_live_frame_hi`, which put
/// the word above the live-frame bound — and a word above that bound is not
/// merely unmapped, it is positively claimed dead by
/// `conservative_roots::spill_slot_is_dead_above_cursor`, which drops it from
/// the root set. It was benign only because the receiver had been popped and
/// nothing read the word back, a property of the code rather than a guarantee.
/// The reservation now happens BEFORE the spill and retargets the
/// abstract-stack entry to `StackSlot::Frame`, so `object_offset` is always a
/// word inside the live frame: published by `collect_live_oop_homes` as a
/// `ShadowHome::Frame` the moving collector rewrites in place, and covered by
/// the conservative band scan on the non-moving path. The post-pop arm that
/// used to reserve late is now a refusal, so the ordering cannot be undone
/// without failing the compile.
///
/// That is also what would make mirroring the IR tier's enter-only store-back
/// here sound rather than hazardous, if the symmetry is ever wanted. It is
/// still not done, for the reason in (3): this emitter serves both opcodes
/// through one `target` and cannot tell enter from exit.
pub(crate) fn emit_monitor_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    object_offset: i32,
    target: usize,
    frame_record: usize,
) {
    // `target` is `monitor_enter` or `monitor_exit` — one sequence for two
    // slots. Asserting BOTH is what makes sharing the emitter legitimate: the
    // two are the same shape today and nothing said so in a way that could
    // fail. They appear in both helper tables (the IR tier takes them from
    // `JitRuntimeHelpers`, the single-pass tier from `DirectHelperTable`), so
    // both descriptions are pinned here.
    assert_helper_call_shape!("monitor_enter", int_args = 2, returns_value = true);
    assert_helper_call_shape!("monitor_exit", int_args = 2, returns_value = true);
    assert_direct_helper_call_shape!("monitor_enter", int_args = 2, returns_value = true);
    assert_direct_helper_call_shape!("monitor_exit", int_args = 2, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_load_frame(buf, ENTRY_ABI_REGS[1], object_offset);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// `LEA reg, [RBP - offset]` — the address of a frame slot.
fn emit_lea_frame(buf: &mut ExecutableBuffer, reg: u8, offset: i32) {
    rex_w(buf, reg, 0, 5);
    buf.emit_byte(0x8D);
    buf.emit_byte(0x80 | ((reg & 7) << 3) | 5);
    buf.emit(&(-offset).to_le_bytes());
}

/// `MOV [RBP - offset], reg` — the store twin of [`emit_load_frame`].
fn emit_store_frame(buf: &mut ExecutableBuffer, offset: i32, reg: u8) {
    rex_w(buf, reg, 0, 5);
    buf.emit_byte(0x89);
    buf.emit_byte(0x80 | ((reg & 7) << 3) | 5);
    buf.emit(&(-offset).to_le_bytes());
}

/// Where a call site's shadow-stack publication lives, for a cold path that
/// must read the relocated values back BEFORE the site's own reload runs.
///
/// The offsets are positive `RBP`-relative displacements (`[rbp - off]`), in
/// the order the site's shadow push stored them — the same list and order its
/// reload copies back.
pub(crate) struct ShadowCopyBack<'a> {
    /// Frame word holding the cached thread pointer (`0` in it = untracked).
    pub thread_slot_off: i32,
    /// Frame word holding this push's shadow base (bit 0 set = the push bailed
    /// on overflow and stored nothing).
    pub savebase_slot_off: i32,
    /// The published frame words, in push order.
    pub offsets: &'a [i16],
}

/// Copy a pending shadow publication back into its frame homes WITHOUT
/// retracting the shadow `top`.
///
/// The optimizing tier's `emit_shadow_reload` is copy-back plus retract, and it
/// runs once per call site at the shared post-call continuation. A cold path
/// that reads frame homes before that point (the callee-deopt service) needs
/// the copy-back half early; leaving `top` published keeps the words roots
/// through the service's own call (which can collect again), and the site's
/// real reload then copies the newest values once more and retracts. Copying
/// twice is idempotent.
///
/// Same protocol as the reload: a null thread word skips it (untracked), and a
/// tagged base (bit 0) means the push stored nothing, so nothing is read.
/// Clobbers R10, R11 and RCX; preserves RAX and every other register.
pub(crate) fn emit_shadow_copy_back(buf: &mut ExecutableBuffer, cb: &ShadowCopyBack<'_>) {
    if cb.offsets.is_empty() || cb.thread_slot_off <= 0 || cb.savebase_slot_off <= 0 {
        return;
    }
    emit_load_frame(buf, R10, cb.thread_slot_off); // MOV R10, [rbp - thread]
    buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    let untracked = emit_jcc(buf, 0x84); // JE done
    emit_load_frame(buf, R11, cb.savebase_slot_off); // MOV R11, [rbp - savebase]
    buf.emit(&[0x49, 0xF7, 0xC3]); // TEST R11, imm32
    buf.emit(&1i32.to_le_bytes());
    let bailed = emit_jcc(buf, 0x85); // JNE done
    for &off in cb.offsets {
        // MOV RCX, [R11] ; MOV [rbp - off], RCX ; LEA R11, [R11 + 8]
        buf.emit(&[0x49, 0x8B, 0x0B]);
        emit_store_frame(buf, i32::from(off), RCX);
        buf.emit(&[0x4D, 0x8D, 0x5B, 0x08]);
    }
    patch_rel32_to_here(buf, untracked);
    patch_rel32_to_here(buf, bailed);
}

/// `CRATONVM_JIT_VOLATILE_LOAD_FENCE=1` — put an `MFENCE` back after every
/// volatile field/static LOAD.
///
/// Default OFF since round 9 wave 2. Under x86-TSO a plain load already has
/// the acquire ordering the JMM asks of a volatile read (LoadLoad|LoadStore are
/// no-ops on x86), and the one ordering x86 does NOT give for free —
/// StoreLoad — is the volatile STORE's obligation: every tier emits its
/// `MFENCE` after the volatile write (`x64/op_field.rs`, "SeqCst store-load
/// barrier"). A fence placed after a load cannot supply a missing StoreLoad
/// either, since that fence must sit between the store and the load. So the
/// load-side fence ordered nothing and cost 30-100 cycles per volatile read.
///
/// No compiler fence is needed in its place: neither tier reorders emitted
/// instructions, and the load is emitted where the bytecode (single-pass) or
/// the schedule (IR, where `Op::LoadStatic` advances the memory token like any
/// `MemAccess::Opaque` node) put it.
///
/// One-release kill switch for a memory-model change: `=1` restores the old
/// emission on every tier that consults this reader. The optimizing tier
/// (`ir_lower`, both `Op::LoadStatic` routes) consults it; the single-pass
/// sites (`x64/objects.rs`, `x64/op_field.rs`, `x64/inlining.rs`) are to follow
/// (`docs/internal/jit-review-r9/NOTES-w2-irlower2.md`).
pub(crate) fn volatile_load_fence_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_VOLATILE_LOAD_FENCE")
}

/// `MOV reg, imm32` sign-extended to 64 bits.
fn emit_mov_imm32_sx(buf: &mut ExecutableBuffer, reg: u8, value: i32) {
    rex_w(buf, 0, 0, reg);
    buf.emit_byte(0xC7);
    buf.emit_byte(0xC0 | (reg & 7));
    buf.emit(&value.to_le_bytes());
}

/// After an inline call to a compiled callee: if it returned the `i64::MIN`
/// deopt/exception sentinel, route it through `service_helper` so the callee's
/// stashed frame is resumed at the site that made the call.
///
/// Without this the sentinel reaches the caller's own epilogue as if the CALLER
/// had deopted, and the callee's reconstructed frame is left in the thread's
/// single stash slot for an unrelated sink to mis-attribute — see
/// `jit_service_callee_deopt` in `vm/src/jit/helpers.rs`.
///
/// `deopt_args_base` is the frame offset of outgoing argument 0 in a
/// CONTIGUOUS descending block (argument `i` at `deopt_args_base - i * 8`)
/// holding the same values as `arg_offsets`. It is a separate parameter
/// because it is NOT interchangeable with `arg_offsets[0]`: the helper reads
/// `num_args` consecutive 8-byte slots from the pointer it is handed, and only
/// one of this function's two callers passes a contiguous block as
/// `arg_offsets`.
///
/// The single-pass backend stages its outgoing arguments into one descending
/// block and passes those offsets, so for it the two coincide. The IR lowerer
/// passes each argument's own register-allocated home slot, which is not
/// contiguous with its neighbours -- LEAing `arg_offsets[0]` there made the
/// helper read unrelated frame words as arguments 1..n. That is a wrong-answer
/// bug, not a crash: `jit_service_callee_deopt` rebuilds the callee's frame
/// from those "arguments" to resume its catch block, so the handler ran with
/// garbage in its incoming locals. Spring Boot's
/// `BatchObservabilityBeanPostProcessor.postProcessAfterInitialization` -- a
/// pass-through `BeanPostProcessor` whose `getBean` call always throws
/// `NoSuchBeanDefinitionException` -- resumed its handler with `this` in local
/// 1 and so returned ITSELF instead of the bean it was given, replacing a
/// `Job` singleton with the post-processor
/// (`BatchJdbcAutoConfigurationTests.testDefinesAndLaunchesLocalJob`).
///
/// No-ops when the runtime supplies no helper (hand-built test tables), or
/// when the caller has no contiguous block to name, which reproduces the
/// pre-service behaviour exactly (the sentinel keeps propagating).
///
/// `reload` (round 9 wave 2): when the caller published this call's live
/// references on the shadow stack, the cold side first copies the
/// (possibly relocated) shadow words back into their frame homes and then
/// re-stages every argument whose home is not already its staging word, so
/// the service rebuilds the callee's frame from POST-call values. Without it
/// the staging block the caller wrote before the `CALL` was read as-is, and a
/// reference argument a moving collection relocated during the callee reached
/// the resumed interpreter frame at its old address. `None` emits exactly the
/// sequence this function always emitted (the single-pass caller).
fn emit_callee_deopt_check(
    buf: &mut ExecutableBuffer,
    service_helper: usize,
    info_ptr: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    deopt_args_base: i32,
    reload: Option<&ShadowCopyBack<'_>>,
) {
    if service_helper == 0 || info_ptr == 0 || arg_offsets.is_empty() || deopt_args_base == 0 {
        return;
    }
    // CMP RAX, 1 — OF=1 iff RAX == i64::MIN (`RAX - 1` overflows exactly
    // there). Four bytes and no scratch register, where `MOV R11, imm64;
    // CMP RAX, R11` was thirteen (round 9 wave 2, deopt #3).
    buf.emit(&[0x48, 0x83, 0xF8, 0x01]);
    // JNO over the servicing call.
    let skip = emit_jcc(buf, 0x81);

    // ── cold: RAX is the sentinel, and is overwritten by the service's own
    // result below, so it is free as the transfer scratch.
    if let Some(cb) = reload {
        emit_shadow_copy_back(buf, cb);
        for (i, &home) in arg_offsets.iter().enumerate() {
            // Cast: an argument index is bounded by the ABI register table
            // (checked by the caller), so `i * 8` cannot overflow.
            let stage = deopt_args_base - (i as i32) * 8;
            if home != stage {
                emit_load_frame(buf, RAX, home);
                emit_store_frame(buf, stage, RAX);
            }
        }
    }

    // (vm_ptr, info_ptr, args_ptr, num_args) in the platform C-ABI registers.
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], info_ptr as u64);
    // The caller's contiguous staging block — NOT `arg_offsets[0]`, which is
    // only the same address when the caller happens to stage its arguments in
    // their home slots. See this function's doc comment.
    emit_lea_frame(buf, ENTRY_ABI_REGS[2], deopt_args_base);
    // Cast: argument count to the helper's i32 parameter (bounded by the ABI
    // register table, so it always fits).
    emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[3], arg_offsets.len() as i32);
    assert_helper_call_shape!("service_callee_deopt", int_args = 4, returns_value = true);
    emit_call_absolute(buf, service_helper);

    patch_rel32_to_here(buf, skip);
}

/// Emit the compact two-way hashed/vtable dispatch stub shared by both tiers.
///
/// `arg_offsets[0]` is the receiver.  The generated code hashes its class id
/// into one of eight sets, checks the two immutable-published ways, marshals the
/// selected compiled entry ABI, and calls it directly.  A null receiver, empty
/// entry, or hash miss falls through to the caller's resolving helper.
///
/// Returned rel32 sites are successful-call jumps which the caller patches to
/// its post-call continuation after emitting the slow helper.  All miss
/// branches are patched inside this function to the fall-through position.
pub(crate) fn emit_hashed_vtable_stub(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
) -> Vec<usize> {
    emit_hashed_vtable_stub_reloading(
        buf,
        pic,
        context_offset,
        arg_offsets,
        frame_record,
        service_helper,
        info_ptr,
        deopt_args_base,
        None,
    )
}

/// [`emit_hashed_vtable_stub`] whose callee-deopt service first copies the
/// call's shadow-published references back into their homes and re-stages
/// the arguments from them (see [`emit_callee_deopt_check`]'s `reload`).
///
/// The optimizing tier's caller (`ir_lower::emit_inline_cache_call`) passes
/// its pending shadow set here: its own reload runs later, at the site's
/// shared `.done`, which is too late for a service that rebuilds the callee's
/// interpreter frame from the staging block (round 9 wave 2,
/// `ir-callee-deopt-service-stages-arguments-before-the-shadow-reload`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_hashed_vtable_stub_reloading(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
    reload: Option<&ShadowCopyBack<'_>>,
) -> Vec<usize> {
    // This is a raw JIT-to-JIT call, exactly like the inline MIC/PIC hits.
    // It must obey the same master gate: the resolving helper enters the
    // callee through JitEntryGuard, whereas this stub deliberately bypasses
    // it. Previously CRATONVM_JIT_DIRECT_CALLEE_CALLS=0 disabled only the
    // MIC/PIC cascades and silently left this megamorphic raw edge live, so
    // the moving collector still found an unguarded callee frame.
    if !crate::direct_jit_callee_calls_enabled() {
        return Vec::new();
    }
    // A zero slot base would be baked as `MOV R10, 0` and then dereferenced —
    // `CMP EDX,[R10+RCX*4+96]` — so the FIRST megamorphic dispatch through this
    // site faults inside generated code, with a PC that names this method and an
    // address that names nothing. Both callers filter a null slot today; refusing
    // here means a third one cannot reintroduce that crash by omission. Emitting
    // nothing leaves the site on its resolving helper, which is always correct.
    if pic == 0 {
        return Vec::new();
    }
    if arg_offsets.is_empty()
        || arg_offsets.len() + 1 > ENTRY_ABI_REGS.len()
        || JitPICSlot::MEGA_SET_SHIFT >= 32
    {
        return Vec::new();
    }

    let mut miss_patches = Vec::with_capacity(6);

    // receiver -> RAX, null -> resolving helper, class id -> EDX.
    emit_load_frame(buf, RAX, arg_offsets[0]);
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    miss_patches.push(emit_jcc(buf, 0x84)); // JZ slow

    // Array-receiver guard. The hashed ways below compare only the 4-byte
    // `ObjectHeader.class_id`, which a reference array fills with its COMPONENT
    // class id — so a `Foo[]` receiver matches a way published for a `Foo`
    // receiver and is called into `Foo`'s method body. The kind tag separates
    // them; it lives in the mark word's byte 6 (`KIND_TAGS_BYTE_OFFSET`, 14
    // since the 24 -> 16 header shrink -- not "offset 4", which is the
    // `NUM_SLOTS_OFFSET` shape word). Anything not a plain object takes the
    // resolving helper, which dispatches arrays on `java/lang/Object`.
    //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], ObjectKind::Object
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    buf.emit(&[
        0x80,
        0x78,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::ObjectKind::Object as u8,
    ]);
    miss_patches.push(emit_jcc(buf, 0x85)); // JNE slow
    buf.emit(&[0x8B, 0x10]); // MOV EDX,[RAX]

    // ECX = ((class_id * golden-ratio hash) >> shift) * 2.
    buf.emit(&[0x69, 0xCA]); // IMUL ECX,EDX,imm32
    buf.emit(&JitPICSlot::MEGA_HASH_MULTIPLIER.to_le_bytes());
    buf.emit(&[0xC1, 0xE9, JitPICSlot::MEGA_SET_SHIFT]); // SHR ECX,shift
    buf.emit(&[0xD1, 0xE1]); // SHL ECX,1
    emit_mov_imm64(buf, R10, pic as u64);

    // The two ways share ONE tail (round 9 wave 3, invoke2 X2 -- the shape
    // the inline PIC cascade took in wave 2): each way only decides WHETHER it
    // hits and leaves the entry word in R11; the ABI selection, the
    // marshalling, the `CALL R11`, the frame-record republish and the
    // callee-deopt service are emitted once. Way 0's hit jumps to the tail;
    // way 1's hit falls into it. The per-way zero test stays per way: each
    // rejects the word IT loaded.
    let mut to_tail = None;
    for way in 0..2 {
        if way == 1 {
            buf.emit(&[0xFF, 0xC1]); // INC ECX
        }

        // CMP EDX,[R10 + RCX*4 + mega_class_ids]
        buf.emit(&[0x41, 0x3B, 0x94, 0x8A]);
        buf.emit(&(JitPICSlot::MEGA_CLASS_IDS_OFFSET as i32).to_le_bytes());
        let next_or_miss = emit_jcc(buf, 0x85); // JNE

        // MOV R11,[R10 + RCX*8 + mega_entry_words]; zero -> slow. The word is
        // loaded ONCE: it carries the target and its ABI flag together.
        buf.emit(&[0x4D, 0x8B, 0x9C, 0xCA]);
        buf.emit(&(JitPICSlot::MEGA_ENTRY_PTRS_OFFSET as i32).to_le_bytes());
        buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11,R11
        miss_patches.push(emit_jcc(buf, 0x84));

        if way == 0 {
            to_tail = Some(emit_jmp(buf)); // hit: JMP tail
            patch_rel32_to_here(buf, next_or_miss);
        } else {
            miss_patches.push(next_or_miss);
        }
    }
    // tail: both hits arrive here with the way's entry word in R11.
    if let Some(p) = to_tail {
        patch_rel32_to_here(buf, p);
    }

    // Select the compiled entry ABI from the word just loaded: bit 0 is the
    // needs-context tag (`JIT_IC_NEEDS_CONTEXT_TAG`). BTR moves it into CF
    // and leaves the bare entry in R11, so the ABI and the target cannot
    // come from two different publications of this way.
    const _: () = assert!(crate::JIT_IC_NEEDS_CONTEXT_TAG == 1);
    buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // BTR R11, 0
    let no_context = emit_jcc(buf, 0x83); // JNC
    emit_marshal(buf, context_offset, arg_offsets, true);
    let call = emit_jmp(buf);
    patch_rel32_to_here(buf, no_context);
    emit_marshal(buf, context_offset, arg_offsets, false);
    patch_rel32_to_here(buf, call);

    // R11 is deliberately outside both platform argument-register sets,
    // so marshalling cannot clobber the target loaded above.
    buf.emit(&[0x41, 0xFF, 0xD3]); // CALL R11
    emit_post_call_frame_republish(buf, frame_record);
    emit_callee_deopt_check(
        buf,
        service_helper,
        info_ptr,
        context_offset,
        arg_offsets,
        deopt_args_base,
        reload,
    );
    let done_patches = vec![emit_jmp(buf)];

    for patch in miss_patches {
        patch_rel32_to_here(buf, patch);
    }
    done_patches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_maps_every_class_to_a_valid_two_way_set() {
        for class_id in 1..100_000u32 {
            let base = JitPICSlot::mega_base_index(class_id);
            assert_eq!(base & 1, 0);
            assert!(base + 1 < crate::JIT_MEGA_ENTRIES);
        }
    }

    /// The hashed ways compare only `ObjectHeader.class_id`, which a reference
    /// array fills with its COMPONENT class id — so the stub must reject a
    /// non-object receiver kind before it hashes that word, or a `Foo[]`
    /// receiver is called into a way published for a `Foo` receiver.
    #[test]
    fn hashed_vtable_stub_rejects_array_receivers() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        emit_hashed_vtable_stub(&mut buf, 0x7fff_0000_0000_2000, 24, &[32, 40], 0, 0, 0, 32);
        let bytes = buf.as_slice();
        let guard = [
            0x80u8,
            0x78,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::ObjectKind::Object as u8,
        ];
        let guard_at = bytes
            .windows(guard.len())
            .position(|w| w == guard)
            .expect("the stub must emit CMP BYTE [RAX+kind], Object");
        let hash_at = bytes
            .windows(2)
            .position(|w| w == [0x8B, 0x10])
            .expect("the stub must load the class id into EDX");
        assert!(
            guard_at < hash_at,
            "the kind guard must precede the class-id load (guard@{guard_at}, load@{hash_at})"
        );
    }

    /// `jit_service_callee_deopt` reads `num_args` CONSECUTIVE 8-byte slots
    /// from the pointer this stub hands it and rebuilds the callee's incoming
    /// locals from them, so it must be given the caller's contiguous staging
    /// block. `arg_offsets` is NOT that block for the IR lowerer: those are
    /// each argument's register-allocated home slot, and the words next to
    /// them belong to unrelated values. LEAing `arg_offsets[0]` there resumed
    /// a callee's catch block with `this` in local 1 -- see
    /// [`emit_callee_deopt_check`].
    #[test]
    fn the_deopt_service_addresses_the_staging_block_not_the_first_home_slot() {
        crate::x64::set_moving_young_override(Some(false));
        let helper = 0x7fff_0000_0000_5000usize;
        let stage_base = 512i32;
        let mut buf = ExecutableBuffer::new(8192).expect("buffer");
        // Home slots deliberately NON-contiguous, and a staging base that is
        // neither of them -- the IR lowerer's real shape.
        emit_hashed_vtable_stub(
            &mut buf,
            0x7fff_0000_0000_2000,
            24,
            &[96, 8],
            0,
            helper,
            0x7fff_0000_0000_4000,
            stage_base,
        );
        let bytes = buf.as_slice();
        assert!(
            bytes.windows(8).any(|w| w == (helper as u64).to_le_bytes()),
            "the service helper must be baked, else this test proves nothing"
        );
        // `LEA reg, [RBP + disp32]` is `0x8D` + ModRM(mod=10, rm=101); the
        // loads around it are `0x8B`, so the opcode alone separates them.
        let lea_disp = |want: i32, bytes: &[u8]| {
            bytes
                .windows(6)
                .any(|w| w[0] == 0x8D && (w[1] & 0xC7) == 0x85 && w[2..6] == (-want).to_le_bytes())
        };
        assert!(
            lea_disp(stage_base, bytes),
            "the staging block base must be the argument pointer the helper is handed"
        );
        assert!(
            !lea_disp(96, bytes),
            "arg_offsets[0] must NOT be LEA'd as the argument block base"
        );
    }

    /// …and with no staging block to name, nothing is serviced at all: the
    /// sentinel keeps propagating, which is what happened before the service
    /// existed. Emitting a read of an address the caller never populated is
    /// the one outcome that is worse than not servicing.
    #[test]
    fn a_stub_with_no_staging_block_emits_no_service_call() {
        crate::x64::set_moving_young_override(Some(false));
        let helper = 0x7fff_0000_0000_5000usize;
        let mut buf = ExecutableBuffer::new(8192).expect("buffer");
        emit_hashed_vtable_stub(
            &mut buf,
            0x7fff_0000_0000_2000,
            24,
            &[96, 8],
            0,
            helper,
            0x7fff_0000_0000_4000,
            0,
        );
        assert!(
            !buf.as_slice()
                .windows(8)
                .any(|w| w == (helper as u64).to_le_bytes()),
            "no staging block means no service call"
        );
    }

    /// Round 9 wave 2: with no reload the reloading entry point is
    /// byte-identical to the historical one (the single-pass caller), and the
    /// service's sentinel test is the four-byte `CMP RAX, 1; JNO` rather than
    /// `MOV R11, imm64; CMP RAX, R11; JNE`.
    #[test]
    fn a_stub_without_a_reload_is_the_historical_stub() {
        crate::x64::set_moving_young_override(Some(false));
        let helper = 0x7fff_0000_0000_5000usize;
        let mut a = ExecutableBuffer::new(8192).expect("buffer");
        let mut b = ExecutableBuffer::new(8192).expect("buffer");
        emit_hashed_vtable_stub(
            &mut a,
            0x7fff_0000_0000_2000,
            24,
            &[96, 8],
            0,
            helper,
            0x7fff_0000_0000_4000,
            512,
        );
        emit_hashed_vtable_stub_reloading(
            &mut b,
            0x7fff_0000_0000_2000,
            24,
            &[96, 8],
            0,
            helper,
            0x7fff_0000_0000_4000,
            512,
            None,
        );
        assert_eq!(a.as_slice(), b.as_slice());
        assert!(
            a.as_slice()
                .windows(6)
                .any(|w| w == [0x48, 0x83, 0xF8, 0x01, 0x0F, 0x81]),
            "the service's sentinel test is `CMP RAX, 1; JNO`"
        );
        assert!(
            !a.as_slice().windows(3).any(|w| w == [0x4C, 0x39, 0xD8]),
            "no `CMP RAX, R11` sentinel compare remains"
        );
    }

    /// Round 9 wave 2: with a reload, the cold side copies the shadow words
    /// back into their homes and re-stages each argument from its home BEFORE
    /// it hands the staging block to the service — the service rebuilds the
    /// callee's interpreter frame from that block, so it must hold POST-call
    /// (possibly relocated) references.
    #[test]
    fn a_reloading_stub_copies_back_and_restages_before_it_services() {
        crate::x64::set_moving_young_override(Some(false));
        let helper = 0x7fff_0000_0000_5000usize;
        let stage_base = 512i32;
        let offsets: [i16; 2] = [96, 8];
        let cb = ShadowCopyBack {
            thread_slot_off: 16,
            savebase_slot_off: 24,
            offsets: &offsets,
        };
        let mut buf = ExecutableBuffer::new(8192).expect("buffer");
        emit_hashed_vtable_stub_reloading(
            &mut buf,
            0x7fff_0000_0000_2000,
            24,
            &[96, 8],
            0,
            helper,
            0x7fff_0000_0000_4000,
            stage_base,
            Some(&cb),
        );
        let bytes = buf.as_slice();
        let pos = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
        let copy_at = pos(&[0x49, 0x8B, 0x0B]).expect("MOV RCX, [R11]: the copy-back");
        // MOV [rbp - 512], RAX — argument 0 re-staged from its home at 96.
        let mut restage = vec![0x48, 0x89, 0x85];
        restage.extend_from_slice(&(-stage_base).to_le_bytes());
        let restage_at = pos(&restage).expect("argument 0 is re-staged");
        // `LEA reg, [RBP + disp32]` naming the staging block (the existing
        // staging-block test's matcher).
        let lea_at = bytes
            .windows(6)
            .position(|w| {
                w[0] == 0x8D && (w[1] & 0xC7) == 0x85 && w[2..6] == (-stage_base).to_le_bytes()
            })
            .expect("the service is handed the staging block");
        assert!(
            copy_at < restage_at && restage_at < lea_at,
            "copy back ({copy_at}) < re-stage ({restage_at}) < service ({lea_at})"
        );
    }

    /// A null slot base must emit NOTHING. The alternative is `MOV R10, 0`
    /// followed by a load through it: a fault inside generated code on the first
    /// megamorphic dispatch, at a PC that names this method and an address that
    /// names nothing.
    #[test]
    fn a_null_pic_slot_emits_no_stub() {
        // No `set_moving_young_override` here on purpose: the gate this stub
        // consults is `direct_jit_callee_calls_enabled`, which is env-only and
        // default-ON. Mutating the process-global moving-young override would
        // be a side effect on every concurrently-running test for no benefit.
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        let patches = emit_hashed_vtable_stub(&mut buf, 0, 24, &[32, 40], 0, 0, 0, 32);
        assert!(
            patches.is_empty(),
            "a refused stub must hand back no continuation patches"
        );
        assert_eq!(
            buf.pos(),
            0,
            "not one byte may be emitted for a slot the stub cannot address"
        );
    }

    /// …and the control: a real slot address IS baked, so the guard above is a
    /// refusal of the null case rather than a refusal of everything.
    #[test]
    fn a_real_pic_slot_is_baked_as_the_probe_base() {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        let slot = 0x7fff_0000_0000_2000usize;
        emit_hashed_vtable_stub(&mut buf, slot, 24, &[32, 40], 0, 0, 0, 32);
        assert!(
            buf.as_slice()
                .windows(8)
                .any(|w| w == (slot as u64).to_le_bytes()),
            "the slot address must appear as an imm64 in the emitted probe"
        );
    }

    #[test]
    fn object_allocation_stub_embeds_runtime_target_and_immediates() {
        let target = 0x1122_3344_5566_7788usize;
        let mut buf = ExecutableBuffer::new(256).expect("buffer");
        emit_new_object_stub(&mut buf, 24, target, 0xAABB_CCDD, 17, 0);
        let bytes = buf.as_slice();
        assert!(bytes
            .windows(8)
            .any(|window| window == target.to_le_bytes()));
        assert!(bytes
            .windows(8)
            .any(|window| window == u64::from(0xAABB_CCDDu32).to_le_bytes()));
        assert!(bytes.windows(8).any(|window| window == 17u64.to_le_bytes()));
        assert!(bytes.ends_with(&[0xFF, 0xD0]));
    }

    #[test]
    fn monitor_stub_embeds_target_and_loads_both_frame_operands() {
        let target = 0x8877_6655_4433_2211usize;
        let mut buf = ExecutableBuffer::new(192).expect("buffer");
        emit_monitor_stub(&mut buf, 24, 40, target, 0);
        let bytes = buf.as_slice();
        assert!(bytes
            .windows(8)
            .any(|window| window == target.to_le_bytes()));
        assert_eq!(bytes.iter().filter(|&&byte| byte == 0x8B).count(), 2);
        assert!(bytes.ends_with(&[0xFF, 0xD0]));
    }
}
