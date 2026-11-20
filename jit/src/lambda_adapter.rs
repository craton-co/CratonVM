// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A lambda receiver the monomorphic inline cache CAN hold, capturing or not.
//!
//! # Why the inline cache could not hold one before
//!
//! `jit_invoke_virtual_mic`'s emitted cascade is the reason an ordinary
//! `invokeinterface` costs ~12 ns in this VM: it compares the receiver's class
//! id against the slot's, loads `cached_entry_word`, and CALLs it with the
//! caller's own argument registers — never leaving compiled code. For a named
//! class the Rust helper is entered ONCE per call site
//! (`mic_calls=1` across 2 200 000 dispatches, measured with
//! `CRATONVM_DBG=mic-prof`).
//!
//! A lambda could not use it, and the obstacle is an argument shuffle rather
//! than anything about caching. The cascade passes what the CALL SITE has —
//! `(proxy, samArg1, …)` — while the implementation method wants
//! `(samArg1, …)`: javac compiles a non-capturing lambda body to a private
//! static synthetic that never sees the proxy at all. One register too many, in
//! the wrong place, and the whole inline cache is unusable.
//!
//! # What this is
//!
//! A per-(proxy class, impl entry) THUNK that performs exactly that shuffle and
//! tail-jumps to the impl:
//!
//! ```text
//!     mov  ARG0, ARG1          ; drop the proxy receiver, slide the SAM args down
//!     mov  ARG1, ARG2
//!     …
//!     mov  r11, <impl entry>
//!     jmp  r11
//! ```
//!
//! It is installed into the MIC slot as if it were the receiver's compiled
//! method, so the cascade dispatches a SAM call in machine code with no Rust on
//! the path at all.
//!
//! # A CAPTURING lambda is the same thunk with a prologue
//!
//! `(int n) -> x + n` compiles to a static `lambda$…(int x, int n)`, and the
//! proxy object carries `x`. So the shuffle grows a step — read the captures out
//! of the receiver before dropping it — and the outgoing registers become
//! `(captures…, samArgs…)`:
//!
//! ```text
//!     mov  r11, ARG0                    ; the proxy, before ARG0 is overwritten
//!     mov  ARG0, [r11 + <capture 0>]    ; the capture lands in front
//!     mov  r11, <impl entry>
//!     jmp  r11
//! ```
//!
//! `n` never moves here: one capture replaces the one receiver, so the SAM
//! arguments are already where the impl wants them. Two captures slide them up
//! by one, none slides them down by one, and the zero-capture thunk is
//! byte-for-byte what it was.
//!
//! What made this look expensive was the compact/legacy body-layout branch
//! (`GC_FLAG_COMPACT`) that every `getfield` arm has to emit, because a class
//! with a registered `CompactLayout` may still have legacy-laid-out instances.
//! **A lambda proxy has neither.** Its class id comes from `alloc_lambda_proxy_id`
//! (`0x8000_0000` upward, disjoint from every defined class), nothing ever
//! registers a layout for one, and `plan_object_alloc` sets `GC_FLAG_COMPACT`
//! only when a layout matches the allocation's field count. So a lambda proxy is
//! *always* a uniform 16-byte-cell object, its capture offsets are the constants
//! `HEADER_SIZE + i * SLOT_SIZE + payload`, and the branch never needs emitting.
//!
//! That is a fact about this VM rather than a property of thunks, so
//! [`lambda_adapter_entry`] asks `class_layout_for_fields` at build time and
//! refuses if it ever answers otherwise: should lambda proxies gain compact
//! layouts, this feature turns itself off instead of reading them at the wrong
//! offsets.
//!
//! Four properties make the thunk safe to be this small:
//!
//! * **It tail-JUMPS.** No frame, no stack adjustment, no return address of its
//!   own — so the impl sees byte-identical stack state to a direct call, the
//!   caller's return address is what the impl returns to, and a conservative
//!   stack walk never sees the thunk at all. Windows shadow space and 16-byte
//!   alignment are inherited unchanged for the same reason.
//! * **The slide is uniform across argument types.** This VM's JIT ABI gives
//!   every Java argument exactly one INTEGER register — `execute_jit_call`
//!   builds its register array with `to_bits()` for a `double` and a raw
//!   pointer for a reference, and the emitted cascade loads each operand-stack
//!   slot into `ARG_REGS[i]` without consulting its type. So the thunk needs no
//!   type information at all, and `probes/LambdaAdapterProbe.java`'s
//!   double-argument arm is the check that this remains true.
//! * **The only memory it touches is the receiver's own body**, at constant
//!   offsets, with the same three loads the `getfield` legacy arm emits: the
//!   8-byte payload for a reference/`long`/`double`, the zero-extended 4-byte
//!   payload for `float` bits, and the sign-extended 4-byte payload for the
//!   whole int category. It reads no length, no class metadata, and nothing it
//!   was not handed.
//! * **The return value is the impl's.** RAX flows straight through, and the
//!   caller's post-call sequence — the `i64::MIN` callee-deopt check, the
//!   pending-exception check — runs exactly as it does after any other cached
//!   call.
//!
//! # A loaded reference capture, and the two collectors' claims on it
//!
//! A reference capture arrives in an argument register that no root map
//! describes — the thunk has no frame to describe one with. That is sound for
//! the same reason an ordinary compiled call with reference arguments is: the
//! thunk contains no CALL and no safepoint poll, so no collection can begin
//! inside it, and the impl's prologue stores its register-passed parameters into
//! frame locals (`emit_prologue`) *before* it polls
//! (`emit_safepoint_poll_prologue`). By the first safepoint the value is a local
//! the impl's own root map covers.
//!
//! The other question a baked thunk cannot answer is a *read barrier*, since
//! ZGC arms its own per cycle — and this emitter briefly refused a reference
//! capture whenever `narrow_oops_block_inline_fields()` held, by analogy with
//! the inline `getfield` codegen.
//!
//! **The analogy was wrong.** That predicate guards a COMPACT slot read, which
//! this emitter never emits: the compact-layout refusal in
//! [`lambda_adapter_entry`] guarantees every capture load addresses a legacy
//! 16-byte `Value` cell, and a legacy cell is neither narrowed under compressed
//! oops (`narrow_oop::ref_field_size` is documented as the width of a *compact*
//! field) nor barriered by ZGC (`load_barrier_slot` is applied in
//! `get_array_element`; `get_field`'s legacy arm is a bare
//! `std::ptr::read::<Value>`). The refusal sent a reference capture to a Rust
//! arm that reads the same word the same way, at ~120 ns a call, and bought
//! nothing. `gc/tests/lambda_proxy_capture_word.rs` is what keeps that true.
//!
//! # Invalidation
//!
//! A thunk bakes the impl's entry address, which is the same commitment a JIT'd
//! caller's baked direct call makes. It is registered the same way: the thunk's
//! `_direct_callee_entries` names the impl, and [`adapters_reaching`] lets the
//! cache's invalidation closure — which already promotes any method whose baked
//! callee is being removed — see thunks too, so a slot holding one is cleared
//! when the impl it targets is evicted.
//!
//! # A FRAMED thunk for an over-budget outgoing side (JIT round 20 wave 19, lane yDisp, DISP20-2)
//!
//! The tail-jumping thunk cannot build a stack argument, so an impl whose
//! `context + captures + samArgs` exceeds the register file (`HttpHeaders.
//! headersOf`'s `BiConsumer`: four captures, two SAM arguments and the context
//! are seven words) used to get no thunk at all. Under
//! `CRATONVM_JIT_LAMBDA_FRAMED_THUNK` (default ON, `=0` restores the refusal)
//! such a thunk is FRAMED (`emit_framed_adapter`):
//!
//! ```text
//!     push rbp ; mov rbp, rsp ; sub rsp, F    ; F = round16(shadow + 8*K)
//!     mov  r11, ARG[leading]                  ; the proxy
//!     <the K stack words: a capture via RAX, a SAM argument from its register>
//!     <the register slide and the register captures, as the plain thunk>
//!     mov  r11, <impl entry> ; call r11
//!     leave ; ret
//! ```
//!
//! The stack words sit where `emit_stack_arg_setup` puts them for any compiled
//! callee (`[rsp + shadow + 8k]` at the CALL), and both tiers' prologues copy
//! every stack-passed parameter into their own frame before their first
//! safepoint, so the thunk's copies are dead residue once the impl runs.
//! Unlike the tail jump it leaves a return address INTO the thunk under the
//! impl's frame, and every compiled-frame walker resolves a caller by its
//! return address. That is why the thunk builds a standard RBP frame and its
//! code (up to, not including, a cast thunk's miss stub) is registered as a
//! JIT code range owned by the thunk's own `CompiledMethod`, published under
//! the existing frame contracts the walkers already honour:
//!
//! * `method_label` is EMPTY: the compiled-frame walk reports the thunk's
//!   activation and then drops it (`walk_jit_chain_opts`' reporting loop), so
//!   the walk continues through `[thunk_rbp]` / `[thunk_rbp + 8]` to the
//!   lambda's caller with the caller's own return address -- the stack trace
//!   is the one a tail-jumping thunk gives;
//! * `frame_holds_no_references`: no word of the thunk's band is a live
//!   reference (the captures it stored are copied by the impl's prologue), so
//!   the collector neither scans, verifies nor rewrites it, and never reads a
//!   safepoint id from it (`sp_id_slot_off` stays 0);
//! * `osr_frame_size` is the band below RBP (`F`), so the band walkers that
//!   need a frame size see a bounded, empty one instead of refusing coverage;
//! * it saves no callee-saved register, so a caller register's image in the
//!   impl's frame is decided one frame further up, as for any frame that
//!   leaves a register alone.
//!
//! RAX (and RDX / XMM0) pass through `leave; ret` untouched, so the caller's
//! post-call sentinel and pending-exception checks see the impl's own return.

