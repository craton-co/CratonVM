// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A lambda receiver the monomorphic inline cache CAN hold.
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
//! Three properties make the thunk safe to be this small:
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
//! * **It touches no memory.** Registers only, which is what confines it to
//!   NON-CAPTURING lambdas: reading a captured field from generated code would
//!   mean replicating the compact/legacy body-layout branch
//!   (`GC_FLAG_COMPACT`) and every per-type width the `getfield` arms handle.
//!   A capturing lambda keeps the Rust fast path, which reads its captures
//!   through the heap API like everything else.
//! * **The return value is the impl's.** RAX flows straight through, and the
//!   caller's post-call sequence — the `i64::MIN` callee-deopt check, the
//!   pending-exception check — runs exactly as it does after any other cached
//!   call.
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

/// The thunk body for a non-capturing lambda.
///
/// `leading` is the number of incoming registers that pass through untouched:
/// 1 when the impl takes the hidden VM context in ARG0, 0 otherwise. The
/// receiver sits immediately after them and is dropped; every SAM argument
/// after it slides down one register.
///
/// The slide runs LOW to HIGH deliberately: each step reads the register the
/// next step will write, so no argument is clobbered before it is moved and no
/// scratch register is needed.
fn emit_adapter(leading: usize, sam_args: usize, impl_entry: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + sam_args * 3 + 13);
    for j in 0..sam_args {
        let dst = ARG_REGS[leading + j];
        let src = ARG_REGS[leading + j + 1];
        emit_mov_reg_reg(&mut out, dst, src);
    }
    emit_tail_jump(&mut out, impl_entry);
    out
}

/// Live thunks, keyed by the (proxy class, impl entry) pair they were built
/// for. Held strongly: a thunk is reachable from an inline-cache slot that this
/// map cannot see, so its lifetime is the process's, matching how the JIT
/// retains compiled code by default.
fn adapters() -> &'static parking_lot::Mutex<
    std::collections::HashMap<(u32, usize), Arc<CompiledMethod>>,
> {
    static ADAPTERS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<(u32, usize), Arc<CompiledMethod>>>,
    > = std::sync::OnceLock::new();
    ADAPTERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// How many SAM arguments a thunk can carry, given the ABI's register count and
/// whether a hidden context register is in play.
///
/// The INCOMING side binds: the thunk is entered with the context (if any), the
/// receiver, and every SAM argument in registers.
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
/// `sam_args` EXCLUDES the receiver. Capturing lambdas must not be passed here
/// — see the module note on why the thunk touches no memory.
pub fn lambda_adapter_entry(
    proxy_class_id: u32,
    impl_owner: &Arc<CompiledMethod>,
    sam_args: usize,
) -> Option<usize> {
    let impl_entry = impl_owner.entry_ptr() as usize;
    let needs_context = impl_owner.needs_context();
    if sam_args > max_sam_args(needs_context) {
        return None;
    }
    let key = (proxy_class_id, impl_entry);
    let mut map = adapters().lock();
    if let Some(existing) = map.get(&key) {
        return Some(existing.entry_ptr() as usize);
    }
    let code = emit_adapter(usize::from(needs_context), sam_args, impl_entry);
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
        let code = emit_adapter(0, 1, 0x1234_5678_9ABC_DEF0);
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
        let code = emit_adapter(1, 1, 0);
        assert_eq!(&code[..3], &[0x48, 0x89, 0xD6], "mov rsi, rdx");
    }

    /// A two-argument SAM slides both, low to high, so neither is clobbered
    /// before it is read.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn two_args_slide_in_order() {
        let code = emit_adapter(0, 2, 0);
        assert_eq!(&code[..3], &[0x48, 0x89, 0xF7], "mov rdi, rsi");
        assert_eq!(&code[3..6], &[0x48, 0x89, 0xD6], "mov rsi, rdx");
    }

    /// A zero-argument SAM (`Runnable.run`) is pure tail jump: the receiver is
    /// simply not passed on.
    #[test]
    fn zero_args_is_only_a_tail_jump() {
        let code = emit_adapter(0, 0, 0xDEAD_BEEF);
        assert_eq!(code.len(), 13, "mov r11, imm64 (10) + jmp r11 (3)");
    }

    /// The register budget is the INCOMING side: receiver plus arguments plus
    /// the optional context must all arrive in registers.
    #[test]
    fn arg_budget_counts_the_receiver_and_the_context() {
        assert_eq!(max_sam_args(false), ARG_REGS.len() - 1);
        assert_eq!(max_sam_args(true), ARG_REGS.len() - 2);
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
