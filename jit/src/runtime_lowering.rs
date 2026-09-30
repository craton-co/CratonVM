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
//! | the shared `MegaDispatchTable`'s slot base and a way's word (round 12 wave 3) | `emit_mega_dispatch_table_probe` | **not baked** — the base is loaded from the PIC (`MEGA_DISPATCH_TABLE_OFFSET`; the table is owned by the VM's `JitCache` and never moves), and the word is retained by the table's per-way owner |
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

/// Most outgoing stack words the hashed stub marshals (the context ABI's
/// count): the inline caches' own cap, so every site that has a cache can
/// have the stub (round 12 wave 2, lane calls). Since round 13 wave 11 (lane
/// mega9, M13-8) the one shared `inline_cache_pic::ic_max_stack_words`.
fn hashed_stub_max_stack_words() -> usize {
    crate::inline_cache_pic::ic_max_stack_words()
}

/// `CRATONVM_JIT_HASHED_STUB_WIDE` (default on; `0` restores the
/// register-only stub, which refuses every site whose context ABI overflows
/// the argument registers). Read per emitted site, so it adds no process
/// global.
pub(crate) fn hashed_stub_wide_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HASHED_STUB_WIDE")
}

/// `(total_sub, base)` for `words` context-ABI stack words: the layout
/// `ir_lower`'s `stack_arg_block_size` writes and every compiled prologue
/// reads (the Win64 home area below the words, SysV none; 16-aligned so RSP
/// stays aligned at the `CALL`). `None` for no stack word.
fn hashed_stub_stack_block(words: usize) -> Option<(i32, i32)> {
    if words == 0 {
        return None;
    }
    let shadow: i32 = if cfg!(target_os = "windows") { 32 } else { 0 };
    let raw = shadow + i32::try_from(words).ok()? * 8;
    Some(((raw + 15) & !15, shadow))
}

/// `MOV [RSP + disp32], reg`.
fn emit_store_rsp_disp32(buf: &mut ExecutableBuffer, disp: i32, reg: u8) {
    rex_w(buf, reg, 0, 4);
    buf.emit_byte(0x89);
    buf.emit_byte(0x84 | ((reg & 7) << 3)); // mod=10, rm=100 (SIB)
    buf.emit_byte(0x24); // SIB: no index, base RSP
    buf.emit(&disp.to_le_bytes());
}

/// [`emit_marshal`] for a site whose context ABI may overflow the registers.
/// `block == None` is [`emit_marshal`] byte for byte. Otherwise: `SUB RSP,
/// total`, the stack words first through RAX (dead here: it held the
/// receiver, and the target is already in R11), then the context and the
/// register words. The caller releases the block after the `CALL`.
fn emit_marshal_with_stack(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    arg_offsets: &[i32],
    needs_context: bool,
    block: Option<(i32, i32)>,
) {
    let Some((total, base)) = block else {
        emit_marshal(buf, context_offset, arg_offsets, needs_context);
        return;
    };
    buf.emit(&[0x48, 0x81, 0xEC]); // SUB RSP, imm32
    buf.emit(&total.to_le_bytes());
    let lead = usize::from(needs_context);
    for (j, &offset) in arg_offsets.iter().enumerate() {
        let Some(k) = (j + lead).checked_sub(ENTRY_ABI_REGS.len()) else {
            continue;
        };
        emit_load_frame(buf, RAX, offset);
        // Cast: `k < hashed_stub_max_stack_words()`, so `k * 8` is tiny.
        emit_store_rsp_disp32(buf, base + (k as i32) * 8, RAX);
    }
    if needs_context {
        emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    }
    for (j, &offset) in arg_offsets.iter().enumerate() {
        if let Some(&reg) = ENTRY_ABI_REGS.get(j + lead) {
            emit_load_frame(buf, reg, offset);
        }
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
        //
        // The zero is transient: both tiers put their own identity back
        // immediately after every stub that ends here
        // (`emit_identity_after_shared_stub`, in `ir_lower::Lowerer` and in
        // `x64/frames.rs`), so a new caller of these stubs owes the same call
        // (`r11-irlower-single-pass-stubs-also-zero-the-frame-identity`).
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
///
/// Returns the buffer offset just past the helper `CALL`: the return address a
/// stack walk finds for a frame suspended inside the helper. A caller that
/// records an inline-frame row for the site (the optimizing tier, inside a
/// spliced body) keys it on this, not on the end of the stub. The same holds
/// for every stub below that returns a `usize`.
pub(crate) fn emit_new_object_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    class_id: u32,
    num_fields: usize,
    frame_record: usize,
) -> usize {
    // `jit_new_object(vm_ptr, class_id, num_fields) -> obj | 0`.
    assert_helper_call_shape!("new_object", int_args = 3, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(class_id));
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], num_fields as u64);
    emit_call_absolute(buf, target);
    let return_address = buf.pos();
    emit_post_call_frame_republish(buf, frame_record);
    return_address
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

pub(crate) fn note_inline_tlab_decline(why: &'static str) {
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
    /// Emit no post-init call at all: the class needs no primitive default and
    /// no finalizer, and the collector does not require registration, so the
    /// header written inline is the whole of it. Mirrors
    /// `x64::objects::TlabPostInit::Skip`.
    pub skip_post_init: bool,
}

/// Inline TLAB bump allocation for the **optimizing tier**, with
/// [`emit_new_object_stub`] as its slow path.
///
/// Returns `None` without emitting anything when the shape is not admitted, in
/// which case the caller emits the stub alone — exactly the previous behaviour.
/// `Some(ret)` names the return address of the slow path's stub `CALL`, for the
/// caller's inline-frame row (see [`emit_new_object_stub`]'s return value).
///
/// # The class must already be initialised
///
/// Neither `jit_post_tlab_init` nor the ZGC announce helper runs `<clinit>`
/// (JVMS 6.5 makes `new` initialise its class); only the slow path's
/// `jit_new_object` does. The caller therefore reaches this only for a class it
/// proved initialised at compile time (`ir_lower`'s
/// `Lowerer::new_class_proven_initialized`). Initialisation is monotone, so that
/// proof never goes stale.
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
/// The emitter decides the object's shape and bakes its size; everything
/// downstream follows the header it stamps. Since the 8-byte header (dev,
/// 2026-09-24) `jit_post_tlab_init` reads the shape back from the
/// `GC_FLAG_COMPACT` bit this emitter wrote rather than re-deciding it, and a
/// walker sizes a compact instance from its layout's `total_size` (header
/// included) and a legacy one from its shape word. So the snapshot must be
/// the one the rest of the VM would pick for a TLAB object:
/// `compact_tlab_total_size` (the interpreter TLAB's own predicate -- current
/// layout, exact field count, single layout domain) plus
/// `layout_replace_guard`, both shared with the single-pass emitter.
///
/// A layout can also be **replaced** between compile and execution, which is
/// what the guard emitted first is for: it compares the class's live
/// replacement counter against the one this compile baked and diverts to the
/// helper on a mismatch, before any state exists to unwind.
///
/// A compact instance's header is ONE word (class id + mark word): offset 8
/// is its first field. Only a legacy instance gets the shape and hash words
/// written, which is what keeps this emitter from storing the field count
/// over field 0.
///
/// # Ordering: every header write lands BEFORE the cursor commits
///
/// Publishing the cursor first exposes an object whose header is still whatever
/// the TLAB slot held — `class_id = 0` to the GC walker, which then mis-decodes
/// it and steps into its neighbour. The mark word is written
/// **unconditionally**: it stopped being padding when the 24 -> 16 shrink folded
/// `kind`, `element_type`, `gc_age` and `gc_flags` into it (today the mark
/// word's bits 16..31, the object's bytes 6 and 7), and skipping it published
/// whatever the slot held as those four fields — the
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
) -> Option<usize> {
    emit_inline_tlab_new_ir_impl(buf, plan, stub_target, frame_record, None)
}

/// [`emit_inline_tlab_new_ir`]'s fast path ALONE, for a `skip_post_init`
/// plan: the bump, the header and `MOV RAX, R11`, falling through with the
/// object in RAX. The slow-path jumps are appended to `slow` for the CALLER to
/// land, so it can put the allocation's safepoint map (and with it any
/// shadow-stack or frame-block publication) on the slow path only -- the fast
/// path makes no call and cannot collect. Returns `false`, having emitted
/// nothing, for any plan that is not `skip_post_init` or that the full
/// emitter would decline.
pub(crate) fn emit_inline_tlab_fast_path_ir(
    buf: &mut ExecutableBuffer,
    plan: &InlineTlabPlan,
    stub_target: usize,
    slow: &mut Vec<usize>,
) -> bool {
    if !plan.skip_post_init {
        return false;
    }
    emit_inline_tlab_new_ir_impl(buf, plan, stub_target, 0, Some(slow)).is_some()
}

fn emit_inline_tlab_new_ir_impl(
    buf: &mut ExecutableBuffer,
    plan: &InlineTlabPlan,
    stub_target: usize,
    frame_record: usize,
    slow_out: Option<&mut Vec<usize>>,
) -> Option<usize> {
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
        return None;
    }

    // The layout snapshot, from the same source the helper reads. `None` means
    // this class allocates with uniform 16-byte cells.
    //
    // Round 11 wave 15 (lane hdr): `compact_tlab_total_size`, not a bare
    // `class_layout`, for the reason `x64::objects::emit_inline_tlab_new`
    // gives -- the bare lookup is domain-blind, and in a multi-`ClassStore`
    // process it hands this VM another VM's layout for the same class id.
    let compact: Option<(usize, *const u32, u32)> =
        cratonvm_types::compact_tlab_total_size(plan.class_id, plan.num_fields).map(|total| {
            let (addr, expected) = cratonvm_types::layout_replace_guard(plan.class_id);
            (total, addr, expected)
        });

    // A compact instance's layout carries its TOTAL size (8-byte header
    // included); a legacy instance is a 16-byte long header plus its cells.
    let total = match compact {
        Some((total, _, _)) => total,
        None => match plan
            .num_fields
            .checked_mul(cratonvm_types::SLOT_SIZE)
            .and_then(|b| b.checked_add(cratonvm_types::HEADER_SIZE))
        {
            Some(t) => t,
            None => return None,
        },
    };
    // The allocator hands out 8-aligned runs and the walker steps by them.
    if total % 8 != 0 {
        return None;
    }
    let Ok(total) = i32::try_from(total) else {
        return None;
    };
    // Every header displacement below is encoded as a disp8 — the shape word
    // at `NUM_SLOTS_OFFSET` included, which this test used to leave out.
    if cratonvm_types::GC_FLAGS_BYTE_OFFSET > 127
        || cratonvm_types::MARK_WORD_OFFSET > 127
        || cratonvm_types::NUM_SLOTS_OFFSET > 127
    {
        return None;
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
    // The mark word: ONE dword at `MARK_WORD_OFFSET` (the header word's high
    // half), UNCONDITIONAL. NEUTRAL, kind=Object, and the flag byte (its top
    // byte, `GC_FLAGS_BYTE_OFFSET`) holding `GC_FLAG_HEADER` -- the bit that
    // separates a published header from zeroed arena space -- plus
    // `GC_FLAG_COMPACT` for a compact instance.
    let flags = if compact.is_some() {
        cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER
    } else {
        cratonvm_types::GC_FLAG_HEADER
    };
    buf.emit(&[0x41, 0xC7, 0x43, cratonvm_types::MARK_WORD_OFFSET as u8]); // MOV DWORD [R11+d8], imm32
    buf.emit(&((flags as u32) << 24).to_le_bytes());
    // A LEGACY instance's long header: the field count at `NUM_SLOTS_OFFSET`
    // (matching `jit_post_tlab_init`, which writes the same value again
    // idempotently) and a zero identity-hash word. A compact instance has
    // neither -- bytes 8.. are its fields.
    if compact.is_none() {
        // Cast: a field count is bounded by the class file's own u16 limits.
        let shape = plan.num_fields as u32;
        buf.emit(&[0x41, 0xC7, 0x43, cratonvm_types::NUM_SLOTS_OFFSET as u8]);
        buf.emit(&shape.to_le_bytes());
        buf.emit(&[0x41, 0xC7, 0x43, cratonvm_types::IDENTITY_HASH_OFFSET as u8]);
        buf.emit(&0u32.to_le_bytes());
    }

    // Step 5 — commit, now that the header is walker-coherent.
    buf.emit(&[0x49, 0x89, 0x82]); // MOV [R10 + disp32], RAX
    buf.emit(&plan.cursor_off.to_le_bytes());

    if plan.skip_post_init {
        // Nothing for the helper to do (`InlineTlabPlan::skip_post_init`):
        // the object is R11, and both arms converge on RAX.
        emit_mov_reg(buf, RAX, R11);
        if let Some(out) = slow_out {
            // The caller owns the slow path (`emit_inline_tlab_fast_path_ir`).
            // No stub CALL was emitted here, so there is no return address
            // to name: the caller records its own stub's.
            out.extend(slow);
            INLINE_TLAB_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Some(0);
        }
        let done = emit_jmp(buf);
        for p in slow {
            patch_rel32_to_here(buf, p);
        }
        let slow_return = emit_new_object_stub(
            buf,
            plan.context_off,
            stub_target,
            plan.class_id,
            plan.num_fields,
            frame_record,
        );
        patch_rel32_to_here(buf, done);
        INLINE_TLAB_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Some(slow_return);
    }

    // Step 6 — the cold header work the helper owns: identity hash, primitive
    // defaults, finalizer registration. It returns the object in RAX, which is
    // where both arms converge.
    emit_load_frame(buf, ENTRY_ABI_REGS[0], plan.context_off);
    emit_mov_reg(buf, ENTRY_ABI_REGS[1], R11);
    if plan.announce != 0 {
        // The thin announce helper takes the footprint, which is this
        // function's own bump displacement (`total`, the LEA above) -- the
        // same footprint `jit_post_tlab_init` would hand `note_tlab_object`
        // (a compact layout's `total_size`, or the legacy long header plus
        // its cells).
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
    let slow_return = emit_new_object_stub(
        buf,
        plan.context_off,
        stub_target,
        plan.class_id,
        plan.num_fields,
        frame_record,
    );
    patch_rel32_to_here(buf, done);
    INLINE_TLAB_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(slow_return)
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
) -> usize {
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
    let return_address = buf.pos();
    emit_post_call_frame_republish(buf, frame_record);
    return_address
}

/// Round 11 wave 15 (lane hdr): the optimizing tier's inline TLAB bump,
/// EXECUTED against a hand-built thread block and TLAB, on dev's 8-byte object
/// header. The source scans in `x64::flag_and_header_contracts` pin where the
/// header stores sit; these pin what they write: one header word for a compact
/// instance and nothing over its first field, the long header for a legacy
/// one, and not a byte outside the object.
#[cfg(all(test, target_arch = "x86_64"))]
mod r11w15_hdr_inline_tlab_tests {
    use super::*;

    /// No header store writes this pattern: a TLAB word still holding it after
    /// the bump was not touched by the allocator.
    const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    /// Class ids no other test registers a layout for.
    const COMPACT_CID: u32 = 915_150;
    const LEGACY_CID: u32 = 915_151;

    fn plan(class_id: u32, num_fields: usize) -> InlineTlabPlan {
        InlineTlabPlan {
            thread_slot_off: 8,
            cursor_off: 0,
            end_off: 8,
            // Never called: a `skip_post_init` plan emits no call. Non-zero
            // only because the emitter declines a plan without one.
            post_init: 0x1000,
            context_off: 16,
            class_id,
            num_fields,
            announce: 0,
            skip_post_init: true,
        }
    }

    /// `extern "C" fn(thread_block) -> object | 0` around the fast path alone:
    /// the object on the fall-through, `0` on every slow-path edge.
    fn build(plan: &InlineTlabPlan) -> Option<ExecutableBuffer> {
        let mut buf = ExecutableBuffer::new(1024)?;
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 32. The plan's thread slot is [RBP-8].
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x20]);
        emit_store_frame(&mut buf, plan.thread_slot_off, ENTRY_ABI_REGS[0]);
        let mut slow = Vec::new();
        if !emit_inline_tlab_fast_path_ir(&mut buf, plan, 0x1000, &mut slow) {
            return None;
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the object
        for p in slow {
            patch_rel32_to_here(&mut buf, p);
        }
        buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // XOR EAX, EAX; LEAVE; RET
        if buf.overflowed() {
            return None;
        }
        buf.finalize();
        Some(buf)
    }

    /// One allocation from a fresh 64-byte TLAB whose end is `room` bytes past
    /// its start: `(object offset or None for the slow path, bytes the cursor
    /// advanced, the TLAB's words afterwards)`.
    fn allocate(plan: &InlineTlabPlan, room: u64) -> Option<(Option<u64>, u64, [u64; 8])> {
        let buf = build(plan)?;
        let mut tlab = Box::new([UNTOUCHED; 8]);
        let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
        let mut thread = [base, base + room];
        // SAFETY: `buf` was finalized above and holds a complete
        // `extern "C" fn(u64) -> u64`. It reads and writes the two thread
        // words and, at most, the first `room` (<= 64) bytes of `tlab`.
        let run =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
        let obj = run(thread.as_mut_ptr() as u64); // Cast: the thread block's address
        let obj_off = (obj != 0).then_some(obj.wrapping_sub(base));
        Some((obj_off, thread[0].wrapping_sub(base), *tlab))
    }

    /// The dword at byte `off` (4-aligned) of `words`.
    fn dword(words: &[u64; 8], off: usize) -> u32 {
        (words[off / 8] >> (8 * (off % 8))) as u32 // Cast: the selected half
    }

    /// The header word a fresh plain object carries: class id, NEUTRAL mark,
    /// kind `Object`, and `flags` in the mark word's top byte.
    fn header_word(class_id: u32, flags: u8) -> u64 {
        u64::from(class_id) | (u64::from(flags) << (8 * cratonvm_types::GC_FLAGS_BYTE_OFFSET))
    }

    #[test]
    fn a_compact_instance_is_one_header_word_and_its_first_field_is_left_alone() {
        cratonvm_types::register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            COMPACT_CID,
            std::sync::Arc::new(cratonvm_types::CompactLayout {
                field_disps: vec![8],
                is_ref: vec![false],
                field_kinds: vec![cratonvm_types::FieldStorageKind::Int],
                ref_disps: Vec::new(),
                total_size: 16,
            }),
        );
        // Compact fields off, or a second layout domain in this process: the
        // bump allocates the legacy shape, which the legacy test pins.
        if cratonvm_types::compact_tlab_total_size(COMPACT_CID, 1) != Some(16) {
            return;
        }
        let (obj, used, words) = allocate(&plan(COMPACT_CID, 1), 64).expect("the bump emits");
        assert_eq!(obj, Some(0), "the object is the aligned TLAB cursor");
        assert_eq!(used, 16, "a compact instance is its layout's total_size");
        let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
        assert_eq!(words[0], header_word(COMPACT_CID, flags));
        let mark = (words[0] >> 32) as u32; // Cast: the mark word, the header word's high half
        assert!(
            cratonvm_types::ObjectHeader::is_short_mark(mark),
            "the header must read back as a COMPACT (8-byte) header: {mark:#010x}"
        );
        assert!(
            words[1] == UNTOUCHED || words[1] == 0,
            "offset 8 is a compact instance's FIRST FIELD, not a shape word; the \
             allocator wrote {:#018x} over it",
            words[1]
        );
        assert!(
            words[2..].iter().all(|&w| w == UNTOUCHED),
            "nothing past the object may be written: {words:#x?}"
        );
    }

    #[test]
    fn a_legacy_instance_gets_the_long_header() {
        if cratonvm_types::class_layout(LEGACY_CID).is_some() {
            return;
        }
        let (obj, used, words) = allocate(&plan(LEGACY_CID, 1), 64).expect("the bump emits");
        assert_eq!(obj, Some(0));
        assert_eq!(
            used as usize, // Cast: a small byte count
            cratonvm_types::HEADER_SIZE + cratonvm_types::SLOT_SIZE,
            "a legacy instance is the long header plus one 16-byte cell"
        );
        assert_eq!(
            words[0],
            header_word(LEGACY_CID, cratonvm_types::GC_FLAG_HEADER)
        );
        let mark = (words[0] >> 32) as u32; // Cast: the mark word
        assert!(!cratonvm_types::ObjectHeader::is_short_mark(mark));
        assert_eq!(
            dword(&words, cratonvm_types::NUM_SLOTS_OFFSET),
            1,
            "the shape word carries the field count"
        );
        assert_eq!(
            dword(&words, cratonvm_types::IDENTITY_HASH_OFFSET),
            0,
            "and the hash word is written as 'no hash yet', not left stale"
        );
        assert!(
            words[4..].iter().all(|&w| w == UNTOUCHED),
            "nothing past the object may be written: {words:#x?}"
        );
    }

    #[test]
    fn an_exhausted_tlab_takes_the_slow_path_and_writes_nothing() {
        let (obj, used, words) = allocate(&plan(LEGACY_CID, 1), 8).expect("the bump emits");
        assert_eq!(obj, None, "no room: the slow path");
        assert_eq!(used, 0, "the cursor must not move");
        assert!(words.iter().all(|&w| w == UNTOUCHED), "{words:#x?}");
    }
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
) -> usize {
    emit_cp_indexed_call(
        buf,
        context_offset,
        target,
        holder_class_id,
        cp_idx,
        frame_record,
    )
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
) -> usize {
    emit_cp_indexed_call(
        buf,
        context_offset,
        target,
        holder_class_id,
        cp_idx,
        frame_record,
    )
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
) -> usize {
    // The two helpers this one sequence serves — `jit_new_object_cp` and
    // `jit_ldc_class_cp` — are deliberately the same shape, which is the whole
    // reason they share an emitter. Assert BOTH, so "deliberately the same"
    // stops being a comment and becomes a build failure when it stops holding.
    assert_helper_call_shape!("new_object_cp", int_args = 3, returns_value = true);
    assert_helper_call_shape!("ldc_class_cp", int_args = 3, returns_value = true);
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    // The holder id with the code's constant-pool stamp in the high half
    // (`crate::cp_holder_word`), so a body still running after its class was
    // redefined can translate `cp_idx` (interpreter round i1 wave 20, L1).
    emit_mov_imm64(
        buf,
        ENTRY_ABI_REGS[1],
        crate::cp_holder_word(holder_class_id),
    );
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], u64::from(cp_idx));
    emit_call_absolute(buf, target);
    let return_address = buf.pos();
    emit_post_call_frame_republish(buf, frame_record);
    return_address
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
) -> usize {
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
    let return_address = buf.pos();
    emit_post_call_frame_republish(buf, frame_record);
    return_address
}

/// Whether a compiled `monitorenter`/`monitorexit` may take the inline
/// thin-lock path at all. `CRATONVM_MONITOR_FASTPATH=0` (or `false`/`off`)
/// refuses it: the spelling `thread_registry::monitor_fastpath_enabled` uses,
/// so one flag A/Bs the uncontended-monitor fast path in the interpreter and in
/// both JIT tiers (the single-pass tier's `x64/op_object.rs` and the
/// optimizing tier's `ir_lower.rs` both ask this function). Read per site at
/// compile time; the VM
/// also publishes `monitor_block_offset_in_thread = 0` and arms no thread under
/// the flag, so the three agree.
pub(crate) fn inline_thin_lock_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_MONITOR_FASTPATH").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Whether [`emit_inline_thin_lock`] encodes the object header's lock word as
/// it is today. It was `false` from the dev merge of 2026-09-25 (the 8-byte
/// header moved the mark word to a 32-bit dword at offset 4 and made the thin
/// owner a per-VM lease slot whose thin-lock count -- `LockSlots::held` then,
/// the lessee's `LeaseBlock::acquired` since wave 17 -- every thin acquire
/// and release keeps) until round 11 wave 15 re-encoded the sequence against
/// that word; see `docs/internal/fixed-bugs/r11-jit-inline-thin-lock-predates-the-8-byte-header-FIXED-20260925.md`.
/// Both tiers emit through the one function, so this is the one switch; with
/// it `false` every monitor site keeps its helper call, byte for byte.
pub(crate) const INLINE_THIN_LOCK_MATCHES_THE_HEADER: bool = true;

/// `CRATONVM_JIT_INLINE_INFLATED_LOCK=0` (or `false`/`off`) keeps compiled
/// `monitorenter` / `monitorexit` of an INFLATED lock on the helper call, byte
/// for byte as before round 11 wave 17: [`emit_inline_thin_lock`] then sends
/// every word its thin path refuses straight to the helper. Default on. Read
/// per site at compile time.
pub(crate) fn inline_inflated_lock_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_INFLATED_LOCK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// `CRATONVM_JIT_INFLATED_EXIT_PRECHECK=0` (or `false`/`off`) restores the
/// round-11 inflated exit, which released first and, when the release owed a
/// wake, took the monitor back and called the helper. Default on: the exit
/// reads `Monitor::wake_successor`'s decision BEFORE its releasing `XCHG` and
/// hands a release that will owe a wake to the helper still held, without the
/// `XCHG` + `LOCK CMPXCHG` round trip (round 12 wave 1, lane lock, proposal
/// W19-2). Read per site at compile time.
pub(crate) fn inflated_exit_precheck_enabled() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_INFLATED_EXIT_PRECHECK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP=0` (or `false`/`off`/`no`): the
/// compiled lock-stack push / pop brackets its two stores with the seqlock's
/// odd and even `seq` stores (`MOV RAX, [seq]; ADD RAX, 1; MOV [seq], RAX; ..
/// ADD RAX, 1; MOV [seq], RAX`), byte for byte as through round 13 wave 11.
/// Default on (round 13 wave 12, lane monitor3, proposal S7-4): the push /
/// pop is its two data stores followed by ONE `ADD QWORD [seq], 2`, so every
/// compiled `monitorenter` / `monitorexit` executes one load, one store and
/// one ALU op fewer, and `seq` ends each write at the same value as before.
///
/// Why the cross-thread reader (`thread_registry.rs` `JmxLockStack::snapshot`,
/// unchanged: `seq` even, read `top` then the slots below it, `seq` unmoved)
/// still sees only states the owner really had, on x86-TSO, where stores
/// become visible in program order and loads are not reordered with loads:
///
/// * each compiled write is itself prefix-consistent. A push stores the slot
///   at `top` and THEN `top + 1`: a reader that loads the old `top` never
///   reads that slot, and one that loads the new `top` then loads the slot
///   after it and sees the stored object. A pop stores `0` into the slot at
///   `top - 1` and THEN `top - 1`: whichever `top` the reader loads, it sees
///   either the object (before the pop) or the tombstone, which it skips
///   (after the pop);
/// * the trailing bump bounds the reader to ONE partially visible write. For
///   the reader to see any store of a second write, the first write's bump
///   (stored before it in program order) is already visible, so the reader's
///   closing `seq` load differs from its opening one and it retries;
/// * the Rust writers (`publish` / `retract`) keep the odd/even bracket, and
///   one thread never interleaves the two kinds of write, so `seq` is never
///   odd when compiled code bumps it.
///
/// The collector reads every slot under stop-the-world, where no compiled
/// write can be in flight (the sequence has no safepoint): unaffected. Read
/// per site at compile time.
pub(crate) fn lock_stack_single_bump_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP")
}

/// `CRATONVM_JIT_INFLATED_STICKY_COUNT=0` (or `false`/`off`/`no`): the
/// inflated arm's acquisition stores `entry_count = 1` and its final release
/// stores `entry_count = 0`, byte for byte as through round 13 wave 11.
/// Default on (round 13 wave 12, lane monitor3; lane monitor2's proposal M2-5,
/// in the form that needs no representation byte): the final release leaves
/// the count at `1` and the acquisition writes it only when it does not read
/// `1` already, so a hand-over between two compiled critical sections writes
/// nothing but the owner word -- the count's line stays shared in every
/// contender's cache instead of moving to each new owner (an RFO that the
/// releasing `XCHG` had to drain, inside the critical section), as HotSpot's
/// `_recursions` is not written by a hand-over either.
///
/// Sound because the count is exact exactly while the monitor is owned:
/// every acquisition makes it `1` (here, or `Monitor::try_acquire_free`),
/// only the owner writes it, and every reader but the owner's own asks the
/// owner word first (`Monitor::is_idle` and `MonitorTable::entry_count` read
/// "unowned" as 0; round 13 wave 12). An unowned monitor's count reads 0 (a
/// Rust release, `wait`, a forced release) or 1 (a compiled release), and
/// both are "unowned". No fence moves: the count is owner-private and ordered
/// by the owner word's acquiring CAS and releasing `XCHG`, which are
/// unchanged. Sites compiled with and without the switch interoperate, since
/// each writes a count the other reads correctly. Read per site at compile
/// time.
pub(crate) fn inflated_sticky_count_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INFLATED_STICKY_COUNT")
}

/// `CRATONVM_JIT_SELF_LOCK_TRIM=0` (or `false`/`off`/`no`): a self-locking
/// body's own monitor enter / release (`x64/frames.rs`) tests its object for
/// null like any `monitorenter`, byte for byte as through round 13 wave 11.
/// Default on (round 13 wave 12, lane monitor3, proposal S7-5 part (a)): the
/// object is `this` of an instance method (local 0, which the admission
/// proves is never stored: `lib.rs` `self_lock_bytecode_admitted`) or the
/// class mirror of a `static synchronized` one (re-read from the VM's mirror
/// slot, which is never empty), so the `TEST R11, R11; JE slow` pair can
/// never branch and is not emitted ([`emit_inline_self_lock`]). Read per
/// site at compile time.
pub(crate) fn self_lock_trim_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SELF_LOCK_TRIM")
}

// Layout of the VM's per-lease words (`LeaseBlock`, `vm/src/threading/
// monitor.rs`), which `JitMonitorBlock::held` points at. The VM pins every
// one of these against its struct with `offset_of!` assertions; they live
// here because this crate bakes them and cannot name the VM's types.