use std::collections::HashSet;
use std::sync::Arc;

use crate::{CompiledMethod, ExecutableBuffer};

/// x86-64 integer argument registers, in ABI order. Encodings only — this
/// module never needs a general register allocator.
#[cfg(target_os = "windows")]
const ARG_REGS: [u8; 4] = [1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
const ARG_REGS: [u8; 6] = [7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9

/// `R11` — caller-saved, not an argument register, and already the register the
/// emitted cascade itself uses to hold an indirect call target.
const R11: u8 = 11;

/// `MOV dst, src` (64-bit, register to register).
fn emit_mov_reg_reg(out: &mut Vec<u8>, dst: u8, src: u8) {
    let rex = 0x48 | ((src >= 8) as u8) << 2 | ((dst >= 8) as u8);
    out.push(rex);
    out.push(0x89);
    out.push(0xC0 | ((src & 7) << 3) | (dst & 7));
}

/// `MOV r11, imm64` followed by `JMP r11`.
fn emit_tail_jump(out: &mut Vec<u8>, target: usize) {
    out.push(0x49); // REX.WB
    out.push(0xB8 | (R11 & 7)); // MOV r11, imm64
    out.extend_from_slice(&(target as u64).to_le_bytes());
    out.extend_from_slice(&[0x41, 0xFF, 0xE3]); // JMP r11
}

/// How one captured value is loaded out of the proxy's 16-byte `Value` cell.
///
/// Three loads cover every Java type, and they are the same three the inline
/// `getfield` legacy arm emits — deliberately, since both are decoding the same
/// cell. Anything a descriptor byte cannot be mapped to is not emitted at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CaptureLoad {
    /// The cell's 8-byte payload: a reference, a `long`, or a `double`. The
    /// reference case is the one the read-barrier gate applies to.
    Reference,
    /// The cell's 8-byte payload for a `long`/`double` — same load as
    /// [`CaptureLoad::Reference`], distinguished only so the barrier gate can
    /// tell a pointer from a number.
    Wide,
    /// The cell's 4-byte payload, zero-extended: `float` bits, which the JIT ABI
    /// carries as `f32::to_bits() as i64`.
    Float,
    /// The cell's 4-byte payload, sign-extended: the whole int category
    /// (`I Z B C S`), which a legacy cell stores as a single `Value::Int`.
    Int,
}

impl CaptureLoad {
    /// Map a JVM field descriptor's leading byte, or `None` for one this module
    /// has no load for (`V`, or anything malformed).
    pub fn from_descriptor(tok: u8) -> Option<Self> {
        Some(match tok {
            b'L' | b'[' => Self::Reference,
            b'J' | b'D' => Self::Wide,
            b'F' => Self::Float,
            b'I' | b'Z' | b'B' | b'C' | b'S' => Self::Int,
            _ => return None,
        })
    }

    /// Does emitting this load mean emitting a raw reference read?
    ///
    /// [`CaptureLoad::Reference`] and [`CaptureLoad::Wide`] encode identically
    /// — both are the cell's 8-byte payload — so this distinction currently
    /// changes no byte of emitted code. It is kept, and kept separate, because
    /// it is the hook any future decode would need: if a legacy cell ever gains
    /// a narrowed or coloured reference representation, this is the predicate
    /// that says which loads must change, and
    /// `gc/tests/lambda_proxy_capture_word.rs` is the test that would fail
    /// first.
    fn is_reference(self) -> bool {
        matches!(self, Self::Reference)
    }
}

/// `MOV dst, [base + disp32]` — one encoder for the three widths, since they
/// differ only in opcode and REX.W.
///
/// `mod = 0b10` (disp32) throughout, which is what keeps this a single form: no
/// `mod = 0b00` RIP-relative special case, and no `disp8` variant to get the
/// length of wrong. `base` is always `R11` here — `R11 & 7 == 3`, so the
/// `rm == 0b100` escape that would demand a SIB byte cannot arise.
fn emit_load_mem(out: &mut Vec<u8>, dst: u8, base: u8, disp: i32, load: CaptureLoad) {
    debug_assert_eq!(base & 7, 3, "the encoding below assumes a non-SIB base");
    let (rex_w, opcode): (bool, &[u8]) = match load {
        // MOV r64, m64
        CaptureLoad::Reference | CaptureLoad::Wide => (true, &[0x8B]),
        // MOV r32, m32 — writing a 32-bit register zeroes the upper half, which
        // is the zero-extension `to_bits() as i64` produces for a `float`.
        CaptureLoad::Float => (false, &[0x8B]),
        // MOVSXD r64, m32.
        //
        // The sign extension is UNOBSERVABLE, which is worth stating rather
        // than leaving a reader to assume a test covers it: an int-category
        // parameter is stored to a frame local by the impl's prologue and read
        // back 32 bits at a time, so the upper half is don't-care. Emitting the
        // zero-extending load instead was tried against
        // `captureShapesChecksum`'s `byte -100` and `short -30000` arms and
        // changed nothing. `MOVSXD` is here because it is what the `getfield`
        // legacy arm emits and what the Rust arm's `Value::Int(x) => x as i64`
        // produces — if anything ever does read the full register, the
        // convention it will read is this one.
        CaptureLoad::Int => (true, &[0x63]),
    };
    let rex = u8::from(rex_w) << 3 | u8::from(dst >= 8) << 2 | u8::from(base >= 8);
    if rex != 0 {
        out.push(0x40 | rex);
    }
    out.extend_from_slice(opcode);
    out.push(0x80 | ((dst & 7) << 3) | (base & 7)); // mod=10, reg=dst, rm=base
    out.extend_from_slice(&disp.to_le_bytes());
}

/// Byte offset of capture `index`'s payload within the proxy object.
///
/// Uniform 16-byte `Value` cells, because a lambda proxy is never compact — see
/// the module note. The 8-byte payload sits at `FIELD_CELL_PAYLOAD64_OFFSET`
/// and the 4-byte one at `FIELD_CELL_PAYLOAD32_OFFSET`, matching what the
/// `getfield` legacy arm addresses.
fn capture_payload_offset(index: usize, load: CaptureLoad) -> i32 {
    let cell = cratonvm_types::HEADER_SIZE + index * cratonvm_types::SLOT_SIZE;
    let payload = match load {
        CaptureLoad::Reference | CaptureLoad::Wide => cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET,
        CaptureLoad::Float | CaptureLoad::Int => cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET,
    };
    (cell + payload) as i32 // Cast: x86-64 disp32; a capture index is small
}

/// The thunk body.
///
/// `leading` is the number of incoming registers that pass through untouched:
/// 1 when the impl takes the hidden VM context in ARG0, 0 otherwise. The
/// receiver sits immediately after them, and the impl wants
/// `(captures…, samArgs…)` in its place — so the SAM arguments move by
/// `captures.len() - 1` registers: down one when there are no captures, not at
/// all for a single capture, up for more.
///
/// Order is the whole correctness argument, and there are three obligations:
///
/// 1. The receiver is saved to `R11` FIRST, because `ARG_REGS[leading]` is the
///    first register the captures overwrite. `R11` is caller-saved, is not an
///    argument register, and is dead by the time the tail jump reuses it.
/// 2. The slide runs in the direction that reads each register before the step
///    that writes it — descending when moving up, ascending when moving down —
///    so no argument is clobbered and no second scratch register is needed.
/// 3. The captures land LAST, into the registers the slide has just vacated.
fn emit_adapter(
    leading: usize,
    captures: &[CaptureLoad],
    sam_args: usize,
    impl_entry: usize,
) -> Vec<u8> {
    // DISP20-2: an outgoing side past the register file is the framed thunk
    // (`adapter_entry` admits one only under its switch and stack-word cap).
    if leading + captures.len() + sam_args > ARG_REGS.len() {
        return emit_framed_adapter(leading, captures, sam_args, impl_entry);
    }
    let mut out = Vec::with_capacity(8 + (captures.len() + sam_args) * 8 + 13);
    if !captures.is_empty() {
        emit_mov_reg_reg(&mut out, R11, ARG_REGS[leading]);
    }
    let first_in = leading + 1;
    let first_out = leading + captures.len();
    if first_out > first_in {
        for j in (0..sam_args).rev() {
            emit_mov_reg_reg(&mut out, ARG_REGS[first_out + j], ARG_REGS[first_in + j]);
        }
    } else if first_out < first_in {
        for j in 0..sam_args {
            emit_mov_reg_reg(&mut out, ARG_REGS[first_out + j], ARG_REGS[first_in + j]);
        }
    }
    for (i, &load) in captures.iter().enumerate() {
        emit_load_mem(
            &mut out,
            ARG_REGS[leading + i],
            R11,
            capture_payload_offset(i, load),
            load,
        );
    }
    emit_tail_jump(&mut out, impl_entry);
    out
}

// ---------------------------------------------------------------------------
// Framed thunks (JIT round 20 wave 19, lane yDisp: DISP20-2)
// ---------------------------------------------------------------------------

/// The most outgoing words a framed thunk passes on the stack. A bound on code
/// size and on the `disp8` stores ([`emit_store_rsp`]), not an ABI limit: the
/// highest store is `shadow + 8 * (FRAMED_MAX_STACK_WORDS - 1)` = 88 on Win64.
const FRAMED_MAX_STACK_WORDS: usize = 8;

/// Bytes of outgoing shadow space below the stack words at the impl CALL.
#[cfg(target_os = "windows")]
const FRAMED_SHADOW: usize = 32;
/// See the Win64 arm.
#[cfg(not(target_os = "windows"))]
const FRAMED_SHADOW: usize = 0;

/// `RBP` / `RSP` encodings (only the framed thunk's prologue names them).
const RBP: u8 = 5;
const RSP: u8 = 4;
/// `RAX`: the scratch a capture travels through on its way to a stack word.
/// Caller-saved, not an argument register, and dead until the impl returns
/// its value in it.
const RAX: u8 = 0;

/// The framed thunk's `SUB RSP` amount for `stack_words` outgoing stack words:
/// the shadow space plus the words, rounded up to 16 so RSP stays aligned at
/// the impl CALL (entered at 8 mod 16, 0 mod 16 after `PUSH RBP`). This is
/// also the frame's band below RBP, published as `osr_frame_size`.
fn framed_frame_bytes(stack_words: usize) -> usize {
    (FRAMED_SHADOW + stack_words * 8 + 15) & !15
}

/// The FRAMED thunk body for an outgoing side of `leading + captures + samArgs`
/// words past the register file (see "A FRAMED thunk" in the module note).
///
/// Order is the correctness argument, as for [`emit_adapter`], with one step in
/// front of it:
///
/// 1. The proxy goes to `R11` first (`captures` has at least two entries here:
///    an outgoing side wider than an incoming side that fits needs them).
/// 2. Every STACK word is stored before any argument register is written, so
///    each SAM argument that goes to the stack is read from the register it
///    arrived in; a capture is loaded through `RAX`.
/// 3. The register part is the plain thunk's: the SAM arguments that stay in
///    registers slide UP, descending; the captures that fit land last in the
///    registers the slide vacated.
/// 4. `CALL` (not `JMP`) the impl, then `LEAVE; RET`: RAX passes through.
fn emit_framed_adapter(
    leading: usize,
    captures: &[CaptureLoad],
    sam_args: usize,
    impl_entry: usize,
) -> Vec<u8> {
    let regs = ARG_REGS.len();
    let outgoing = leading + captures.len() + sam_args;
    debug_assert!(outgoing > regs && leading + 1 + sam_args <= regs);
    debug_assert!(captures.len() >= 2);
    let stack_words = outgoing - regs;
    debug_assert!(stack_words <= FRAMED_MAX_STACK_WORDS);
    let frame = framed_frame_bytes(stack_words);
    let mut out = Vec::with_capacity(32 + outgoing * 10 + 16);
    // PUSH RBP; MOV RBP, RSP.
    out.push(0x55);
    emit_mov_reg_reg(&mut out, RBP, RSP);
    // SUB RSP, F. Cast: at most round16(32 + 8 * 8) = 96, a positive imm8.
    out.extend_from_slice(&[0x48, 0x83, 0xEC, frame as u8]);
    emit_mov_reg_reg(&mut out, R11, ARG_REGS[leading]);
    // 2. The stack words, ascending ABI index.
    for k in regs..outgoing {
        // Cast: at most 32 + 8 * 7 = 88, a positive disp8.
        let disp = (FRAMED_SHADOW + (k - regs) * 8) as u8;
        let slot = k - leading;
        if slot < captures.len() {
            let load = captures[slot];
            emit_load_mem(&mut out, RAX, R11, capture_payload_offset(slot, load), load);
            emit_store_rsp(&mut out, disp, RAX);
        } else {
            let j = slot - captures.len();
            emit_store_rsp(&mut out, disp, ARG_REGS[leading + 1 + j]);
        }
    }
    // 3. The register slide (always UP: at least two captures) ...
    for j in (0..sam_args).rev() {
        let dst = leading + captures.len() + j;
        if dst < regs {
            emit_mov_reg_reg(&mut out, ARG_REGS[dst], ARG_REGS[leading + 1 + j]);
        }
    }
    // ... and the captures that fit.
    for (i, &load) in captures.iter().enumerate() {
        if leading + i < regs {
            emit_load_mem(
                &mut out,
                ARG_REGS[leading + i],
                R11,
                capture_payload_offset(i, load),
                load,
            );
        }
    }
    // 4. CALL the impl, tear the frame down, return its RAX.
    out.push(0x49); // REX.WB
    out.push(0xB8 | (R11 & 7)); // MOV r11, imm64
    out.extend_from_slice(&(impl_entry as u64).to_le_bytes());
    out.extend_from_slice(&[0x41, 0xFF, 0xD3]); // CALL r11
    out.push(0xC9); // LEAVE
    out.push(0xC3); // RET
    out
}

/// `CRATONVM_JIT_LAMBDA_FRAMED_THUNK` (JIT round 20 wave 19, lane yDisp;
/// DISP20-2 of `docs/feature-designs/jit-ir-optimizations.md`) --
/// **default ON**, `=0` restores the register-budget refusal exactly. Read once
/// per thunk build, never per call.
pub fn lambda_framed_thunk_enabled() -> bool {
    true
}

/// Does `adapter_entry` admit this arity, and is the thunk framed? `None`:
/// refused (an incoming side past the register file, which the call site
/// itself passes on the stack; or an over-budget outgoing side with the switch
/// off or more than [`FRAMED_MAX_STACK_WORDS`] stack words). `Some(framed)`
/// otherwise. `framed_on` is the switch, asked only for an over-budget shape.
fn adapter_arity(
    incoming: usize,
    outgoing: usize,
    framed_on: impl FnOnce() -> bool,
) -> Option<bool> {
    let regs = ARG_REGS.len();
    if incoming > regs {
        return None;
    }
    if outgoing <= regs {
        return Some(false);
    }
    (outgoing - regs <= FRAMED_MAX_STACK_WORDS && framed_on()).then_some(true)
}

// ---------------------------------------------------------------------------
// Cast thunks (nio21 wave 18, lane L2)
// ---------------------------------------------------------------------------
//
// A generic SAM call site (`Function<Integer,Integer>.apply(Object)`, the
// `BiConsumer<String,List<String>>` `HttpHeaders.headersOf` hands
// `TreeMap.forEach`) reaches its impl through a javac bridge that `checkcast`s
// each such argument to the instantiated type. The plain thunk above cannot
// replay that, so every call of such a site used to leave compiled code for
// `jit_invoke_virtual_mic` (`r21-lambda-ic-thunk-cannot-replay-a-cast-FIXED-20261010.md`).
//
// A CAST THUNK is the plain thunk with a guard in front and a miss edge
// behind, and it is PER CALL SITE:
//
// ```text
//   ; for every replayed cast k (SAM argument i_k, proved class id c_k):
//     test  ARG[i_k], ARG[i_k]       ; null passes every checkcast
//     jz    .ok_k
//     test  byte [ARG[i_k] + KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK
//     jnz   .miss                    ; an array (its header names the COMPONENT)
//     cmp   dword [ARG[i_k]], c_k    ; the class id the VM proved passes
//     jne   .miss
//   .ok_k:
//   <the plain thunk: captures, shuffle, tail jump>
//   .miss:                           ; a framed call of the site's own helper
//     sub   rsp, 0x58
//     mov   [rsp + BLOCK + 8*j], ARG[leading + j]   ; receiver + SAM args
//     mov   ARG0, vm ; mov ARG1, info ; lea ARG2, [rsp + BLOCK] ; mov ARG3, n
//     mov   ARG4, mic ; mov ARG5, pic                 ; (Win64: on the stack)
//     mov   rax, jit_invoke_virtual_mic ; call rax
//     add   rsp, 0x58 ; ret
// ```
//
// Why the compare is sound: the VM bakes `c_k` only when its verdict PROVED
// that class passes the cast from a class-pair arm (`LambdaJitSite::
// cast_pass_cids`, nio21 w15 L5). That is a fact about two loaded classes:
// supertypes never change (a redefinition may not change the hierarchy), class
// ids are never recycled, and a lambda proxy's registration is never removed.
// An ARRAY argument carries its component's class id in the header, so the
// kind tag is tested first and every array takes the miss edge.
//
// Why the miss edge is sound: it is exactly the call the site's own cascade
// would have made had its inline cache missed -- the same helper, the same
// `(vm, info, args, n, mic, pic)` -- so the full cast verdict, the
// `ClassCastException` and its HotSpot message are the Rust path's, unchanged.
// The stub is shaped like the not-entrant stub (`not_entrant.rs`): it builds
// no RBP frame and leaves the published frame identity alone, so to every
// stack walker the helper was called from the caller's own call site; its
// spilled block is read by the helper only before its first safepoint
// (`forward_jit_reference_args` canonicalises it), and the same values sit in
// the caller's own staged or spilled argument homes. Its return value goes
// back through the caller's inline-hit continuation, whose callee-deopt
// service already handles every `i64::MIN` the impl itself could return
// (a lambda proxy class has no class-store entry, so the service never routes
// a pending throwable into a handler of its own).
//
// Because the miss edge bakes the call site's `JitInvokeInfo`, MIC and PIC
// addresses, a cast thunk is installed into THAT site's slots only (never into
// the shared megamorphic table), and those addresses are part of its key.

/// One replayed bridge `checkcast` a cast thunk tests in machine code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CastGuard {
    /// Index among the SAM's own arguments (the receiver excluded).
    pub sam_index: usize,
    /// The argument class id the VM proved passes this cast.
    pub class_id: u32,
    /// When `Some(tag)`, guards a 1-D primitive array (`[B`, `[I`, etc.)
    /// by comparing `BYTE [reg + KIND_TAGS_BYTE_OFFSET] == tag` instead of
    /// `class_id`.
    pub prim_array_tag: Option<u8>,
}

