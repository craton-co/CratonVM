// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A lambda receiver the monomorphic inline cache CAN hold, capturing or not.
//!
//! # Why the inline cache could not hold one before
//!
//! `jit_invoke_virtual_mic`'s emitted cascade is the reason an ordinary
//! `invokeinterface` costs ~12 ns in this VM: it compares the receiver's class
//! id against the slot's, loads `cached_entry_ptr`, and CALLs it with the
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

/// Everything `emit_adapter` reads. See the note at the `map.get` in
/// [`lambda_adapter_entry`] for why the arity has to be in here.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AdapterKey {
    proxy_class_id: u32,
    impl_entry: usize,
    leading: usize,
    sam_args: usize,
    captures: Vec<CaptureLoad>,
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

/// Live thunks, keyed by every input their body depends on. Held strongly: a
/// thunk is reachable from an inline-cache slot that this map cannot see, so
/// its lifetime is the process's, matching how the JIT retains compiled code
/// by default.
fn adapters(
) -> &'static parking_lot::Mutex<std::collections::HashMap<AdapterKey, Arc<CompiledMethod>>> {
    static ADAPTERS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<AdapterKey, Arc<CompiledMethod>>>,
    > = std::sync::OnceLock::new();
    ADAPTERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
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
///   stack argument the impl would look for.
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
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    capture_descs: &[u8],
    sam_args: usize,
) -> Option<usize> {
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
    if incoming.max(outgoing) > ARG_REGS.len() {
        return None;
    }
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
    };
    let mut map = adapters().lock();
    if let Some(existing) = map.get(&key) {
        return Some(existing.entry_ptr() as usize);
    }
    if map
        .keys()
        .any(|k| k.proxy_class_id == proxy_class_id && k.impl_entry == impl_entry)
    {
        SHAPE_COLLISIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let code = emit_adapter(leading, &captures, sam_args, impl_entry);
    let mut buffer = ExecutableBuffer::new(code.len().max(64))?;
    buffer.set_tag("lambda-adapter");
    buffer.emit(&code);
    if buffer.overflowed() {
        return None;
    }
    let mut cm = CompiledMethod::new(buffer);
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
    cm._direct_callee_roots.push(Arc::clone(impl_owner));
    cm.method_label = format!("lambda-adapter->{impl_entry:#x}");
    let arc = Arc::new(cm);
    let entry = arc.entry_ptr() as usize;
    crate::register_jit_entry_owner_for_adapter(entry, &arc);
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
pub fn adapters_reaching(targets: &HashSet<usize>) -> Vec<usize> {
    adapters()
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
pub fn forget_adapters(entries: &HashSet<usize>) {
    adapters()
        .lock()
        .retain(|_, cm| !entries.contains(&(cm.entry_ptr() as usize)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shuffle is the whole thunk, so its encoding is worth pinning
    /// byte-for-byte. Without a context register on SysV, a one-argument SAM
    /// becomes `mov rdi, rsi` (48 89 F7).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn one_arg_no_context_moves_rsi_into_rdi() {
        let code = emit_adapter(0, &[], 1, 0x1234_5678_9ABC_DEF0);
        assert_eq!(&code[..3], &[0x48, 0x89, 0xF7], "mov rdi, rsi");
        assert_eq!(&code[3..5], &[0x49, 0xBB], "mov r11, imm64");
        assert_eq!(
            &code[5..13],
            &0x1234_5678_9ABC_DEF0u64.to_le_bytes(),
            "the impl entry address"
        );
        assert_eq!(&code[13..], &[0x41, 0xFF, 0xE3], "jmp r11");
    }

    /// With a context register the context stays in ARG0 and the slide starts
    /// one register later: `mov rsi, rdx`.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn one_arg_with_context_keeps_arg0_and_slides_from_arg2() {
        let code = emit_adapter(1, &[], 1, 0);
        assert_eq!(&code[..3], &[0x48, 0x89, 0xD6], "mov rsi, rdx");
    }

    /// A two-argument SAM slides both, low to high, so neither is clobbered
    /// before it is read.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn two_args_slide_in_order() {
        let code = emit_adapter(0, &[], 2, 0);
        assert_eq!(&code[..3], &[0x48, 0x89, 0xF7], "mov rdi, rsi");
        assert_eq!(&code[3..6], &[0x48, 0x89, 0xD6], "mov rsi, rdx");
    }

    /// A zero-argument SAM (`Runnable.run`) is pure tail jump: the receiver is
    /// simply not passed on.
    #[test]
    fn zero_args_is_only_a_tail_jump() {
        let code = emit_adapter(0, &[], 0, 0xDEAD_BEEF);
        assert_eq!(code.len(), 13, "mov r11, imm64 (10) + jmp r11 (3)");
    }

    /// The register budget is the INCOMING side: receiver plus arguments plus
    /// the optional context must all arrive in registers.
    #[test]
    fn arg_budget_counts_the_receiver_and_the_context() {
        assert_eq!(max_sam_args(false), ARG_REGS.len() - 1);
        assert_eq!(max_sam_args(true), ARG_REGS.len() - 2);
    }

    /// One `int` capture and one SAM argument — the `(int n) -> x + n` shape.
    /// The SAM argument must NOT move (one capture replaces one receiver), and
    /// the capture is a sign-extending 4-byte load from the first cell's
    /// 32-bit payload.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn one_int_capture_loads_the_cell_and_leaves_the_sam_arg_alone() {
        let code = emit_adapter(0, &[CaptureLoad::Int], 1, 0);
        assert_eq!(
            &code[..3],
            &[0x49, 0x89, 0xFB],
            "mov r11, rdi (save the proxy)"
        );
        // MOVSXD rdi, [r11 + HEADER_SIZE + PAYLOAD32] = 16 + 4 = 20.
        assert_eq!(&code[3..6], &[0x49, 0x63, 0xBB], "movsxd rdi, [r11+disp32]");
        assert_eq!(&code[6..10], &20i32.to_le_bytes());
        assert_eq!(code.len(), 3 + 7 + 13, "no slide for a single capture");
    }

    /// Two captures push the SAM argument UP a register, and the slide must run
    /// high-to-low so the argument is read before the capture overwrites it.
    /// The second capture's cell is one `SLOT_SIZE` further along.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn two_captures_slide_the_sam_arg_up_before_loading() {
        let code = emit_adapter(0, &[CaptureLoad::Int, CaptureLoad::Int], 1, 0);
        assert_eq!(&code[..3], &[0x49, 0x89, 0xFB], "mov r11, rdi");
        assert_eq!(
            &code[3..6],
            &[0x48, 0x89, 0xF2],
            "mov rdx, rsi — the SAM arg moves first"
        );
        assert_eq!(&code[6..9], &[0x49, 0x63, 0xBB], "movsxd rdi, [r11+20]");
        assert_eq!(&code[9..13], &20i32.to_le_bytes());
        assert_eq!(&code[13..16], &[0x49, 0x63, 0xB3], "movsxd rsi, [r11+36]");
        assert_eq!(&code[16..20], &36i32.to_le_bytes());
    }

    /// A reference capture is the cell's 64-bit payload, 8 bytes past the cell
    /// base — never the 32-bit load, which would sign-extend half a pointer.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn a_reference_capture_reads_the_64_bit_payload() {
        let code = emit_adapter(0, &[CaptureLoad::Reference], 0, 0);
        assert_eq!(&code[3..6], &[0x49, 0x8B, 0xBB], "mov rdi, [r11+disp32]");
        assert_eq!(
            &code[6..10],
            &24i32.to_le_bytes(),
            "HEADER_SIZE + PAYLOAD64"
        );
    }

    /// A `float` capture is a 4-byte load into the 32-bit register, which
    /// zero-extends — matching the `f32::to_bits() as i64` the JIT ABI carries.
    /// No REX.W, so no `0x48`.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn a_float_capture_zero_extends() {
        let code = emit_adapter(0, &[CaptureLoad::Float], 0, 0);
        assert_eq!(&code[3..6], &[0x41, 0x8B, 0xBB], "mov edi, [r11+disp32]");
        assert_eq!(&code[6..10], &20i32.to_le_bytes());
    }

    /// The three capture loads, encoded against a fixed register pair so the
    /// assertion holds on both ABIs — the SysV shuffle tests above cannot run
    /// on Windows, and these encodings are the half that has nothing to do with
    /// which registers the platform passes arguments in.
    ///
    /// `RDI` (7) as the destination and `R11` (11) as the base, which is what
    /// the emitter actually uses for a base.
    #[test]
    fn the_three_loads_encode_the_same_on_both_abis() {
        let mut out = Vec::new();
        emit_load_mem(&mut out, 7, R11, 0x18, CaptureLoad::Reference);
        assert_eq!(
            out[..3],
            [0x49, 0x8B, 0xBB],
            "REX.WB + MOV r64,m64 + mod=10"
        );
        assert_eq!(out[3..], 0x18i32.to_le_bytes(), "disp32, always");

        out.clear();
        emit_load_mem(&mut out, 7, R11, 0x14, CaptureLoad::Wide);
        assert_eq!(
            out[..3],
            [0x49, 0x8B, 0xBB],
            "a Wide is the same 8-byte load"
        );

        out.clear();
        emit_load_mem(&mut out, 7, R11, 0x14, CaptureLoad::Float);
        assert_eq!(
            out[..3],
            [0x41, 0x8B, 0xBB],
            "no REX.W: a 32-bit destination zero-extends, which is the float's \
             `to_bits() as i64`"
        );

        out.clear();
        emit_load_mem(&mut out, 7, R11, 0x14, CaptureLoad::Int);
        assert_eq!(
            out[..3],
            [0x49, 0x63, 0xBB],
            "MOVSXD — the int category sign-extends"
        );
    }

    /// Where each capture's payload sits: uniform 16-byte cells from
    /// `HEADER_SIZE`, 8 bytes in for the wide payload and 4 for the narrow one.
    /// These are the constants that stand in for the compact/legacy branch a
    /// `getfield` would need, so they are worth pinning against the types crate
    /// rather than against themselves.
    #[test]
    fn capture_offsets_are_uniform_cells_from_the_header() {
        use cratonvm_types::{
            FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
        };
        assert_eq!(
            capture_payload_offset(0, CaptureLoad::Reference),
            (HEADER_SIZE + FIELD_CELL_PAYLOAD64_OFFSET) as i32
        );
        assert_eq!(
            capture_payload_offset(0, CaptureLoad::Int),
            (HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET) as i32
        );
        assert_eq!(
            capture_payload_offset(3, CaptureLoad::Wide),
            (HEADER_SIZE + 3 * SLOT_SIZE + FIELD_CELL_PAYLOAD64_OFFSET) as i32
        );
        // A `float` and an `int` share the narrow payload; a `long` and a
        // reference share the wide one. Only the load differs.
        assert_eq!(
            capture_payload_offset(2, CaptureLoad::Float),
            capture_payload_offset(2, CaptureLoad::Int)
        );
        assert_eq!(
            capture_payload_offset(2, CaptureLoad::Wide),
            capture_payload_offset(2, CaptureLoad::Reference)
        );
    }

    /// The receiver is saved to `R11` before anything overwrites `ARG0`, and
    /// `R11` is never one of the registers the shuffle writes — otherwise the
    /// captures would be read through a base that a previous capture clobbered.
    /// True on both ABIs, and the reason the thunk needs no second scratch.
    #[test]
    fn r11_is_not_an_argument_register() {
        assert!(!ARG_REGS.contains(&R11));
    }

    /// A stand-in implementation to build thunks against. Its code is never
    /// executed here — only its entry address and ABI are read.
    fn dummy_impl() -> Arc<CompiledMethod> {
        let mut buffer = ExecutableBuffer::new(64).expect("executable memory");
        buffer.emit(&[0xC3]); // RET
        Arc::new(CompiledMethod::new(buffer))
    }

    /// **Compressed oops must NOT stop a reference capture being thunked.**
    ///
    /// This emitter used to refuse one whenever
    /// `narrow_oops_block_inline_fields()` held, by analogy with the inline
    /// `getfield` codegen. The analogy was wrong: that predicate guards a
    /// COMPACT slot read, and a lambda proxy's capture is always a legacy
    /// 16-byte `Value` cell, which compressed oops do not narrow. See
    /// `gc/tests/lambda_proxy_capture_word.rs`, which pins that against every
    /// collector's own `get_field`.
    ///
    /// The refusal was inert in a default run — compressed oops is opt-in and
    /// ZGC's barrier never arms — so nothing but a test that turns the flag ON
    /// can tell the two behaviours apart. Which is exactly why one is here.
    #[test]
    fn a_reference_capture_is_still_served_under_compressed_oops() {
        let owner = dummy_impl();
        // A distinct proxy id per arm: thunks are cached by (proxy, impl), so
        // reusing one would answer the second arm from the first arm's entry.
        let base = 0x1_0000u64;
        assert!(
            cratonvm_types::narrow_oop::enable(base, 3),
            "could not turn compressed oops on — the assertion below would then \
             be testing the default configuration and proving nothing"
        );
        assert_eq!(
            cratonvm_types::narrow_oop::ref_field_size(),
            4,
            "compressed oops reported on but a compact reference field is still \
             8 bytes, so the condition under test is not in force"
        );
        let armed = lambda_adapter_entry(0x8000_0101, &owner, b"L", 1);
        cratonvm_types::narrow_oop::disable_for_test();

        assert!(
            armed.is_some(),
            "a REFERENCE capture was refused under compressed oops. That was the \
             old behaviour and it cost ~120 ns a call for nothing: the capture \
             lives in a legacy `Value` cell, which is not narrowed, and the Rust \
             arm it was diverted to reads the identical word."
        );
        // And the primitive arm, which was never gated, still works — so a
        // failure above cannot be a general breakage of the emitter.
        assert!(lambda_adapter_entry(0x8000_0102, &owner, b"I", 1).is_some());
    }

    /// `long`/`double` take the same 8-byte payload a reference does, and
    /// `V`/junk map to no load at all.
    #[test]
    fn descriptor_bytes_map_to_the_three_loads() {
        assert_eq!(CaptureLoad::from_descriptor(b'J'), Some(CaptureLoad::Wide));
        assert_eq!(CaptureLoad::from_descriptor(b'D'), Some(CaptureLoad::Wide));
        assert_eq!(
            CaptureLoad::from_descriptor(b'['),
            Some(CaptureLoad::Reference)
        );
        assert_eq!(CaptureLoad::from_descriptor(b'Z'), Some(CaptureLoad::Int));
        assert_eq!(CaptureLoad::from_descriptor(b'V'), None);
        assert_eq!(CaptureLoad::from_descriptor(b'?'), None);
        // Only a Reference is a raw pointer read, so only it answers to the
        // read-barrier gate.
        assert!(CaptureLoad::Reference.is_reference());
        assert!(!CaptureLoad::Wide.is_reference());
    }

    /// With a context register in ARG0 the captures start at ARG1, and the
    /// proxy is read from ARG1 rather than ARG0.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn a_context_register_shifts_the_whole_shuffle_along() {
        let code = emit_adapter(1, &[CaptureLoad::Int], 1, 0);
        assert_eq!(&code[..3], &[0x49, 0x89, 0xF3], "mov r11, rsi");
        assert_eq!(&code[3..6], &[0x49, 0x63, 0xB3], "movsxd rsi, [r11+20]");
    }

    /// REX prefixes for the extended registers, which a three- or
    /// four-argument SAM reaches on SysV (`mov rcx, r8` needs REX.R).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn extended_registers_get_their_rex_bits() {
        let mut out = Vec::new();
        emit_mov_reg_reg(&mut out, 1, 8); // mov rcx, r8
        assert_eq!(out, vec![0x4C, 0x89, 0xC1]);
        out.clear();
        emit_mov_reg_reg(&mut out, 8, 1); // mov r8, rcx
        assert_eq!(out, vec![0x49, 0x89, 0xC8]);
    }
}