/// `LeaseBlock::acquired`: the lessee's thin-lock count (`u32`), which the
/// inline thin path adds to / subtracts from. Must stay 0: the emitted `ADD` /
/// `SUB` addresses `[held]` with no displacement.
pub const LEASE_BLOCK_ACQUIRED_OFFSET: usize = 0;
/// `LeaseBlock::owner_word`: `ThreadId + 1` of the lessee, what an inflated
/// monitor it owns holds in its owner word (`u64`).
pub const LEASE_BLOCK_OWNER_WORD_OFFSET: usize = 8;
/// `LeaseBlock::inflated_key`: object address of the cached inflated monitor,
/// `0` = empty (`u64`).
pub const LEASE_BLOCK_KEY_OFFSET: usize = 16;
/// `LeaseBlock::inflated_monitor`: the cached `*const Monitor` (`u64`).
pub const LEASE_BLOCK_MONITOR_OFFSET: usize = 24;
/// `LeaseBlock::inflated_epoch`: the monitor index epoch the entry is valid
/// at (`u64`).
pub const LEASE_BLOCK_EPOCH_OFFSET: usize = 32;
/// `LeaseBlock::epoch_addr`: address of the table's current index epoch
/// (`*const AtomicU64`).
pub const LEASE_BLOCK_EPOCH_ADDR_OFFSET: usize = 40;
/// `LeaseBlock::inflated_key2`: object address of the cache's second way,
/// valid at the same `inflated_epoch`, `0` = empty (`u64`). Round 13 wave 10.
pub const LEASE_BLOCK_KEY2_OFFSET: usize = 48;
/// `LeaseBlock::inflated_monitor2`: the second way's `*const Monitor` (`u64`).
pub const LEASE_BLOCK_MONITOR2_OFFSET: usize = 56;

/// `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY=0` (or `false`/`off`/`no`): the
/// inflated arm ([`emit_inline_inflated_arm`]) looks its receiver up in the
/// lease cache's first way only, byte for byte as through round 13 wave 9.
/// Default on (round 13 wave 10, lane monitor2, lock proposal W17-3): a
/// receiver the first way does not name is looked for in the second
/// (`LeaseBlock::inflated_key2`, the entry the last fill displaced, kept by
/// the VM only while valid at the block's one epoch), so a thread
/// alternating between two inflated monitors -- nested `synchronized` on two
/// hashed or once-contended objects -- takes both inline instead of calling
/// the helper for every enter of the other one. Same epoch check, same
/// liveness argument; three more instructions on a first-way miss only.
/// Read per site at compile time.
pub(crate) fn inline_inflated_two_way_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_INFLATED_TWO_WAY")
}

// Layout of the hand-over words of the VM's inflated `Monitor`
// (`#[repr(C, align(64))]`, `vm/src/threading/monitor.rs`), pinned there the
// same way.

/// `Monitor::owner`: `0` free, else the owner's `ThreadId + 1` (`u64`).
pub const INFLATED_MONITOR_OWNER_OFFSET: usize = 0;
/// `Monitor::thin_seed`: non-zero while the owner is the one a thin lock's
/// inflation seeded (`u64`); its release is the helper's.
pub const INFLATED_MONITOR_THIN_SEED_OFFSET: usize = 8;
/// `Monitor::entry_count`: the owner's re-entry count (`u32`). On a line of
/// its own since round 12 wave 8 (off the owner word's line, which every
/// spinner polls), so it is addressed with a disp32. Exact while the monitor
/// is owned; an unowned monitor's reads 0 or 1 (round 13 wave 12,
/// [`inflated_sticky_count_enabled`]).
pub const INFLATED_MONITOR_ENTRY_COUNT_OFFSET: usize = 128;
/// `Monitor::jfr_enter_recorded` (`bool`); set, the release is the helper's.
pub const INFLATED_MONITOR_JFR_OFFSET: usize = 20;
// Since round 11 wave 19 the three words contenders WRITE sit on the
// monitor's second cache line (the "hand-off line"), away from the owner
// word every spinner polls; see `Monitor`'s `_owner_line_pad`.
/// `Monitor::entry_waiters`: threads registered to park for entry (`u32`).
pub const INFLATED_MONITOR_ENTRY_WAITERS_OFFSET: usize = 64;
/// `Monitor::spinners`: threads spinning for entry (`u32`).
pub const INFLATED_MONITOR_SPINNERS_OFFSET: usize = 68;
/// `Monitor::succ_pending`: a woken entrant is on its way (`bool`).
pub const INFLATED_MONITOR_SUCC_PENDING_OFFSET: usize = 72;
/// `Monitor::spin_limit`: the monitor's adaptive spin budget (`u32`), which
/// the inline spin ([`inline_inflated_spin_enabled`]) reads and never writes.
pub const INFLATED_MONITOR_SPIN_LIMIT_OFFSET: usize = 76;
/// The VM's `SPIN_MAX`, the ceiling every `spin_limit` store clamps to; the
/// inline spin refuses a budget above it (or of zero) rather than trust it.
pub const INFLATED_MONITOR_SPIN_MAX: u32 = 1024;
/// Round 14 wave 1 (lane sync, M2-1): `Monitor::census_inline_spin_wins`
/// (`u64`), on the entry count's line (the census perturbs what it counts;
/// written only by compiled sites built under `CRATONVM_DBG_JITC`). The
/// inline spin's three outcomes, drained into the VM's contention census
/// (`CRATONVM_DBG_MONITOR_CONTENTION`) by the helper path.
pub const INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET: usize = 136;
/// `Monitor::census_inline_spin_budget_outs` (`u64`); see above.
pub const INFLATED_MONITOR_CENSUS_SPIN_BUDGET_OUTS_OFFSET: usize = 144;
/// `Monitor::census_inline_spin_waiter_exits` (`u64`); see above.
pub const INFLATED_MONITOR_CENSUS_SPIN_WAITER_EXITS_OFFSET: usize = 152;

/// `CRATONVM_JIT_INLINE_INFLATED_SPIN=0` (or `false`/`off`/`no`): a compiled
/// `monitorenter` that finds an inflated monitor its lease caches OWNED by
/// another thread goes straight to the helper, byte for byte as before round
/// 12 wave 2. Default on (lane lock2): it first spins inline -- `PAUSE`,
/// poll the owner word, up to the monitor's adaptive budget `spin_limit` --
/// and takes the monitor with the same `LOCK CMPXCHG` as the free case,
/// joining the same lock-stack push. The census of `ThreadChurn 192 20000 1`
/// counted 3 392 443 contended enters won by the helper's spin
/// (`Monitor::spin_try_enter_adaptive`) against 189 parks: every one of them
/// paid the helper call, the Rust spin's bookkeeping and the JMX publish,
/// and the part after the winning CAS (the helper's return path) ran INSIDE
/// the critical section 47 other threads were queued behind. An inline spin
/// that runs its budget out still takes the helper, which spins adaptively
/// and parks exactly as before.
///
/// Sound for the reasons the rest of the arm is: the monitor pointer is live
/// for as long as no stop-the-world pass completes, and the sequence has no
/// safepoint (a pause waits at most the budget, `SPIN_MAX` `PAUSE`s, as it
/// waits out the helper's own adaptive spin); the inline spinner is never
/// counted in `Monitor::spinners`, so no release skips a wake-up because of
/// it and it owes none when it gives up. Off on a one-CPU host, where the
/// owner cannot run while this spins (the helper's spins are off there too).
/// Read per site at compile time.
pub(crate) fn inline_inflated_spin_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_INFLATED_SPIN")
        && std::thread::available_parallelism().is_ok_and(|n| n.get() > 1)
}

/// `PAUSE`s the inline monitor spin skips after LOSING a free monitor to
/// another thread (its `LOCK CMPXCHG` failed), each charged to the budget
/// ([`inline_spin_backoff_enabled`]).
pub(crate) const INLINE_SPIN_LOST_CAS_BACKOFF: u32 = 16;

/// `CRATONVM_JIT_INLINE_SPIN_BACKOFF=0` (or `false`/`off`/`no`): the inline
/// spin polls again right after a lost CAS, byte for byte as in round 12
/// wave 2. Default on (round 12 wave 8, lane monitor, lock proposal W2L2-2):
/// after a lost race it executes [`INLINE_SPIN_LOST_CAS_BACKOFF`] `PAUSE`s
/// without touching the monitor, each decrementing the budget (so the spin's
/// bound, `SPIN_MAX` `PAUSE`s, is unchanged), then polls again. The winner
/// now holds the monitor for at least its critical section, and a loser that
/// re-polls at once only keeps the owner line shared by one more core when
/// the winner writes it. Unobservable (it changes only when a spinner looks
/// again). Read per site at compile time.
pub(crate) fn inline_spin_backoff_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_SPIN_BACKOFF")
}

/// `CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION=0` (or `false`/`off`/`no`): a
/// compiled `monitorenter` / `monitorexit` of an object this thread already
/// holds THIN goes to the helper both ways, byte for byte as before round 13
/// wave 8. Default on (lane sync5, proposals S3-1 / S4-1): the recursion
/// count lives in the mark word itself (`THIN_LOCK_RECURSION_MASK`, 0..=7),
/// so a re-entry is `try_thin_recursive_lock`'s one CAS (`recursion + 1`)
/// and a non-final exit `try_thin_unlock`'s (`recursion - 1`), inline
/// ([`emit_inline_thin_recursion_arm`]). Nested synchronized calls on one
/// receiver -- a self-locking body called from a synchronized caller, or
/// under the interpreter door's hold -- paid two helper round trips per
/// level. Read per site at compile time.
pub(crate) fn inline_thin_lock_recursion_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION")
}

/// `CRATONVM_JIT_INLINE_INFLATED_RECURSION=0` (or `false`/`off`/`no`): a
/// compiled re-entry on, or non-final exit of, an INFLATED monitor this thread
/// owns goes to the helper, byte for byte as before round 13 wave 8. Default on
/// (lane sync5): the inflated arm ([`emit_inline_inflated_arm`]) does what
/// `Monitor::try_enter` / `Monitor::exit_reporting_release` do for the owner --
/// `entry_count + 1` / `entry_count - 1` on the owner-private count line, no
/// owner-word write, no wake decision (nothing is released). Only with the
/// inflated arm itself on. Read per site at compile time.
pub(crate) fn inline_inflated_recursion_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_INFLATED_RECURSION")
}

/// Mark-word bits of an object's `kind` tag (`KIND_TAG_BYTE_MASK` of the byte
/// at `KIND_TAGS_BYTE_OFFSET`, seen through the 32-bit mark at
/// `MARK_WORD_OFFSET`).
const MARK_KIND_BITS: u32 = (cratonvm_types::KIND_TAG_BYTE_MASK as u32)
    << ((cratonvm_types::KIND_TAGS_BYTE_OFFSET - cratonvm_types::MARK_WORD_OFFSET) * 8);
/// Mark-word bit of `GC_FLAG_COMPACT` (the byte at `GC_FLAGS_BYTE_OFFSET`).
const MARK_COMPACT_BIT: u32 = (cratonvm_types::GC_FLAG_COMPACT as u32)
    << ((cratonvm_types::GC_FLAGS_BYTE_OFFSET - cratonvm_types::MARK_WORD_OFFSET) * 8);
/// The state field and its payload: zero exactly when the word is NEUTRAL and
/// carries no in-payload identity hash.
const MARK_LOCK_BITS: u32 = cratonvm_types::MARK_STATE_MASK | cratonvm_types::MARK_PAYLOAD_MASK;
// The two bytes the kind / compact tests read lie inside the mark word, and the
// lock bits are its low 16, so `OR`ing owner bits into an observed word keeps
// its quartet (`ObjectHeader::make_thin_locked`), and `XOR`ing them back out
// of a recursion-0 word leaves exactly `ObjectHeader::make_neutral`.
const _: () = assert!(
    cratonvm_types::KIND_TAGS_BYTE_OFFSET >= cratonvm_types::MARK_WORD_OFFSET
        && cratonvm_types::KIND_TAGS_BYTE_OFFSET < cratonvm_types::MARK_WORD_OFFSET + 4
        && cratonvm_types::GC_FLAGS_BYTE_OFFSET >= cratonvm_types::MARK_WORD_OFFSET
        && cratonvm_types::GC_FLAGS_BYTE_OFFSET < cratonvm_types::MARK_WORD_OFFSET + 4
);
const _: () = assert!(MARK_LOCK_BITS == 0xFFFF);
const _: () = assert!(MARK_LOCK_BITS & cratonvm_types::MARK_QUARTET_MASK == 0);
const _: () = assert!(
    (cratonvm_types::THIN_LOCK_OWNER_MASK | cratonvm_types::MARK_STATE_MASK) & !MARK_LOCK_BITS
        == 0
);
const _: () = assert!(
    (MARK_KIND_BITS | MARK_COMPACT_BIT | cratonvm_types::MARK_SHORT_HASH_HI_MASK)
        & !cratonvm_types::MARK_QUARTET_MASK
        == 0
);

/// Where [`emit_inline_thin_lock`] finds this thread's `*mut JvmThread`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ThinLockThread {
    /// `MOV R10, <seg>:[disp32]` through the `JIT_THREAD` mirror
    /// (`crate::x64::jit_thread_tls_disp`), which is what the helpers'
    /// `jit_thread_mut()` reads. Independent of any prologue fetch, so it stays
    /// valid in a method whose lazy thread fetch was erased.
    Tls(u32),
    /// `MOV R10, [RBP - off]`: a frame word holding the thread, `0` meaning
    /// untracked (the sequence then takes the helper).
    Frame(i32),
}

/// Inline uncontended `monitorenter` (`enter`) / `monitorexit` on the receiver
/// in `[RBP - obj_off]`: the helper's thin-lock fast path
/// (`monitor::try_thin_lock` / `try_thin_unlock` plus `LockSlots::note_acquired`
/// / `note_owner_released`) and its JMX lock-stack publish / retract, for BOTH tiers
/// -- the optimizing tier (`ir_lower.rs`) and the single-pass tier
/// (`x64/op_object.rs`) call this one emitter. Contract: page
/// `r11w5-sync-inline-thin-lock-contract-patch-FIXED-20260924`, re-encoded for
/// the 8-byte object header in round 11 wave 15.
///
/// Emits the fast path followed by `JMP done` and returns the `done` patch
/// site. Every refusal branches to the position right after that `JMP`, which
/// is where the caller emits its unchanged helper sequence (the slow path); the
/// caller patches `done` to the point where both paths agree again. `Err` names
/// why nothing was emitted (not a single byte), for the caller's census.
///
/// The lock word is the 32-bit mark at `MARK_WORD_OFFSET` (4). A thin lock
/// names its owner by the thread's per-VM lock-slot LEASE, which
/// `JitMonitorBlock::thin_owner` publishes pre-shifted with the state bits
/// (`MARK_THIN_LOCKED | slot << THIN_LOCK_OWNER_SHIFT`, what
/// `ObjectHeader::make_thin_locked(0, slot, 0)` answers).
///
/// Taken only when ALL of these hold at run time; anything else is the helper,
/// which handles it exactly as before:
///
/// * the receiver is non-null (null: the helper raises the NPE);
/// * the thread is known and ARMED (`JitMonitorBlock::thin_owner != u64::MAX`:
///   armed by the thread's first helper-path `monitorenter` once it holds a
///   lease, never under `CRATONVM_MONITOR_FASTPATH=0`);
/// * enter: the mark word is `ObjectHeader::thin_lockable` -- its low 16 bits
///   (state + payload) are zero and, for a compact plain instance (`kind` 0,
///   `GC_FLAG_COMPACT`), its high identity-hash bits are zero too -- and the
///   lock stack has a free slot and has not spilled. The new word is the
///   observed one ORed with the owner bits: the quartet rides along, exactly
///   `make_thin_locked(mark, slot, 0)`;
/// * exit: the lock stack's top slot names this receiver and the mark word's
///   low 16 bits are exactly the owner bits (thin-locked by this lease at
///   recursion 0); the new word is the observed one XORed with them, i.e. the
///   quartet alone, exactly `make_neutral(mark)`.
///
/// Every winning CAS is followed by the lease-count bookkeeping the helper
/// does: `ADD DWORD [held], 1` after an acquire, `SUB DWORD [held], 1` after a
/// final release, through `JitMonitorBlock::held` (the lessee's
/// `LeaseBlock::acquired`; no `LOCK` since round 11 wave 17, the word has one
/// writer). That is what keeps the count exact -- one increment per NEUTRAL
/// -> THIN_LOCKED transition, one decrement per release, and an inflation
/// uncounts in the VM's separate `stolen` word -- whichever path made each;
/// `LockSlots::release` gives a dying thread's lease back only when the net
/// count reads 0, so a skipped increment would let a second thread lease the
/// slot and "own" this thread's lock.
///
/// A word the thin path refuses goes on to the INFLATED arm
/// ([`emit_inline_inflated_arm`], round 11 wave 17) unless
/// `CRATONVM_JIT_INLINE_INFLATED_LOCK=0`: an inflated monitor that this
/// thread's lease cache names at the current index epoch is entered with one
/// `CMPXCHG` on its owner word and left with one `XCHG`, and joins the same
/// lock-stack push / pop.
///
/// Enter pushes AFTER its CAS and exit pops AFTER its CAS, so a failed CAS
/// leaves the lock stack and the count untouched for the helper; the push/pop
/// is the single-writer seqlock write (`seq` odd, slot, `top`, `seq` even),
/// plain stores ordered by x86-TSO -- since round 13 wave 12 slot, `top`,
/// `seq += 2` ([`lock_stack_single_bump_enabled`]). No call: no safepoint,
/// oop map or shadow publication is needed on this path.
///
/// Clobbers RAX, RCX, RDX, R10 and R11, all caller-saved on both ABIs and all
/// destroyed by the helper CALL this path replaces.
///
/// `census` (the caller passes `crate::x64::dbg_jitc_enabled()`) adds the
/// monitor census bumps (lock proposal W7-2):
/// `ADD QWORD [thread + block + JIT_MONITOR_BLOCK_INLINE_*], 1` before
/// `JMP done`, and `..._SLOW_*` at the slow label, where the thread is
/// re-read into RAX and tested because a refusal may have been "untracked
/// thread". Owner-written counters, no `LOCK`; the VM prints them as
/// `[monitor-inline census]`. With `census == false` not a byte changes.
///
/// Round 13 wave 8 (lane sync5): a word thin-locked by THIS lease is taken
/// by the recursion arm ([`emit_inline_thin_recursion_arm`],
/// [`inline_thin_lock_recursion_enabled`]) before the inflated arm.
pub(crate) fn emit_inline_thin_lock(
    buf: &mut ExecutableBuffer,
    enter: bool,
    obj_off: i32,
    monitor_block_off: usize,
    thread: ThinLockThread,
    census: bool,
) -> Result<usize, &'static str> {
    emit_inline_thin_lock_with(
        buf,
        enter,
        obj_off,
        monitor_block_off,
        thread,
        census,
        InlineRecursion {
            thin: inline_thin_lock_recursion_enabled(),
            inflated: inline_inflated_recursion_enabled(),
            two_way: inline_inflated_two_way_enabled(),
        },
        false,
    )
}

/// [`emit_inline_thin_lock`] for an object the caller proves is never null
/// (round 13 wave 12, lane monitor3, proposal S7-5 (a)): a self-locking
/// body's own monitor, `this` or its class mirror (`x64/frames.rs`,
/// `x64/op_object.rs` for the release at a `*return`). With
/// [`self_lock_trim_enabled`] the sequence has no null test; otherwise, and
/// in every other respect, it is [`emit_inline_thin_lock`]'s.
pub(crate) fn emit_inline_self_lock(
    buf: &mut ExecutableBuffer,
    enter: bool,
    obj_off: i32,
    monitor_block_off: usize,
    thread: ThinLockThread,
    census: bool,
) -> Result<usize, &'static str> {
    emit_inline_thin_lock_with(
        buf,
        enter,
        obj_off,
        monitor_block_off,
        thread,
        census,
        InlineRecursion {
            thin: inline_thin_lock_recursion_enabled(),
            inflated: inline_inflated_recursion_enabled(),
            two_way: inline_inflated_two_way_enabled(),
        },
        self_lock_trim_enabled(),
    )
}

/// Which re-entry arms [`emit_inline_thin_lock_with`] emits (round 13 wave 8,
/// lane sync5): [`inline_thin_lock_recursion_enabled`] and
/// [`inline_inflated_recursion_enabled`] in production; pinned by the
/// executed tests. Since round 13 wave 10 (lane monitor2) also whether the
/// inflated arm probes the lease cache's second way
/// ([`inline_inflated_two_way_enabled`]).
#[derive(Clone, Copy, Debug)]
struct InlineRecursion {
    thin: bool,
    inflated: bool,
    two_way: bool,
}

/// [`emit_inline_thin_lock`] with the re-entry arms' switches passed in
/// (`recursion`), so the executed tests can pin either shape whatever the
/// environment says. `receiver_non_null` (round 13 wave 12): the caller
/// proves the object non-null, so the null test is not emitted
/// ([`emit_inline_self_lock`]).
#[allow(clippy::too_many_arguments)]
fn emit_inline_thin_lock_with(
    buf: &mut ExecutableBuffer,
    enter: bool,
    obj_off: i32,
    monitor_block_off: usize,
    thread: ThinLockThread,
    census: bool,
    recursion: InlineRecursion,
    receiver_non_null: bool,
) -> Result<usize, &'static str> {
    let thin_recursion = recursion.thin;
    use cratonvm_jit_api::{
        JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS, JIT_LOCK_STACK_SLOTS_OFFSET,
        JIT_LOCK_STACK_SPILLED_OFFSET, JIT_LOCK_STACK_TOP_OFFSET, JIT_MONITOR_BLOCK_HELD_OFFSET,
        JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET,
        JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET, JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET,
        JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET, JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET,
    };
    if !INLINE_THIN_LOCK_MATCHES_THE_HEADER {
        return Err("inline thin lock predates the object header (disabled)");
    }
    // Every displacement hand-encoded below as a disp8 is pinned here.
    const _: () = assert!(JIT_LOCK_STACK_SLOTS_OFFSET >= 8 && JIT_LOCK_STACK_SLOTS_OFFSET < 128);
    const _: () = assert!(JIT_LOCK_STACK_SLOTS < 128);
    const _: () = assert!(cratonvm_types::MARK_WORD_OFFSET < 128);
    const POP_DISP: usize = JIT_LOCK_STACK_SLOTS_OFFSET - 8;
    // Cast: pinned < 128 just above.
    let mark_disp = cratonvm_types::MARK_WORD_OFFSET as u8;

    if obj_off <= 0 {
        return Err("receiver has no frame home");
    }
    // 0 = not wired (hand-built tables; the VM also publishes 0 under
    // `CRATONVM_MONITOR_FASTPATH=0`).
    let block = i32::try_from(monitor_block_off)
        .ok()
        .filter(|&b| b > 0)
        .ok_or("helpers.monitor_block_offset_in_thread is 0")?;
    // Casts: the block/lock-stack displacements are small constants pinned by
    // the VM against `JitMonitorBlock` / `JmxLockStack`.
    let owner_disp = block
        .checked_add(JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET as i32)
        .ok_or("monitor block displacement overflows")?;
    let ls_disp = block
        .checked_add(JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET as i32)
        .ok_or("monitor block displacement overflows")?;
    let held_disp = block
        .checked_add(JIT_MONITOR_BLOCK_HELD_OFFSET as i32)
        .ok_or("monitor block displacement overflows")?;
    let (hit_off, miss_off) = if enter {
        (
            JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET,
            JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET,
        )
    } else {
        (
            JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET,
            JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET,
        )
    };
    // Cast: the census offsets are small constants in the same block.
    let hit_disp = block
        .checked_add(hit_off as i32)
        .ok_or("monitor block displacement overflows")?;
    let miss_disp = block
        .checked_add(miss_off as i32)
        .ok_or("monitor block displacement overflows")?;
    match thread {
        ThinLockThread::Tls(0) => return Err("no JIT_THREAD mirror"),
        ThinLockThread::Frame(off) if off <= 0 => return Err("no frame thread slot"),
        _ => {}
    }
    let seq = JIT_LOCK_STACK_SEQ_OFFSET as i32;
    let top = JIT_LOCK_STACK_TOP_OFFSET as i32;
    let spilled = JIT_LOCK_STACK_SPILLED_OFFSET as i32;
    let slots = JIT_LOCK_STACK_SLOTS as u8;
    // `[R10 + RDX*8 + disp8]`: slot `top` (enter) / slot `top - 1` (exit).
    let push_disp = JIT_LOCK_STACK_SLOTS_OFFSET as u8;
    let pop_disp = POP_DISP as u8;
    // `op reg, [R10 + disp32]` / `op [R10 + disp32], reg` for reg in
    // RAX/RCX/RDX/R10: REX.W (+R for R10) +B, opcode, ModRM(mod=10, reg, rm=R10).
    fn r10_mem(buf: &mut ExecutableBuffer, opcode: u8, reg: u8, disp: i32) {
        rex_w(buf, reg, 0, R10);
        buf.emit_byte(opcode);
        buf.emit_byte(0x80 | ((reg & 7) << 3) | (R10 & 7));
        buf.emit(&disp.to_le_bytes());
    }

    // `reg` (RAX or RCX) = *mut JvmThread, from the same source as R10 below:
    // the census, and the `held` bookkeeping after a winning CAS.
    let thread_into = |buf: &mut ExecutableBuffer, reg: u8| match thread {
        ThinLockThread::Tls(disp) => {
            buf.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
            // MOV reg, <seg>:[disp32]: REX.W, 8B, ModRM(00, reg, SIB), SIB 25.
            buf.emit(&[0x48, 0x8B, 0x04 | ((reg & 7) << 3), 0x25]);
            buf.emit(&disp.to_le_bytes());
        }
        ThinLockThread::Frame(off) => emit_load_frame(buf, reg, off),
    };
    let thread_into_rax = |buf: &mut ExecutableBuffer| thread_into(buf, RAX);
    // RCX = the lease's `LeaseBlock` (its first word is the lessee's
    // `acquired` count), then `ADD|SUB DWORD [RCX], 1`: the `note_acquired` /
    // `note_owner_released` of the helper path. Emitted only after a WINNING
    // CAS, when the thread was already proven non-null and armed (an armed
    // block's `held` is never null). No `LOCK` since round 11 wave 17 (lock
    // proposal W15-1): the word has one writer, this thread -- the helpers
    // write it only on the lessee's own behalf and an inflating contender
    // uncounts in a separate word (`vm/src/threading/monitor.rs`,
    // `LeaseBlock`) -- so the RMW needs no atomicity, and the two locked RMWs
    // it saves per `synchronized` block were half of its locked operations.
    const _: () = assert!(LEASE_BLOCK_ACQUIRED_OFFSET == 0);
    let adjust_held = |buf: &mut ExecutableBuffer, acquired: bool| {
        thread_into(&mut *buf, RCX);
        buf.emit(&[0x48, 0x8B, 0x89]); // MOV RCX, [RCX + disp32]
        buf.emit(&held_disp.to_le_bytes());
        // ADD (83 /0) or SUB (83 /5) DWORD [RCX], 1.
        buf.emit(&[0x83, if acquired { 0x01 } else { 0x29 }, 0x01]);
    };
    // `ADD QWORD [RAX + disp32], 1`: REX.W, 83 /0 ib, ModRM(mod=10, /0, RAX).
    let bump_at_rax = |buf: &mut ExecutableBuffer, disp: i32| {
        buf.emit(&[0x48, 0x83, 0x80]);
        buf.emit(&disp.to_le_bytes());
        buf.emit_byte(0x01);
    };

    // Round 11 wave 17: whether a word that is not the thin path's goes on to
    // the INFLATED arm below instead of straight to the helper. Read per site
    // at compile time, like `inline_thin_lock_enabled`.
    let inflated_arm = inline_inflated_lock_enabled();
    // Where a word the thin path refuses branches to: the recursion arm and /
    // or the inflated arm, or (both off) the slow label, which is then
    // exactly the pre-wave-17 byte sequence.
    let refusal_arms = inflated_arm || thin_recursion;
    let mut not_thin: Option<usize> = None;
    // The start of the lock-stack push (enter) / pop (exit), which the
    // inflated arm jumps back into once it holds / has released the monitor.
    let join_at: usize;

    // Round 13 wave 12 (lane monitor3, [`lock_stack_single_bump_enabled`]):
    // the push / pop is its two data stores and one trailing `seq += 2`.
    let single_bump = lock_stack_single_bump_enabled();
    // `ADD QWORD [R10 + seq], 2`: REX.WB, 83 /0 ib, ModRM(mod=10, /0, R10).
    let bump_seq_by_two = |buf: &mut ExecutableBuffer| {
        buf.emit(&[0x49, 0x83, 0x82]);
        buf.emit(&seq.to_le_bytes());
        buf.emit_byte(0x02);
    };

    let mut slow: Vec<usize> = Vec::new();
    // R11 = receiver.
    emit_load_frame(buf, R11, obj_off);
    if !receiver_non_null {
        buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        slow.push(emit_jcc(buf, 0x84)); // JE slow (null: the helper raises the NPE)
    }
    // R10 = *mut JvmThread.
    match thread {
        ThinLockThread::Tls(disp) => {
            buf.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
            buf.emit(&[0x4C, 0x8B, 0x14, 0x25]); // MOV R10, <seg>:[disp32]
            buf.emit(&disp.to_le_bytes());
        }
        ThinLockThread::Frame(off) => emit_load_frame(buf, R10, off),
    }
    buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    slow.push(emit_jcc(buf, 0x84)); // JE slow (untracked thread)

    // RCX = the owner bits (`THIN_LOCKED | slot << 2`); u64::MAX = not armed.
    r10_mem(buf, 0x8B, RCX, owner_disp); // MOV RCX, [R10 + thin_owner]
    buf.emit(&[0x48, 0x83, 0xF9, 0xFF]); // CMP RCX, -1
    slow.push(emit_jcc(buf, 0x84)); // JE slow

    // R10 = lock stack.
    r10_mem(buf, 0x8B, R10, ls_disp); // MOV R10, [R10 + lock_stack]
    buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    slow.push(emit_jcc(buf, 0x84)); // JE slow

    if enter {
        // A spilled stack publishes to its overflow list, in order.
        buf.emit(&[0x41, 0x80, 0xBA]); // CMP BYTE [R10 + disp32], imm8
        buf.emit(&spilled.to_le_bytes());
        buf.emit_byte(0);
        slow.push(emit_jcc(buf, 0x85)); // JNE slow

        // EAX = the 32-bit mark word. `ObjectHeader::thin_lockable`: state
        // and payload zero ...
        buf.emit(&[0x41, 0x8B, 0x43, mark_disp]); // MOV EAX, [R11 + mark]
        buf.emit_byte(0xA9); // TEST EAX, imm32
        buf.emit(&MARK_LOCK_BITS.to_le_bytes());
        // JNE not_thin (locked, hashed, forwarded): the recursion / inflated
        // arms, or slow. ECX still holds the owner bits there.
        let refused = emit_jcc(buf, 0x85);
        if refusal_arms {
            not_thin = Some(refused);
        } else {
            slow.push(refused);
        }
        // ... and, for a compact plain instance, no high identity-hash bits.
        buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        buf.emit(&[0x81, 0xE2]); // AND EDX, imm32
        buf.emit(&(MARK_KIND_BITS | MARK_COMPACT_BIT).to_le_bytes());
        buf.emit(&[0x81, 0xFA]); // CMP EDX, imm32
        buf.emit(&MARK_COMPACT_BIT.to_le_bytes());
        let lockable = emit_jcc(buf, 0x85); // JNE lockable (not a compact instance)
        buf.emit_byte(0xA9); // TEST EAX, imm32
        buf.emit(&cratonvm_types::MARK_SHORT_HASH_HI_MASK.to_le_bytes());
        slow.push(emit_jcc(buf, 0x85)); // JNE slow (a hashed compact instance)
        patch_rel32_to_here(buf, lockable);

        // ECX = mark | THIN_LOCKED | slot << 2 (recursion 0): the quartet rides
        // along, `ObjectHeader::make_thin_locked(mark, slot, 0)`.
        buf.emit(&[0x09, 0xC1]); // OR ECX, EAX
        // RDX = top; a free slot is required.
        r10_mem(buf, 0x8B, RDX, top); // MOV RDX, [R10 + top]
        buf.emit(&[0x48, 0x83, 0xFA, slots]); // CMP RDX, slots
        slow.push(emit_jcc(buf, 0x83)); // JAE slow
        // The acquisition itself.
        buf.emit(&[0xF0, 0x41, 0x0F, 0xB1, 0x4B, mark_disp]); // LOCK CMPXCHG [R11+mark], ECX
        slow.push(emit_jcc(buf, 0x85)); // JNE slow (lost a race)
        // Ours now: count it under the lease, as `note_acquired` does.
        adjust_held(&mut *buf, true);
        // Publish: seq odd, slot[top] = obj, top + 1, seq even. Entered with
        // RDX = top, R10 = lock stack, R11 = receiver (also from the
        // inflated arm).
        join_at = buf.pos();
        if single_bump {
            // Slot, then `top`, then `seq += 2` (see the switch for why a
            // reader needs no odd `seq` around a compiled push).
            buf.emit(&[0x4D, 0x89, 0x5C, 0xD2, push_disp]); // MOV [R10+RDX*8+slots], R11
            buf.emit(&[0x48, 0x83, 0xC2, 0x01]); // ADD RDX, 1
            r10_mem(buf, 0x89, RDX, top); // MOV [R10 + top], RDX
            bump_seq_by_two(&mut *buf); // ADD QWORD [R10 + seq], 2
        } else {
            r10_mem(buf, 0x8B, RAX, seq); // MOV RAX, [R10 + seq]
            buf.emit(&[0x48, 0x83, 0xC0, 0x01]); // ADD RAX, 1
            r10_mem(buf, 0x89, RAX, seq); // MOV [R10 + seq], RAX
            buf.emit(&[0x4D, 0x89, 0x5C, 0xD2, push_disp]); // MOV [R10+RDX*8+slots], R11
            buf.emit(&[0x48, 0x83, 0xC2, 0x01]); // ADD RDX, 1
            r10_mem(buf, 0x89, RDX, top); // MOV [R10 + top], RDX
            buf.emit(&[0x48, 0x83, 0xC0, 0x01]); // ADD RAX, 1
            r10_mem(buf, 0x89, RAX, seq); // MOV [R10 + seq], RAX
        }
    } else {
        // RDX = top; the top slot must name this receiver.
        r10_mem(buf, 0x8B, RDX, top); // MOV RDX, [R10 + top]
        buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        slow.push(emit_jcc(buf, 0x84)); // JE slow (empty)
        buf.emit(&[0x48, 0x83, 0xFA, slots]); // CMP RDX, slots
        slow.push(emit_jcc(buf, 0x87)); // JA slow (never: bounds)
        buf.emit(&[0x4D, 0x3B, 0x5C, 0xD2, pop_disp]); // CMP R11, [R10+RDX*8+slots-8]
        slow.push(emit_jcc(buf, 0x85)); // JNE slow (not LIFO)
        // EAX = mark; its low 16 bits must be exactly the owner bits: thin,
        // this lease, recursion 0. Then ECX = mark ^ owner bits = the quartet
        // alone, unlocked (`try_thin_unlock`'s recursion-0 arm,
        // `ObjectHeader::make_neutral(mark)`).
        buf.emit(&[0x41, 0x8B, 0x43, mark_disp]); // MOV EAX, [R11 + mark]
        buf.emit(&[0x31, 0xC1]); // XOR ECX, EAX
        buf.emit(&[0xF7, 0xC1]); // TEST ECX, imm32
        buf.emit(&MARK_LOCK_BITS.to_le_bytes());
        // JNE not_thin (recursive, foreign, inflated): the recursion /
        // inflated arms (EAX still holds the mark, ECX the XOR), or slow.
        let refused = emit_jcc(buf, 0x85);
        if refusal_arms {
            not_thin = Some(refused);
        } else {
            slow.push(refused);
        }
        // The release itself.
        buf.emit(&[0xF0, 0x41, 0x0F, 0xB1, 0x4B, mark_disp]); // LOCK CMPXCHG [R11+mark], ECX
        slow.push(emit_jcc(buf, 0x85)); // JNE slow (inflated under us)
        // Released: uncount it, as `note_owner_released` does.
        adjust_held(&mut *buf, false);
        // Retract: seq odd, slot[top-1] = 0, top - 1, seq even. Entered with
        // RDX = top and R10 = lock stack (also from the inflated arm).
        join_at = buf.pos();
        if single_bump {
            // Tombstone, then `top`, then `seq += 2`.
            // MOV QWORD [R10+RDX*8+slots-8], 0
            buf.emit(&[0x49, 0xC7, 0x44, 0xD2, pop_disp, 0x00, 0x00, 0x00, 0x00]);
            buf.emit(&[0x48, 0x83, 0xC2, 0xFF]); // ADD RDX, -1
            r10_mem(buf, 0x89, RDX, top); // MOV [R10 + top], RDX
            bump_seq_by_two(&mut *buf); // ADD QWORD [R10 + seq], 2
        } else {
            r10_mem(buf, 0x8B, RAX, seq); // MOV RAX, [R10 + seq]
            buf.emit(&[0x48, 0x83, 0xC0, 0x01]); // ADD RAX, 1
            r10_mem(buf, 0x89, RAX, seq); // MOV [R10 + seq], RAX
                                          // MOV QWORD [R10+RDX*8+slots-8], 0
            buf.emit(&[0x49, 0xC7, 0x44, 0xD2, pop_disp, 0x00, 0x00, 0x00, 0x00]);
            buf.emit(&[0x48, 0x83, 0xC2, 0xFF]); // ADD RDX, -1
            r10_mem(buf, 0x89, RDX, top); // MOV [R10 + top], RDX
            buf.emit(&[0x48, 0x83, 0xC0, 0x01]); // ADD RAX, 1
            r10_mem(buf, 0x89, RAX, seq); // MOV [R10 + seq], RAX
        }
    }
    // Where the recursion arm joins: the census bump, then `JMP done`.
    let hit_at = buf.pos();
    if census {
        // Inline hit: RAX is free after the last `seq` store, and the thread
        // was proven non-null above.
        thread_into_rax(&mut *buf);
        bump_at_rax(&mut *buf, hit_disp);
    }
    let done = emit_jmp(buf);
    if let Some(site) = not_thin {
        patch_rel32_to_here(buf, site);
        // Round 13 wave 8 (lane sync5): a word thin-locked by this lease is
        // the recursion arm's; any other word goes on, EAX still the mark.
        let not_mine = if thin_recursion {
            Some(emit_inline_thin_recursion_arm(
                buf, enter, hit_at, &mut slow,
            ))
        } else {
            None
        };
        if inflated_arm {
            if let Some(site) = not_mine {
                patch_rel32_to_here(buf, site);
            }
            emit_inline_inflated_arm(
                buf,
                enter,
                &thread_into,
                InflatedArm {
                    held_disp,
                    top,
                    slots,
                    join_at,
                    obj_off,
                    reentry_to: recursion.inflated.then_some(hit_at),
                    two_way: recursion.two_way,
                    census,
                },
                &mut slow,
            );
        } else if let Some(site) = not_mine {
            slow.push(site);
        }
    }
    for p in slow {
        patch_rel32_to_here(buf, p);
    }
    if census {
        // Fell to the helper; the thread may be the untracked 0.
        thread_into_rax(&mut *buf);
        buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let skip = emit_jcc(buf, 0x84); // JE skip
        bump_at_rax(&mut *buf, miss_disp);
        patch_rel32_to_here(buf, skip);
    }
    Ok(done)
}