/// Where a cast thunk's miss edge goes: the call site's own resolving helper
/// and the five words its emitted miss path would have passed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CastMissSite {
    /// `jit_invoke_virtual_mic`.
    pub helper: usize,
    /// The `SharedVm` the call site's compiled body belongs to.
    pub vm_ptr: usize,
    /// The call site's `JitInvokeInfo`.
    pub info_ptr: usize,
    /// The call site's `JitMICSlot` (never 0: the helper dereferences it).
    pub mic_ptr: usize,
    /// The call site's `JitPICSlot`, or 0.
    pub pic_ptr: usize,
}

/// Bytes the miss stub reserves below its return address. `0x58` leaves RSP
/// 16-byte aligned at the helper `CALL` (entered with RSP = 8 mod 16), as in
/// the not-entrant stub.
const CAST_MISS_FRAME: u8 = 0x58;

/// Where the miss stub spills the incoming Java arguments, from RSP. Win64:
/// past the 32-byte shadow space and the helper's two stack-passed arguments
/// (`mic`, `pic` at `0x20` / `0x28`); four registers at most, so the block
/// ends at `0x50`. SysV: from RSP, six registers at most (`0x30`).
#[cfg(target_os = "windows")]
const CAST_MISS_BLOCK: u8 = 0x30;
/// See the Win64 arm.
#[cfg(not(target_os = "windows"))]
const CAST_MISS_BLOCK: u8 = 0x00;

