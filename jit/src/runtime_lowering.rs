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

use crate::{ExecutableBuffer, JitPICSlot};

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
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(class_id));
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], num_fields as u64);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
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
/// What [`emit_inline_tlab_new_ir`] needs from its caller, grouped so the call
/// site reads as a contract rather than as nine positional arguments.
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
/// polices. Instead this body is held to the *same* oracle: the scans in
/// `x64::flag_and_header_contracts` cover both functions, so a divergence fails
/// them rather than going quietly stale in one of the two.
pub(crate) fn emit_inline_tlab_new_ir(
    buf: &mut ExecutableBuffer,
    plan: &InlineTlabPlan,
    stub_target: usize,
    frame_record: usize,
) -> bool {
    // Name the refusal. This bump is default-ON, carries a documented SIGSEGV
    // risk, and is the stated reason `c2_alloc_upgrade_enabled` stays shut —
    // and a census across all five collectors found it emitting ZERO sites in
    // every one of them, with the allocation lowering through the stub instead.
    // A silent `false` is how a feature gets held responsible for blocking
    // another one while never executing.
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
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
            eprintln!("[cratonvm-jitc] ir inline-TLAB bump DECLINED: {why}");
        }
        note_inline_tlab_decline(why);
        return false;
    }

    // The layout snapshot, from the same source the helper reads. `None` means
    // this class allocates with uniform 16-byte cells.
    let compact: Option<(usize, *const u32, u32)> = if cratonvm_types::compact_ref_fields_enabled() {
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
    // Every header displacement below is encoded as a disp8.
    if cratonvm_types::GC_FLAGS_BYTE_OFFSET > 127 || cratonvm_types::MARK_WORD_OFFSET > 127 {
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
    buf.emit(&[
        0x41,
        0xC7,
        0x43,
        cratonvm_types::NUM_SLOTS_OFFSET as u8,
    ]);
    buf.emit(&shape.to_le_bytes());
    // mark_word at MARK_WORD_OFFSET — UNCONDITIONAL. See the doc comment.
    buf.emit(&[
        0x49,
        0xC7,
        0x43,
        cratonvm_types::MARK_WORD_OFFSET as u8,
    ]);
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
    // Casts: a class id is a u32; a field count is bounded by the class file.
    emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[2], plan.class_id as i32);
    emit_mov_imm32_sx(buf, ENTRY_ABI_REGS[3], plan.num_fields as i32);
    emit_call_absolute(buf, plan.post_init);
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

pub(crate) fn emit_new_array_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    element_type_or_class_id: u32,
    length_offset: i32,
    frame_record: usize,
) {
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
pub(crate) fn emit_monitor_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    object_offset: i32,
    target: usize,
    frame_record: usize,
) {
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_load_frame(buf, ENTRY_ABI_REGS[1], object_offset);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
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
/// `LEA reg, [RBP - offset]` — the address of a frame slot.
fn emit_lea_frame(buf: &mut ExecutableBuffer, reg: u8, offset: i32) {
    rex_w(buf, reg, 0, 5);
    buf.emit_byte(0x8D);
    buf.emit_byte(0x80 | ((reg & 7) << 3) | 5);
    buf.emit(&(-offset).to_le_bytes());
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
fn emit_callee_deopt_check(
    buf: &mut ExecutableBuffer,
    service_helper: usize,
    info_ptr: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    deopt_args_base: i32,
) {
    if service_helper == 0 || info_ptr == 0 || arg_offsets.is_empty() || deopt_args_base == 0 {
        return;
    }
    // MOV R11, imm64(i64::MIN)
    emit_mov_imm64(buf, R11, i64::MIN as u64);
    // CMP RAX, R11 — REX.WR + 39 /r + ModRM(11, R11, RAX)
    buf.emit(&[0x4C, 0x39, 0xD8]);
    // JNE over the servicing call.
    let skip = emit_jcc(buf, 0x85);

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
    emit_call_absolute(buf, service_helper);

    patch_rel32_to_here(buf, skip);
}

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
    let mut done_patches = Vec::with_capacity(2);

    // receiver -> RAX, null -> resolving helper, class id -> EDX.
    emit_load_frame(buf, RAX, arg_offsets[0]);
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    miss_patches.push(emit_jcc(buf, 0x84)); // JZ slow
                                            // Array-receiver guard. The hashed ways below compare only the 4-byte
                                            // `ObjectHeader.class_id`, which a reference array fills with its COMPONENT
                                            // class id — so a `Foo[]` receiver matches a way published for a `Foo`
                                            // receiver and is called into `Foo`'s method body. `ObjectHeader.kind`
                                            // (offset 4) separates them; anything not a plain object takes the
                                            // resolving helper, which dispatches arrays on `java/lang/Object`.
                                            //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], ObjectKind::Object
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
        );
        done_patches.push(emit_jmp(buf));

        if way == 0 {
            patch_rel32_to_here(buf, next_or_miss);
        } else {
            miss_patches.push(next_or_miss);
        }
    }

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