/// What [`emit_inline_inflated_arm`] needs from [`emit_inline_thin_lock`].
struct InflatedArm {
    /// `JitMonitorBlock::held` (the lease's `LeaseBlock` address) relative to
    /// the thread.
    held_disp: i32,
    /// `JmxLockStack::top` relative to the lock stack.
    top: i32,
    /// `JIT_LOCK_STACK_SLOTS`.
    slots: u8,
    /// The lock-stack push (enter) / pop (exit) to join, expecting RDX = top,
    /// R10 = lock stack, R11 = receiver.
    join_at: usize,
    /// The receiver's frame home (`[RBP - obj_off]`), from which the inline
    /// spin restores R11 after using it as its counter.
    obj_off: i32,
    /// Round 13 wave 8 (lane sync5, [`inline_inflated_recursion_enabled`]):
    /// where a re-entry / non-final exit of a monitor this thread owns joins
    /// once its entry count moved (the census bump, then `JMP done`), or
    /// `None` to leave both to the helper as before.
    reentry_to: Option<usize>,
    /// Round 13 wave 10 (lane monitor2, [`inline_inflated_two_way_enabled`]):
    /// a receiver the lease cache's first way does not name is looked for in
    /// its second way (`LEASE_BLOCK_KEY2_OFFSET`) before the helper.
    two_way: bool,
    /// Round 14 wave 1 (lane sync, M2-1): the inline spin's census bumps
    /// ([`emit_inline_inflated_spin_acquire`]); the site's own census flag.
    census: bool,
}

/// The RECURSION arm of [`emit_inline_thin_lock`] (round 13 wave 8, lane
/// sync5, proposals S3-1 / S4-1; [`inline_thin_lock_recursion_enabled`]): a
/// re-entry on, or a non-final exit of, a word thin-locked by THIS lease.
/// The thin lock keeps its recursion count in the mark word
/// (`THIN_LOCK_RECURSION_MASK`), so this is exactly the helper's
/// `try_thin_recursive_lock` (`recursion + 1`, refused at
/// `MAX_THIN_LOCK_RECURSION`: the helper inflates) and `try_thin_unlock`'s
/// `recursion > 0` arm (`recursion - 1`), one `LOCK CMPXCHG` each. Neither
/// moves the lease count (it counts NEUTRAL -> THIN transitions only) nor the
/// lock stack (a set of held objects: the helper's `publish` of a re-entry
/// dedupes to nothing, and its `retract` runs only once nothing is held).
///
/// Entered at the thin path's refusal with EAX = the mark word, R11 = the
/// receiver and ECX = the owner bits (`THIN_LOCKED | slot << 2`, enter) or
/// the owner bits XOR the mark (exit, whose low 16 bits are non-zero, and
/// whose lock-stack top already names the receiver). A word thin-locked by
/// this lease is taken (joining `hit_at`: the census bump, then `JMP
/// done`) or goes to `slow` (recursion at its maximum, a lost CAS: another
/// thread inflated the word). Any other word branches to the returned
/// `Jcc` patch site with EAX, ECX, R10 and R11 unchanged, for the inflated
/// arm or `slow`. Clobbers EDX (enter) and ECX (taken path only).
///
/// ```text
/// enter:  MOV EDX, EAX ; AND EDX, STATE|OWNER ; CMP EDX, ECX ; JNE other
///         MOV EDX, EAX ; AND EDX, RECURSION ; CMP EDX, MAX ; JAE slow
///         LEA ECX, [RAX + ONE] ; LOCK CMPXCHG [R11+mark], ECX ; JNE slow
///         JMP hit_at
/// exit:   TEST ECX, STATE|OWNER ; JNE other   ; recursion >= 1 remains
///         LEA ECX, [RAX - ONE] ; LOCK CMPXCHG [R11+mark], ECX ; JNE slow
///         JMP hit_at
/// ```
///
/// No carry leaves the recursion field: enter adds only below the maximum,
/// and exit subtracts only from a non-zero count, so the quartet (the high
/// 16 bits) rides along untouched, as `make_thin_locked` keeps it.
fn emit_inline_thin_recursion_arm(
    buf: &mut ExecutableBuffer,
    enter: bool,
    hit_at: usize,
    slow: &mut Vec<usize>,
) -> usize {
    use cratonvm_types::{
        MARK_WORD_OFFSET, MAX_THIN_LOCK_RECURSION, THIN_LOCK_RECURSION_MASK,
        THIN_LOCK_RECURSION_SHIFT,
    };
    // The state and owner fields: the low 16 lock bits minus the count.
    const STATE_OWNER: u32 = MARK_LOCK_BITS & !THIN_LOCK_RECURSION_MASK;
    const ONE: u32 = 1 << THIN_LOCK_RECURSION_SHIFT;
    const MAX: u32 = MAX_THIN_LOCK_RECURSION << THIN_LOCK_RECURSION_SHIFT;
    const _: () = assert!(THIN_LOCK_RECURSION_MASK & MARK_LOCK_BITS == THIN_LOCK_RECURSION_MASK);
    const _: () = assert!(MAX & !THIN_LOCK_RECURSION_MASK == 0 && MAX + ONE <= MARK_LOCK_BITS + 1);
    const _: () = assert!(MARK_WORD_OFFSET < 128);
    // Cast: pinned < 128 just above.
    let mark_disp = MARK_WORD_OFFSET as u8;
    // Cast: ONE is 1 << 13, far inside i32.
    let one = ONE as i32;
    let other = if enter {
        buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        buf.emit(&[0x81, 0xE2]); // AND EDX, imm32
        buf.emit(&STATE_OWNER.to_le_bytes());
        buf.emit(&[0x39, 0xCA]); // CMP EDX, ECX
        let other = emit_jcc(buf, 0x85); // JNE other (not thin-locked by this lease)
        buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        buf.emit(&[0x81, 0xE2]); // AND EDX, imm32
        buf.emit(&THIN_LOCK_RECURSION_MASK.to_le_bytes());
        buf.emit(&[0x81, 0xFA]); // CMP EDX, imm32
        buf.emit(&MAX.to_le_bytes());
        slow.push(emit_jcc(buf, 0x83)); // JAE slow (the helper inflates)
        buf.emit(&[0x8D, 0x88]); // LEA ECX, [RAX + disp32]
        buf.emit(&one.to_le_bytes());
        other
    } else {
        buf.emit(&[0xF7, 0xC1]); // TEST ECX, imm32
        buf.emit(&STATE_OWNER.to_le_bytes());
        let other = emit_jcc(buf, 0x85); // JNE other (foreign, inflated, forwarded)
        buf.emit(&[0x8D, 0x88]); // LEA ECX, [RAX + disp32]
        buf.emit(&(-one).to_le_bytes());
        other
    };
    buf.emit(&[0xF0, 0x41, 0x0F, 0xB1, 0x4B, mark_disp]); // LOCK CMPXCHG [R11+mark], ECX
    slow.push(emit_jcc(buf, 0x85)); // JNE slow (inflated under us)
    emit_jmp_back(buf, hit_at);
    other
}

/// `JMP rel32` to an already-emitted `target` (a backward jump).
fn emit_jmp_back(buf: &mut ExecutableBuffer, target: usize) {
    buf.emit_byte(0xE9);
    let displacement = target as i64 - (buf.pos() as i64 + 4);
    match i32::try_from(displacement) {
        Ok(d) => buf.emit(&d.to_le_bytes()),
        Err(_) => {
            buf.emit(&[0; 4]);
            buf.mark_codegen_unencodable("rel32-displacement-out-of-range");
        }
    }
}

/// `Jcc rel32` (`0F cc`) to an already-emitted `target` (a backward branch).
fn emit_jcc_back(buf: &mut ExecutableBuffer, cc: u8, target: usize) {
    buf.emit(&[0x0F, cc]);
    let displacement = target as i64 - (buf.pos() as i64 + 4);
    match i32::try_from(displacement) {
        Ok(d) => buf.emit(&d.to_le_bytes()),
        Err(_) => {
            buf.emit(&[0; 4]);
            buf.mark_codegen_unencodable("rel32-displacement-out-of-range");
        }
    }
}

/// The acquisition of the INFLATED arm's `monitorenter` with an inline spin
/// ([`inline_inflated_spin_enabled`], round 12 wave 2, lane lock2). Entered
/// with RCX = the monitor (cached, epoch-checked), RDX = this thread's owner
/// word (`ThreadId + 1`, non-zero), R10 = the lock stack, R11 = the receiver;
/// falls through OWNING the monitor with RCX, R10 and R11 as they were (the
/// caller then sets `entry_count = 1` and joins the push). Every refusal goes
/// to `slow` having written nothing but R11 and RAX (both the helper
/// sequence's to clobber):
///
/// ```text
///     MOV RAX, [RCX+owner] ; TEST RAX, RAX ; JNE owned
///     XOR EAX, EAX ; LOCK CMPXCHG [RCX+owner], RDX ; JE acquired
/// owned:                                ; RAX = the owner
///     CMP RAX, RDX ; JE slow            ; re-entry: the helper's
///     MOV R11D, [RCX+spin_limit]        ; the monitor's adaptive budget
///     TEST R11D, R11D ; JE slow ; CMP R11D, SPIN_MAX ; JA slow
/// spin:
///     CMP DWORD [RCX+entry_waiters], 0 ; JNE slow   ; somebody parked
///     PAUSE ; CMP QWORD [RCX+owner], 0 ; JNE next
///     XOR EAX, EAX ; LOCK CMPXCHG [RCX+owner], RDX ; JE won
///     MOV EAX, 16                       ; lost: back off (switch on)
/// backoff:
///     PAUSE ; SUB R11D, 1 ; JE slow ; SUB EAX, 1 ; JNE backoff
/// next:
///     SUB R11D, 1 ; JNE spin ; JMP slow ; budget out: the helper
/// won:
///     MOV R11, [RBP - obj_off]          ; the receiver again
/// acquired:
/// ```
///
/// A free monitor is taken exactly as without the spin (one load, one CAS),
/// except that a lost race now spins instead of calling the helper. The
/// budget is read, never written: a spin that runs out goes to the helper,
/// whose adaptive spin (`Monitor::spin_try_enter_adaptive`) keeps adapting
/// it. The spinner is not counted in `Monitor::spinners` (see the switch),
/// and so it spins only while no entrant is registered to park: with one
/// parked, the helper's spinners (which count themselves then) keep the
/// wake-up economy of round 12 wave 1.
///
/// Round 13 wave 8 (lane sync5): the re-entry edge (`owned` by this thread)
/// goes to `reentry` instead of `slow` when the caller passes one (the inflated
/// recursion arm, [`inline_inflated_recursion_enabled`]), with RCX still the
/// monitor and nothing written.
///
/// Round 14 wave 1 (lane sync, proposal M2-1 of `jit-r13-monitor2-proposals-RETIRED-20260929.md`):
/// `census` (the caller's `crate::x64::dbg_jitc_enabled()`) adds one
/// `LOCK ADD QWORD [RCX + disp], 1` on each of the spin's three outcomes --
/// won (`INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET`), budget out
/// (`..._BUDGET_OUTS_OFFSET`) and gave way to a parked entrant
/// (`..._WAITER_EXITS_OFFSET`) -- so the VM's contention census
/// (`CRATONVM_DBG_MONITOR_CONTENTION`) finally sees the inline spin. RCX is
/// the monitor on all three edges. With `census == false` not a byte changes.
fn emit_inline_inflated_spin_acquire(
    buf: &mut ExecutableBuffer,
    obj_off: i32,
    slow: &mut Vec<usize>,
    reentry: Option<&mut Vec<usize>>,
    census: bool,
) {
    const _: () = assert!(
        INFLATED_MONITOR_OWNER_OFFSET < 128
            && INFLATED_MONITOR_SPIN_LIMIT_OFFSET < 128
            && INFLATED_MONITOR_ENTRY_WAITERS_OFFSET < 128
    );
    // `LOCK ADD QWORD [RCX + disp32], 1`: F0, REX.W, 83 /0 ib,
    // ModRM(mod=10, /0, RCX). Census only (M2-1): contenders share the word.
    let census_bump = |buf: &mut ExecutableBuffer, disp: usize| {
        buf.emit(&[0xF0, 0x48, 0x83, 0x81]);
        // Cast: the census offsets are small constants pinned by the VM.
        buf.emit(&(disp as i32).to_le_bytes());
        buf.emit_byte(0x01);
    };
    // With `census`, the edges that leave the spin for the helper gather here
    // and go through their bump first; without it they are `slow`'s.
    let mut budget_outs: Vec<usize> = Vec::new();
    let mut waiter_exits: Vec<usize> = Vec::new();
    // Casts: pinned < 128 just above.
    let m_owner = INFLATED_MONITOR_OWNER_OFFSET as u8;
    let m_spin = INFLATED_MONITOR_SPIN_LIMIT_OFFSET as u8;
    let m_waiters = INFLATED_MONITOR_ENTRY_WAITERS_OFFSET as u8;
    // `XOR EAX, EAX ; LOCK CMPXCHG [RCX + owner], RDX` (0 -> me).
    let cas_owner = |buf: &mut ExecutableBuffer| {
        buf.emit(&[0x31, 0xC0]);
        buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x51, m_owner]);
    };

    buf.emit(&[0x48, 0x8B, 0x41, m_owner]); // MOV RAX, [RCX + owner]
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    let owned_now = emit_jcc(buf, 0x85); // JNE owned
    cas_owner(&mut *buf);
    let acquired_first = emit_jcc(buf, 0x84); // JE acquired
                                              // owned: RAX = the owner word (loaded, or what the failed CMPXCHG read).
    patch_rel32_to_here(buf, owned_now);
    buf.emit(&[0x48, 0x39, 0xD0]); // CMP RAX, RDX
    let reentered = emit_jcc(buf, 0x84); // JE slow (re-entry) / reentry
    match reentry {
        Some(sites) => sites.push(reentered),
        None => slow.push(reentered),
    }
    buf.emit(&[0x44, 0x8B, 0x59, m_spin]); // MOV R11D, [RCX + spin_limit]
    buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
    slow.push(emit_jcc(buf, 0x84)); // JE slow (no budget)
    buf.emit(&[0x41, 0x81, 0xFB]); // CMP R11D, imm32
    buf.emit(&INFLATED_MONITOR_SPIN_MAX.to_le_bytes());
    slow.push(emit_jcc(buf, 0x87)); // JA slow (not a budget the VM stores)
    let spin = buf.pos();
    // Only while nobody is parked: an entrant registered to park is the
    // helper's business (its spinners count themselves and so spare the
    // parker a wake-up this uncounted spin would make useless).
    buf.emit(&[0x83, 0x79, m_waiters, 0x00]); // CMP DWORD [RCX + entry_waiters], 0
    let parked = emit_jcc(buf, 0x85); // JNE slow (or the census's waiter exit)
    if census {
        waiter_exits.push(parked);
    } else {
        slow.push(parked);
    }
    buf.emit(&[0xF3, 0x90]); // PAUSE
    buf.emit(&[0x48, 0x83, 0x79, m_owner, 0x00]); // CMP QWORD [RCX + owner], 0
    let still_owned = emit_jcc(buf, 0x85); // JNE next
    cas_owner(&mut *buf);
    let won = emit_jcc(buf, 0x84); // JE won
    if inline_spin_backoff_enabled() {
        // Lost the race (round 12 wave 8): leave the owner line alone for
        // INLINE_SPIN_LOST_CAS_BACKOFF PAUSEs, each charged to the budget.
        // EAX is free (the failed CMPXCHG loaded the owner into it, which
        // nothing reads); R11D is the budget, never 0 here.
        buf.emit_byte(0xB8); // MOV EAX, imm32
        buf.emit(&INLINE_SPIN_LOST_CAS_BACKOFF.to_le_bytes());
        let backoff = buf.pos();
        buf.emit(&[0xF3, 0x90]); // PAUSE
        buf.emit(&[0x41, 0x83, 0xEB, 0x01]); // SUB R11D, 1
        let out = emit_jcc(buf, 0x84); // JE slow (budget out)
        if census {
            budget_outs.push(out);
        } else {
            slow.push(out);
        }
        buf.emit(&[0x83, 0xE8, 0x01]); // SUB EAX, 1
        emit_jcc_back(buf, 0x85, backoff); // JNE backoff
    }
    // next:
    patch_rel32_to_here(buf, still_owned);
    buf.emit(&[0x41, 0x83, 0xEB, 0x01]); // SUB R11D, 1
    emit_jcc_back(buf, 0x85, spin); // JNE spin
    if census {
        // budget out (falls in from the loop above): RCX is the monitor.
        for site in budget_outs {
            patch_rel32_to_here(buf, site);
        }
        census_bump(&mut *buf, INFLATED_MONITOR_CENSUS_SPIN_BUDGET_OUTS_OFFSET);
        slow.push(emit_jmp(buf)); // JMP slow
        // waiter exit: RCX is the monitor.
        for site in waiter_exits {
            patch_rel32_to_here(buf, site);
        }
        census_bump(&mut *buf, INFLATED_MONITOR_CENSUS_SPIN_WAITER_EXITS_OFFSET);
    }
    slow.push(emit_jmp(buf)); // JMP slow (budget out; with `census`, the waiter exit's)
    // won:
    patch_rel32_to_here(buf, won);
    if census {
        // Owning the monitor, RCX still the monitor.
        census_bump(&mut *buf, INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET);
    }
    emit_load_frame(buf, R11, obj_off); // MOV R11, [RBP - obj_off]
    // acquired:
    patch_rel32_to_here(buf, acquired_first);
}