/// REX.B / REX.R bit for an extended register.
fn rex_bit(reg: u8) -> u8 {
    u8::from(reg >= 8)
}

/// `TEST r64, r64`.
fn emit_test_self(out: &mut Vec<u8>, reg: u8) {
    out.push(0x48 | rex_bit(reg) << 2 | rex_bit(reg));
    out.push(0x85);
    out.push(0xC0 | ((reg & 7) << 3) | (reg & 7));
}

/// `JCC rel32` with a zero displacement; returns the displacement's offset.
fn emit_jcc_rel32(out: &mut Vec<u8>, cc: u8) -> usize {
    out.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
    out.len() - 4
}

/// The guards in front of a cast thunk. Returns the offsets of the rel32
/// displacements that must be patched to the miss stub.
///
/// Only flags change: every argument register is intact on both exits, which
/// is what lets the miss stub spill the call exactly as it arrived.
fn emit_cast_guards(out: &mut Vec<u8>, leading: usize, guards: &[CastGuard]) -> Vec<usize> {
    let mut to_miss = Vec::with_capacity(guards.len() * 2);
    for guard in guards {
        let reg = ARG_REGS[leading + 1 + guard.sam_index];
        // `[reg]` with mod=00 and `[reg + disp8]` with mod=01 need no SIB and
        // are not RIP-relative only when the register's low bits are not
        // 4 (RSP/R12) or 5 (RBP/R13); no argument register is either
        // (`no_argument_register_needs_a_sib_or_rip_form`).
        debug_assert!(reg & 7 != 4 && reg & 7 != 5);
        emit_test_self(out, reg);
        out.extend_from_slice(&[0x74, 0x00]); // JZ .ok (rel8, patched below)
        let jz_disp = out.len() - 1;
        if let Some(tag) = guard.prim_array_tag {
            // CMP BYTE [reg + KIND_TAGS_BYTE_OFFSET], tag
            if reg >= 8 {
                out.push(0x41);
            }
            out.extend_from_slice(&[
                0x80,
                0x78 | (reg & 7),
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                tag,
            ]);
            to_miss.push(emit_jcc_rel32(out, 0x85)); // JNE .miss
        } else {
            // TEST BYTE [reg + KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK
            if reg >= 8 {
                out.push(0x41);
            }
            // Cast: header offsets are single-digit constants.
            out.extend_from_slice(&[
                0xF6,
                0x40 | (reg & 7),
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                cratonvm_types::KIND_TAG_BYTE_MASK,
            ]);
            to_miss.push(emit_jcc_rel32(out, 0x85)); // JNZ .miss
                                                     // CMP DWORD [reg], imm32 -- the class id at header offset 0.
            if reg >= 8 {
                out.push(0x41);
            }
            out.extend_from_slice(&[0x81, 0x38 | (reg & 7)]);
            out.extend_from_slice(&guard.class_id.to_le_bytes());
            to_miss.push(emit_jcc_rel32(out, 0x85)); // JNE .miss
        }
        // Cast: the guard body is at most 25 bytes, a positive rel8.
        out[jz_disp] = (out.len() - (jz_disp + 1)) as u8;
    }
    to_miss
}

/// `MOV r64, imm64`.
fn emit_mov_imm64(out: &mut Vec<u8>, dst: u8, imm: u64) {
    out.push(0x48 | rex_bit(dst));
    out.push(0xB8 + (dst & 7));
    out.extend_from_slice(&imm.to_le_bytes());
}

/// `MOV [RSP + disp8], r64`.
fn emit_store_rsp(out: &mut Vec<u8>, disp: u8, src: u8) {
    out.push(0x48 | rex_bit(src) << 2);
    out.push(0x89);
    out.push(0x44 | ((src & 7) << 3));
    out.push(0x24);
    out.push(disp);
}

/// The miss stub: spill the `java_args` incoming Java arguments (receiver
/// first) into a block on its own small frame and call the site's helper with
/// the words its emitted miss path passes. RAX comes back as the call's value.
fn emit_cast_miss_stub(out: &mut Vec<u8>, leading: usize, java_args: usize, miss: &CastMissSite) {
    out.extend_from_slice(&[0x48, 0x83, 0xEC, CAST_MISS_FRAME]); // SUB RSP, 0x58
    for j in 0..java_args {
        // Cast: j < 6, the block stays inside the reserved frame.
        emit_store_rsp(out, CAST_MISS_BLOCK + (j as u8) * 8, ARG_REGS[leading + j]);
    }
    // Cast: addresses and a count, baked as 64-bit immediates.
    emit_mov_imm64(out, ARG_REGS[0], miss.vm_ptr as u64);
    emit_mov_imm64(out, ARG_REGS[1], miss.info_ptr as u64);
    // LEA ARG2, [RSP + CAST_MISS_BLOCK]
    let a2 = ARG_REGS[2];
    out.extend_from_slice(&[
        0x48 | rex_bit(a2) << 2,
        0x8D,
        0x44 | ((a2 & 7) << 3),
        0x24,
        CAST_MISS_BLOCK,
    ]);
    emit_mov_imm64(out, ARG_REGS[3], java_args as u64);
    #[cfg(target_os = "windows")]
    {
        // The fifth and sixth arguments go on the stack, above the shadow space.
        out.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64
        out.extend_from_slice(&(miss.mic_ptr as u64).to_le_bytes());
        out.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]); // MOV [RSP+0x20], RAX
        out.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64
        out.extend_from_slice(&(miss.pic_ptr as u64).to_le_bytes());
        out.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x28]); // MOV [RSP+0x28], RAX
    }
    #[cfg(not(target_os = "windows"))]
    {
        emit_mov_imm64(out, ARG_REGS[4], miss.mic_ptr as u64);
        emit_mov_imm64(out, ARG_REGS[5], miss.pic_ptr as u64);
    }
    out.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64 (helper)
    out.extend_from_slice(&(miss.helper as u64).to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xD0]); // CALL RAX
    out.extend_from_slice(&[0x48, 0x83, 0xC4, CAST_MISS_FRAME]); // ADD RSP, 0x58
    out.push(0xC3); // RET
}

/// A cast thunk's body: guards, the plain thunk, the miss stub.
fn emit_cast_adapter(
    leading: usize,
    captures: &[CaptureLoad],
    sam_args: usize,
    impl_entry: usize,
    guards: &[CastGuard],
    miss: &CastMissSite,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(guards.len() * 25 + 160);
    let to_miss = emit_cast_guards(&mut out, leading, guards);
    out.extend_from_slice(&emit_adapter(leading, captures, sam_args, impl_entry));
    let miss_at = out.len();
    for at in to_miss {
        // Cast: a displacement inside one small buffer.
        let rel = (miss_at - (at + 4)) as i32;
        out[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    emit_cast_miss_stub(&mut out, leading, 1 + sam_args, miss);
    out
}

// ---------------------------------------------------------------------------
// Receiver thunks (JIT round 20 wave 12, lane qLam: W15L5-5 / W17L1-5 /
// W18L2-2 = DISP20-3)
// ---------------------------------------------------------------------------
//
// An INSTANCE-method impl (`x::m` bound, `Type::m` unbound, an interface
// default) is entered with its receiver as Java argument 0, which is exactly
// what the plain thunk's register shuffle already produces: a bound reference's
// receiver is the proxy's capture 0 (the shuffle loads it into `ARG[leading]`),
// an unbound one's is the SAM's argument 0 (the shuffle slides it down into
// `ARG[leading]`). What the static thunk never needed is a decision about
// WHICH body runs: the VM resolves the impl against the receiver's EXACT class
// and bakes that class id, so the thunk must prove the receiver is of it:
//
// ```text
//   ; bound only: the receiver out of the proxy's capture 0
//     mov   r11, ARG[leading]
//     mov   r11, [r11 + <capture 0 payload>]
//     test  R, R                     ; R = r11 (bound) or ARG[leading + 1]
//     jz    .miss                    ; null: the Rust path raises the NPE
//     test  byte [R + KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK
//     jnz   .miss                    ; an array (its header names the COMPONENT)
//     cmp   dword [R], <receiver class id>
//     jne   .miss                    ; any other class: resolved again in Rust
//   <the cast guards, if any; the plain thunk; the cast thunk's miss stub>
// ```
//
// Sound for the cast thunk's reasons: the guard only reads (R11 is scratch,
// every argument register is intact on both exits, so the miss stub spills the
// call exactly as it arrived), the miss edge is the call site's own resolving
// helper, and the baked class id is a fact for life (class ids are never
// recycled; a redefinition keeps the method set, and the VM's site is keyed by
// the impl's redefinition gate). Virtual dispatch stays exact for ANY class
// hierarchy, final or not: the body baked is the one the receiver's exact
// class resolves to, and every other class misses.

/// Where an instance-method impl's receiver comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReceiverSource {
    /// A bound method reference (`x::m`): the proxy's capture 0.
    Capture0,
    /// An unbound method reference (`Type::m`): the SAM's argument 0.
    SamArg0,
}

/// The receiver guard of a receiver thunk: where the receiver is, and the
/// exact class id the VM resolved the impl against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReceiverGuard {
    /// Capture 0 or SAM argument 0.
    pub source: ReceiverSource,
    /// The receiver's exact class id.
    pub class_id: u32,
}

/// The receiver guard in front of a receiver thunk. Returns the offsets of the
/// rel32 displacements that must be patched to the miss stub. Only flags and
/// R11 change.
fn emit_receiver_guard(out: &mut Vec<u8>, leading: usize, guard: &ReceiverGuard) -> Vec<usize> {
    let reg = match guard.source {
        ReceiverSource::SamArg0 => ARG_REGS[leading + 1],
        ReceiverSource::Capture0 => {
            emit_mov_reg_reg(out, R11, ARG_REGS[leading]);
            emit_load_mem(
                out,
                R11,
                R11,
                capture_payload_offset(0, CaptureLoad::Reference),
                CaptureLoad::Reference,
            );
            R11
        }
    };
    // `[reg]` / `[reg + disp8]` need no SIB and are not RIP-relative (see
    // `emit_cast_guards`); R11's low bits are 3.
    debug_assert!(reg & 7 != 4 && reg & 7 != 5);
    let mut to_miss = Vec::with_capacity(3);
    emit_test_self(out, reg);
    to_miss.push(emit_jcc_rel32(out, 0x84)); // JZ .miss (null receiver)
    if reg >= 8 {
        out.push(0x41);
    }
    // Cast: header offsets are single-digit constants.
    out.extend_from_slice(&[
        0xF6,
        0x40 | (reg & 7),
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::KIND_TAG_BYTE_MASK,
    ]);
    to_miss.push(emit_jcc_rel32(out, 0x85)); // JNZ .miss (an array)
    if reg >= 8 {
        out.push(0x41);
    }
    out.extend_from_slice(&[0x81, 0x38 | (reg & 7)]);
    out.extend_from_slice(&guard.class_id.to_le_bytes());
    to_miss.push(emit_jcc_rel32(out, 0x85)); // JNE .miss (another class)
    to_miss
}