/// The INFLATED arm of [`emit_inline_thin_lock`] (round 11 wave 17, lane
/// lock, proposal W16-1): a compiled `monitorenter` / `monitorexit` of an
/// object whose lock is an inflated `Monitor`, without the helper call, as
/// HotSpot's C2 CASes `ObjectMonitor::_owner` inline. Until this arm every
/// such operation called `jit_monitor_enter` / `jit_monitor_exit`, and on a
/// contended lock the helper round trip sat INSIDE the critical section (the
/// enter's return path and the exit's entry path run while the monitor is
/// held), so it bounded the hand-over rate of every contended lock
/// (`r11w15-orch-contended-monitor-throughput-80x-hotspot`).
///
/// Entered with EAX = the mark word (which the thin path refused), R10 = the
/// lock stack, R11 = the receiver, and for exit RDX = the lock stack's `top`
/// with its top slot already proven to name the receiver. Every refusal goes
/// to `slow` (the helper) having written nothing.
///
/// The monitor is found through this thread's lease cache (`LeaseBlock` in
/// `vm/src/threading/monitor.rs`, reached through `JitMonitorBlock::held`,
/// filled by the helpers on the lessee's own behalf): taken only when its key
/// is this receiver AND its epoch is still the table's `index_epoch` -- the
/// conditions `MonitorTable::cached_inflated_monitor` trusts a cached monitor
/// under, which hold across this sequence because it has no safepoint and
/// never blocks (the index entry can be dropped or replaced only by a
/// stop-the-world pass, or by an insert at this key, which a live INFLATED
/// receiver rules out). A word that is not INFLATED is refused first, so a
/// stale entry for a dead object at the same address is never consulted.
///
/// * **enter**: the lock stack has room; the owner word reads `0` (a load
///   first, so a contender does not take the owner's line with a failing
///   CAS); `CMPXCHG [owner], 0 -> me` (the lease's `owner_word`,
///   `ThreadId + 1`); `entry_count = 1`; then the thin
///   path's push. A monitor owned by anyone (this thread included: re-entry
///   stays the helper's) is refused -- the helper spins, parks, and publishes
///   the JMX contention as before. Since round 12 wave 2 a monitor owned by
///   ANOTHER thread is first spun for inline, up to its adaptive budget
///   ([`emit_inline_inflated_spin_acquire`], [`inline_inflated_spin_enabled`]),
///   and only a spin that runs out takes the helper. Since round 13 wave 8
///   a monitor THIS thread owns is re-entered here (`entry_count + 1`,
///   [`inline_inflated_recursion_enabled`]; `InflatedArm::reentry_to`).
/// * **exit**: owned by this thread at entry count 1, no JFR enter recorded
///   and no thin seed (both would need the helper's release); `entry_count =
///   0`, `XCHG [owner], 0`, then `Monitor::wake_successor`'s decision,
///   `entry_waiters != 0 && !succ_pending && spinners == 0`, read AFTER the
///   releasing `XCHG` (a full barrier), which is the releaser's half of the
///   lost-wake-up argument on `Monitor::wake_successor`. When no wake is owed
///   it pops the lock stack. When one is, this code cannot issue it (a wake
///   takes the state mutex and a condvar), so it CASes the monitor back (0 ->
///   me), restores `entry_count = 1` and goes to the helper, which releases
///   and wakes -- or, if the CAS finds a newer owner, leaves the wake to that
///   owner's release, which reads the same waiter count (the duty is handed
///   on exactly as a release that skips a wake hands it on). Since round 12
///   wave 1 the same decision is also read BEFORE the `XCHG`
///   ([`inflated_exit_precheck_enabled`]): a release that would already owe
///   a wake goes to the helper still held and never releases here, so the
///   take-back above only runs when a parker registered during the release.
///   Since round 13 wave 8 an entry count above 1 is a non-final exit
///   (`entry_count - 1`, nothing released, nothing owed) under the same
///   switch as the re-entry.
/// * Since round 13 wave 12 ([`inflated_sticky_count_enabled`]) the final
///   release leaves `entry_count` at 1 and the acquisition stores 1 only when
///   it reads something else: a compiled hand-over writes the owner word
///   alone.
///
/// Clobbers RAX, RCX, RDX (and the flags), like the rest of the sequence.
fn emit_inline_inflated_arm(
    buf: &mut ExecutableBuffer,
    enter: bool,
    thread_into: &dyn Fn(&mut ExecutableBuffer, u8),
    arm: InflatedArm,
    slow: &mut Vec<usize>,
) {
    // Every displacement below is a disp8 or an imm8.
    const _: () = assert!(
        LEASE_BLOCK_OWNER_WORD_OFFSET < 128
            && LEASE_BLOCK_KEY_OFFSET < 128
            && LEASE_BLOCK_MONITOR_OFFSET < 128
            && LEASE_BLOCK_EPOCH_OFFSET < 128
            && LEASE_BLOCK_EPOCH_ADDR_OFFSET < 128
            && INFLATED_MONITOR_OWNER_OFFSET < 128
            && INFLATED_MONITOR_THIN_SEED_OFFSET < 128
            && INFLATED_MONITOR_ENTRY_WAITERS_OFFSET < 128
            && INFLATED_MONITOR_SPINNERS_OFFSET < 128
            && INFLATED_MONITOR_JFR_OFFSET < 128
            && INFLATED_MONITOR_SUCC_PENDING_OFFSET < 128
    );
    // The entry count is the one disp32 (round 12 wave 8).
    const _: () = assert!(INFLATED_MONITOR_ENTRY_COUNT_OFFSET <= i32::MAX as usize);
    // INFLATED is exactly state 0b10: bit 0 clear, bit 1 set.
    const _: () = assert!(
        cratonvm_types::MARK_INFLATED == 0b10 && cratonvm_types::MARK_STATE_MASK == 0b11
    );
    // Casts: pinned < 128 just above.
    let lb_owner = LEASE_BLOCK_OWNER_WORD_OFFSET as u8;
    let lb_key = LEASE_BLOCK_KEY_OFFSET as u8;
    let lb_monitor = LEASE_BLOCK_MONITOR_OFFSET as u8;
    let lb_epoch = LEASE_BLOCK_EPOCH_OFFSET as u8;
    let lb_epoch_addr = LEASE_BLOCK_EPOCH_ADDR_OFFSET as u8;
    let m_owner = INFLATED_MONITOR_OWNER_OFFSET as u8;
    let m_seed = INFLATED_MONITOR_THIN_SEED_OFFSET as u8;
    // Cast: pinned <= i32::MAX just above.
    let m_count = INFLATED_MONITOR_ENTRY_COUNT_OFFSET as i32;
    let m_waiters = INFLATED_MONITOR_ENTRY_WAITERS_OFFSET as u8;
    let m_spinners = INFLATED_MONITOR_SPINNERS_OFFSET as u8;
    let m_jfr = INFLATED_MONITOR_JFR_OFFSET as u8;
    let m_succ = INFLATED_MONITOR_SUCC_PENDING_OFFSET as u8;
    // Round 13 wave 12 (lane monitor3, [`inflated_sticky_count_enabled`]).
    let sticky_count = inflated_sticky_count_enabled();
    // `MOV RDX, [R10 + top]`: REX.WB, 8B, ModRM(mod=10, RDX, R10), disp32.
    let load_top = |buf: &mut ExecutableBuffer| {
        buf.emit(&[0x49, 0x8B, 0x92]);
        buf.emit(&arm.top.to_le_bytes());
    };

    buf.emit(&[0xA8, 0x01]); // TEST AL, 1
    slow.push(emit_jcc(buf, 0x85)); // JNE slow (thin-locked, forwarded)
    buf.emit(&[0xA8, 0x02]); // TEST AL, 2
    slow.push(emit_jcc(buf, 0x84)); // JE slow (neutral: a hashed word)
    if enter {
        // A free lock-stack slot, proven BEFORE the monitor is taken.
        load_top(&mut *buf);
        buf.emit(&[0x48, 0x83, 0xFA, arm.slots]); // CMP RDX, slots
        slow.push(emit_jcc(buf, 0x83)); // JAE slow
    }
    // RCX = this thread's LeaseBlock.
    thread_into(&mut *buf, RCX);
    buf.emit(&[0x48, 0x8B, 0x89]); // MOV RCX, [RCX + disp32]
    buf.emit(&arm.held_disp.to_le_bytes());
    buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
    slow.push(emit_jcc(buf, 0x84)); // JE slow
    if arm.two_way {
        // Round 13 wave 10 (lane monitor2): either way may name THIS
        // receiver; RAX = that way's monitor. Both ways are valid only at the
        // block's one epoch, checked next exactly as for one way. EDX is free
        // here (enter reloads `top` before its push, exit before its pop).
        const _: () = assert!(LEASE_BLOCK_KEY2_OFFSET < 128 && LEASE_BLOCK_MONITOR2_OFFSET < 128);
        // Casts: pinned < 128 just above.
        let lb_key2 = LEASE_BLOCK_KEY2_OFFSET as u8;
        let lb_monitor2 = LEASE_BLOCK_MONITOR2_OFFSET as u8;
        buf.emit(&[0x48, 0x8B, 0x41, lb_monitor]); // MOV RAX, [RCX + monitor]
        buf.emit(&[0x4C, 0x3B, 0x59, lb_key]); // CMP R11, [RCX + key]
        let first_way = emit_jcc(buf, 0x84); // JE found
        buf.emit(&[0x48, 0x8B, 0x41, lb_monitor2]); // MOV RAX, [RCX + monitor2]
        buf.emit(&[0x4C, 0x3B, 0x59, lb_key2]); // CMP R11, [RCX + key2]
        slow.push(emit_jcc(buf, 0x85)); // JNE slow (neither way)
        patch_rel32_to_here(buf, first_way);
        // found: at the index epoch that is still current.
        buf.emit(&[0x48, 0x8B, 0x51, lb_epoch_addr]); // MOV RDX, [RCX + epoch_addr]
        buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
        buf.emit(&[0x48, 0x8B, 0x12]); // MOV RDX, [RDX]
        buf.emit(&[0x48, 0x3B, 0x51, lb_epoch]); // CMP RDX, [RCX + epoch]
        slow.push(emit_jcc(buf, 0x85)); // JNE slow
        // RDX = me (the lessee's owner word), RCX = the monitor.
        buf.emit(&[0x48, 0x8B, 0x51, lb_owner]); // MOV RDX, [RCX + owner_word]
        buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
        buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
        buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
    } else {
        // The cache names THIS receiver ...
        buf.emit(&[0x4C, 0x3B, 0x59, lb_key]); // CMP R11, [RCX + key]
        slow.push(emit_jcc(buf, 0x85)); // JNE slow
        // ... at the index epoch that is still current.
        buf.emit(&[0x48, 0x8B, 0x41, lb_epoch_addr]); // MOV RAX, [RCX + epoch_addr]
        buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
        buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
        buf.emit(&[0x48, 0x3B, 0x41, lb_epoch]); // CMP RAX, [RCX + epoch]
        slow.push(emit_jcc(buf, 0x85)); // JNE slow
        // RDX = me (the lessee's owner word), RCX = the monitor.
        buf.emit(&[0x48, 0x8B, 0x51, lb_owner]); // MOV RDX, [RCX + owner_word]
        buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
        buf.emit(&[0x48, 0x8B, 0x49, lb_monitor]); // MOV RCX, [RCX + monitor]
        buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
        slow.push(emit_jcc(buf, 0x84)); // JE slow
    }
    // `LOCK CMPXCHG [RCX + owner], RDX` (0 -> me when EAX = 0).
    let cas_owner = |buf: &mut ExecutableBuffer| {
        buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x51, m_owner]);
    };
    // `MOV DWORD [RCX + disp32], imm32`: C7 /0, ModRM(mod=10, /0, RCX).
    let set_count = |buf: &mut ExecutableBuffer, count: u32| {
        buf.emit(&[0xC7, 0x81]);
        buf.emit(&m_count.to_le_bytes());
        buf.emit(&count.to_le_bytes());
    };
    // `ADD|SUB DWORD [RCX + disp32], 1`: 83 /0|/5 ib, ModRM(mod=10, /r, RCX).
    // Round 13 wave 8 (lane sync5): the owner's re-entry count, which only
    // the owner writes (`Monitor::try_enter` / `exit_reporting_release`).
    let adjust_count = |buf: &mut ExecutableBuffer, up: bool| {
        buf.emit(&[0x83, if up { 0x81 } else { 0xA9 }]);
        buf.emit(&m_count.to_le_bytes());
        buf.emit_byte(0x01);
    };
    if enter {
        // Round 13 wave 8 (lane sync5): a monitor this thread already owns is
        // re-entered here (`reentry_to`), not by the helper.
        let mut reentered: Vec<usize> = Vec::new();
        // Test before the CAS (test-and-test-and-set, as `Monitor::try_enter`
        // does): a contender that finds the monitor owned spins on loads (or
        // goes to the helper, which does), without first taking the owner's
        // line exclusive with a failing `LOCK CMPXCHG`.
        if inline_inflated_spin_enabled() {
            // Round 12 wave 2: owned by another thread -> spin inline first.
            let reentry = if arm.reentry_to.is_some() {
                Some(&mut reentered)
            } else {
                None
            };
            emit_inline_inflated_spin_acquire(buf, arm.obj_off, slow, reentry, arm.census);
        } else {
            if arm.reentry_to.is_some() {
                buf.emit(&[0x48, 0x3B, 0x51, m_owner]); // CMP RDX, [RCX + owner]
                reentered.push(emit_jcc(buf, 0x84)); // JE reentry (ours)
            }
            buf.emit(&[0x48, 0x83, 0x79, m_owner, 0x00]); // CMP QWORD [RCX + owner], 0
            slow.push(emit_jcc(buf, 0x85)); // JNE slow (owned: contended, or ours)
            cas_owner(&mut *buf);
            slow.push(emit_jcc(buf, 0x85)); // JNE slow (lost a race)
        }
        if sticky_count {
            // Round 13 wave 12 (lane monitor3): the count of an unowned
            // monitor reads 1 after a compiled release; write it only when it
            // does not, so a compiled hand-over leaves its line shared.
            // CMP DWORD [RCX + disp32], 1: 83 /7 ib, ModRM(mod=10, /7, RCX).
            buf.emit(&[0x83, 0xB9]);
            buf.emit(&m_count.to_le_bytes());
            buf.emit_byte(0x01);
            let already_one = emit_jcc(buf, 0x84); // JE already_one
            set_count(&mut *buf, 1);
            patch_rel32_to_here(buf, already_one);
        } else {
            set_count(&mut *buf, 1);
        }
        load_top(&mut *buf);
        emit_jmp_back(buf, arm.join_at);
        if let Some(hit_at) = arm.reentry_to {
            if !reentered.is_empty() {
                // reentry: RCX = the monitor, owned by this thread. The lock
                // stack already names it (a set, as the helper's `publish`).
                for site in reentered {
                    patch_rel32_to_here(buf, site);
                }
                adjust_count(&mut *buf, true);
                emit_jmp_back(buf, hit_at);
            }
        }
        return;
    }
    buf.emit(&[0x48, 0x3B, 0x51, m_owner]); // CMP RDX, [RCX + owner]
    slow.push(emit_jcc(buf, 0x85)); // JNE slow (not ours: the helper's IMSE)
    // CMP DWORD [RCX + disp32], 1: 83 /7 ib, ModRM(mod=10, /7, RCX).
    buf.emit(&[0x83, 0xB9]);
    buf.emit(&m_count.to_le_bytes());
    buf.emit_byte(0x01);
    match arm.reentry_to {
        Some(hit_at) => {
            // Round 13 wave 8 (lane sync5): count 1 is the final release
            // below; above 1 a non-final exit drops the count and releases
            // nothing (no owner write, no wake owed); 0 is the helper's IMSE.
            let final_release = emit_jcc(buf, 0x84); // JE final
            slow.push(emit_jcc(buf, 0x82)); // JB slow (count 0)
            adjust_count(&mut *buf, false);
            emit_jmp_back(buf, hit_at);
            patch_rel32_to_here(buf, final_release);
        }
        None => slow.push(emit_jcc(buf, 0x85)), // JNE slow (re-entered)
    }
    buf.emit(&[0x80, 0x79, m_jfr, 0x00]); // CMP BYTE [RCX + jfr_enter_recorded], 0
    slow.push(emit_jcc(buf, 0x85)); // JNE slow
    buf.emit(&[0x48, 0x83, 0x79, m_seed, 0x00]); // CMP QWORD [RCX + thin_seed], 0
    slow.push(emit_jcc(buf, 0x85)); // JNE slow
    if inflated_exit_precheck_enabled() {
        // Round 12 wave 1 (lock proposal W19-2): the wake decision, read
        // BEFORE the release. A release that would owe a wake goes to the
        // helper still held -- it releases and wakes -- instead of releasing
        // here, taking the monitor back with a second `LOCK CMPXCHG` and then
        // calling the same helper (and holding the monitor again for the
        // round trip, in front of every spinner). Only a hint: the decision
        // after the `XCHG` below is still the one the lost-wake-up argument
        // rests on, and a helper release is always sound.
        buf.emit(&[0x83, 0x79, m_waiters, 0x00]); // CMP DWORD [RCX + entry_waiters], 0
        let none_parked = emit_jcc(buf, 0x84); // JE release
        buf.emit(&[0x80, 0x79, m_succ, 0x00]); // CMP BYTE [RCX + succ_pending], 0
        let succ_coming = emit_jcc(buf, 0x85); // JNE release
        buf.emit(&[0x83, 0x79, m_spinners, 0x00]); // CMP DWORD [RCX + spinners], 0
        slow.push(emit_jcc(buf, 0x84)); // JE slow (a wake would be owed)
        patch_rel32_to_here(buf, none_parked);
        patch_rel32_to_here(buf, succ_coming);
    }
    // The release: count first, then the owner word (a full barrier). With
    // the sticky count (round 13 wave 12) the count stays 1: it is read only
    // by an owner, and the next acquisition finds the 1 it would write.
    if !sticky_count {
        set_count(&mut *buf, 0);
    }
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    buf.emit(&[0x48, 0x87, 0x41, m_owner]); // XCHG [RCX + owner], RAX
    // `successor_wake_needed`, read after the release.
    buf.emit(&[0x83, 0x79, m_waiters, 0x00]); // CMP DWORD [RCX + entry_waiters], 0
    let nobody_parked = emit_jcc(buf, 0x84); // JE released
    buf.emit(&[0x80, 0x79, m_succ, 0x00]); // CMP BYTE [RCX + succ_pending], 0
    let successor_coming = emit_jcc(buf, 0x85); // JNE released
    buf.emit(&[0x83, 0x79, m_spinners, 0x00]); // CMP DWORD [RCX + spinners], 0
    let spinner_takes_it = emit_jcc(buf, 0x85); // JNE released
    // A wake is owed: take the monitor back and let the helper release it.
    cas_owner(&mut *buf);
    let newer_owner = emit_jcc(buf, 0x85); // JNE released (its release wakes)
    set_count(&mut *buf, 1);
    slow.push(emit_jmp(buf)); // JMP slow
    for p in [nobody_parked, successor_coming, spinner_takes_it, newer_owner] {
        patch_rel32_to_here(buf, p);
    }
    // released:
    load_top(&mut *buf);
    emit_jmp_back(buf, arm.join_at);
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
/// StoreLoad — is the volatile STORE's obligation: every tier emits a full
/// fence after the volatile write (`x64/op_field.rs`, "SeqCst store-load
/// barrier"; the optimizing tier's is `LOCK ADD DWORD [RSP], 0` since round 11
/// wave 2). A fence placed after a load cannot supply a missing StoreLoad
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
///
/// Test-only since round 12 wave 3: the single-pass caller uses
/// [`emit_hashed_vtable_stub_located`], which also names the baked slot.
#[cfg(test)]
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

/// The hashed stub with no shadow reload (the single-pass tier's), and the
/// buffer offset of its baked `JitPICSlot` imm64 (`MOV R10, imm64`, the
/// 10-byte form: the imm64 at `+2`), `None` when nothing was emitted.
///
/// The offset is what lets the single-pass loop duplicator re-point a
/// copy's stub at the copy's own slot, as it re-points the copy's cascade and
/// helper arguments (`ic_patches`, kind 1). Before round 12 wave 3 (lane
/// mega2) the stub's imm64 was not recorded, so an unrolled copy's stub probed
/// the ORIGINAL site's hashed table (and now selector) while its helper
/// filled the copy's: valid targets, since both slots serve one call-site
/// record, but a copy's overflow receivers were published where its stub
/// never looked.
///
/// `fast_entry` (round 12 wave 7, lane mega6, W7-1) also emits the stub's
/// gate entry and returns its offset third, `None` when not asked for or when
/// nothing was emitted: see [`emit_hashed_vtable_stub_body`] for its entry
/// state, which only the single-pass cascade's megamorphic gate provides.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_hashed_vtable_stub_located(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
    fast_entry: bool,
) -> (Vec<usize>, Option<usize>, Option<usize>) {
    let mut pic_imm_at = None;
    let mut fast = None;
    let hits = emit_hashed_vtable_stub_gated(
        buf,
        pic,
        context_offset,
        arg_offsets,
        frame_record,
        service_helper,
        info_ptr,
        deopt_args_base,
        None,
        &mut pic_imm_at,
        fast_entry.then_some(&mut fast),
    );
    (hits, pic_imm_at, fast)
}

/// `CRATONVM_JIT_MEGA_GATE_FAST_ENTRY` (default on; `0` sends the single-pass
/// megamorphic gate to the stub's full entry, as round 12 wave 6 did). Round
/// 12 wave 7, lane mega6, W7-1 (W6-1 / W5-5 in `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`):
/// the gate enters the hashed stub past its receiver reload, null test, kind
/// screen and class-id reload, which the cascade in front of it has already
/// done. Read per emitted site.
pub(crate) fn mega_gate_fast_entry_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_GATE_FAST_ENTRY")
}

/// The hashed stub whose callee-deopt service first copies the
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
    let mut pic_imm_at = None;
    emit_hashed_vtable_stub_gated(
        buf,
        pic,
        context_offset,
        arg_offsets,
        frame_record,
        service_helper,
        info_ptr,
        deopt_args_base,
        reload,
        &mut pic_imm_at,
        None,
    )
}

/// [`emit_hashed_vtable_stub_reloading`] that also emits the stub's gate
/// entry and returns its offset second (`None` when no stub was emitted).
/// Round 13 wave 5, lane callcost2, proposal M13-2
/// (`jit-r13-mega-proposals-RETIRED-20260929.md`): the optimizing tier's cascade reaches its
/// megamorphic region with EAX = the class id of a non-null, kind-screened
/// receiver on every edge but the null and kind ones, so those edges can
/// enter past the stub's receiver reload, null test, kind screen and class-id
/// reload, as the single-pass cascade has since round 12 wave 7 (W7-1). The
/// caller must reach the gate entry with EAX intact and R10 free.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_hashed_vtable_stub_reloading_with_gate_entry(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
    reload: Option<&ShadowCopyBack<'_>>,
) -> (Vec<usize>, Option<usize>) {
    let mut pic_imm_at = None;
    let mut fast = None;
    let hits = emit_hashed_vtable_stub_gated(
        buf,
        pic,
        context_offset,
        arg_offsets,
        frame_record,
        service_helper,
        info_ptr,
        deopt_args_base,
        reload,
        &mut pic_imm_at,
        Some(&mut fast),
    );
    (hits, fast)
}

/// Whether [`emit_hashed_vtable_stub_reloading`] emits a stub at all for a
/// site with `num_args` Java arguments (receiver included) and PIC slot
/// `pic`: the admission it applies, so a caller can decide before emitting a
/// branch that only the stub's gate entry can take (round 13 wave 5, lane
/// callcost2).
pub(crate) fn hashed_stub_admitted(pic: usize, num_args: usize) -> bool {
    hashed_stub_admission(pic, num_args).is_some()
}

/// The hashed stub's admission, then [`emit_hashed_vtable_stub_body`];
/// `pic_imm_at` receives the offset of the baked slot imm64 when a stub is
/// emitted, and `fast_entry`, when given, the offset of its gate entry.
#[allow(clippy::too_many_arguments)]
fn emit_hashed_vtable_stub_gated(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
    reload: Option<&ShadowCopyBack<'_>>,
    pic_imm_at: &mut Option<usize>,
    fast_entry: Option<&mut Option<usize>>,
) -> Vec<usize> {
    let Some(block) = hashed_stub_admission(pic, arg_offsets.len()) else {
        return Vec::new();
    };
    let mega_table = hashed_stub_mega_table_enabled();
    let class_slots = mega_table && hashed_stub_class_slots_enabled();
    let (hits, imm_at, fast) = emit_hashed_vtable_stub_body(
        buf,
        pic,
        context_offset,
        arg_offsets,
        frame_record,
        service_helper,
        info_ptr,
        deopt_args_base,
        reload,
        block,
        mega_table,
        class_slots,
        class_slots && mega_cell_first_enabled(),
        fast_entry.is_some(),
    );
    *pic_imm_at = Some(imm_at);
    if let Some(out) = fast_entry {
        *out = fast;
    }
    hits
}

/// Whether a site with `num_args` Java arguments (receiver included) and PIC
/// slot `pic` gets the hashed stub, and its outgoing stack block if so:
/// `Some(None)` for a site that fits the registers. The one admission both the
/// stub and [`megamorphic_gate_admitted`] read, so a gate is never emitted in
/// front of a stub that is not.
fn hashed_stub_admission(pic: usize, num_args: usize) -> Option<Option<(i32, i32)>> {
    // This is a raw JIT-to-JIT call, exactly like the inline MIC/PIC hits.
    // It must obey the same master gate: the resolving helper enters the
    // callee through JitEntryGuard, whereas this stub deliberately bypasses
    // it. Previously CRATONVM_JIT_DIRECT_CALLEE_CALLS=0 disabled only the
    // MIC/PIC cascades and silently left this megamorphic raw edge live, so
    // the moving collector still found an unguarded callee frame.
    if !crate::direct_jit_callee_calls_enabled() {
        return None;
    }
    // A zero slot base would be baked as `MOV R10, 0` and then dereferenced —
    // `CMP EDX,[R10+RCX*4+96]` — so the FIRST megamorphic dispatch through this
    // site faults inside generated code, with a PC that names this method and an
    // address that names nothing. Both callers filter a null slot today; refusing
    // here means a third one cannot reintroduce that crash by omission. Emitting
    // nothing leaves the site on its resolving helper, which is always correct.
    if pic == 0 {
        return None;
    }
    // Round 12 wave 2 (lane calls, `r12w2-calls-hashed-stub-refuses-over-wide-
    // sites-patch`): a site whose context ABI overflows the registers marshals
    // the overflow on the stack, as its MIC/PIC hits have since round 11 (up to
    // the same cap). It used to be refused outright, which on Win64 sent every
    // overflow receiver of a receiver + 3 or wider site to the Rust helper.
    let stack_words = (num_args + 1).saturating_sub(ENTRY_ABI_REGS.len());
    if num_args == 0
        || stack_words > hashed_stub_max_stack_words()
        || (stack_words > 0 && !hashed_stub_wide_enabled())
        || JitPICSlot::MEGA_SET_SHIFT >= 32
    {
        return None;
    }
    Some(hashed_stub_stack_block(stack_words))
}

/// `CRATONVM_JIT_MEGA_CELL_FIRST` (default on; `0` emits no megamorphic gate
/// and the hashed stub in its round-12-wave-5 order, byte for byte). Round 12
/// wave 6, lane mega5, proposal W5-4: a site whose PIC has overflowed goes
/// from its cascade's head (after the MIC in the optimizing tier, after inline
/// way 0 in the single-pass one) straight to the hashed stub, and the stub
/// probes the receiver's class cell BEFORE the site's own hashed ways --
/// HotSpot's vtable-stub order, one predictable path for every receiver in
/// place of a chain of receiver-dependent compares. Read per emitted site.
pub(crate) fn mega_cell_first_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MEGA_CELL_FIRST")
}

/// Whether a cascade may put [`emit_megamorphic_gate`] in front of its ways:
/// the site gets the hashed stub ([`hashed_stub_admission`]) and that stub is
/// the cell-first one. The caller must also emit the stub itself (its own
/// switches: the single-pass `CRATONVM_JIT_SP_INLINE_MEGA`), since the gate's
/// only target is the region that starts with it.
pub(crate) fn megamorphic_gate_admitted(pic: usize, num_args: usize) -> bool {
    hashed_stub_admission(pic, num_args).is_some()
        && hashed_stub_mega_table_enabled()
        && hashed_stub_class_slots_enabled()
        && mega_cell_first_enabled()
}

/// The megamorphic gate (round 12 wave 6, lane mega5, W5-4), with R10 = the
/// site's [`JitPICSlot`]: `CMP dword [R10 + MEGAMORPHIC_OFFSET], 0` then
/// `JNE rel32`, whose rel32 offset is returned for the caller to patch to the
/// start of its megamorphic region -- the code its cascade's final miss
/// reaches, which runs the hashed stub and then the resolving helper. Touches
/// flags only, so every register the cascade set up survives the fall-through.
///
/// Sound by construction: the jump skips cache ways, and a cascade may always
/// miss. Everything the cascade's own final miss relies on (R10 still the
/// slot, nothing staged that the region re-reads) holds at the gate, which
/// sits where the cascade's ways would have missed from. Since round 12 wave 7
/// (lane mega6, W7-1) the single-pass cascade patches it to the stub's gate
/// entry instead ([`emit_hashed_vtable_stub_located`] with `fast_entry`),
/// whose contract -- EAX the class id of a non-null plain-object receiver --
/// that cascade meets at the gate; the optimizing tier's `.slow` stages its
/// arguments through RAX first, so it keeps the full entry.
pub(crate) fn emit_megamorphic_gate(buf: &mut ExecutableBuffer) -> usize {
    const _: () = assert!(JitPICSlot::MEGAMORPHIC_OFFSET <= 127);
    // 83 /7 ib with REX.B and ModRM(01, 111, 010): CMP dword [R10 + disp8], imm8.
    buf.emit(&[0x41, 0x83, 0x7A, JitPICSlot::MEGAMORPHIC_OFFSET as u8, 0x00]);
    emit_jcc(buf, 0x85) // JNE rel32
}

/// `CRATONVM_JIT_HASHED_STUB_MEGA_TABLE` (default on; `0` emits the stub
/// without its shared-table probe, byte for byte the round-12-wave-2 stub).
/// Round 12 wave 3, lane mega2, proposal W2-1 step 2. Read per emitted site,
/// so it adds no process global.
pub(crate) fn hashed_stub_mega_table_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HASHED_STUB_MEGA_TABLE")
}

/// `CRATONVM_JIT_HASHED_STUB_CLASS_SLOTS` (default on; `0` emits the stub
/// without its class-slot probe, byte for byte the round-12-wave-3 stub).
/// Round 12 wave 4, lane mega3, proposal M3-1. Only meaningful with the
/// shared-table probe on. Read per emitted site, like the switch above.
pub(crate) fn hashed_stub_class_slots_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HASHED_STUB_CLASS_SLOTS")
}

/// [`emit_hashed_vtable_stub_reloading`] past its admission: `block` is the
/// site's outgoing stack block, and `mega_table` selects the shared-table
/// probe on the per-site hashed table's miss, preceded by the class-slot probe
/// when `class_slots` is set too. `cell_first` (with both) moves the class-slot
/// probe AHEAD of the per-site hashed ways instead (round 12 wave 6, lane
/// mega5, W5-4; [`mega_cell_first_enabled`]). Returns the hit exits, the
/// offset of the baked slot imm64 and, with `fast_entry`, the offset of the
/// gate entry (round 12 wave 7, lane mega6, W7-1).
///
/// The full entry is the stub's first byte: it loads the receiver from
/// `arg_offsets[0]`, sends a null or non-object receiver to the fall-through
/// and loads the class id into EDX. The gate entry skips all of that: it is
/// entered with EAX = the receiver's class id (zero-extended), the receiver
/// non-null and a plain object -- screened, or at a site no array can reach
/// -- and R10 free, which is exactly the single-pass cascade's state at its
/// megamorphic gate. It moves EAX into EDX and joins the full entry right
/// after its class-id load. It is emitted out of line after the tail's
/// unconditional `JMP`, so nothing falls into it.
#[allow(clippy::too_many_arguments)]
fn emit_hashed_vtable_stub_body(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
    service_helper: usize,
    info_ptr: usize,
    deopt_args_base: i32,
    reload: Option<&ShadowCopyBack<'_>>,
    block: Option<(i32, i32)>,
    mega_table: bool,
    class_slots: bool,
    cell_first: bool,
    fast_entry: bool,
) -> (Vec<usize>, usize, Option<usize>) {
    let cell_first = cell_first && mega_table && class_slots;
    // Straight to the resolving helper: a null or non-object receiver.
    let mut miss_patches = Vec::with_capacity(6);
    // The per-site hashed table missed with EDX = the receiver's class id and
    // R10 = the PIC: the shared table's probe starts from exactly that state
    // (round 12 wave 3, lane mega2). Without the probe these join
    // `miss_patches`, and the stub is the one it always was.
    let mut hashed_miss_patches: Vec<usize> = Vec::with_capacity(3);

    // receiver -> RAX, null -> resolving helper, class id -> EDX.
    emit_load_frame(buf, RAX, arg_offsets[0]);
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    miss_patches.push(emit_jcc(buf, 0x84)); // JZ slow

    // Array-receiver guard. The hashed ways below compare only the 4-byte
    // `ObjectHeader.class_id`, which a reference array fills with its COMPONENT
    // class id — so a `Foo[]` receiver matches a way published for a `Foo`
    // receiver and is called into `Foo`'s method body. The kind tag separates
    // them; it lives in the `KIND_TAGS_BYTE_OFFSET`, 6 since the 8-byte
    // header (the mark word at 4, its byte 2); offset 8 is a long header's
    // shape word and a compact instance's first field. Anything not a plain
    // object takes the resolving helper, which dispatches arrays on
    // `java/lang/Object`.
    //   TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    buf.emit(&[
        0xF6,
        0x40,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::KIND_TAG_BYTE_MASK,
    ]);
    miss_patches.push(emit_jcc(buf, 0x85)); // JNE slow
    buf.emit(&[0x8B, 0x10]); // MOV EDX,[RAX]
    // Where the gate entry joins (W7-1): EDX = the class id, RAX dead.
    let after_class_id = buf.pos();

    // Round 12 wave 6 (lane mega5, W5-4): the receiver's class cell first. The
    // probe's entry state is EDX = class id and R10 = the slot; it leaves both
    // as it found them, so its every miss falls into the hash below. Its hit
    // jumps FORWARD to the shared tail, patched there.
    let mut cell_hit: Option<usize> = None;
    let early_imm_at = if cell_first {
        // `emit_mov_imm64` is always the 10-byte `REX.W B8+r imm64` form.
        let at = buf.pos() + 2;
        emit_mov_imm64(buf, R10, pic as u64);
        cell_hit = emit_mega_class_slot_probe(buf, None);
        Some(at)
    } else {
        None
    };

    // ECX = ((class_id * golden-ratio hash) >> shift) * 2.
    buf.emit(&[0x69, 0xCA]); // IMUL ECX,EDX,imm32
    buf.emit(&JitPICSlot::MEGA_HASH_MULTIPLIER.to_le_bytes());
    buf.emit(&[0xC1, 0xE9, JitPICSlot::MEGA_SET_SHIFT]); // SHR ECX,shift
    buf.emit(&[0xD1, 0xE1]); // SHL ECX,1
    let pic_imm_at = match early_imm_at {
        Some(at) => at,
        None => {
            // `emit_mov_imm64` is always the 10-byte `REX.W B8+r imm64` form.
            let at = buf.pos() + 2;
            emit_mov_imm64(buf, R10, pic as u64);
            at
        }
    };

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
        hashed_miss_patches.push(emit_jcc(buf, 0x84));

        if way == 0 {
            to_tail = Some(emit_jmp(buf)); // hit: JMP tail
            patch_rel32_to_here(buf, next_or_miss);
        } else {
            hashed_miss_patches.push(next_or_miss);
        }
    }
    // tail: both hits arrive here with the way's entry word in R11, and so
    // does a hit of the shared-table probe below (a backward jump).
    if let Some(p) = to_tail {
        patch_rel32_to_here(buf, p);
    }
    if let Some(p) = cell_hit {
        patch_rel32_to_here(buf, p);
    }
    let tail = buf.pos();

    // Select the compiled entry ABI from the word just loaded: bit 0 is the
    // needs-context tag (`JIT_IC_NEEDS_CONTEXT_TAG`). BTR moves it into CF
    // and leaves the bare entry in R11, so the ABI and the target cannot
    // come from two different publications of this way.
    const _: () = assert!(crate::JIT_IC_NEEDS_CONTEXT_TAG == 1);
    buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // BTR R11, 0
    let no_context = emit_jcc(buf, 0x83); // JNC
    emit_marshal_with_stack(buf, context_offset, arg_offsets, true, block);
    let call = emit_jmp(buf);
    patch_rel32_to_here(buf, no_context);
    emit_marshal_with_stack(buf, context_offset, arg_offsets, false, block);
    patch_rel32_to_here(buf, call);

    // R11 is deliberately outside both platform argument-register sets,
    // so marshalling cannot clobber the target loaded above.
    buf.emit(&[0x41, 0xFF, 0xD3]); // CALL R11
    // Release the outgoing block before anything else runs (RAX, the result,
    // is untouched); the republish and the deopt service run at the frame's
    // own RSP.
    if let Some((total, _)) = block {
        buf.emit(&[0x48, 0x81, 0xC4]); // ADD RSP, imm32
        buf.emit(&total.to_le_bytes());
    }
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

    // The gate entry (W7-1), behind the unconditional `JMP` above: 2 + 5
    // bytes in place of the full entry's receiver reload, null test, kind
    // screen and class-id reload.
    let fast = if fast_entry {
        let at = buf.pos();
        buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX (zero-extends into RDX)
        emit_jmp_back(buf, after_class_id);
        Some(at)
    } else {
        None
    };

    // Out of line, after the tail: a site whose receivers all fit its own
    // ways never executes a byte of it.
    if mega_table {
        for patch in hashed_miss_patches {
            patch_rel32_to_here(buf, patch);
        }
        // Round 12 wave 4 (lane mega3, M3-1): the site's class-slot cell
        // first; its every miss falls into the shared table's probe with EDX
        // and R10 as it found them. A cell-first stub probed it already.
        if class_slots && !cell_first {
            let _ = emit_mega_class_slot_probe(buf, Some(tail));
        }
        miss_patches.extend(emit_mega_dispatch_table_probe(buf, tail));
    } else {
        miss_patches.extend(hashed_miss_patches);
    }
    for patch in miss_patches {
        patch_rel32_to_here(buf, patch);
    }
    (done_patches, pic_imm_at, fast)
}

/// The machine-code reader of the VM's shared megamorphic dispatch table
/// (`inline_cache_pic::MegaDispatchTable`; round 12 wave 3, lane mega2,
/// proposal W2-1 step 2 in `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`).
///
/// Entered with EDX = the receiver's class id (zero-extended into RDX) and
/// R10 = the site's [`JitPICSlot`]; clobbers RCX, R10 and R11 only, which the
/// stub already owns (RAX, the receiver, is dead). A hit jumps back to the
/// stub's shared `tail` with the way's tagged entry word in R11, so it takes
/// the same `BTR`, marshalling, `CALL R11`, block release, frame republish and
/// callee-deopt service as a per-site hit, and the caller's frame-identity
/// restore after the stub (`emit_identity_after_shared_stub`). Returns the
/// rel32 exits to the resolving helper; the fall-through past the last way is
/// one too.
///
/// ```text
///   MOV  ECX, [R10 + MEGA_DISPATCH_SELECTOR_OFFSET] ; TEST ECX,ECX ; JZ slow
///   MOV  R10, [R10 + MEGA_DISPATCH_TABLE_OFFSET]    ; TEST R10,R10 ; JZ slow
///   MOV  R11D, ECX ; ROL R11D, 16 ; XOR R11D, EDX    ; MegaDispatchTable::set_base
///   IMUL R11D, R11D, MEGA_HASH_MULTIPLIER
///   SHR  R11D, 32 - MEGA_DISPATCH_SET_BITS ; SHL R11D, 6   ; set * 4 ways * 16 bytes
///   ADD  R10, R11                                    ; the set's first way
///   SHL  RCX, 32 ; OR RCX, RDX                       ; MegaDispatchTable::key
///   4 x { CMP RCX, [R10 + 16w] ; JNE next
///         MOV R11, [R10 + 16w + 8] ; TEST R11,R11 ; JNZ tail ; next: }
/// ```
///
/// Why a lock-free read is sound is the table's publication protocol, the
/// per-site hashed table's exactly: a way is published word first and key
/// last, retired key first (to `RETIRED_KEY`, which no key equals) and word
/// second, and its owner is released through the retirement queue, which
/// frees nothing while any thread is inside compiled code. So a reader past
/// the key compare loads the matching word, whose body the way (or the queue)
/// keeps mapped, or zero, which falls to the next way and then the helper. The
/// selector is read before the table word and was stored after it
/// (`JitPICSlot::bind_mega_dispatch`), and x86 does not reorder loads with
/// loads. What the Rust reader re-validated per hit (a superseded or retired
/// owner, the redefinition epoch) is covered by the retirement passes that
/// run before any such state is visible to a new call: see the soundness table
/// in `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md` "Wave 2 continuation".
fn emit_mega_dispatch_table_probe(buf: &mut ExecutableBuffer, tail: usize) -> Vec<usize> {
    use crate::{MEGA_DISPATCH_SET_BITS, MEGA_DISPATCH_WAYS};
    // The encoding below is written for exactly these shapes: four 16-byte
    // ways per set (key, then tagged word), a set index that fits a byte-sized
    // shift, and 32-bit displacements for the two PIC words.
    const _: () = assert!(MEGA_DISPATCH_WAYS == 4);
    const _: () = assert!(MEGA_DISPATCH_SET_BITS >= 1 && MEGA_DISPATCH_SET_BITS <= 24);
    const _: () = assert!(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET <= i32::MAX as usize);
    const _: () = assert!(JitPICSlot::MEGA_DISPATCH_TABLE_OFFSET <= i32::MAX as usize);
    const _: () = assert!(crate::JIT_IC_NEEDS_CONTEXT_TAG == 1);
    /// `log2(MEGA_DISPATCH_WAYS * 16)`: a set's byte offset from its index.
    const SET_BYTES_SHIFT: u8 = 6;
    let mut slow = Vec::with_capacity(2);

    // MOV ECX, [R10 + selector]; an unbound site (0) has nothing to find.
    buf.emit(&[0x41, 0x8B, 0x8A]);
    buf.emit(&(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET as i32).to_le_bytes());
    buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
    slow.push(emit_jcc(buf, 0x84)); // JZ slow
    // MOV R10, [R10 + table]: the table's slot base. Never zero once a
    // selector is visible; tested anyway, so no ordering slip can fault here.
    buf.emit(&[0x4D, 0x8B, 0x92]);
    buf.emit(&(JitPICSlot::MEGA_DISPATCH_TABLE_OFFSET as i32).to_le_bytes());
    buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    slow.push(emit_jcc(buf, 0x84)); // JZ slow

    // R11 = the set's byte offset. Every 32-bit operation zero-extends, so
    // the ADD below reads a clean 64-bit offset.
    buf.emit(&[0x41, 0x89, 0xCB]); // MOV R11D, ECX
    buf.emit(&[0x41, 0xC1, 0xC3, 16]); // ROL R11D, 16
    buf.emit(&[0x41, 0x31, 0xD3]); // XOR R11D, EDX
    buf.emit(&[0x45, 0x69, 0xDB]); // IMUL R11D, R11D, imm32
    buf.emit(&JitPICSlot::MEGA_HASH_MULTIPLIER.to_le_bytes());
    // Cast: `MEGA_DISPATCH_SET_BITS` is asserted in 1..=24 above.
    buf.emit(&[0x41, 0xC1, 0xEB, (32 - MEGA_DISPATCH_SET_BITS) as u8]); // SHR R11D
    buf.emit(&[0x41, 0xC1, 0xE3, SET_BYTES_SHIFT]); // SHL R11D
    buf.emit(&[0x4D, 0x01, 0xDA]); // ADD R10, R11
    // RCX = (selector << 32) | class id.
    buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
    buf.emit(&[0x48, 0x09, 0xD1]); // OR RCX, RDX

    for way in 0..MEGA_DISPATCH_WAYS {
        // Cast: `way < 4`, so both displacements fit a signed byte.
        let key_disp = (way * 16) as u8;
        buf.emit(&[0x49, 0x3B, 0x4A, key_disp]); // CMP RCX, [R10 + key]
        buf.emit(&[0x75, 0x00]); // JNE next (rel8, patched below)
        let next = buf.pos();
        buf.emit(&[0x4D, 0x8B, 0x5A, key_disp + 8]); // MOV R11, [R10 + word]
        buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        // A zero word (a way between its retirement's two stores) is a miss
        // of this way only, as in `MegaDispatchTable::lookup`.
        emit_jcc_back(buf, 0x85, tail); // JNZ tail
        // Cast: the skipped run is 13 bytes. Saturating, because an
        // overflowed buffer stops advancing (it is discarded anyway).
        let skip = buf.pos().saturating_sub(next) as u8;
        let _ = buf.try_patch_byte(next.saturating_sub(1), skip);
    }
    // Fall-through: the helper (the caller patches it with its own misses).
    slow
}

/// Bytes [`emit_mega_class_slot_probe`] emits: every forward branch in it is a
/// `rel8` to its end, which this bounds.
const MEGA_CLASS_SLOT_PROBE_BYTES: usize = 85;