/// A receiver thunk's body: the receiver guard, the cast guards, the plain
/// thunk, the miss stub.
fn emit_receiver_adapter(
    leading: usize,
    captures: &[CaptureLoad],
    sam_args: usize,
    impl_entry: usize,
    receiver: &ReceiverGuard,
    guards: &[CastGuard],
    miss: &CastMissSite,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(guards.len() * 25 + 200);
    let mut to_miss = emit_receiver_guard(&mut out, leading, receiver);
    to_miss.extend(emit_cast_guards(&mut out, leading, guards));
    out.extend_from_slice(&emit_adapter(leading, captures, sam_args, impl_entry));
    let miss_at = out.len();
    for at in to_miss {
        // Cast: a displacement inside one small buffer.
        let rel = (miss_at - (at + 4)) as i32;
        out[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    emit_cast_miss_stub(&mut out, leading, 1 + sam_args, miss);
    out
}

/// Everything `emit_adapter` reads. See the note at the `map.get` in
/// [`lambda_adapter_entry`] for why the arity has to be in here.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AdapterKey {
    proxy_class_id: u32,
    impl_entry: usize,
    leading: usize,
    sam_args: usize,
    captures: Vec<CaptureLoad>,
    /// A cast thunk's guards and miss edge (nio21 w18 L2); `None` for the
    /// plain, call-site-independent thunk.
    cast: Option<(Vec<CastGuard>, CastMissSite)>,
    /// A receiver thunk's receiver guard (JIT round 20 wave 12, lane qLam);
    /// always with `cast` (its miss edge).
    receiver: Option<ReceiverGuard>,
}

/// Thunk requests refused reuse by the arity/capture terms of [`AdapterKey`] —
/// i.e. the ones the old `(proxy class, impl entry)` key would have answered
/// with a thunk emitted for a different shape. Reported by the
/// `CRATONVM_DBG=lambda-jit` census; UNGATED, because a correctness counter
/// that only exists when a debug variable is set cannot answer "did this ever
/// happen" after the fact.
static SHAPE_COLLISIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times the shape terms of [`AdapterKey`] refused a stale thunk.
pub fn lambda_adapter_shape_collisions() -> u64 {
    SHAPE_COLLISIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// One VM's live thunks, keyed by every input their body depends on. Owned by
/// that VM's [`crate::JitCache`] (`JitCache::lambda_adapters`), so it dies with
/// the VM.
///
/// Held strongly: a thunk is reachable from an inline-cache slot that this map
/// cannot see, so it lives as long as the cache whose bodies those inline
/// caches belong to, unless an invalidation forgets it first.
///
/// t1 wave 2 (lane Y): this was a process `OnceLock`, so every thunk a dropped
/// VM had built -- and, through `_direct_callee_roots`, the impl body it jumps
/// into -- stayed mapped for the life of the process
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// Dropping the map drops each thunk's `Arc`; a published buffer whose last
/// owner goes away is handed to the retirement queue by `ExecutableBuffer`'s
/// own drop, and a framed thunk's code range is withdrawn by
/// `CompiledMethod::drop`, so no extra teardown is needed here.
#[derive(Default)]
pub struct LambdaAdapterMap {
    map: parking_lot::Mutex<std::collections::HashMap<AdapterKey, Arc<CompiledMethod>>>,
}

impl LambdaAdapterMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many thunks this map holds right now.
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A weak handle on every thunk this map holds right now. For a test that
    /// drops the owning VM and checks no thunk outlived it.
    pub fn weak_thunks(&self) -> Vec<std::sync::Weak<CompiledMethod>> {
        self.map.lock().values().map(Arc::downgrade).collect()
    }
}

/// How many SAM arguments a thunk can carry when the lambda captures nothing.
///
/// The INCOMING side binds in that case: the thunk is entered with the context
/// (if any), the receiver, and every SAM argument in registers, and the outgoing
/// side is one register shorter. With captures the outgoing side can be the
/// wider of the two, so [`lambda_adapter_entry`] checks both.
pub fn max_sam_args(needs_context: bool) -> usize {
    ARG_REGS
        .len()
        .saturating_sub(1 + usize::from(needs_context))
}

/// Get, building on first use, the thunk that lets an inline cache dispatch
/// this proxy class's SAM call straight to `impl_owner`.
///
/// Returns the entry address to install in the MIC slot, or `None` when the
/// shape does not fit (too many arguments for the register ABI, or executable
/// memory could not be obtained).
///
/// `capture_descs` are the leading bytes of the impl's first parameters — the
/// captured values, in impl-parameter order — and `sam_args` counts the SAM's
/// own arguments, EXCLUDING the receiver.
///
/// Three things are refused here rather than at the call site, because each is a
/// question about what can be *emitted*:
///
/// * a capture descriptor with no load ([`CaptureLoad::from_descriptor`]);
/// * a proxy class with a registered compact layout, which would put the
///   captures somewhere other than where this emits (it never happens — see the
///   module note — and this is the ONE guard that keeps every emission legacy,
///   which is what the removed one below turned out to depend on);
/// * an arity the register file cannot carry on EITHER side. Both sides bind:
///   incoming is `context + receiver + samArgs`, outgoing is
///   `context + captures + samArgs`, and a thunk with no frame cannot build a
///   stack argument the impl would look for. Since JIT round 20 wave 19
///   (DISP20-2, `CRATONVM_JIT_LAMBDA_FRAMED_THUNK`) an outgoing side up to
///   eight words past the file gets a FRAMED thunk instead (see the module
///   note); the incoming side still has to fit.
///
/// # The fourth refusal, and why it is gone
///
/// A REFERENCE capture used to be refused whenever
/// `narrow_oops_block_inline_fields()` held — compressed oops on, or ZGC's read
/// barrier armed — by analogy with the inline `getfield` codegen, which refuses
/// under exactly that condition.
///
/// The analogy did not hold. That predicate guards the emission of a COMPACT
/// slot read: a compact reference field narrows to four bytes under compressed
/// oops, and it is the compact/array decode paths that ZGC's colouring reaches.
/// This emitter never emits one — the compact-layout refusal above is what
/// guarantees that — and a legacy 16-byte `Value` cell is neither narrowed nor
/// barriered. `narrow_oop::ref_field_size()` is documented as the width of a
/// *compact* instance field; ZGC applies `load_barrier_slot` in
/// `get_array_element` and not in `get_field`, whose legacy arm is a bare
/// `std::ptr::read::<Value>`.
///
/// So the refusal diverted a reference capture to a Rust arm that reads the
/// identical word in the identical way, at a cost of ~120 ns a call. It bought
/// nothing.
///
/// That is a claim about two other crates, so it is pinned executably rather
/// than argued here: `gc/tests/lambda_proxy_capture_word.rs` compares this
/// emitter's baked address and width against each collector's own `get_field`,
/// with compressed oops on and with the ZGC barrier armed. If either fact ever
/// changes, that file fails and names this function.
pub fn lambda_adapter_entry(
    adapters: &LambdaAdapterMap,
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
) -> Option<usize> {
    adapter_entry(
        adapters,
        proxy_class_id,
        impl_owner,
        capture_descs,
        sam_args,
        None,
        None,
    )
}

/// [`lambda_adapter_entry`] for a call site whose SAM call replays bridge
/// `checkcast`s (nio21 wave 18, lane L2; see "Cast thunks" above): the thunk
/// tests each guarded argument's class id against the one the VM proved
/// passes, and takes the site's own resolving helper on anything else.
///
/// Refused (`None`) for an empty guard list (that is the plain thunk), a guard
/// naming an argument the SAM does not have, a miss edge with no helper, VM,
/// site record or MIC, and everything [`lambda_adapter_entry`] refuses.
/// Megamorphic-table-safe variant of [`lambda_cast_adapter_entry`]: `mic_ptr`
/// and `pic_ptr` in `miss` must be `0` so a cast miss from another megamorphic
/// call site never mutates any site's MIC/PIC.
pub fn lambda_mega_cast_adapter_entry(
    adapters: &LambdaAdapterMap,
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
    guards: &[CastGuard],
    miss: CastMissSite,
) -> Option<usize> {
    if guards.is_empty()
        || guards.iter().any(|g| g.sam_index >= sam_args)
        || miss.helper == 0
        || miss.vm_ptr == 0
        || miss.info_ptr == 0
        || miss.mic_ptr != 0
        || miss.pic_ptr != 0
    {
        return None;
    }
    adapter_entry(
        adapters,
        proxy_class_id,
        impl_owner,
        capture_descs,
        sam_args,
        Some((guards.to_vec(), miss)),
        None,
    )
}

pub fn lambda_cast_adapter_entry(
    adapters: &LambdaAdapterMap,
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
    guards: &[CastGuard],
    miss: CastMissSite,
) -> Option<usize> {
    if guards.is_empty()
        || guards.iter().any(|g| g.sam_index >= sam_args)
        || miss.helper == 0
        || miss.vm_ptr == 0
        || miss.info_ptr == 0
        || miss.mic_ptr == 0
    {
        return None;
    }
    adapter_entry(
        adapters,
        proxy_class_id,
        impl_owner,
        capture_descs,
        sam_args,
        Some((guards.to_vec(), miss)),
        None,
    )
}

/// JIT round 20 wave 12, lane qLam (DISP20-3; see "Receiver thunks" above):
/// the thunk for an INSTANCE-method impl, resolved by the VM against the
/// receiver's exact class `receiver.class_id`. Per call site (its miss edge
/// is the site's helper). `guards` are the site's replayed bridge casts, if
/// any (may be empty).
///
/// Refused (`None`) for a receiver source the shape cannot have (a bound
/// receiver needs a reference capture 0; an unbound one needs no captures and
/// at least one SAM argument), a cast guard past the SAM's arity, a miss edge
/// with no helper, VM, site record or MIC, and everything
/// [`lambda_adapter_entry`] refuses.
pub fn lambda_receiver_adapter_entry(
    adapters: &LambdaAdapterMap,
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
    receiver: ReceiverGuard,
    guards: &[CastGuard],
    miss: CastMissSite,
) -> Option<usize> {
    let shape_ok = match receiver.source {
        ReceiverSource::Capture0 => capture_descs.first() == Some(&b'L'),
        ReceiverSource::SamArg0 => capture_descs.is_empty() && sam_args >= 1,
    };
    if !shape_ok
        || guards.iter().any(|g| g.sam_index >= sam_args)
        || miss.helper == 0
        || miss.vm_ptr == 0
        || miss.info_ptr == 0
        || miss.mic_ptr == 0
    {
        return None;
    }
    adapter_entry(
        adapters,
        proxy_class_id,
        impl_owner,
        capture_descs,
        sam_args,
        Some((guards.to_vec(), miss)),
        Some(receiver),
    )
}

/// The shared body of [`lambda_adapter_entry`],
/// [`lambda_cast_adapter_entry`] and [`lambda_receiver_adapter_entry`].
fn adapter_entry(
    adapters: &LambdaAdapterMap,
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
    cast: Option<(Vec<CastGuard>, CastMissSite)>,
    receiver: Option<ReceiverGuard>,
) -> Option<usize> {
    // A RETIRED impl is refused outright. A thunk built for one would sit in
    // the adapter map jumping into withdrawn code, and the MIC/PIC installers'
    // own retired check looks at the THUNK's owner (never retired), not at
    // the impl behind it. See the re-check before the insert below for the
    // window this leaves and what closes it.
    if impl_owner
        .retired
        .load(std::sync::atomic::Ordering::Acquire)
        // JIT r25 wave 10 (W10-5): nor a SUPERSEDED one, whose thunk would
        // keep entering the displaced body after the supersede sweep.
        || w10_5_supersede::refuses_impl(impl_owner)
    {
        return None;
    }
    let impl_entry = impl_owner.entry_ptr() as usize;
    let needs_context = impl_owner.needs_context();
    let leading = usize::from(needs_context);
    let mut captures = Vec::with_capacity(capture_descs.len());
    for &tok in capture_descs {
        captures.push(CaptureLoad::from_descriptor(tok)?);
    }
    if !captures.is_empty()
        && cratonvm_types::class_layout_for_fields(proxy_class_id, captures.len() as u32) // Cast: field count
            .is_some()
    {
        return None;
    }
    let incoming = leading + 1 + sam_args;
    let outgoing = leading + captures.len() + sam_args;
    // DISP20-2: an over-budget OUTGOING side is a framed thunk under its
    // switch; the incoming side still has to fit (the call site's own stack
    // words would sit under the caller's return address).
    let framed = adapter_arity(incoming, outgoing, lambda_framed_thunk_enabled)?;
    // The KEY must name every input the BODY depends on.
    //
    // `emit_adapter` is a function of `(leading, captures, sam_args,
    // impl_entry)`. `leading` and `captures` follow from the impl, but
    // **`sam_args` does not** — it is the SAM's own arity, a property of the
    // CALL SITE's functional interface, and the register slide is emitted for
    // exactly that many arguments. Keying on `(proxy_class_id, impl_entry)`
    // alone therefore returns a thunk built for a DIFFERENT arity whenever one
    // impl is reached through two functional interfaces, or whenever two
    // proxies share a class id. The impl then reads an argument register the
    // caller never wrote.
    //
    // `lambda_adapter_shape_collisions()` counts exactly the reuses the old key
    // would have made and this one refuses, so the change cannot go silently
    // inert: it reads 0 on any run that never had the hazard.
    let key = AdapterKey {
        proxy_class_id,
        impl_entry,
        leading,
        sam_args,
        captures: captures.clone(),
        cast,
        receiver,
    };
    let mut map = adapters.map.lock();
    if let Some(existing) = map.get(&key) {
        return Some(existing.entry_ptr() as usize);
    }
    // Cast thunks are per call site by construction (their miss edge names
    // the site), so several of them for one `(proxy, impl)` are not a shape
    // collision; only plain thunks are compared.
    if key.cast.is_none()
        && map.keys().any(|k| {
            k.cast.is_none() && k.proxy_class_id == proxy_class_id && k.impl_entry == impl_entry
        })
    {
        SHAPE_COLLISIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let code = match (&key.cast, &key.receiver) {
        (None, _) => emit_adapter(leading, &captures, sam_args, impl_entry),
        (Some((guards, miss)), None) => {
            emit_cast_adapter(leading, &captures, sam_args, impl_entry, guards, miss)
        }
        (Some((guards, miss)), Some(receiver)) => emit_receiver_adapter(
            leading, &captures, sam_args, impl_entry, receiver, guards, miss,
        ),
    };
    // `new_in`, not `new`: the process-wide JIT code arena carves this body
    // out of a shared executable reservation instead of taking its own
    // `VirtualAlloc`/`mmap`. Behaviour is byte-identical while
    // `CRATONVM_JIT_CODE_ARENA` is unset, because `new_in` falls straight
    // through to `new` then — so this call site is what makes the flag
    // reachable, not what changes the default.
    let mut buffer =
        ExecutableBuffer::new_in(crate::platform::shared_jit_code_arena(), code.len().max(64))?;
    buffer.set_tag("lambda-adapter");
    buffer.emit(&code);
    if buffer.overflowed() {
        return None;
    }
    // `try_new`: a refused RW->RX flip declines the adapter (the caller keeps
    // the interpreter path) instead of aborting the process.
    let mut cm = CompiledMethod::try_new(buffer)?;
    // The thunk's ABI is the impl's: the caller must supply the context
    // register exactly when the impl wants one, because the thunk passes it
    // through untouched.
    cm.needs_context = needs_context;
    // Name the impl as a baked direct callee — the same declaration a JIT'd
    // caller makes for a call it bakes. `_direct_callee_roots` is what keeps
    // the impl's buffer alive for as long as this thunk can jump into it, and
    // `_direct_callee_entries` is what lets the cache's invalidation closure
    // reach the thunk through `adapters_reaching`.
    cm._direct_callee_entries.push(impl_entry);
    // This path has always captured the owner at bake time -- exactly what
    // the method paths were missing. Record its identity too, so
    // `prepare_for_publication` checks an adapter the same way.
    cm._direct_callee_expected
        .push((impl_entry, impl_owner.artifact_id));
    cm._direct_callee_roots
        .push(crate::RetainedCode::new(Arc::clone(impl_owner)));
    let symbol = if key.receiver.is_some() {
        format!("lambda-receiver-adapter->{impl_entry:#x}")
    } else if key.cast.is_some() {
        format!("lambda-cast-adapter->{impl_entry:#x}")
    } else {
        format!("lambda-adapter->{impl_entry:#x}")
    };
    // DISP20-2: the bytes a framed thunk registers as its JIT code range --
    // everything up to a cast / receiver thunk's miss stub, whose own CALL of
    // the Rust helper keeps the unregistered shape it always had. Guards hold
    // no call, so the only return address in the range is the impl CALL's.
    let framed_range = framed.then(|| match &key.cast {
        None => code.len(),
        Some((_, miss)) => {
            let mut stub = Vec::new();
            emit_cast_miss_stub(&mut stub, leading, 1 + sam_args, miss);
            code.len() - stub.len()
        }
    });
    if framed {
        // The frame contracts every compiled-frame walker already honours
        // (see "A FRAMED thunk" in the module note): an EMPTY label (the
        // stack-trace walk drops the activation), no reference in its band, a
        // bounded band, no safepoint-id slot (`sp_id_slot_off` stays 0).
        cm.method_label = String::new();
        cm.frame_holds_no_references = true;
        // Cast: at most 96 bytes.
        cm.osr_frame_size = framed_frame_bytes(outgoing - ARG_REGS.len()) as i32;
    } else {
        cm.method_label = symbol.clone();
    }
    let arc = Arc::new(cm);
    let entry = arc.entry_ptr() as usize;
    // JVMTI `DynamicCodeGenerated` (interpreter round i2 wave 63, lane L1).
    crate::code_events::publish_stub(entry, arc.code_len(), "lambda-adapter");
    // Profiler/debugger symbols (perf map, jitdump, GDB); no-op unless enabled.
    crate::code_events::publish(entry, arc.code_len(), || {
        crate::code_events::with_tier_suffix(
            &symbol,
            crate::code_events::CodeTier::Stub("lambda-adapter"),
        )
    });
    crate::register_jit_entry_owner_for_adapter(entry, &arc);
    // Re-check while still holding the map lock, immediately before the
    // insert makes the thunk findable: a thunk for an impl ALREADY marked
    // retired is one no later sweep is guaranteed to find, so it is withdrawn
    // here exactly as `forget_adapters` would — marked retired (refused by
    // every installer and by `prepare_for_publication`) and handed to the
    // retirement queue rather than dropped on the spot.
    //
    // This NARROWS the race and does not close it on its own:
    // `JitCache::invalidate_matching_collecting` runs `adapters_reaching` /
    // `forget_adapters` BEFORE it marks the doomed bodies retired, so a thunk
    // built between that sweep and the mark passes both checks here and is
    // never swept. The closing half is a second `adapters_reaching` sweep
    // AFTER the mark — `NOTES-invoke.md` cross-lane request X3 and
    // `lambda-adapter-built-for-retiring-impl-20260918.md`. With it, this
    // check is what makes the pair sound: the mark is visible here (the
    // second sweep's lock acquire orders it), or the sweep runs after this
    // lock is released and finds the inserted thunk.
    // W10-5: the same pairing for the supersede sweep
    // (`JitCache::retarget_superseded_lambda_adapters` takes this map's lock
    // after the `superseded` mark).
    if impl_owner.retired.load(std::sync::atomic::Ordering::SeqCst)
        || w10_5_supersede::refuses_impl(impl_owner)
    {
        arc.retired.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::defer_jit_owner(Some(arc));
        return None;
    }
    // DISP20-2: a framed thunk's return address must resolve to it before any
    // inline cache can hold the entry (the caller installs only after this
    // returns). Unconditional, unlike a method body's gate: without the range
    // every walk would stop at the thunk and lose the lambda's caller. Withdrawn
    // by `CompiledMethod::drop` (keyed by the buffer's entry, which is where
    // this range starts).
    if let Some(len) = framed_range {
        crate::register_jit_code_range_owned(entry, len, &arc);
    }
    map.insert(key, arc);
    Some(entry)
}

/// Every thunk entry that jumps into one of `targets`.
///
/// The JIT cache's invalidation pass already promotes any compiled method whose
/// baked direct callee is being removed, but it walks the cache's own maps and
/// a thunk is not in them. This is how it reaches thunks: the returned entries
/// join the removal set, and the MIC/PIC sweep then clears every slot holding
/// one.
pub fn adapters_reaching(adapters: &LambdaAdapterMap, targets: &HashSet<usize>) -> Vec<usize> {
    adapters
        .map
        .lock()
        .values()
        .filter(|cm| {
            cm._direct_callee_entries
                .iter()
                .any(|entry| targets.contains(entry))
        })
        .map(|cm| cm.entry_ptr() as usize)
        .collect()
}

/// Drop thunks whose entries are being invalidated, so a later call site does
/// not resurrect one that names evicted code.
///
/// A forgotten thunk is marked `retired` and handed to the retirement queue,
/// the same path a method body takes. Marked, it is refused by
/// `prepare_for_publication` and by the MIC/PIC installers, so a caller
/// compiled after this point cannot bake it. Queued, its mapping is freed only
/// once no thread can still be executing it, instead of on the spot when the
/// map's `Arc` was the last one.
pub fn forget_adapters(adapters: &LambdaAdapterMap, entries: &HashSet<usize>) {
    let mut forgotten = Vec::new();
    adapters.map.lock().retain(|_, cm| {
        let keep = !entries.contains(&(cm.entry_ptr() as usize));
        if !keep {
            forgotten.push(Arc::clone(cm));
        }
        keep
    });
    for cm in forgotten {
        cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::defer_jit_owner(Some(cm));
    }
}



/// JIT r25 wave 10, lane W10-5 (`CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE`,
/// default ON, `=0` kill): the thunk map's half of the supersede of an
/// implementation body. See `JitCache::retarget_superseded_lambda_adapters`
/// (`lib.rs`, `r25_w10_5_adapter_supersede`) for the whole fix.
pub mod w10_5_supersede {
    use super::*;

    /// `CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE` (default ON, `=0` restores the
    /// thunk that keeps entering a superseded body). Read per supersede and
    /// per thunk build, never on a call path; no `static`.
    pub fn lambda_adapter_supersede_enabled() -> bool {
        true    }

    /// Does [`super::adapter_entry`] refuse `impl_owner` as superseded? It is
    /// asked twice, as for a retired body: before the build, and again under
    /// the map lock right before the insert, which orders it against
    /// [`take_adapters_into`]'s sweep. That sweep runs after the `superseded`
    /// mark, so either this check sees the mark or the sweep finds the
    /// inserted thunk.
    pub(crate) fn refuses_impl(impl_owner: &CompiledMethod) -> bool {
        impl_owner
            .superseded
            .load(std::sync::atomic::Ordering::SeqCst)
            && lambda_adapter_supersede_enabled()
    }

    /// Take out of the thunk map every thunk built for `old`: its recorded
    /// `(entry, artifact id)` names `old`, so a later body the allocator
    /// placed at the same address is never matched. The caller withdraws them.
    pub fn take_adapters_into(
        adapters: &LambdaAdapterMap,
        old: &CompiledMethod,
    ) -> Vec<Arc<CompiledMethod>> {
        let entry = old.entry_ptr() as usize;
        let id = old.artifact_id;
        let mut taken = Vec::new();
        adapters.map.lock().retain(|_, cm| {
            let hit = cm
                ._direct_callee_expected
                .iter()
                .any(|&(e, a)| e == entry && a == id);
            if hit {
                taken.push(Arc::clone(cm));
            }
            !hit
        });
        taken
    }
}