/// The machine-code reader of the shared table's per-class cells
/// (`inline_cache_pic::MegaDispatchTable`, "Per-class columns"; round 12 wave
/// 4, lane mega3, proposal M3-1 in `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`): HotSpot's
/// vtable-stub shape, a direct index by the receiver's class and the site's
/// column, plus the key compare that makes the column an index and never a
/// target.
///
/// Entered with EDX = the receiver's class id (RDX zero-extended) and R10 =
/// the site's [`JitPICSlot`]; clobbers RCX and R11 only, and leaves RDX and
/// R10 as it found them, so every miss FALLS THROUGH into
/// [`emit_mega_dispatch_table_probe`]. A hit jumps back to the stub's shared
/// `tail` with the cell's tagged word in R11, exactly as a hashed hit.
///
/// ```text
///   MOV  ECX, [R10 + MEGA_CLASS_SLOT_OFFSET] ; TEST ECX,ECX ; JZ next  ; column
///   MOV  R11, [R10 + MEGA_CLASS_ROOT_OFFSET] ; TEST R11,R11 ; JZ next  ; root
///   MOV  R11, [R11]                                                  ; directory
///   CMP  RDX, [R11] ; JAE next                                       ; class < len
///   MOV  R11, [R11 + RDX*8 + 8] ; TEST R11,R11 ; JZ next             ; its array
///   CMP  RCX, [R11] ; JA next                                        ; column <= n
///   SHL  RCX, 4 ; ADD R11, RCX                                       ; the cell
///   MOV  ECX, [R10 + MEGA_DISPATCH_SELECTOR_OFFSET]
///   SHL  RCX, 32 ; OR RCX, RDX                                       ; the key
///   CMP  RCX, [R11] ; JNE next
///   MOV  R11, [R11 + 8] ; TEST R11,R11 ; JNZ tail
/// next:
/// ```
///
/// Why the lock-free read is sound: the cell is a way with the hashed ways'
/// protocol (word first and key last, key retired first, owner released
/// through the retirement queue), and every retirement pass reaches it
/// (`MegaDispatchTable`'s owner index and sweeps). The site binds the root
/// before the column and the selector before both
/// (`JitPICSlot::bind_mega_class_slot`), and x86 does not reorder loads with
/// loads, so a visible column implies a visible root and selector; an unbound
/// selector reads as 0, whose keys no cell holds. A directory and an array are
/// fully written before the release store that publishes them, and neither is
/// moved; the only array ever freed before the table is an unloaded class's,
/// once the retirement stamped after its row was zeroed is graced (no reader
/// is still between the row load and the cell load). The KEY, not the column,
/// decides a hit: a cell holding another selector's way (a column two
/// resolutions share, a package-private method that took a new vtable slot, a
/// column bound before a redefinition) cannot match, since the key carries
/// this site's selector and the receiver's class id.
///
/// `tail` is the hit target when it is already emitted (the stub's
/// out-of-line order: a backward `JNZ`). `None` (the cell-first order, round
/// 12 wave 6, lane mega5, W5-4) emits the same `JNZ rel32` with a zero
/// displacement and returns its rel32 offset for the caller to patch to the
/// tail; the encoding and its length are the same either way.
fn emit_mega_class_slot_probe(buf: &mut ExecutableBuffer, tail: Option<usize>) -> Option<usize> {
    const _: () = assert!(JitPICSlot::MEGA_CLASS_SLOT_OFFSET <= i32::MAX as usize);
    const _: () = assert!(JitPICSlot::MEGA_CLASS_ROOT_OFFSET <= i32::MAX as usize);
    const _: () = assert!(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET <= i32::MAX as usize);
    const _: () = assert!(crate::JIT_IC_NEEDS_CONTEXT_TAG == 1);
    const _: () = assert!(MEGA_CLASS_SLOT_PROBE_BYTES < 128);
    // A column is at most `MEGA_CLASS_SLOT_MAX`, so `SHL RCX, 4` cannot carry.
    const _: () = assert!(crate::MEGA_CLASS_SLOT_MAX < (1 << 28));
    let start = buf.pos();
    // Offsets just past each `rel8` to `next`, patched at the end.
    let mut to_next: Vec<usize> = Vec::with_capacity(6);
    /// `Jcc rel8` to `next`, recorded for the patch below.
    fn jcc8(buf: &mut ExecutableBuffer, to_next: &mut Vec<usize>, opcode: u8) {
        buf.emit(&[opcode, 0x00]);
        to_next.push(buf.pos());
    }

    // MOV ECX, [R10 + column]; an unbound site (0) has no cell.
    buf.emit(&[0x41, 0x8B, 0x8A]);
    buf.emit(&(JitPICSlot::MEGA_CLASS_SLOT_OFFSET as i32).to_le_bytes());
    buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
    jcc8(buf, &mut to_next, 0x74); // JZ next
    // MOV R11, [R10 + root]: never zero once a column is visible; tested
    // anyway, so no ordering slip can fault here.
    buf.emit(&[0x4D, 0x8B, 0x9A]);
    buf.emit(&(JitPICSlot::MEGA_CLASS_ROOT_OFFSET as i32).to_le_bytes());
    buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    jcc8(buf, &mut to_next, 0x74); // JZ next
    buf.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]: the current directory
    buf.emit(&[0x49, 0x3B, 0x13]); // CMP RDX, [R11]: class id < len?
    jcc8(buf, &mut to_next, 0x73); // JAE next
    buf.emit(&[0x4D, 0x8B, 0x5C, 0xD3, 0x08]); // MOV R11, [R11 + RDX*8 + 8]
    buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11: no array for the class
    jcc8(buf, &mut to_next, 0x74); // JZ next
    buf.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]: column <= columns?
    jcc8(buf, &mut to_next, 0x77); // JA next
    buf.emit(&[0x48, 0xC1, 0xE1, 0x04]); // SHL RCX, 4: a cell is 16 bytes
    buf.emit(&[0x49, 0x01, 0xCB]); // ADD R11, RCX: the cell (header = column 0)
    // RCX = (selector << 32) | class id, `MegaDispatchTable::key`.
    buf.emit(&[0x41, 0x8B, 0x8A]);
    buf.emit(&(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET as i32).to_le_bytes());
    buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
    buf.emit(&[0x48, 0x09, 0xD1]); // OR RCX, RDX
    buf.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]
    jcc8(buf, &mut to_next, 0x75); // JNE next
    buf.emit(&[0x4D, 0x8B, 0x5B, 0x08]); // MOV R11, [R11 + 8]: the tagged word
    buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    // A zero word (a cell between its retirement's two stores) is a miss.
    let hit = match tail {
        Some(tail) => {
            emit_jcc_back(buf, 0x85, tail); // JNZ tail
            None
        }
        // JNZ tail, forward: `0F 85` + rel32, the backward form's six bytes.
        None => Some(emit_jcc(buf, 0x85)),
    };

    let next = buf.pos();
    if next.saturating_sub(start) != MEGA_CLASS_SLOT_PROBE_BYTES && !buf.overflowed() {
        // The rel8 bound below was argued for exactly this encoding.
        buf.mark_codegen_unencodable("mega-class-slot-probe-length");
    }
    for after in to_next {
        // Cast: at most `MEGA_CLASS_SLOT_PROBE_BYTES` (< 128) ahead.
        let skip = next.saturating_sub(after) as u8;
        let _ = buf.try_patch_byte(after.saturating_sub(1), skip);
    }
    hit
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
            0xF6u8,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ];
        let guard_at = bytes
            .windows(guard.len())
            .position(|w| w == guard)
            .expect("the stub must emit TEST BYTE [RAX+kind], KIND_TAG_BYTE_MASK");
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

    /// Round 12 wave 2 (lane calls): a site whose context ABI overflows the
    /// argument registers gets the stub with ONE outgoing block, reserved on
    /// each ABI branch and released once after the single `CALL R11`; past the
    /// cap it still gets nothing. It used to get nothing at any overflow.
    #[test]
    fn r12w2_an_over_wide_site_gets_the_stub_with_one_stack_block() {
        crate::x64::set_moving_young_override(Some(false));
        if !hashed_stub_wide_enabled() || !crate::direct_jit_callee_calls_enabled() {
            crate::x64::set_moving_young_override(None);
            return;
        }
        let count = |hay: &[u8], needle: &[u8]| {
            hay.windows(needle.len()).filter(|w| *w == needle).count()
        };
        // Context + receiver + regs + 1 arguments: three stack words.
        let wide: Vec<i32> = (0..ENTRY_ABI_REGS.len() + 2)
            .map(|i| 8 * (i as i32 + 1))
            .collect();
        let mut buf = ExecutableBuffer::new(8192).expect("buffer");
        let hits =
            emit_hashed_vtable_stub(&mut buf, 0x7fff_0000_0000_2000, 24, &wide, 0, 0, 0, 32);
        let bytes = buf.as_slice();
        assert_eq!(hits.len(), 1, "the stub is emitted");
        let (total, _) = hashed_stub_stack_block(3).expect("three words need a block");
        let mut sub = vec![0x48, 0x81, 0xEC];
        sub.extend_from_slice(&total.to_le_bytes());
        let mut add = vec![0x48, 0x81, 0xC4];
        add.extend_from_slice(&total.to_le_bytes());
        assert_eq!(count(bytes, &sub), 2, "one reservation per ABI branch");
        assert_eq!(count(bytes, &add), 1, "one release after the shared CALL");
        assert_eq!(count(bytes, &[0x41, 0xFF, 0xD3]), 1, "one CALL R11");
        assert_eq!(total % 16, 0);
        // Past the cap: nothing.
        let too_wide: Vec<i32> = (0..ENTRY_ABI_REGS.len() + hashed_stub_max_stack_words())
            .map(|i| 8 * (i as i32 + 1))
            .collect();
        let mut buf = ExecutableBuffer::new(8192).expect("buffer");
        let hits =
            emit_hashed_vtable_stub(&mut buf, 0x7fff_0000_0000_2000, 24, &too_wide, 0, 0, 0, 32);
        assert!(hits.is_empty() && buf.pos() == 0, "past the cap the site keeps the helper");
        crate::x64::set_moving_young_override(None);
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

    /// Nothing is emitted, not a byte, when the inline thin-lock path is not
    /// wired: the caller then emits its helper sequence alone, the pre-wave-7
    /// code exactly.
    #[test]
    fn inline_thin_lock_declines_without_emitting() {
        let mut buf = ExecutableBuffer::new(512).expect("buffer");
        let cases = [
            (8, 0, ThinLockThread::Tls(0x1480)),
            (8, 64, ThinLockThread::Tls(0)),
            (8, 64, ThinLockThread::Frame(0)),
            (0, 64, ThinLockThread::Frame(16)),
        ];
        for (obj_off, block, thread) in cases {
            for (enter, census) in [(true, false), (false, false), (true, true), (false, true)] {
                assert!(
                    emit_inline_thin_lock(&mut buf, enter, obj_off, block, thread, census).is_err()
                );
                assert_eq!(buf.pos(), 0, "{obj_off} {block} {thread:?}");
            }
        }
    }

    /// Where the executed thin-lock tests place the `JitMonitorBlock` inside
    /// their fake `JvmThread`.
    #[cfg(target_arch = "x86_64")]
    const THIN_BLOCK: usize = 64;

    /// A fake object for the executed thin-lock tests: the 8-byte header's
    /// class id and 32-bit mark word, then a body.
    #[cfg(target_arch = "x86_64")]
    #[repr(C, align(8))]
    #[derive(Default)]
    struct FakeObj {
        _class_id: std::sync::atomic::AtomicU32,
        mark: std::sync::atomic::AtomicU32,
        _body: [std::sync::atomic::AtomicU64; 2],
    }

    #[cfg(target_arch = "x86_64")]
    const _: () =
        assert!(std::mem::offset_of!(FakeObj, mark) == cratonvm_types::MARK_WORD_OFFSET);

    /// A fake `LeaseBlock` (`vm/src/threading/monitor.rs`) at the
    /// `LEASE_BLOCK_*` offsets: what `JitMonitorBlock::held` points at.
    #[cfg(target_arch = "x86_64")]
    #[repr(C, align(64))]
    #[derive(Default)]
    struct FakeLease {
        acquired: std::sync::atomic::AtomicU32,
        _pad: std::sync::atomic::AtomicU32,
        owner_word: std::sync::atomic::AtomicU64,
        key: std::sync::atomic::AtomicU64,
        monitor: std::sync::atomic::AtomicU64,
        epoch: std::sync::atomic::AtomicU64,
        epoch_addr: std::sync::atomic::AtomicU64,
        key2: std::sync::atomic::AtomicU64,
        monitor2: std::sync::atomic::AtomicU64,
    }

    #[cfg(target_arch = "x86_64")]
    const _: () = {
        assert!(std::mem::offset_of!(FakeLease, key2) == LEASE_BLOCK_KEY2_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, monitor2) == LEASE_BLOCK_MONITOR2_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, acquired) == LEASE_BLOCK_ACQUIRED_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, owner_word) == LEASE_BLOCK_OWNER_WORD_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, key) == LEASE_BLOCK_KEY_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, monitor) == LEASE_BLOCK_MONITOR_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, epoch) == LEASE_BLOCK_EPOCH_OFFSET);
        assert!(std::mem::offset_of!(FakeLease, epoch_addr) == LEASE_BLOCK_EPOCH_ADDR_OFFSET);
    };

    /// A fake inflated `Monitor`'s hand-over words at the
    /// `INFLATED_MONITOR_*` offsets.
    #[cfg(target_arch = "x86_64")]
    #[repr(C, align(64))]
    #[derive(Default)]
    struct FakeMonitor {
        owner: std::sync::atomic::AtomicU64,
        thin_seed: std::sync::atomic::AtomicU64,
        _count_hole: [u8; 4],
        jfr: std::sync::atomic::AtomicU8,
        _pad0: [u8; 3],
        _pad1: [u64; 5],
        entry_waiters: std::sync::atomic::AtomicU32,
        spinners: std::sync::atomic::AtomicU32,
        succ: std::sync::atomic::AtomicU8,
        _pad2: [u8; 3],
        spin_limit: std::sync::atomic::AtomicU32,
        _pad3: [u64; 6],
        entry_count: std::sync::atomic::AtomicU32,
        _count_pad: [u8; 4],
        census_spin_wins: std::sync::atomic::AtomicU64,
        census_spin_budget_outs: std::sync::atomic::AtomicU64,
        census_spin_waiter_exits: std::sync::atomic::AtomicU64,
    }

    #[cfg(target_arch = "x86_64")]
    const _: () = {
        assert!(std::mem::offset_of!(FakeMonitor, owner) == INFLATED_MONITOR_OWNER_OFFSET);
        assert!(std::mem::offset_of!(FakeMonitor, thin_seed) == INFLATED_MONITOR_THIN_SEED_OFFSET);
        assert!(
            std::mem::offset_of!(FakeMonitor, entry_count) == INFLATED_MONITOR_ENTRY_COUNT_OFFSET
        );
        assert!(
            std::mem::offset_of!(FakeMonitor, entry_waiters)
                == INFLATED_MONITOR_ENTRY_WAITERS_OFFSET
        );
        assert!(std::mem::offset_of!(FakeMonitor, spinners) == INFLATED_MONITOR_SPINNERS_OFFSET);
        assert!(std::mem::offset_of!(FakeMonitor, jfr) == INFLATED_MONITOR_JFR_OFFSET);
        assert!(std::mem::offset_of!(FakeMonitor, succ) == INFLATED_MONITOR_SUCC_PENDING_OFFSET);
        assert!(
            std::mem::offset_of!(FakeMonitor, spin_limit) == INFLATED_MONITOR_SPIN_LIMIT_OFFSET
        );
        assert!(
            std::mem::offset_of!(FakeMonitor, census_spin_wins)
                == INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET
        );
        assert!(
            std::mem::offset_of!(FakeMonitor, census_spin_budget_outs)
                == INFLATED_MONITOR_CENSUS_SPIN_BUDGET_OUTS_OFFSET
        );
        assert!(
            std::mem::offset_of!(FakeMonitor, census_spin_waiter_exits)
                == INFLATED_MONITOR_CENSUS_SPIN_WAITER_EXITS_OFFSET
        );
    };

    /// The fake thread (a word array holding a `JitMonitorBlock` at
    /// [`THIN_BLOCK`]), its JMX lock stack and its lease block, all at the
    /// `cratonvm_jit_api` / `LEASE_BLOCK_*` offsets compiled code bakes.
    #[cfg(target_arch = "x86_64")]
    struct ThinLockRig {
        ls: [std::sync::atomic::AtomicU64; cratonvm_jit_api::JIT_LOCK_STACK_SPILLED_OFFSET / 8 + 1],
        thread: [std::sync::atomic::AtomicU64;
            (THIN_BLOCK + cratonvm_jit_api::JIT_MONITOR_BLOCK_HELD_OFFSET) / 8 + 1],
        held: FakeLease,
    }

    #[cfg(target_arch = "x86_64")]
    impl ThinLockRig {
        fn new() -> Self {
            use std::sync::atomic::AtomicU64;
            Self {
                ls: std::array::from_fn(|_| AtomicU64::new(0)),
                thread: std::array::from_fn(|_| AtomicU64::new(0)),
                held: FakeLease::default(),
            }
        }

        /// One `JitMonitorBlock` word.
        fn block(&self, off: usize) -> &std::sync::atomic::AtomicU64 {
            &self.thread[(THIN_BLOCK + off) / 8]
        }

        /// One `JmxLockStack` word.
        fn ls_word(&self, off: usize) -> u64 {
            self.ls[off / 8].load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Arm the block for `owner_bits` (in place: it stores addresses of
        /// this rig's own fields, so the rig must not move afterwards).
        fn arm(&self, owner_bits: u32) {
            use cratonvm_jit_api::{
                JIT_MONITOR_BLOCK_HELD_OFFSET, JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET,
                JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET,
            };
            use std::sync::atomic::Ordering::Relaxed;
            self.block(JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET)
                .store(self.ls.as_ptr() as u64, Relaxed);
            self.block(JIT_MONITOR_BLOCK_HELD_OFFSET)
                .store(&self.held as *const _ as u64, Relaxed);
            self.block(JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET)
                .store(u64::from(owner_bits), Relaxed);
        }

        fn thread_addr(&self) -> u64 {
            self.thread.as_ptr() as u64
        }
    }

    /// `extern "C" fn(obj, thread) -> 1 (inline path taken) | 0 (fell to the
    /// slow label)`, around one emitted sequence whose thread comes from a
    /// frame word.
    ///
    /// The recursion arm is pinned OFF here (the tests written before round
    /// 13 wave 8 assert that a re-entry falls to the helper); see
    /// [`build_thin_lock_probe_with`].
    #[cfg(target_arch = "x86_64")]
    fn build_thin_lock_probe(enter: bool, census: bool) -> ExecutableBuffer {
        let off = InlineRecursion {
            thin: false,
            inflated: false,
            two_way: false,
        };
        build_thin_lock_probe_with(enter, census, off)
    }

    /// [`build_thin_lock_probe`] with the re-entry arms' switches explicit.
    ///
    /// Round 13 wave 12 (lane monitor3): the tests written before the sticky
    /// inflated entry count assert that a compiled final release leaves the
    /// count at 0, so this pins `CRATONVM_JIT_INFLATED_STICKY_COUNT` off; the
    /// sticky arm has its own builder ([`build_thin_lock_probe_shaped`]) and
    /// test. Neither lock-stack shape changes `seq`'s value after a write, so
    /// every test passes in both `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP` arms.
    #[cfg(target_arch = "x86_64")]
    fn build_thin_lock_probe_with(
        enter: bool,
        census: bool,
        recursion: InlineRecursion,
    ) -> ExecutableBuffer {
        build_thin_lock_probe_shaped(
            enter,
            census,
            recursion,
            false,
            &[(STICKY_COUNT_SWITCH, Some("0"))],
        )
    }

    /// `CRATONVM_JIT_INFLATED_STICKY_COUNT`, pinned by the probe builders.
    #[cfg(target_arch = "x86_64")]
    const STICKY_COUNT_SWITCH: &str = "CRATONVM_JIT_INFLATED_STICKY_COUNT";
    /// `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`, pinned by the round-13-wave-12
    /// tests.
    #[cfg(target_arch = "x86_64")]
    const SINGLE_BUMP_SWITCH: &str = "CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP";

    /// [`build_thin_lock_probe_with`] with the null test's presence
    /// (`receiver_non_null`) explicit and the switches the emitter reads at
    /// emission pinned by `edits` (one override: a nested
    /// `with_thread_overrides` would start again from the environment).
    #[cfg(target_arch = "x86_64")]
    fn build_thin_lock_probe_shaped(
        enter: bool,
        census: bool,
        recursion: InlineRecursion,
        receiver_non_null: bool,
        edits: &[(&str, Option<&str>)],
    ) -> ExecutableBuffer {
        let mut buf = ExecutableBuffer::new(1024).expect("buffer");
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 32
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x20]);
        emit_store_frame(&mut buf, 8, ENTRY_ABI_REGS[0]);
        emit_store_frame(&mut buf, 16, ENTRY_ABI_REGS[1]);
        let done = cratonvm_types::flags::with_thread_overrides(edits, || {
            emit_inline_thin_lock_with(
                &mut buf,
                enter,
                8,
                THIN_BLOCK,
                ThinLockThread::Frame(16),
                census,
                recursion,
                receiver_non_null,
            )
        })
        .expect("wired");
        buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // slow: XOR EAX,EAX; LEAVE; RET
        patch_rel32_to_here(&mut buf, done);
        buf.emit(&[0xB8, 1, 0, 0, 0, 0xC9, 0xC3]); // done: MOV EAX,1; LEAVE; RET
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    /// Runs the emitted enter/exit sequences against a hand-built thread
    /// block, lock stack, `held` counter and 8-byte object header laid out by
    /// the `cratonvm_jit_api` / `cratonvm_types` constants. The enter takes
    /// EXACTLY the words `ObjectHeader::thin_lockable` accepts and writes
    /// exactly `make_thin_locked(mark, slot, 0)`; the exit restores exactly
    /// `make_neutral`; each winning CAS moves `held` by one, and every refusal
    /// of the contract (`r11w5-sync-inline-thin-lock-contract-patch`) leaves
    /// every word untouched for the helper.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_thin_lock_takes_only_the_uncontended_case() {
        use cratonvm_jit_api::{
            JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS, JIT_LOCK_STACK_SLOTS_OFFSET,
            JIT_LOCK_STACK_SPILLED_OFFSET, JIT_LOCK_STACK_TOP_OFFSET,
            JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET,
        };
        use cratonvm_types::{
            ObjectHeader, GC_FLAG_COMPACT, GC_FLAG_OLD_GEN, MARK_FORWARDED, MARK_INFLATED,
            MARK_SHORT_HASH_HI_SHIFT,
        };
        use std::sync::atomic::Ordering::Relaxed;

        let enter_buf = build_thin_lock_probe(true, false);
        let exit_buf = build_thin_lock_probe(false, false);
        // SAFETY: both buffers were finalized above and hold a complete
        // `extern "C" fn(u64, u64) -> u64` built by this test.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };

        let rig = ThinLockRig::new();
        let slot: u32 = 5;
        let owner = ObjectHeader::make_thin_locked(0, slot, 0);
        rig.arm(owner);
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let other = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let other_addr = &other as *const FakeObj as u64;
        let seq = || rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET);
        let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
        let slot0 = || rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET);
        let held = || rig.held.acquired.load(Relaxed);

        // The enter screen is exactly `thin_lockable`, over quartets that
        // matter: a legacy aged/old instance, arrays with and without an
        // element type (their bits 18..21 are NOT a hash), a compact
        // instance, and a compact instance whose only hash bits are the high
        // ones (payload zero -- the case a low-16-bits screen gets wrong).
        let compact = u32::from(GC_FLAG_COMPACT) << 24;
        let quartets = [
            0u32,
            (u32::from(GC_FLAG_OLD_GEN) << 24) | (7 << 28),
            (1 << 16) | (5 << 18),
            1 << 16,
            (2 << 16) | (0xF << 18),
            compact,
            compact | (1 << MARK_SHORT_HASH_HI_SHIFT),
            compact | (0x20 << MARK_SHORT_HASH_HI_SHIFT),
            compact | (1 << 16) | (5 << 18),
        ];
        let lows = [
            0u32,
            0x2A << 2, // a compact instance's in-payload hash
            1 << 15,
            owner,
            ObjectHeader::make_thin_locked(0, slot + 1, 0),
            MARK_INFLATED,
            MARK_FORWARDED,
        ];
        let mut taken = 0u32;
        for q in quartets {
            for low in lows {
                let mark = q | low;
                obj.mark.store(mark, Relaxed);
                let (s0, t0, h0) = (seq(), top(), held());
                let lockable = ObjectHeader::thin_lockable(mark);
                assert_eq!(enter_fn(obj_addr, thread_addr), u64::from(lockable), "{mark:#x}");
                if !lockable {
                    assert_eq!(obj.mark.load(Relaxed), mark, "{mark:#x} untouched");
                    assert_eq!((seq(), top(), held()), (s0, t0, h0), "{mark:#x}");
                    continue;
                }
                taken += 1;
                assert_eq!(
                    obj.mark.load(Relaxed),
                    ObjectHeader::make_thin_locked(mark, slot, 0),
                    "{mark:#x}: the quartet rides along"
                );
                assert_eq!((seq(), top(), slot0(), held()), (s0 + 2, 1, obj_addr, h0 + 1));
                assert_eq!(exit_fn(obj_addr, thread_addr), 1, "{mark:#x}");
                assert_eq!(
                    obj.mark.load(Relaxed),
                    ObjectHeader::make_neutral(mark),
                    "{mark:#x}: the outermost release keeps the quartet"
                );
                assert_eq!((seq(), top(), slot0(), held()), (s0 + 4, 0, 0, h0));
            }
        }
        assert!(taken >= 7, "the sweep must exercise the fast path ({taken})");

        let quartet: u32 = 0x1234 << cratonvm_types::MARK_QUARTET_SHIFT;
        let locked = ObjectHeader::make_thin_locked(quartet, slot, 0);
        // Uncontended enter, then the exit refusals.
        obj.mark.store(quartet, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        let (s1, h1) = (seq(), held());
        assert_eq!((top(), h1), (1, 1));
        // Recursion: not neutral, so the helper.
        assert_eq!(enter_fn(obj_addr, thread_addr), 0);
        // Someone else's object on top: not LIFO.
        other.mark.store(locked, Relaxed);
        assert_eq!(exit_fn(other_addr, thread_addr), 0);
        // Recursion 1, another lease, inflated: all the helper's.
        for mark in [
            ObjectHeader::make_thin_locked(quartet, slot, 1),
            ObjectHeader::make_thin_locked(quartet, slot + 1, 0),
            ObjectHeader::make_inflated(quartet),
        ] {
            obj.mark.store(mark, Relaxed);
            assert_eq!(exit_fn(obj_addr, thread_addr), 0, "{mark:#x}");
            assert_eq!(obj.mark.load(Relaxed), mark);
        }
        assert_eq!((seq(), top(), held()), (s1, 1, h1), "no refusal moved a word");
        // The outermost release.
        obj.mark.store(locked, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(obj.mark.load(Relaxed), quartet);
        assert_eq!((top(), held()), (0, 0));
        // Empty stack: the helper.
        obj.mark.store(locked, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0);

        // Enter refusals, each leaving every word as it was.
        obj.mark.store(quartet, Relaxed);
        let s2 = seq();
        assert_eq!(enter_fn(0, thread_addr), 0, "null receiver");
        assert_eq!(enter_fn(obj_addr, 0), 0, "untracked thread");
        rig.block(JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET).store(u64::MAX, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "unarmed thread");
        rig.block(JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET).store(u64::from(owner), Relaxed);
        rig.ls[JIT_LOCK_STACK_SPILLED_OFFSET / 8].store(1, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "spilled lock stack");
        rig.ls[JIT_LOCK_STACK_SPILLED_OFFSET / 8].store(0, Relaxed);
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(JIT_LOCK_STACK_SLOTS as u64, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "full lock stack");
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(0, Relaxed);
        assert_eq!(obj.mark.load(Relaxed), quartet);
        assert_eq!((seq(), held()), (s2, 0));
        // And the fast path still works after all of that.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!((obj.mark.load(Relaxed), top(), seq(), held()), (quartet, 0, s2 + 4, 0));
        drop((enter_buf, exit_buf));
    }

    /// Lock proposal W7-2: with `census` on, the thread's `JitMonitorBlock`
    /// counts every inline hit and every fall to the helper, per direction,
    /// and an untracked thread (a 0 thread word) falls without a bump or a
    /// fault. The fast path itself is the one the test above pins.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_thin_lock_census_counts_hits_and_falls() {
        use cratonvm_jit_api::{
            JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET,
            JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET, JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET,
        };
        use std::sync::atomic::Ordering::Relaxed;
        let enter_buf = build_thin_lock_probe(true, true);
        let exit_buf = build_thin_lock_probe(false, true);
        // SAFETY: both buffers were finalized by `build_thin_lock_probe` and
        // hold a complete `extern "C" fn(u64, u64) -> u64`.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };

        let rig = ThinLockRig::new();
        rig.arm(cratonvm_types::ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let count = |off: usize| rig.block(off).load(Relaxed);

        assert_eq!(enter_fn(obj_addr, thread_addr), 1, "uncontended enter");
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "recursive enter falls");
        assert_eq!(exit_fn(obj_addr, thread_addr), 1, "outermost exit");
        assert_eq!(
            exit_fn(obj_addr, thread_addr),
            0,
            "exit on an empty stack falls"
        );
        assert_eq!(
            enter_fn(obj_addr, 0),
            0,
            "untracked thread falls, uncounted"
        );
        assert_eq!(
            (
                count(JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET),
                count(JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET),
                count(JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET),
                count(JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET),
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(obj.mark.load(Relaxed), 0, "the object ends unlocked");
        assert_eq!(rig.held.acquired.load(Relaxed), 0, "and uncounted");
        drop((enter_buf, exit_buf));
    }

    /// Round 13 wave 8 (lane sync5, proposals S3-1 / S4-1): with the
    /// recursion arm, a re-entry on a word thin-locked by this lease bumps the
    /// mark's recursion field inline (exactly `try_thin_recursive_lock`) up to
    /// `MAX_THIN_LOCK_RECURSION`, where the helper (which inflates) takes
    /// over; each non-final exit drops it (exactly `try_thin_unlock`), and
    /// only the final exit pops the lock stack and uncounts the lease. No
    /// re-entry or non-final exit moves the lock stack, its `seq` or the lease
    /// count. A foreign lease's word, and a recursive exit whose lock-stack top
    /// is another object, still fall with every word untouched. The census
    /// counts the recursive hits as inline.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_thin_lock_recursion_arm_counts_in_the_mark_word_only() {
        use cratonvm_jit_api::{
            JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS_OFFSET, JIT_LOCK_STACK_TOP_OFFSET,
            JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET,
            JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET, JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET,
        };
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT, MAX_THIN_LOCK_RECURSION};
        use std::sync::atomic::Ordering::Relaxed;
        let thin_only = InlineRecursion {
            thin: true,
            inflated: false,
            two_way: false,
        };
        for census in [false, true] {
            let enter_buf = build_thin_lock_probe_with(true, census, thin_only);
            let exit_buf = build_thin_lock_probe_with(false, census, thin_only);
            // SAFETY: both buffers were finalized by `build_thin_lock_probe_with`
            // and hold a complete `extern "C" fn(u64, u64) -> u64`.
            let enter_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
            };
            // SAFETY: as above.
            let exit_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
            };

            let rig = ThinLockRig::new();
            let slot: u32 = 5;
            rig.arm(ObjectHeader::make_thin_locked(0, slot, 0));
            let thread_addr = rig.thread_addr();
            let obj = FakeObj::default();
            let other = FakeObj::default();
            let obj_addr = &obj as *const FakeObj as u64;
            let other_addr = &other as *const FakeObj as u64;
            let seq = || rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET);
            let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
            let slot0 = || rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET);
            let held = || rig.held.acquired.load(Relaxed);
            let count = |off: usize| rig.block(off).load(Relaxed);

            let quartet: u32 = 0x1234 << MARK_QUARTET_SHIFT;
            obj.mark.store(quartet, Relaxed);
            // The outermost enter: the thin path.
            assert_eq!(enter_fn(obj_addr, thread_addr), 1);
            let (s1, h1) = (seq(), held());
            assert_eq!((top(), slot0(), h1), (1, obj_addr, 1));
            // Re-entries up to the maximum: inline, the mark word only.
            for rec in 1..=MAX_THIN_LOCK_RECURSION {
                assert_eq!(enter_fn(obj_addr, thread_addr), 1, "re-entry to {rec}");
                assert_eq!(
                    obj.mark.load(Relaxed),
                    ObjectHeader::make_thin_locked(quartet, slot, rec),
                    "re-entry to {rec}: the quartet rides along"
                );
                assert_eq!((seq(), top(), slot0(), held()), (s1, 1, obj_addr, h1));
            }
            // At the maximum: the helper's (it inflates), nothing moved.
            let at_max = ObjectHeader::make_thin_locked(quartet, slot, MAX_THIN_LOCK_RECURSION);
            assert_eq!(enter_fn(obj_addr, thread_addr), 0, "recursion overflow");
            assert_eq!(obj.mark.load(Relaxed), at_max);
            assert_eq!((seq(), top(), held()), (s1, 1, h1));
            // A recursive exit whose lock-stack top is another object falls.
            other.mark.store(at_max, Relaxed);
            assert_eq!(exit_fn(other_addr, thread_addr), 0, "not the top slot");
            assert_eq!(other.mark.load(Relaxed), at_max);
            // Non-final exits: inline, the mark word only.
            for rec in (0..MAX_THIN_LOCK_RECURSION).rev() {
                assert_eq!(exit_fn(obj_addr, thread_addr), 1, "exit to {rec}");
                assert_eq!(
                    obj.mark.load(Relaxed),
                    ObjectHeader::make_thin_locked(quartet, slot, rec)
                );
                assert_eq!((seq(), top(), slot0(), held()), (s1, 1, obj_addr, h1));
            }
            // A word another lease holds, at any recursion: the helper's both
            // ways, untouched (the top slot still names `obj`).
            for rec in [0, 1, MAX_THIN_LOCK_RECURSION] {
                let foreign = ObjectHeader::make_thin_locked(quartet, slot + 1, rec);
                obj.mark.store(foreign, Relaxed);
                assert_eq!(enter_fn(obj_addr, thread_addr), 0, "foreign {rec}");
                assert_eq!(exit_fn(obj_addr, thread_addr), 0, "foreign {rec}");
                assert_eq!(obj.mark.load(Relaxed), foreign);
                assert_eq!((seq(), top(), held()), (s1, 1, h1));
            }
            // The final exit: the thin path pops and uncounts.
            obj.mark
                .store(ObjectHeader::make_thin_locked(quartet, slot, 0), Relaxed);
            assert_eq!(exit_fn(obj_addr, thread_addr), 1, "the final exit");
            assert_eq!(obj.mark.load(Relaxed), quartet);
            assert_eq!((seq(), top(), slot0(), held()), (s1 + 2, 0, 0, 0));
            if census {
                // Enters: 1 + MAX inline, then the overflow and three foreign
                // falls; exits: MAX + 1 inline, then `other` and three
                // foreign falls.
                let max = u64::from(MAX_THIN_LOCK_RECURSION);
                assert_eq!(
                    (
                        count(JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET),
                        count(JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET),
                        count(JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET),
                        count(JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET),
                    ),
                    (1 + max, 4, max + 1, 4)
                );
            }
            drop((enter_buf, exit_buf));
        }
    }

    /// Round 13 wave 8 (lane sync5, `CRATONVM_JIT_INLINE_INFLATED_RECURSION`):
    /// the inflated arm re-enters a monitor this thread owns by bumping its
    /// entry count (exactly `Monitor::try_enter`'s owner arm) and leaves a
    /// non-final hold by dropping it (`exit_reporting_release`'s `count > 1`
    /// arm): no owner-word write, no lock-stack write, no wake decision, JFR
    /// flag and thin seed ignored as the helper ignores them there. Count 0
    /// (not a hold) and a monitor owned by another thread still fall to the
    /// helper with every word untouched.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_inflated_recursion_moves_only_the_entry_count() {
        use cratonvm_jit_api::{
            JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS, JIT_LOCK_STACK_TOP_OFFSET,
        };
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() {
            return;
        }
        let inflated_only = InlineRecursion {
            thin: false,
            inflated: true,
            two_way: false,
        };
        let enter_buf = build_thin_lock_probe_with(true, false, inflated_only);
        let exit_buf = build_thin_lock_probe_with(false, false, inflated_only);
        // SAFETY: both buffers were finalized by `build_thin_lock_probe_with`
        // and hold a complete `extern "C" fn(u64, u64) -> u64`.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };

        let rig = ThinLockRig::new();
        rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let mon = FakeMonitor::default();
        let epoch = AtomicU64::new(9);
        let me: u64 = 0x2C;
        let inflated = ObjectHeader::make_inflated(0x77 << MARK_QUARTET_SHIFT);
        obj.mark.store(inflated, Relaxed);
        let lease = &rig.held;
        lease.owner_word.store(me, Relaxed);
        lease
            .monitor
            .store(&mon as *const FakeMonitor as u64, Relaxed);
        lease.epoch.store(9, Relaxed);
        lease
            .epoch_addr
            .store(&epoch as *const AtomicU64 as u64, Relaxed);
        lease.key.store(obj_addr, Relaxed);
        let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
        let seq = || rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET);
        let words = || {
            (
                mon.owner.load(Relaxed),
                mon.entry_count.load(Relaxed),
                top(),
                seq(),
            )
        };

        // The outermost enter takes the free monitor and pushes.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (me, 1, 1, 2));
        // Re-entries: the count alone.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (me, 3, 1, 2));
        // A non-final exit ignores a JFR flag, a thin seed and a parked
        // entrant: it releases nothing, so it owes no wake.
        mon.jfr.store(1, Relaxed);
        mon.thin_seed.store(me, Relaxed);
        mon.entry_waiters.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (me, 2, 1, 2));
        mon.jfr.store(0, Relaxed);
        mon.thin_seed.store(0, Relaxed);
        mon.entry_waiters.store(0, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (me, 1, 1, 2));
        // Count 0 is not a hold: the helper's IMSE, untouched.
        mon.entry_count.store(0, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "count 0");
        assert_eq!(words(), (me, 0, 1, 2));
        mon.entry_count.store(1, Relaxed);
        // A full lock stack refuses a re-entry too (checked before the owner).
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(JIT_LOCK_STACK_SLOTS as u64, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "full lock stack");
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(1, Relaxed);
        assert_eq!(words(), (me, 1, 1, 2));
        // The final release pops.
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (0, 0, 0, 4));
        // Another thread's monitor is never re-entered (no budget: no spin).
        mon.owner.store(me + 1, Relaxed);
        mon.entry_count.store(1, Relaxed);
        assert_eq!(
            enter_fn(obj_addr, thread_addr),
            0,
            "owned by another thread"
        );
        assert_eq!(words(), (me + 1, 1, 0, 4));
        assert_eq!(
            obj.mark.load(Relaxed),
            inflated,
            "the mark word is never written"
        );
        assert_eq!(
            rig.held.acquired.load(Relaxed),
            0,
            "no thin lock was counted"
        );
        drop((enter_buf, exit_buf));
    }

    /// Round 13 wave 10 (lane monitor2, `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY`,
    /// lock proposal W17-3): with the second way probed, a receiver named by
    /// EITHER way of the lease cache is entered and left inline on that way's
    /// monitor at the block's one epoch, and the other way's monitor is not
    /// touched; a receiver named by neither, or any receiver once the index
    /// epoch moved, falls to the helper with every word as it was. With the
    /// switch off the second way's receiver falls, as before.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_inflated_arm_finds_its_monitor_in_either_way_of_the_lease_cache() {
        use cratonvm_jit_api::{JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_TOP_OFFSET};
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() {
            return;
        }
        for two_way in [true, false] {
            let arms = InlineRecursion {
                thin: false,
                inflated: false,
                two_way,
            };
            let enter_buf = build_thin_lock_probe_with(true, false, arms);
            let exit_buf = build_thin_lock_probe_with(false, false, arms);
            // SAFETY: both buffers were finalized by `build_thin_lock_probe_with`
            // and hold a complete `extern "C" fn(u64, u64) -> u64`.
            let enter_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(
                    enter_buf.as_ptr(),
                )
            };
            // SAFETY: as above.
            let exit_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
            };

            let rig = ThinLockRig::new();
            rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
            let thread_addr = rig.thread_addr();
            let (a, b, c) = (FakeObj::default(), FakeObj::default(), FakeObj::default());
            let addr = |o: &FakeObj| o as *const FakeObj as u64;
            let (mon_a, mon_b) = (FakeMonitor::default(), FakeMonitor::default());
            let epoch = AtomicU64::new(9);
            let me: u64 = 0x2D;
            let inflated = ObjectHeader::make_inflated(0x77 << MARK_QUARTET_SHIFT);
            for o in [&a, &b, &c] {
                o.mark.store(inflated, Relaxed);
            }
            let lease = &rig.held;
            lease.owner_word.store(me, Relaxed);
            lease.epoch.store(9, Relaxed);
            lease
                .epoch_addr
                .store(&epoch as *const AtomicU64 as u64, Relaxed);
            lease
                .monitor
                .store(&mon_a as *const FakeMonitor as u64, Relaxed);
            lease.key.store(addr(&a), Relaxed);
            lease
                .monitor2
                .store(&mon_b as *const FakeMonitor as u64, Relaxed);
            lease.key2.store(addr(&b), Relaxed);
            let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
            let held = |m: &FakeMonitor| (m.owner.load(Relaxed), m.entry_count.load(Relaxed));

            // The first way, in both arms of the switch.
            assert_eq!(enter_fn(addr(&a), thread_addr), 1, "two_way={two_way}");
            assert_eq!((held(&mon_a), top()), ((me, 1), 1));
            assert_eq!(exit_fn(addr(&a), thread_addr), 1);
            assert_eq!((held(&mon_a), top()), ((0, 0), 0));

            // The second way: inline only when it is probed.
            let hit = enter_fn(addr(&b), thread_addr);
            if two_way {
                assert_eq!(hit, 1, "the second way names b");
                assert_eq!((held(&mon_b), top()), ((me, 1), 1));
                assert_eq!(held(&mon_a), (0, 0), "the first way's monitor is untouched");
                assert_eq!(exit_fn(addr(&b), thread_addr), 1);
                assert_eq!((held(&mon_b), top()), ((0, 0), 0));
            } else {
                assert_eq!(hit, 0, "the second way is not probed");
                assert_eq!((held(&mon_b), top()), ((0, 0), 0));
            }

            // Neither way, then a moved epoch for both: the helper, nothing
            // written.
            let seq_before = rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET);
            assert_eq!(enter_fn(addr(&c), thread_addr), 0, "neither way");
            epoch.store(10, Relaxed);
            assert_eq!(enter_fn(addr(&a), thread_addr), 0, "stale epoch, first way");
            assert_eq!(enter_fn(addr(&b), thread_addr), 0, "stale epoch, second way");
            assert_eq!(rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET), seq_before);
            assert_eq!((held(&mon_a), held(&mon_b), top()), ((0, 0), (0, 0), 0));
            drop((enter_buf, exit_buf));
        }
    }

    /// Round 13 wave 12 (lane monitor3, proposal S7-4,
    /// `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`): the single-bump push / pop
    /// leaves the lock stack, `seq` included, exactly where the odd/even
    /// bracket does -- `seq` moves by 2 per write and is never left odd --
    /// and is 21 bytes shorter per push and per pop (the `seq` load, its two
    /// stores and two `ADD`s become one `ADD QWORD [seq], 2`).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_single_bump_lock_stack_write_ends_where_the_bracket_did() {
        use cratonvm_jit_api::{
            JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS_OFFSET, JIT_LOCK_STACK_TOP_OFFSET,
        };
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::Ordering::Relaxed;
        let off = InlineRecursion {
            thin: false,
            inflated: false,
            two_way: false,
        };
        let build = |enter: bool, single: &str| {
            build_thin_lock_probe_shaped(
                enter,
                false,
                off,
                false,
                &[(STICKY_COUNT_SWITCH, Some("0")), (SINGLE_BUMP_SWITCH, Some(single))],
            )
        };
        let mut sizes = Vec::new();
        for single in ["1", "0"] {
            let enter_buf = build(true, single);
            let exit_buf = build(false, single);
            sizes.push((enter_buf.pos(), exit_buf.pos()));
            // SAFETY: both buffers were finalized by `build_thin_lock_probe_shaped`
            // and hold a complete `extern "C" fn(u64, u64) -> u64`.
            let enter_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
            };
            // SAFETY: as above.
            let exit_fn = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
            };
            let rig = ThinLockRig::new();
            let slot: u32 = 3;
            rig.arm(ObjectHeader::make_thin_locked(0, slot, 0));
            let thread_addr = rig.thread_addr();
            let (a, b) = (FakeObj::default(), FakeObj::default());
            let addr = |o: &FakeObj| o as *const FakeObj as u64;
            let quartet: u32 = 0x55 << MARK_QUARTET_SHIFT;
            a.mark.store(quartet, Relaxed);
            b.mark.store(quartet, Relaxed);
            let state = || {
                (
                    rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET),
                    rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET),
                    rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET),
                    rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET + 8),
                )
            };
            // Nested: push a, push b, pop b, pop a -- twice.
            for round in 0..2u64 {
                let s = 8 * round;
                assert_eq!(enter_fn(addr(&a), thread_addr), 1, "single={single}");
                assert_eq!(state(), (s + 2, 1, addr(&a), 0), "single={single}");
                assert_eq!(enter_fn(addr(&b), thread_addr), 1);
                assert_eq!(state(), (s + 4, 2, addr(&a), addr(&b)), "single={single}");
                assert_eq!(exit_fn(addr(&b), thread_addr), 1);
                assert_eq!(state(), (s + 6, 1, addr(&a), 0), "single={single}");
                assert_eq!(exit_fn(addr(&a), thread_addr), 1);
                assert_eq!(state(), (s + 8, 0, 0, 0), "single={single}");
            }
            assert_eq!((a.mark.load(Relaxed), b.mark.load(Relaxed)), (quartet, quartet));
            assert_eq!(rig.held.acquired.load(Relaxed), 0);
            drop((enter_buf, exit_buf));
        }
        let (single, bracket) = (sizes[0], sizes[1]);
        assert_eq!(bracket.0 - single.0, 21, "enter: one push");
        assert_eq!(bracket.1 - single.1, 21, "exit: one pop");
    }

    /// Round 13 wave 12 (lane monitor3, proposal S7-5 (a),
    /// `CRATONVM_JIT_SELF_LOCK_TRIM`): a sequence for an object the caller
    /// proves non-null has no `TEST R11, R11; JE slow` (9 bytes) and takes
    /// and releases a lock exactly as the full one does.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_non_null_receiver_sequence_drops_only_the_null_test() {
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::Ordering::Relaxed;
        let arms = InlineRecursion {
            thin: true,
            inflated: false,
            two_way: false,
        };
        let pinned = [(STICKY_COUNT_SWITCH, Some("0"))];
        let full = (
            build_thin_lock_probe_shaped(true, false, arms, false, &pinned),
            build_thin_lock_probe_shaped(false, false, arms, false, &pinned),
        );
        let trimmed = (
            build_thin_lock_probe_shaped(true, false, arms, true, &pinned),
            build_thin_lock_probe_shaped(false, false, arms, true, &pinned),
        );
        assert_eq!(full.0.pos() - trimmed.0.pos(), 9, "enter");
        assert_eq!(full.1.pos() - trimmed.1.pos(), 9, "exit");
        // SAFETY: both buffers were finalized by `build_thin_lock_probe_shaped`
        // and hold a complete `extern "C" fn(u64, u64) -> u64`.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(trimmed.0.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(trimmed.1.as_ptr())
        };
        let rig = ThinLockRig::new();
        let slot: u32 = 4;
        rig.arm(ObjectHeader::make_thin_locked(0, slot, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let quartet: u32 = 0x21 << MARK_QUARTET_SHIFT;
        obj.mark.store(quartet, Relaxed);
        // Outermost enter, a re-entry, the non-final and the final exit.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(obj.mark.load(Relaxed), ObjectHeader::make_thin_locked(quartet, slot, 0));
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(obj.mark.load(Relaxed), ObjectHeader::make_thin_locked(quartet, slot, 1));
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(obj.mark.load(Relaxed), quartet);
        assert_eq!(rig.held.acquired.load(Relaxed), 0);
        // The untracked thread still falls (the thread test is kept).
        assert_eq!(enter_fn(obj_addr, 0), 0, "untracked thread");
        assert_eq!(obj.mark.load(Relaxed), quartet);
        drop((full, trimmed));
    }

    /// Round 13 wave 12 (lane monitor3, `CRATONVM_JIT_INFLATED_STICKY_COUNT`,
    /// lane monitor2's proposal M2-5): with the sticky count a compiled final
    /// release leaves `entry_count` at 1 and an acquisition that reads 1
    /// writes nothing; one that reads anything else (0 after a Rust release)
    /// stores 1. Re-entry and non-final exits count exactly as before, and a
    /// wake-owing release still hands the monitor to the helper at count 1.
    /// The enter grows by the `CMP` + `JE` (13 bytes) and the exit loses its
    /// `MOV DWORD [count], 0` (10 bytes).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_sticky_count_is_exact_while_owned_and_left_at_one_by_a_release() {
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() {
            return;
        }
        let arms = InlineRecursion {
            thin: false,
            inflated: true,
            two_way: false,
        };
        let build = |enter: bool, sticky: &str| {
            build_thin_lock_probe_shaped(
                enter,
                false,
                arms,
                false,
                &[(STICKY_COUNT_SWITCH, Some(sticky))],
            )
        };
        let (enter_buf, exit_buf) = (build(true, "1"), build(false, "1"));
        let (plain_enter, plain_exit) = (build(true, "0"), build(false, "0"));
        assert_eq!(enter_buf.pos() - plain_enter.pos(), 13, "enter: CMP + JE");
        assert_eq!(plain_exit.pos() - exit_buf.pos(), 10, "exit: no count store");
        // SAFETY: both buffers were finalized by `build_thin_lock_probe_shaped`
        // and hold a complete `extern "C" fn(u64, u64) -> u64`.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };

        let rig = ThinLockRig::new();
        rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let mon = FakeMonitor::default();
        let epoch = AtomicU64::new(3);
        let me: u64 = 0x31;
        obj.mark
            .store(ObjectHeader::make_inflated(0x42 << MARK_QUARTET_SHIFT), Relaxed);
        let lease = &rig.held;
        lease.owner_word.store(me, Relaxed);
        lease
            .monitor
            .store(&mon as *const FakeMonitor as u64, Relaxed);
        lease.epoch.store(3, Relaxed);
        lease
            .epoch_addr
            .store(&epoch as *const AtomicU64 as u64, Relaxed);
        lease.key.store(obj_addr, Relaxed);
        let held = || (mon.owner.load(Relaxed), mon.entry_count.load(Relaxed));

        // A free monitor a Rust release left at 0: the acquisition stores 1.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (me, 1));
        // Re-entry and the non-final exit count exactly.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (me, 2));
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (me, 1));
        // The final release leaves the count at 1, owner free.
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (0, 1));
        // The next acquisition finds its 1 and keeps it; a stale value from
        // any other release is overwritten.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (me, 1));
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        mon.entry_count.store(7, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (me, 1));
        // A release that owes a wake (a parked entrant, no successor, no
        // spinner) goes to the helper still held, at count 1, under either
        // exit shape.
        mon.entry_waiters.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "wake owed: the helper's");
        assert_eq!(held(), (me, 1));
        mon.entry_waiters.store(0, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(held(), (0, 1));
        drop((enter_buf, exit_buf, plain_enter, plain_exit));
    }

    /// Round 11 wave 17 (lock proposal W16-1): the INFLATED arm, run against a
    /// fake lease block and monitor at the `LEASE_BLOCK_*` /
    /// `INFLATED_MONITOR_*` offsets. It takes exactly a FREE monitor that the
    /// lease caches for this receiver at the current epoch, releases exactly
    /// a monitor this thread holds once with no JFR flag and no thin seed,
    /// joins the thin path's lock-stack push / pop, never writes the mark word
    /// or the thin-lock count, and hands a release that owes a wake to the
    /// helper still HELD (taken back), unless a successor or a spinner will
    /// take the monitor. Every refusal leaves every word as it was.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_inflated_lock_takes_only_a_free_monitor_its_lease_caches() {
        use cratonvm_jit_api::{
            JIT_LOCK_STACK_SEQ_OFFSET, JIT_LOCK_STACK_SLOTS, JIT_LOCK_STACK_SLOTS_OFFSET,
            JIT_LOCK_STACK_TOP_OFFSET,
        };
        use cratonvm_types::{ObjectHeader, MARK_FORWARDED, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() {
            // `CRATONVM_JIT_INLINE_INFLATED_LOCK=0`: no arm is emitted, and
            // the thin-path tests above pin that an inflated word falls.
            return;
        }
        let enter_buf = build_thin_lock_probe(true, false);
        let exit_buf = build_thin_lock_probe(false, false);
        // SAFETY: both buffers were finalized by `build_thin_lock_probe` and
        // hold a complete `extern "C" fn(u64, u64) -> u64`.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };

        let rig = ThinLockRig::new();
        rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let mon = FakeMonitor::default();
        let epoch = AtomicU64::new(7);
        let me: u64 = 0x2A;
        let quartet: u32 = 0x1234 << MARK_QUARTET_SHIFT;
        let inflated = ObjectHeader::make_inflated(quartet);
        obj.mark.store(inflated, Relaxed);
        let lease = &rig.held;
        lease.owner_word.store(me, Relaxed);
        lease
            .monitor
            .store(&mon as *const FakeMonitor as u64, Relaxed);
        lease.epoch.store(7, Relaxed);
        lease
            .epoch_addr
            .store(&epoch as *const AtomicU64 as u64, Relaxed);
        lease.key.store(obj_addr, Relaxed);
        let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
        let seq = || rig.ls_word(JIT_LOCK_STACK_SEQ_OFFSET);
        let slot0 = || rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET);
        let words = || {
            (
                mon.owner.load(Relaxed),
                mon.entry_count.load(Relaxed),
                top(),
                seq(),
            )
        };

        // A free monitor the lease names at the current epoch: taken inline.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (me, 1, 1, 2));
        assert_eq!(slot0(), obj_addr, "pushed like a thin acquisition");
        // Re-entry is the helper's.
        let held = words();
        assert_eq!(enter_fn(obj_addr, thread_addr), 0);
        assert_eq!(words(), held);
        // Exit refusals.
        mon.entry_count.store(2, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "re-entered");
        mon.entry_count.store(1, Relaxed);
        mon.jfr.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "a JFR enter is recorded");
        mon.jfr.store(0, Relaxed);
        mon.thin_seed.store(me, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "a thin seed");
        mon.thin_seed.store(0, Relaxed);
        mon.owner.store(me + 1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "not ours");
        mon.owner.store(me, Relaxed);
        assert_eq!(words(), held, "no exit refusal moved a word");
        // The release, inline when no wake is owed.
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (0, 0, 0, 4));
        assert_eq!(slot0(), 0, "popped like a thin release");

        // Enter refusals.
        let idle = words();
        mon.owner.store(me + 1, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "owned by another thread");
        mon.owner.store(0, Relaxed);
        epoch.store(8, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "the index moved on");
        epoch.store(7, Relaxed);
        lease.key.store(obj_addr + 64, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "another object's monitor");
        lease.key.store(obj_addr, Relaxed);
        lease.owner_word.store(0, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "no owner word");
        lease.owner_word.store(me, Relaxed);
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(JIT_LOCK_STACK_SLOTS as u64, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "full lock stack");
        rig.ls[JIT_LOCK_STACK_TOP_OFFSET / 8].store(0, Relaxed);
        // Words that are neither the thin path's nor INFLATED.
        for mark in [
            quartet | (0x2A << 2), // a hashed NEUTRAL word
            ObjectHeader::make_thin_locked(quartet, 6, 0),
            quartet | MARK_FORWARDED,
        ] {
            obj.mark.store(mark, Relaxed);
            assert_eq!(enter_fn(obj_addr, thread_addr), 0, "{mark:#x}");
            assert_eq!(obj.mark.load(Relaxed), mark);
        }
        obj.mark.store(inflated, Relaxed);
        assert_eq!(words(), idle, "no enter refusal moved a word");

        // A release that owes a wake: taken back and handed to the helper,
        // still held once and still on the lock stack.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        let held = words();
        mon.entry_waiters.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 0, "a parked entrant");
        assert_eq!(words(), held);
        // ... unless a woken successor is on its way ...
        mon.succ.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!((mon.owner.load(Relaxed), top()), (0, 0));
        mon.succ.store(0, Relaxed);
        // ... or a spinner will take the monitor.
        assert_eq!(enter_fn(obj_addr, thread_addr), 1);
        mon.spinners.store(1, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!((mon.owner.load(Relaxed), top()), (0, 0));

        assert_eq!(obj.mark.load(Relaxed), inflated, "the mark word is never written");
        assert_eq!(rig.held.acquired.load(Relaxed), 0, "no thin lock was counted");
        drop((enter_buf, exit_buf));
    }

    /// Round 12 wave 1 (lock proposal W19-2): with the pre-release wake
    /// check on, an exit that would owe a wake goes to the helper WITHOUT
    /// ever releasing -- the owner word never reads `0` -- where the round-11
    /// exit released, took the monitor back and then fell to the helper. A
    /// watcher thread polls the owner word while the exit runs many times;
    /// with the check it can never observe the monitor free.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_wake_owing_inflated_exit_goes_to_the_helper_without_releasing() {
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() || !inflated_exit_precheck_enabled() {
            return;
        }
        let exit_buf = build_thin_lock_probe(false, false);
        // SAFETY: finalized by `build_thin_lock_probe`; a complete
        // `extern "C" fn(u64, u64) -> u64`.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };
        let rig = ThinLockRig::new();
        rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        let mon = FakeMonitor::default();
        let epoch = AtomicU64::new(3);
        let me: u64 = 0x2B;
        obj.mark
            .store(ObjectHeader::make_inflated(0x55 << MARK_QUARTET_SHIFT), Relaxed);
        let lease = &rig.held;
        lease.owner_word.store(me, Relaxed);
        lease
            .monitor
            .store(&mon as *const FakeMonitor as u64, Relaxed);
        lease.epoch.store(3, Relaxed);
        lease
            .epoch_addr
            .store(&epoch as *const AtomicU64 as u64, Relaxed);
        lease.key.store(obj_addr, Relaxed);
        // Held once, pushed on the lock stack, one entrant parked.
        mon.owner.store(me, Relaxed);
        mon.entry_count.store(1, Relaxed);
        rig.ls[cratonvm_jit_api::JIT_LOCK_STACK_TOP_OFFSET / 8].store(1, Relaxed);
        rig.ls[cratonvm_jit_api::JIT_LOCK_STACK_SLOTS_OFFSET / 8].store(obj_addr, Relaxed);
        mon.entry_waiters.store(1, Relaxed);

        let stop = AtomicBool::new(false);
        let saw_free = std::thread::scope(|s| {
            let watcher = s.spawn(|| {
                let mut free = 0u64;
                while !stop.load(Relaxed) {
                    if mon.owner.load(Relaxed) == 0 {
                        free += 1;
                    }
                    std::hint::spin_loop();
                }
                free
            });
            for _ in 0..20_000 {
                assert_eq!(exit_fn(obj_addr, thread_addr), 0, "the helper releases");
            }
            stop.store(true, Relaxed);
            watcher.join().unwrap_or(u64::MAX)
        });
        assert_eq!(saw_free, 0, "the monitor was released inline");
        assert_eq!(
            (mon.owner.load(Relaxed), mon.entry_count.load(Relaxed)),
            (me, 1),
            "still held once for the helper"
        );
        drop(exit_buf);
    }

    /// Point `rig`'s lease cache at `mon` for the receiver at `obj_addr`, at
    /// epoch `*epoch`, for the owner word `me`.
    #[cfg(target_arch = "x86_64")]
    fn wire_inflated(
        rig: &ThinLockRig,
        mon: &FakeMonitor,
        epoch: &std::sync::atomic::AtomicU64,
        obj_addr: u64,
        me: u64,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        let lease = &rig.held;
        lease.owner_word.store(me, Relaxed);
        lease.monitor.store(mon as *const FakeMonitor as u64, Relaxed);
        lease.epoch.store(epoch.load(Relaxed), Relaxed);
        lease
            .epoch_addr
            .store(epoch as *const std::sync::atomic::AtomicU64 as u64, Relaxed);
        lease.key.store(obj_addr, Relaxed);
    }

    /// Round 12 wave 2 (lane lock2): the inline spin of a compiled
    /// `monitorenter` on an inflated monitor OWNED by another thread. It
    /// refuses re-entry, a zero budget and a budget above `SPIN_MAX` at once;
    /// gives up on a monitor held for its whole budget having written nothing
    /// (the lock stack, the entry count and the owner word as they were); and
    /// takes a monitor released while it spins exactly as it takes a free one
    /// (owner word, entry count 1, lock-stack push).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_inline_spin_takes_a_monitor_released_during_it_and_gives_up_on_a_held_one() {
        use cratonvm_jit_api::{JIT_LOCK_STACK_SLOTS_OFFSET, JIT_LOCK_STACK_TOP_OFFSET};
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() || !inline_inflated_spin_enabled() {
            return;
        }
        let enter_buf = build_thin_lock_probe(true, false);
        let exit_buf = build_thin_lock_probe(false, false);
        // SAFETY: finalized by `build_thin_lock_probe`; complete
        // `extern "C" fn(u64, u64) -> u64`s.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };
        let rig = ThinLockRig::new();
        rig.arm(ObjectHeader::make_thin_locked(0, 5, 0));
        let thread_addr = rig.thread_addr();
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        obj.mark
            .store(ObjectHeader::make_inflated(0x77 << MARK_QUARTET_SHIFT), Relaxed);
        let mon = FakeMonitor::default();
        let epoch = AtomicU64::new(11);
        let (me, other): (u64, u64) = (0x31, 0x32);
        wire_inflated(&rig, &mon, &epoch, obj_addr, me);
        let top = || rig.ls_word(JIT_LOCK_STACK_TOP_OFFSET);
        let words = || {
            (
                mon.owner.load(Relaxed),
                mon.entry_count.load(Relaxed),
                top(),
            )
        };

        // Refused at once, nothing written.
        mon.owner.store(other, Relaxed);
        for budget in [0, INFLATED_MONITOR_SPIN_MAX + 1, u32::MAX] {
            mon.spin_limit.store(budget, Relaxed);
            assert_eq!(enter_fn(obj_addr, thread_addr), 0, "budget {budget}");
            assert_eq!(words(), (other, 0, 0));
        }
        // Held for the whole budget: gives up, nothing written.
        mon.spin_limit.store(INFLATED_MONITOR_SPIN_MAX, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "held throughout");
        assert_eq!(words(), (other, 0, 0));
        // A parked entrant: no inline spin at all (the helper's spinners
        // count themselves); a FREE monitor is still taken.
        mon.entry_waiters.store(1, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "somebody parked");
        assert_eq!(words(), (other, 0, 0));
        mon.owner.store(0, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 1, "free, parked entrant or not");
        assert_eq!(words(), (me, 1, 1));
        mon.entry_waiters.store(0, Relaxed);
        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
        assert_eq!(words(), (0, 0, 0));
        mon.owner.store(other, Relaxed);
        // Re-entry stays the helper's, whatever the budget.
        mon.owner.store(me, Relaxed);
        mon.entry_count.store(1, Relaxed);
        assert_eq!(enter_fn(obj_addr, thread_addr), 0, "re-entry");
        assert_eq!(words(), (me, 1, 0));
        mon.entry_count.store(0, Relaxed);

        // Released while it spins: a releaser thread frees the monitor a
        // little after each attempt starts. Most attempts see it owned first,
        // so a win is a spin win; every outcome must be exact.
        let go = AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        let mut wins = 0u32;
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Relaxed) {
                    if go.swap(false, Relaxed) {
                        for _ in 0..64 {
                            std::hint::spin_loop();
                        }
                        let _ = mon.owner.compare_exchange(other, 0, Relaxed, Relaxed);
                    }
                    std::hint::spin_loop();
                }
            });
            for _ in 0..500 {
                mon.owner.store(other, Relaxed);
                go.store(true, Relaxed);
                if enter_fn(obj_addr, thread_addr) == 1 {
                    wins += 1;
                    assert_eq!(mon.owner.load(Relaxed), me, "taken for this thread");
                    assert_eq!(mon.entry_count.load(Relaxed), 1);
                    let slot0 = rig.ls_word(JIT_LOCK_STACK_SLOTS_OFFSET);
                    assert_eq!((top(), slot0), (1, obj_addr));
                    assert_eq!(exit_fn(obj_addr, thread_addr), 1, "released inline");
                } else {
                    assert_ne!(mon.owner.load(Relaxed), me, "a refusal takes nothing");
                    assert_eq!(top(), 0);
                }
                // Let the releaser finish this round before the next one.
                while go.load(Relaxed) {
                    std::hint::spin_loop();
                }
                for _ in 0..256 {
                    std::hint::spin_loop();
                }
            }
            stop.store(true, Relaxed);
        });
        assert!(wins > 0, "no attempt ever won a monitor released during its spin");
        assert_eq!(top(), 0);
        assert_eq!(rig.held.acquired.load(Relaxed), 0, "no thin lock was counted");
        drop((enter_buf, exit_buf));
    }

    /// Round 12 wave 2 (lane lock2): threads that each take one inflated
    /// monitor through the inline spin and leave it through the inline exit
    /// exclude each other -- a plain read-modify-write done only under the
    /// monitor comes out exact.
    /// Round 12 wave 8 (lane monitor): with the switch on the only new bytes
    /// are the lost-CAS back-off block, whose `MOV EAX, 16` appears exactly
    /// once in the enter sequence; with it off, not at all.
    #[test]
    fn the_inline_spin_backoff_is_only_the_lost_cas_arm() {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        let mut slow = Vec::new();
        emit_inline_inflated_spin_acquire(&mut buf, 16, &mut slow, None, false);
        let mov = [0xB8u8, 16, 0, 0, 0];
        let hits = buf
            .as_slice()
            .windows(mov.len())
            .filter(|w| *w == mov)
            .count();
        assert_eq!(hits, usize::from(inline_spin_backoff_enabled()));
    }

    /// Round 14 wave 1 (lane sync, M2-1): the inline spin's census is one
    /// `LOCK ADD QWORD [RCX + disp32], 1` per outcome word, and without the
    /// census flag not one byte of it (the rest of the sequence is the same
    /// length either way apart from the bumps and the one extra `JMP`).
    #[test]
    fn the_inline_spin_census_bumps_each_outcome_word_once() {
        let bump = |disp: usize| {
            let mut v = vec![0xF0u8, 0x48, 0x83, 0x81];
            v.extend_from_slice(&(disp as i32).to_le_bytes());
            v.push(0x01);
            v
        };
        let count = |hay: &[u8], needle: &[u8]| {
            hay.windows(needle.len()).filter(|w| *w == needle).count()
        };
        let mut lens = Vec::new();
        for census in [false, true] {
            let mut buf = ExecutableBuffer::new(4096).expect("buffer");
            let mut slow = Vec::new();
            emit_inline_inflated_spin_acquire(&mut buf, 16, &mut slow, None, census);
            for disp in [
                INFLATED_MONITOR_CENSUS_SPIN_WINS_OFFSET,
                INFLATED_MONITOR_CENSUS_SPIN_BUDGET_OUTS_OFFSET,
                INFLATED_MONITOR_CENSUS_SPIN_WAITER_EXITS_OFFSET,
            ] {
                assert_eq!(count(buf.as_slice(), &bump(disp)), usize::from(census), "{disp}");
            }
            lens.push(buf.pos());
        }
        // Three 9-byte bumps and one 5-byte `JMP rel32`.
        assert_eq!(lens[1], lens[0] + 3 * 9 + 5);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn inline_spinners_exclude_each_other() {
        use cratonvm_types::{ObjectHeader, MARK_QUARTET_SHIFT};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        if !inline_inflated_lock_enabled() || !inline_inflated_spin_enabled() {
            return;
        }
        const THREADS: u64 = 4;
        const ROUNDS: u64 = 20_000;
        let enter_buf = build_thin_lock_probe(true, false);
        let exit_buf = build_thin_lock_probe(false, false);
        // SAFETY: finalized by `build_thin_lock_probe`; complete
        // `extern "C" fn(u64, u64) -> u64`s.
        let enter_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(enter_buf.as_ptr())
        };
        // SAFETY: as above.
        let exit_fn = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(exit_buf.as_ptr())
        };
        let obj = FakeObj::default();
        let obj_addr = &obj as *const FakeObj as u64;
        obj.mark
            .store(ObjectHeader::make_inflated(0x5A << MARK_QUARTET_SHIFT), Relaxed);
        let mon = FakeMonitor::default();
        mon.spin_limit.store(INFLATED_MONITOR_SPIN_MAX, Relaxed);
        let epoch = AtomicU64::new(5);
        let counter = AtomicU64::new(0);
        std::thread::scope(|s| {
            for t in 0..THREADS {
                let (mon, epoch, counter) = (&mon, &epoch, &counter);
                s.spawn(move || {
                    let rig = ThinLockRig::new();
                    rig.arm(ObjectHeader::make_thin_locked(0, 7 + t as u32, 0));
                    let me = 0x100 + t;
                    wire_inflated(&rig, mon, epoch, obj_addr, me);
                    let thread_addr = rig.thread_addr();
                    for _ in 0..ROUNDS {
                        // A spin that runs out is the helper's; here, retry.
                        while enter_fn(obj_addr, thread_addr) != 1 {
                            std::thread::yield_now();
                        }
                        assert_eq!(mon.owner.load(Relaxed), me);
                        let v = counter.load(Relaxed);
                        std::hint::spin_loop();
                        counter.store(v + 1, Relaxed);
                        assert_eq!(exit_fn(obj_addr, thread_addr), 1);
                    }
                });
            }
        });
        assert_eq!(counter.load(Relaxed), THREADS * ROUNDS);
        assert_eq!(mon.owner.load(Relaxed), 0);
        drop((enter_buf, exit_buf));
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod r12w3_mega2_table_probe_tests {
    //! Round 12 wave 3, lane mega2 (proposal W2-1 step 2): the hashed stub's
    //! shared-table probe, EXECUTED on this host's ABI (Win64 or SysV), around
    //! real published bodies and the real `MegaDispatchTable` protocol.
    use super::*;
    use crate::{CompiledMethod, JitCache};
    use std::sync::Arc;

    /// `[RBP - off]` homes of the harness frame: context, receiver, argument.
    pub(super) const CTX: i32 = 8;
    pub(super) const RECV: i32 = 16;
    pub(super) const ARG1: i32 = 24;
    /// What the harness returns when the stub falls through to its helper.
    pub(super) const MISS: u64 = 0x0DEA_D000;
    pub(super) const ARG: u64 = 0x5000_0000;

    /// A plain object: class id in the first four bytes, a zero mark word
    /// (kind bits 0 = object).
    #[repr(C, align(8))]
    pub(super) struct Obj {
        pub(super) words: [u64; 4],
    }

    impl Obj {
        pub(super) fn new(class_id: u32) -> Box<Self> {
            Box::new(Self {
                words: [u64::from(class_id), 0, 0, 0],
            })
        }

        pub(super) fn addr(&self) -> u64 {
            self as *const Self as u64
        }
    }

    /// A callee returning its argument word `arg_reg` XOR `marker`: which body
    /// ran, and whether the stub marshalled the ABI the way's word said.
    pub(super) fn body(arg_reg: u8, marker: i32) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("buffer");
        emit_mov_reg(&mut buf, RAX, arg_reg); // MOV RAX, arg
        buf.emit(&[0x48, 0x35]); // XOR RAX, imm32
        buf.emit(&marker.to_le_bytes());
        buf.emit_byte(0xC3); // RET
        CompiledMethod::new(buf)
    }

    pub(super) fn publish(
        cache: &JitCache,
        name: &str,
        code: CompiledMethod,
    ) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x1203);
        let class: Arc<str> = Arc::from("R12w3Mega2");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("(J)J");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, code);
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    /// `extern "C" fn(context, receiver, arg) -> u64`: a frame holding the
    /// three words, the stub for a `(receiver, arg)` site, `MISS` on its
    /// fall-through and the callee's result on its hit exit. The class-slot
    /// probe (round 12 wave 4) rides with the shared-table probe, as in
    /// production; an unbound column skips it.
    fn build(pic: &JitPICSlot, mega_table: bool) -> ExecutableBuffer {
        build_with(pic, mega_table, mega_table)
    }

    /// [`build`] with the class-slot probe chosen separately.
    pub(super) fn build_with(
        pic: &JitPICSlot,
        mega_table: bool,
        class_slots: bool,
    ) -> ExecutableBuffer {
        build_ordered(pic, mega_table, class_slots, false)
    }

    /// [`build_with`], and `cell_first` for the round-12-wave-6 order (the
    /// class-slot probe ahead of the site's own hashed ways).
    pub(super) fn build_ordered(
        pic: &JitPICSlot,
        mega_table: bool,
        class_slots: bool,
        cell_first: bool,
    ) -> ExecutableBuffer {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 64 (16-aligned at the CALL, and the
        // homes sit above a Win64 callee's 32-byte shadow area).
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        emit_store_frame(&mut buf, CTX, ENTRY_ABI_REGS[0]);
        emit_store_frame(&mut buf, RECV, ENTRY_ABI_REGS[1]);
        emit_store_frame(&mut buf, ARG1, ENTRY_ABI_REGS[2]);
        let (hits, imm_at, fast) = emit_hashed_vtable_stub_body(
            &mut buf,
            pic as *const JitPICSlot as usize,
            CTX,
            &[RECV, ARG1],
            0,
            0,
            0,
            0,
            None,
            None,
            mega_table,
            class_slots,
            cell_first,
            false,
        );
        assert_eq!(fast, None, "no gate entry unless asked for");
        assert_eq!(hits.len(), 1, "one hit exit");
        assert_eq!(
            buf.as_slice().get(imm_at..imm_at + 8),
            Some(&(pic as *const JitPICSlot as u64).to_le_bytes()[..]),
            "the reported offset names the baked slot"
        );
        emit_mov_imm64(&mut buf, RAX, MISS);
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
        for hit in hits {
            patch_rel32_to_here(&mut buf, hit);
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    pub(super) fn run(stub: &ExecutableBuffer, receiver: u64) -> u64 {
        // SAFETY: `build` finalized `stub`, a complete
        // `extern "C" fn(u64, u64, u64) -> u64` whose callees are published
        // bodies retained by the table, the PIC or the test's own `Arc`s.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(stub.as_ptr())
        };
        f(0x1234, receiver, ARG)
    }

    #[test]
    fn a_shared_table_hit_is_called_from_machine_code() {
        let cache = JitCache::new();
        let plain = publish(&cache, "plain", body(ENTRY_ABI_REGS[1], 0x100));
        let ctx = publish(&cache, "ctx", body(ENTRY_ABI_REGS[2], 0x200));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/I", "f", "(J)J", 2, 11, 0)
            .expect("selector");
        let pic = Box::new(JitPICSlot::new());
        let stub = build(&pic, true);
        let seven = Obj::new(7);
        let nine = Obj::new(9);

        table.install(7, selector, plain.entry_ptr() as u64, false, false);
        assert_eq!(run(&stub, seven.addr()), MISS, "an unbound site reads no table");
        pic.bind_mega_dispatch(table, selector);
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x100, "the shared way is called");

        // A context-tagged way: the context goes first, the argument third.
        assert_eq!(run(&stub, nine.addr()), MISS);
        table.install(9, selector, ctx.entry_ptr() as u64, true, false);
        assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200, "the tag selects the ABI");

        // Another selector's way is not this site's.
        let other = table
            .selector_id("p/I", "g", "(J)J", 2, 11, 0)
            .expect("selector");
        let ten = Obj::new(10);
        table.install(10, other, plain.entry_ptr() as u64, false, false);
        assert_eq!(run(&stub, ten.addr()), MISS);

        // Null and non-object receivers never reach the probe.
        assert_eq!(run(&stub, 0), MISS);
        let mut array = Obj::new(7);
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
        array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
            << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
        assert_eq!(run(&stub, array.addr()), MISS, "an array never matches a class way");

        // The site's own hashed way is probed first.
        pic.install(7, "Seven", ctx.entry_ptr() as u64, true, false);
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x200);
        pic.clear_entries();
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x100, "back to the shared way");

        // A retired way (key to RETIRED_KEY, then word to zero) is never live.
        table.retire_entry(plain.entry_ptr() as usize);
        assert_eq!(run(&stub, seven.addr()), MISS, "a retired way was probed as live");
        assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200, "only the named body retires");
        let mut doomed = std::collections::HashSet::new();
        doomed.insert(ctx.entry_ptr() as usize);
        table.invalidate_targets(&doomed);
        assert_eq!(run(&stub, nine.addr()), MISS, "an invalidated way was probed as live");

        // The same stub without the probe never reads the table.
        table.install(9, selector, ctx.entry_ptr() as u64, true, false);
        assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200);
        let bare = build(&pic, false);
        assert_eq!(run(&bare, nine.addr()), MISS);
        let mut load = vec![0x41, 0x8B, 0x8A];
        load.extend_from_slice(&(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET as i32).to_le_bytes());
        assert!(!bare.as_slice().windows(load.len()).any(|w| w == load));
        assert!(stub.as_slice().windows(load.len()).any(|w| w == load));

        table.clear_entries();
        assert_eq!(run(&stub, nine.addr()), MISS);
        drop((stub, bare));
        for b in [plain, ctx] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// The machine hash and key are `MegaDispatchTable::set_base` / `key`: for
    /// every class, the stub hits exactly when the Rust reader does (a full
    /// set publishes nothing, so some classes miss in both).
    #[test]
    fn the_machine_probe_agrees_with_the_rust_reader_on_every_class() {
        let cache = JitCache::new();
        let plain = publish(&cache, "agree", body(ENTRY_ABI_REGS[1], 0x300));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/J", "h", "(J)J", 0, 12, 0)
            .expect("selector");
        let pic = Box::new(JitPICSlot::new());
        pic.bind_mega_dispatch(table, selector);
        let stub = build(&pic, true);
        let classes: Vec<u32> = (1..1200).step_by(3).collect();
        for &c in &classes {
            table.install(c, selector, plain.entry_ptr() as u64, false, false);
        }
        let mut hits = 0;
        for &c in &classes {
            let obj = Obj::new(c);
            let expect = if table.lookup(c, selector).is_some() {
                hits += 1;
                ARG ^ 0x300
            } else {
                MISS
            };
            assert_eq!(run(&stub, obj.addr()), expect, "class {c}");
        }
        assert!(hits > classes.len() / 2, "the table published too little: {hits}");
        table.clear_entries();
        drop(stub);
        crate::defer_jit_owner(Some(plain));
    }

    /// The single-pass entry point reports where the slot is baked (what the
    /// loop duplicator re-points), and nothing for a refused stub.
    #[test]
    fn the_located_stub_names_its_baked_slot() {
        let slot = 0x7fff_0000_0000_2000usize;
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        let (hits, imm_at, fast) =
            emit_hashed_vtable_stub_located(&mut buf, slot, CTX, &[RECV, ARG1], 0, 0, 0, 0, true);
        if !crate::direct_jit_callee_calls_enabled() {
            assert!(hits.is_empty() && imm_at.is_none() && fast.is_none());
            return;
        }
        // The gate entry, when asked for: `MOV EDX, EAX` then a JMP back.
        let fast = fast.expect("an emitted stub has its gate entry when asked");
        assert_eq!(buf.as_slice().get(fast..fast + 3), Some(&[0x89u8, 0xC2, 0xE9][..]));
        let at = imm_at.expect("an emitted stub names its slot");
        assert_eq!(
            buf.as_slice().get(at - 2..at + 8),
            Some(&[&[0x49u8, 0xBA][..], &(slot as u64).to_le_bytes()[..]].concat()[..]),
            "`MOV R10, imm64` in its 10-byte form"
        );
        let mut refused = ExecutableBuffer::new(4096).expect("buffer");
        let (hits, imm_at, fast) =
            emit_hashed_vtable_stub_located(&mut refused, 0, CTX, &[RECV, ARG1], 0, 0, 0, 0, true);
        assert!(hits.is_empty() && imm_at.is_none() && fast.is_none() && refused.pos() == 0);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod r12w4_mega3_class_slot_probe_tests {
    //! Round 12 wave 4, lane mega3 (proposal M3-1): the hashed stub's
    //! class-slot probe, EXECUTED on this host's ABI (Win64 or SysV), against
    //! the real `MegaDispatchTable` class arrays and directory.
    use super::r12w3_mega2_table_probe_tests::{body, build_with, publish, run, Obj, ARG, MISS};
    use super::*;
    use crate::{JitCache, MegaDispatchTable, MEGA_DISPATCH_WAYS};

    /// Fill `class_id`'s hashed set for `selector` with other classes, so a
    /// later publication for `class_id` lands in its class cell ONLY and a
    /// hit can only have come from the class-slot probe. The fillers are all
    /// above 1000, so they are never a class the tests below drive.
    fn crowd_out(table: &MegaDispatchTable, selector: u32, class_id: u32, filler: u64) {
        assert!(class_id <= 1000);
        let set = MegaDispatchTable::set_base(class_id, selector);
        let others: Vec<u32> = (1001u32..400_000)
            .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
            .take(MEGA_DISPATCH_WAYS)
            .collect();
        assert_eq!(others.len(), MEGA_DISPATCH_WAYS);
        for c in others {
            table.install(c, selector, filler, false, false);
        }
    }

    fn bound_pic(table: &MegaDispatchTable, selector: u32, column: u32) -> Box<JitPICSlot> {
        let pic = Box::new(JitPICSlot::new());
        pic.bind_mega_dispatch(table, selector);
        pic.bind_mega_class_slot(table, column);
        pic
    }

    #[test]
    fn a_class_cell_is_called_from_machine_code() {
        let cache = JitCache::new();
        let plain = publish(&cache, "cs-plain", body(ENTRY_ABI_REGS[1], 0x100));
        let ctx = publish(&cache, "cs-ctx", body(ENTRY_ABI_REGS[2], 0x200));
        let filler = publish(&cache, "cs-filler", body(ENTRY_ABI_REGS[1], 0x7));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/V", "f", "(J)J", 0, 21, 0)
            .expect("selector");
        let other = table
            .selector_id("p/V", "g", "(J)J", 0, 21, 0)
            .expect("selector");
        let pic = bound_pic(table, selector, 3);
        let stub = build_with(&pic, true, true);
        let without = build_with(&pic, true, false);
        let fill = filler.entry_ptr() as u64;
        let (p, c) = (plain.entry_ptr() as u64, ctx.entry_ptr() as u64);

        let seven = Obj::new(7);
        crowd_out(table, selector, 7, fill);
        assert_eq!(run(&stub, seven.addr()), MISS);
        table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
        assert!(table.lookup(7, selector).is_none(), "the hashed set is full");
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x100, "the class cell is called");
        assert_eq!(run(&without, seven.addr()), MISS, "only the class probe reads it");

        // A context-tagged cell: the context goes first, the argument third.
        let nine = Obj::new(9);
        crowd_out(table, selector, 9, fill);
        table.install_with_class_slot(9, selector, Some((3, 8)), c, true, false);
        assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200, "the tag selects the ABI");

        // Another selector's cell at this site's column is not this site's.
        let ten = Obj::new(10);
        crowd_out(table, selector, 10, fill);
        table.install_with_class_slot(10, other, Some((3, 8)), p, false, false);
        assert_eq!(run(&stub, ten.addr()), MISS, "a shared column matched another key");

        // The site's column past the class's array: the bound refuses it.
        let eleven = Obj::new(11);
        crowd_out(table, selector, 11, fill);
        table.install_with_class_slot(11, selector, Some((2, 2)), p, false, false);
        assert!(table.lookup_class_cell(11, selector, 2).is_some());
        assert_eq!(run(&stub, eleven.addr()), MISS, "column 3 of a 2-column array");

        // A class the directory does not cover, and one with no array.
        assert_eq!(run(&stub, Obj::new(1_000_000).addr()), MISS);
        // (A class inside the directory with no array: not one of the
        // fillers, which hold hashed ways for this selector.)
        let lonely = (13u32..256)
            .find(|&c| table.lookup(c, selector).is_none())
            .expect("a class with no way");
        assert_eq!(run(&stub, Obj::new(lonely).addr()), MISS);

        // Null and non-object receivers never reach the probe.
        assert_eq!(run(&stub, 0), MISS);
        let mut array = Obj::new(7);
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
        array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
            << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
        assert_eq!(run(&stub, array.addr()), MISS, "an array never matches a class cell");

        // A site with no column (or another one) reads no cell.
        let unbound = Box::new(JitPICSlot::new());
        unbound.bind_mega_dispatch(table, selector);
        let unbound_stub = build_with(&unbound, true, true);
        assert_eq!(run(&unbound_stub, seven.addr()), MISS);
        let elsewhere = bound_pic(table, selector, 4);
        let elsewhere_stub = build_with(&elsewhere, true, true);
        assert_eq!(run(&elsewhere_stub, seven.addr()), MISS);

        // Retired cells (key first, then word) are never probed as live.
        table.retire_entry(plain.entry_ptr() as usize);
        assert_eq!(run(&stub, seven.addr()), MISS, "a retired cell was probed as live");
        assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200, "only the named body retires");
        let mut doomed = std::collections::HashSet::new();
        doomed.insert(ctx.entry_ptr() as usize);
        table.invalidate_targets(&doomed);
        assert_eq!(run(&stub, nine.addr()), MISS, "an invalidated cell was probed as live");

        table.clear_entries();
        drop((stub, without, unbound_stub, elsewhere_stub));
        for b in [plain, ctx, filler] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// For every class, across two directory growths, the stub hits exactly
    /// when the Rust mirror finds a cell or a hashed way for it.
    #[test]
    fn the_machine_class_probe_agrees_with_the_rust_mirror() {
        let cache = JitCache::new();
        let plain = publish(&cache, "cs-agree", body(ENTRY_ABI_REGS[1], 0x300));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/W", "h", "(J)J", 0, 22, 0)
            .expect("selector");
        let pic = bound_pic(table, selector, 2);
        let stub = build_with(&pic, true, true);
        let classes: Vec<u32> = (1..1200).step_by(3).collect();
        for &c in &classes {
            // Every fifth class gets a one-column array: column 2 is past it.
            let columns = if c % 5 == 0 { 1 } else { 4 };
            let column = if c % 5 == 0 { 1 } else { 2 };
            table.install_with_class_slot(
                c,
                selector,
                Some((column, columns)),
                plain.entry_ptr() as u64,
                false,
                false,
            );
        }
        for &c in &classes {
            let expect = if table.lookup_class_cell(c, selector, 2).is_some()
                || table.lookup(c, selector).is_some()
            {
                ARG ^ 0x300
            } else {
                MISS
            };
            assert_eq!(run(&stub, Obj::new(c).addr()), expect, "class {c}");
        }
        table.clear_entries();
        for &c in &classes {
            assert_eq!(run(&stub, Obj::new(c).addr()), MISS, "class {c} after the clear");
        }
        drop(stub);
        crate::defer_jit_owner(Some(plain));
    }

    /// The probe's encoding length is what its `rel8` branches were argued
    /// for, and the switch-off stub carries none of it.
    #[test]
    fn the_probe_has_its_argued_length_and_is_absent_when_off() {
        let mut buf = ExecutableBuffer::new(256).expect("buffer");
        let tail = buf.pos();
        assert_eq!(emit_mega_class_slot_probe(&mut buf, Some(tail)), None);
        assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
        assert_eq!(buf.pos() - tail, MEGA_CLASS_SLOT_PROBE_BYTES);
        // The forward form (the cell-first order) is the same length.
        let start = buf.pos();
        let hit = emit_mega_class_slot_probe(&mut buf, None).expect("a forward hit exit");
        assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
        assert_eq!(buf.pos() - start, MEGA_CLASS_SLOT_PROBE_BYTES);
        assert_eq!(hit, buf.pos() - 4, "the hit is the probe's last rel32");

        let pic = Box::new(JitPICSlot::new());
        let mut load = vec![0x41, 0x8B, 0x8A];
        load.extend_from_slice(&(JitPICSlot::MEGA_CLASS_SLOT_OFFSET as i32).to_le_bytes());
        let on = build_with(&pic, true, true);
        let off = build_with(&pic, true, false);
        let bare = build_with(&pic, false, true);
        assert!(on.as_slice().windows(load.len()).any(|w| w == load));
        assert!(!off.as_slice().windows(load.len()).any(|w| w == load));
        assert!(!bare.as_slice().windows(load.len()).any(|w| w == load));
        assert_eq!(on.pos(), off.pos() + MEGA_CLASS_SLOT_PROBE_BYTES);
        drop((buf, on, off, bare));
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod r12w6_mega5_cell_first_tests {
    //! Round 12 wave 6, lane mega5 (proposals W5-4 and W5-6): the cell-first
    //! stub order, the megamorphic gate and the inline-first publication,
    //! EXECUTED on this host's ABI against real published bodies and the real
    //! `MegaDispatchTable`.
    use super::r12w3_mega2_table_probe_tests::{
        body, build_ordered, build_with, publish, run, Obj, ARG, MISS,
    };
    use super::*;
    use crate::{JitCache, MegaDispatchTable};

    fn bound_pic(table: &MegaDispatchTable, selector: u32, column: u32) -> Box<JitPICSlot> {
        let pic = Box::new(JitPICSlot::new());
        pic.bind_mega_dispatch(table, selector);
        pic.bind_mega_class_slot(table, column);
        pic
    }

    /// With a receiver in BOTH the site's own hashed way (one body) and its
    /// class cell (another), the cell-first stub calls the cell and the
    /// wave-5 order calls the hashed way; every other answer is the same.
    #[test]
    fn a_cell_first_stub_calls_the_cell_before_the_sites_own_way() {
        let cache = JitCache::new();
        let plain = publish(&cache, "cf-plain", body(ENTRY_ABI_REGS[1], 0x100));
        let ctx = publish(&cache, "cf-ctx", body(ENTRY_ABI_REGS[2], 0x200));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/CF", "f", "(J)J", 0, 31, 0)
            .expect("selector");
        let pic = bound_pic(table, selector, 3);
        let first = build_ordered(&pic, true, true, true);
        let wave5 = build_with(&pic, true, true);
        let (p, c) = (plain.entry_ptr() as u64, ctx.entry_ptr() as u64);
        let seven = Obj::new(7);

        assert_eq!(run(&first, seven.addr()), MISS);
        pic.install(7, "Seven", c, true, false);
        assert_eq!(run(&first, seven.addr()), ARG ^ 0x200, "no cell: the hashed way");
        table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
        assert!(table.lookup_class_cell(7, selector, 3).is_some());
        assert_eq!(run(&first, seven.addr()), ARG ^ 0x100, "the cell is probed first");
        assert_eq!(run(&wave5, seven.addr()), ARG ^ 0x200, "the wave-5 order: the site's way");
        pic.clear_entries();
        assert_eq!(run(&first, seven.addr()), ARG ^ 0x100);
        assert_eq!(run(&wave5, seven.addr()), ARG ^ 0x100);

        // A context-tagged cell reached first selects its ABI.
        let nine = Obj::new(9);
        table.install_with_class_slot(9, selector, Some((3, 8)), c, true, false);
        assert_eq!(run(&first, nine.addr()), ARG ^ 0x200, "the tag selects the ABI");

        // A column-less site (and another column) falls into the hashed ways
        // and then the shared table.
        let unbound = Box::new(JitPICSlot::new());
        unbound.bind_mega_dispatch(table, selector);
        let unbound_first = build_ordered(&unbound, true, true, true);
        let eleven = Obj::new(11);
        unbound.install(11, "Eleven", c, true, false);
        assert_eq!(run(&unbound_first, eleven.addr()), ARG ^ 0x200);
        let elsewhere = bound_pic(table, selector, 4);
        let elsewhere_first = build_ordered(&elsewhere, true, true, true);
        let shared = if table.lookup(7, selector).is_some() {
            ARG ^ 0x100
        } else {
            MISS
        };
        assert_eq!(run(&unbound_first, seven.addr()), shared, "the shared table");
        assert_eq!(run(&elsewhere_first, seven.addr()), shared, "the shared table");

        // Null and non-object receivers never reach a probe.
        assert_eq!(run(&first, 0), MISS);
        let mut array = Obj::new(7);
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
        array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
            << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
        assert_eq!(run(&first, array.addr()), MISS, "an array never matches a cell");

        // Retired cells are never probed as live.
        table.retire_entry(plain.entry_ptr() as usize);
        assert_eq!(run(&first, seven.addr()), MISS, "a retired cell was probed as live");
        assert_eq!(run(&first, nine.addr()), ARG ^ 0x200, "only the named body retires");
        table.clear_entries();
        assert_eq!(run(&first, nine.addr()), MISS);

        // The order costs no bytes: the same probe, moved.
        assert_eq!(first.pos(), wave5.pos());
        let bare = build_ordered(&pic, true, false, true);
        assert_eq!(
            bare.pos(),
            build_with(&pic, true, false).pos(),
            "no class-slot probe, no cell-first order"
        );
        drop((first, wave5, unbound_first, elsewhere_first, bare));
        unbound.clear_entries();
        for b in [plain, ctx] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// For every class, across directory growths, the cell-first stub hits
    /// exactly when the wave-5 stub does, and both agree with the Rust mirror
    /// (a cell, or a way of the shared table).
    #[test]
    fn the_cell_first_order_agrees_with_the_wave5_order_on_every_class() {
        let cache = JitCache::new();
        let plain = publish(&cache, "cf-agree", body(ENTRY_ABI_REGS[1], 0x300));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/CG", "h", "(J)J", 0, 32, 0)
            .expect("selector");
        let pic = bound_pic(table, selector, 2);
        let first = build_ordered(&pic, true, true, true);
        let wave5 = build_with(&pic, true, true);
        let classes: Vec<u32> = (1..1200).step_by(3).collect();
        for &c in &classes {
            let columns = if c % 5 == 0 { 1 } else { 4 };
            let column = if c % 5 == 0 { 1 } else { 2 };
            table.install_with_class_slot(
                c,
                selector,
                Some((column, columns)),
                plain.entry_ptr() as u64,
                false,
                false,
            );
        }
        for &c in &classes {
            let expect = if table.lookup_class_cell(c, selector, 2).is_some()
                || table.lookup(c, selector).is_some()
            {
                ARG ^ 0x300
            } else {
                MISS
            };
            let obj = Obj::new(c);
            assert_eq!(run(&first, obj.addr()), expect, "class {c}, cell first");
            assert_eq!(run(&wave5, obj.addr()), expect, "class {c}, wave-5 order");
        }
        table.clear_entries();
        drop((first, wave5));
        crate::defer_jit_owner(Some(plain));
    }

    /// `extern "C" fn(pic) -> u64`: R10 = the slot, the gate, 1 on its
    /// fall-through and 2 where it jumps.
    fn gate_harness() -> ExecutableBuffer {
        let mut buf = ExecutableBuffer::new(128).expect("buffer");
        emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
        let jump = emit_megamorphic_gate(&mut buf);
        emit_mov_imm64(&mut buf, RAX, 1);
        buf.emit_byte(0xC3); // RET
        patch_rel32_to_here(&mut buf, jump);
        emit_mov_imm64(&mut buf, RAX, 2);
        buf.emit_byte(0xC3); // RET
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn gate(harness: &ExecutableBuffer, pic: &JitPICSlot) -> u64 {
        // SAFETY: `gate_harness` finalized a complete
        // `extern "C" fn(u64) -> u64` that reads one dword of `pic`.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(harness.as_ptr())
        };
        f(pic as *const JitPICSlot as u64)
    }

    /// The gate falls through until a fifth live receiver class overflows the
    /// four inline ways, and jumps from then on. Re-publishing a receiver the
    /// ways hold, or one whose way was retired for a recompiled callee, is not
    /// an overflow.
    #[test]
    fn the_megamorphic_gate_jumps_only_once_the_site_overflowed() {
        let harness = gate_harness();
        let pic = Box::new(JitPICSlot::new());
        assert_eq!(gate(&harness, &pic), 1, "a fresh site");
        // Raw, even entry words: never called, only published.
        let word = |c: u32| 0x1000 + (u64::from(c) << 4);
        for c in 1..=4u32 {
            pic.install(c, "C", word(c), false, false);
            assert_eq!(gate(&harness, &pic), 1, "{c} receivers fit the ways");
        }
        pic.install(2, "C", word(2), false, false);
        assert_eq!(gate(&harness, &pic), 1, "a receiver the ways hold");
        // Class 3's callee recompiled: its way retires, and a way that is
        // retired (graced or not) is never a fifth live class.
        pic.install(3, "C", word(3) + 0x10_0000, false, false);
        assert_eq!(gate(&harness, &pic), 1, "a retired way is not a fifth class");
        assert!(!pic.is_megamorphic());

        let site = Box::new(JitPICSlot::new());
        for c in 1..=5u32 {
            site.install(c, "C", word(c), false, false);
        }
        assert!(site.is_megamorphic());
        assert_eq!(gate(&harness, &site), 2, "the fifth class overflowed the ways");
        site.clear_entries();
        assert_eq!(gate(&harness, &site), 2, "the state is sticky");
        pic.clear_entries();
    }

    /// Five receiver classes whose per-site hashed sets are pairwise distinct,
    /// so each has a way of its own in the stub's table.
    fn five_spread_classes() -> Vec<u32> {
        let mut seen = std::collections::HashSet::new();
        (1u32..10_000)
            .filter(|&c| seen.insert(JitPICSlot::mega_base_index(c)))
            .take(5)
            .collect()
    }

    /// W5-6, as machine code sees it: at a site marked as probing its inline
    /// ways, a receiver the inline ways take does not also take a hashed way,
    /// so the hashed stub misses it; the fifth receiver overflows into the
    /// hashed ways, turns the site megamorphic and promotes the four inline
    /// receivers into hashed ways, which the stub (the gate's target) then
    /// finds. An unmarked site keeps the historical duplicates.
    #[test]
    fn an_inline_first_site_promotes_its_ways_when_it_overflows() {
        let cache = JitCache::new();
        let plain = publish(&cache, "cf-inline", body(ENTRY_ABI_REGS[1], 0x400));
        let p = plain.entry_ptr() as u64;
        let classes = five_spread_classes();
        assert_eq!(classes.len(), 5);
        let objs: Vec<Box<Obj>> = classes.iter().map(|&c| Obj::new(c)).collect();

        let marked = Box::new(JitPICSlot::new());
        marked.note_inline_ways_probed();
        let unmarked = Box::new(JitPICSlot::new());
        // Only the site's own hashed ways: no shared table, no cells.
        let marked_stub = build_with(&marked, false, false);
        let unmarked_stub = build_with(&unmarked, false, false);
        for &c in &classes[..4] {
            marked.install(c, "C", p, false, false);
            unmarked.install(c, "C", p, false, false);
        }
        for obj in &objs[..4] {
            assert_eq!(run(&marked_stub, obj.addr()), MISS, "inline only, no duplicate");
            assert_eq!(run(&unmarked_stub, obj.addr()), ARG ^ 0x400, "the historical duplicate");
        }
        assert_eq!(marked.entries_used(), 4);
        assert!(!marked.is_megamorphic());

        marked.install(classes[4], "C", p, false, false);
        assert!(marked.is_megamorphic(), "the fifth class overflowed");
        for obj in &objs {
            assert_eq!(run(&marked_stub, obj.addr()), ARG ^ 0x400, "promoted or overflowed");
        }
        drop((marked_stub, unmarked_stub));
        marked.clear_entries();
        unmarked.clear_entries();
        crate::defer_jit_owner(Some(plain));
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod r12w7_mega6_refill_probe_tests {
    //! Round 12 wave 7, lane mega6 (`CRATONVM_JIT_IC_SAME_KEY_REFILL`): what
    //! the cell-first stub a megamorphic site runs sees when a receiver's
    //! callee tiers up while a thread is inside compiled code (an OSR loop).
    //! The supersede retires the receiver's class cell; the helper's next
    //! publication must refill it, or every later call of that receiver
    //! misses the cell for the life of the VM. EXECUTED on this host's ABI
    //! against real published bodies and the real `MegaDispatchTable`, with a
    //! JIT execution token held so nothing is graced in between.
    use super::r12w3_mega2_table_probe_tests::{
        body, build_ordered, publish, run, Obj, ARG, ARG1, CTX, MISS, RECV,
    };
    use super::*;
    use crate::{JitCache, MegaDispatchTable, MEGA_DISPATCH_WAYS};

    /// `build_ordered`'s frame and stub, entered the way the single-pass
    /// megamorphic gate enters it (W7-1): the harness loads the receiver's
    /// class id into EAX as the cascade does, poisons RDX (the gate entry must
    /// set it), and jumps to the stub's gate entry. Only for a non-null
    /// plain-object receiver: the gate entry's contract.
    fn build_gate_entry(
        pic: &JitPICSlot,
        mega_table: bool,
        class_slots: bool,
        cell_first: bool,
    ) -> ExecutableBuffer {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        emit_store_frame(&mut buf, CTX, ENTRY_ABI_REGS[0]);
        emit_store_frame(&mut buf, RECV, ENTRY_ABI_REGS[1]);
        emit_store_frame(&mut buf, ARG1, ENTRY_ABI_REGS[2]);
        emit_load_frame(&mut buf, RAX, RECV);
        buf.emit(&[0x8B, 0x00]); // MOV EAX, [RAX]: the cascade's class id
        emit_mov_imm64(&mut buf, RDX, 0xDEAD_BEEF_DEAD_BEEF);
        let to_gate = emit_jmp(&mut buf);
        let (hits, _, fast) = emit_hashed_vtable_stub_body(
            &mut buf,
            pic as *const JitPICSlot as usize,
            CTX,
            &[RECV, ARG1],
            0,
            0,
            0,
            0,
            None,
            None,
            mega_table,
            class_slots,
            cell_first,
            true,
        );
        assert_eq!(hits.len(), 1, "one hit exit");
        let fast = fast.expect("a gate entry when asked for");
        let rel = i32::try_from(fast as i64 - (to_gate as i64 + 4)).expect("rel32");
        assert!(buf.try_patch_i32(to_gate, rel).is_ok());
        emit_mov_imm64(&mut buf, RAX, MISS);
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
        for hit in hits {
            patch_rel32_to_here(&mut buf, hit);
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    /// W7-1: for every class, in every stub order, the gate entry answers
    /// exactly what the full entry answers -- the class cell, the site's own
    /// hashed ways, the shared table, or the fall-through -- including the
    /// context ABI, although RDX was poisoned on the way in.
    #[test]
    fn the_gate_entry_answers_what_the_full_entry_answers() {
        let cache = JitCache::new();
        let plain = publish(&cache, "ge-plain", body(ENTRY_ABI_REGS[1], 0x500));
        let ctx = publish(&cache, "ge-ctx", body(ENTRY_ABI_REGS[2], 0x600));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/GE", "f", "(J)J", 0, 51, 0)
            .expect("selector");
        let pic = Box::new(JitPICSlot::new());
        pic.bind_mega_dispatch(table, selector);
        pic.bind_mega_class_slot(table, 2);
        let classes: Vec<u32> = (1..900).step_by(7).collect();
        for &c in &classes {
            let (entry, tagged) = if c % 2 == 0 {
                (ctx.entry_ptr() as u64, true)
            } else {
                (plain.entry_ptr() as u64, false)
            };
            if c % 3 == 0 {
                // A cell (and a shared way, when its set has room).
                let slot = Some((2, 4));
                table.install_with_class_slot(c, selector, slot, entry, tagged, false);
            } else if c % 3 == 1 {
                // The site's own hashed ways (and inline ways).
                pic.install(c, "C", entry, tagged, false);
            }
            // Otherwise nothing: the fall-through.
        }
        let orders = [
            (true, true, true),
            (true, true, false),
            (true, false, false),
            (false, false, false),
        ];
        for (mega_table, class_slots, cell_first) in orders {
            let full = build_ordered(&pic, mega_table, class_slots, cell_first);
            let gate = build_gate_entry(&pic, mega_table, class_slots, cell_first);
            let mut hits = 0;
            for &c in &classes {
                let obj = Obj::new(c);
                let expect = run(&full, obj.addr());
                if expect != MISS {
                    hits += 1;
                }
                assert_eq!(
                    run(&gate, obj.addr()),
                    expect,
                    "class {c}, order {mega_table}/{class_slots}/{cell_first}"
                );
            }
            assert!(hits > 0, "the orders must hit something");
        }
        pic.clear_entries();
        table.clear_entries();
        for b in [plain, ctx] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// Fill `class_id`'s shared set for `selector` with other classes, so a
    /// hit for `class_id` can only come from its class cell.
    fn crowd_out(table: &MegaDispatchTable, selector: u32, class_id: u32, filler: u64) {
        let set = MegaDispatchTable::set_base(class_id, selector);
        let others: Vec<u32> = (1001u32..400_000)
            .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
            .take(MEGA_DISPATCH_WAYS)
            .collect();
        assert_eq!(others.len(), MEGA_DISPATCH_WAYS);
        for c in others {
            table.install(c, selector, filler, false, false);
        }
    }

    #[test]
    fn a_cell_a_supersede_retired_is_called_again_once_its_key_refills_it() {
        let cache = JitCache::new();
        let c1 = publish(&cache, "rf-c1-body", body(ENTRY_ABI_REGS[1], 0x100));
        let c2 = publish(&cache, "rf-c2-body", body(ENTRY_ABI_REGS[2], 0x200));
        let filler = publish(&cache, "rf-filler", body(ENTRY_ABI_REGS[1], 0x7));
        let table = cache.mega_dispatch_table();
        let selector = table
            .selector_id("p/RF", "f", "(J)J", 0, 41, 0)
            .expect("selector");
        let other = table
            .selector_id("p/RF", "g", "(J)J", 0, 41, 0)
            .expect("selector");
        let pic = Box::new(JitPICSlot::new());
        pic.bind_mega_dispatch(table, selector);
        pic.bind_mega_class_slot(table, 3);
        let stub = build_ordered(&pic, true, true, true);
        let seven = Obj::new(7);
        crowd_out(table, selector, 7, filler.entry_ptr() as u64);

        let (e1, e2) = (c1.entry_ptr() as u64, c2.entry_ptr() as u64);
        let execution = crate::jit_execution_enter();
        table.install_with_class_slot(7, selector, Some((3, 8)), e1, false, false);
        assert!(table.lookup(7, selector).is_none(), "the shared set is full");
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x100, "the C1 body, from the cell");
        // The C2 body supersedes it: the cell is retired, the stub misses.
        table.retire_entry(e1 as usize);
        assert_eq!(run(&stub, seven.addr()), MISS);
        // Another selector sharing the column must wait for the grace.
        table.install_with_class_slot(7, other, Some((3, 8)), e1, false, false);
        assert_eq!(run(&stub, seven.addr()), MISS, "another key's word was called");
        // The helper's re-publication of the SAME key refills the cell at
        // once, and the stub calls the new body, with the new body's ABI.
        table.install_with_class_slot(7, selector, Some((3, 8)), e2, true, false);
        let refilled = crate::inline_cache_pic::same_key_refill_enabled();
        let expect = if refilled { ARG ^ 0x200 } else { MISS };
        assert_eq!(run(&stub, seven.addr()), expect, "refill {refilled}");
        crate::jit_execution_leave(execution);

        table.clear_entries();
        assert_eq!(run(&stub, seven.addr()), MISS);
        drop(stub);
        for b in [c1, c2, filler] {
            crate::defer_jit_owner(Some(b));
        }
    }

    /// The same at the site's own hashed ways (the stub's second probe): a
    /// retired way its receiver refills is called; nothing else is.
    #[test]
    fn a_sites_retired_hashed_way_is_called_again_once_its_class_refills_it() {
        let cache = JitCache::new();
        let c1 = publish(&cache, "rf-h1-body", body(ENTRY_ABI_REGS[1], 0x300));
        let c2 = publish(&cache, "rf-h2-body", body(ENTRY_ABI_REGS[2], 0x400));
        let pic = Box::new(JitPICSlot::new());
        // No shared table and no cells: only the site's own hashed ways.
        let stub = build_ordered(&pic, false, false, false);
        let target = JitPICSlot::mega_base_index(7);
        let partner = (8u32..100_000)
            .find(|&c| JitPICSlot::mega_base_index(c) == target)
            .expect("a class sharing class 7's set");
        let seven = Obj::new(7);

        let execution = crate::jit_execution_enter();
        pic.install(7, "Seven", c1.entry_ptr() as u64, false, false);
        pic.install(partner, "Partner", c1.entry_ptr() as u64, false, false);
        assert_eq!(run(&stub, seven.addr()), ARG ^ 0x300);
        assert!(pic.retire_entry(c1.entry_ptr() as usize), "the supersede walk");
        assert_eq!(run(&stub, seven.addr()), MISS);
        pic.install(partner, "Partner", c2.entry_ptr() as u64, true, false);
        pic.install(7, "Seven", c2.entry_ptr() as u64, true, false);
        let refilled = crate::inline_cache_pic::same_key_refill_enabled();
        let expect = if refilled { ARG ^ 0x400 } else { MISS };
        assert_eq!(run(&stub, seven.addr()), expect, "refill {refilled}");
        crate::jit_execution_leave(execution);

        pic.clear_entries();
        assert_eq!(run(&stub, seven.addr()), MISS);
        drop(stub);
        for b in [c1, c2] {
            crate::defer_jit_owner(Some(b));
        }
    }
}
