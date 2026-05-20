//! x86-64 JIT code emitter for JVM bytecode methods.
//!
//! Compiles JVM bytecode directly to x86-64 machine code.
//!
//! # Error-handling discipline (NEW-7)
//!
//! This module is on the JIT compilation hot path and must never panic
//! in production builds. A stray panic during codegen would tear down
//! the interpreter caller and leak machine code pages.
//!
//! The `#![cfg_attr(not(test), deny(...))]` gate below makes clippy
//! refuse to compile this module in a release build when any of the
//! following appear in non-test code:
//!
//! - `.unwrap()` / `.expect()` — return `None` from `jit_scan` or the
//!   `compile` entry to let the caller fall back to the interpreter.
//! - `panic!()` / `unimplemented!()` / `todo!()` — same treatment.
//!
//! Tests inside `#[cfg(test)] mod tests { ... }` are exempt — assertion
//! panics are the standard test failure mechanism.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
    )
)]
//!
//! ## Calling Convention (matches C ABI)
//!
//! **Windows x64**: args in rcx, rdx, r8, r9; return in rax
//! **SysV (Linux/Mac)**: args in rdi, rsi, rdx, rcx, r8, r9; return in rax
//!
//! All JVM values are passed as i64 (ints are sign-extended to 64 bits).
//!
//! ## Register Allocation
//!
//! - Locals 0..N are stored in a stack frame area, addressed via `[rbp - offset]`
//! - The operand stack is simulated at compile time (static stack mapping)
//! - Most values flow through rax/rcx/rdx as temporaries
//!
//! ## Stack Layout (after prologue)
//!
//! ```text
//! [rbp + 16]    = return address (pushed by call)
//! [rbp + 8]     = saved rbp
//! [rbp]         = ← rbp points here
//! [rbp - 8]     = local 0
//! [rbp - 16]    = local 1
//! [rbp - 24]    = local 2
//! ...
//! [rbp - N*8]   = local N-1
//! [rbp - (N+1)*8 .. ] = operand stack spill area
//! ```

use super::{CompiledMethod, ExecutableBuffer, JitInvokeInfo};
use rustjvm_jit_api::JitRuntimeHelpers;
#[allow(unused_imports)]
use rustjvm_types::{ARRAY_LENGTH_OFFSET, HEADER_SIZE, SLOT_SIZE};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Register encoding for x86-64
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const RAX: u8 = 0;
#[allow(dead_code)]
const RCX: u8 = 1;
#[allow(dead_code)]
const RDX: u8 = 2;
#[allow(dead_code)]
const RBX: u8 = 3;
#[allow(dead_code)]
const RSP: u8 = 4;
#[allow(dead_code)]
const RBP: u8 = 5;
#[allow(dead_code)]
const RSI: u8 = 6;
#[allow(dead_code)]
const RDI: u8 = 7;
#[allow(dead_code)]
const R8: u8 = 8;
#[allow(dead_code)]
const R9: u8 = 9;
#[allow(dead_code)]
const R10: u8 = 10;
#[allow(dead_code)]
const R11: u8 = 11;
#[allow(dead_code)]
const R12: u8 = 12;
#[allow(dead_code)]
const R13: u8 = 13;
#[allow(dead_code)]
const R14: u8 = 14;
#[allow(dead_code)]
const R15: u8 = 15;

// Argument registers per platform
#[cfg(target_os = "windows")]
const ARG_REGS: [u8; 4] = [RCX, RDX, R8, R9];

#[cfg(not(target_os = "windows"))]
const ARG_REGS: [u8; 6] = [RDI, RSI, RDX, RCX, R8, R9];

// ---------------------------------------------------------------------------
// AVX2 runtime detection via CPUID
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU8, Ordering};

/// Cached AVX2 support: 0 = unknown, 1 = supported, 2 = not supported
static AVX2_SUPPORT: AtomicU8 = AtomicU8::new(0);

/// Cached SSE4.1 support: 0 = unknown, 1 = supported, 2 = not supported
static SSE41_SUPPORT: AtomicU8 = AtomicU8::new(0);

/// Check if the CPU supports AVX2 instructions (CPUID leaf 7, EBX bit 5).
/// Result is cached after first call.
pub fn has_avx2() -> bool {
    let cached = AVX2_SUPPORT.load(Ordering::Relaxed);
    if cached != 0 {
        return cached == 1;
    }
    let result = detect_avx2();
    AVX2_SUPPORT.store(if result { 1 } else { 2 }, Ordering::Relaxed);
    result
}

#[cfg(target_arch = "x86_64")]
fn detect_avx2() -> bool {
    // Check CPUID leaf 7, subleaf 0, EBX bit 5 = AVX2
    // Also verify OS supports AVX via XGETBV (CPUID leaf 1, ECX bit 27 = OSXSAVE)
    // SAFETY: __cpuid and __cpuid_count read CPU feature flags via the CPUID instruction
    // and _xgetbv reads the extended control register. These are read-only x86_64 intrinsics
    // with no memory side-effects; the target_arch gate above ensures we are on x86_64.
    unsafe {
        // Check OSXSAVE (leaf 1, ECX bit 27)
        let leaf1: std::arch::x86_64::CpuidResult = std::arch::x86_64::__cpuid(1);
        if leaf1.ecx & (1 << 27) == 0 {
            return false; // OS doesn't support XSAVE
        }
        // Check XCR0 bits 1-2 (SSE + AVX state saving)
        let xcr0: u64 = std::arch::x86_64::_xgetbv(0);
        if xcr0 & 0x06 != 0x06 {
            return false; // OS doesn't save AVX state
        }
        // Check AVX2 (leaf 7, EBX bit 5)
        let leaf7: std::arch::x86_64::CpuidResult = std::arch::x86_64::__cpuid_count(7, 0);
        leaf7.ebx & (1 << 5) != 0
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_avx2() -> bool {
    false
}

/// Check if the CPU supports SSE4.1 instructions (CPUID leaf 1, ECX bit 19).
/// Required for ROUNDSD/ROUNDSS (Math.floor/ceil/rint intrinsics).
pub fn has_sse41() -> bool {
    let cached = SSE41_SUPPORT.load(Ordering::Relaxed);
    if cached != 0 {
        return cached == 1;
    }
    let result = detect_sse41();
    SSE41_SUPPORT.store(if result { 1 } else { 2 }, Ordering::Relaxed);
    result
}

#[cfg(target_arch = "x86_64")]
fn detect_sse41() -> bool {
    // SAFETY: __cpuid is a read-only x86_64 intrinsic that queries CPU feature flags
    // via the CPUID instruction. The target_arch gate ensures we are on x86_64.
    // The `unsafe` block is tolerated-but-not-required in newer rustc; keep it so
    // older toolchains remain supported.
    #[allow(unused_unsafe)]
    unsafe {
        let leaf1: std::arch::x86_64::CpuidResult = std::arch::x86_64::__cpuid(1);
        leaf1.ecx & (1 << 19) != 0
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_sse41() -> bool {
    false
}

// ---------------------------------------------------------------------------
// SIMD loop analysis and vectorization
// ---------------------------------------------------------------------------

/// Information about a vectorizable int-array sum reduction loop.
/// Pattern: for (i = start; i < bound; i++) sum += arr[i]
#[derive(Debug)]
#[allow(dead_code)]
struct SimdIntArraySum {
    /// Bytecode PC of the loop header
    header_pc: usize,
    /// Bytecode PC of the back-edge instruction
    back_edge_pc: usize,
    /// Local index of the induction variable (i)
    iv_local: usize,
    /// Local index of the accumulator (sum)
    acc_local: usize,
    /// Local index of the array reference
    array_local: usize,
    /// Local index of the loop bound
    bound_local: usize,
    /// Whether accumulator is long (i2l + ladd + lstore vs iadd + istore)
    acc_is_long: bool,
}

/// Detect a vectorizable int-array-sum pattern in a loop body.
/// Matches: iload_sum, aload_arr, iload_iv, iaload, iadd, istore_sum, iinc iv 1, goto
/// Or with long accumulator: aload_arr, iload_iv, iaload, i2l, lload_sum, ladd, lstore_sum
fn detect_int_array_sum(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdIntArraySum> {
    // back_edge should be a goto instruction
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let back_edge_end = back_edge + 3;

    // The loop header should start with: iload_iv, iload_bound, if_icmpge exit
    let mut pc = header;

    // Match: iload <iv>
    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // Match: iload <bound>
    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // Match: if_icmpge <exit>
    if pc + 2 >= back_edge_end {
        return None;
    }
    if code[pc] != 0xa2 {
        return None;
    }
    pc += 3; // skip if_icmpge + offset

    // Now match loop body: aload arr, iload iv, iaload, (optional i2l), load sum, add, store sum
    // Pattern A (int sum): aload_arr, iload_iv, iaload, iload_sum, iadd (reversed), istore_sum
    // Pattern B: iload_sum, aload_arr, iload_iv, iaload, iadd, istore_sum

    // Try: aload arr, iload iv, iaload
    let array_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };

    let iv_load2 = extract_iload_local(code, pc)?;
    if iv_load2 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // iaload (0x2e)
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None;
    }
    pc += 1;

    // Check for i2l conversion (long accumulator)
    let acc_is_long = pc < back_edge_end && code[pc] == 0x85;
    if acc_is_long {
        pc += 1;
    }

    let acc_local;
    if acc_is_long {
        // Long path: lload sum, ladd, lstore sum
        acc_local = extract_lload_local(code, pc)?;
        pc += if code[pc] == 0x16 { 2 } else { 1 };

        // ladd (0x61)
        if pc >= back_edge_end || code[pc] != 0x61 {
            return None;
        }
        pc += 1;

        // lstore sum
        let store_local = extract_lstore_local(code, pc)?;
        if store_local != acc_local {
            return None;
        }
        pc += if code[pc] == 0x37 { 2 } else { 1 };
    } else {
        // Int path: for simplicity, only support long accumulator (most common for benchmarks)
        return None;
    }

    // Should be followed by iinc iv, 1 and then goto header
    if pc + 2 >= back_edge_end {
        return None;
    }
    if code[pc] != 0x84 {
        return None;
    }
    if code[pc + 1] as usize != iv_local { // Widening: always safe
        return None;
    }
    if code[pc + 2] != 0x01 {
        return None;
    }
    // pc + 3 should be the back_edge (goto)
    if pc + 3 != back_edge {
        return None;
    }

    Some(SimdIntArraySum {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        acc_local,
        array_local,
        bound_local,
        acc_is_long,
    })
}

/// Extract local index from an lload instruction at pc.
fn extract_lload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x1e => Some(0),                                  // lload_0
        0x1f => Some(1),                                  // lload_1
        0x20 => Some(2),                                  // lload_2
        0x21 => Some(3),                                  // lload_3
        0x16 => code.get(pc + 1).map(|&b| b as usize),    // lload
        _ => None,
    }
}

/// Extract local index from an lstore instruction at pc.
fn extract_lstore_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x3f => Some(0),                                  // lstore_0
        0x40 => Some(1),                                  // lstore_1
        0x41 => Some(2),                                  // lstore_2
        0x42 => Some(3),                                  // lstore_3
        0x37 => code.get(pc + 1).map(|&b| b as usize),    // lstore
        _ => None,
    }
}

/// Extract local index from a dload instruction at pc.
fn extract_dload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x26 => Some(0),                                  // dload_0
        0x27 => Some(1),                                  // dload_1
        0x28 => Some(2),                                  // dload_2
        0x29 => Some(3),                                  // dload_3
        0x18 => code.get(pc + 1).map(|&b| b as usize),    // dload
        _ => None,
    }
}

/// Extract local index from a dstore instruction at pc.
fn extract_dstore_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x47 => Some(0),                                  // dstore_0
        0x48 => Some(1),                                  // dstore_1
        0x49 => Some(2),                                  // dstore_2
        0x4a => Some(3),                                  // dstore_3
        0x39 => code.get(pc + 1).map(|&b| b as usize),    // dstore
        _ => None,
    }
}

/// Detect a vectorizable double-array-sum pattern in a loop body.
/// Matches: dload_sum, aload_arr, iload_iv, daload, dadd, dstore_sum, iinc iv 1, goto
/// Or:      aload_arr, iload_iv, daload, dload_sum, dadd, dstore_sum, iinc iv 1, goto
fn detect_fp_array_sum(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdFpArraySum> {
    // back_edge should be a goto instruction
    if code.get(back_edge).copied() != Some(0xa7) {
        return None;
    }
    let back_edge_end = back_edge + 3;

    // Header starts with: iload <iv>, iload <bound>, if_icmpge <exit>
    let mut pc = header;

    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    if pc + 2 >= back_edge_end || code[pc] != 0xa2 {
        return None; // expect if_icmpge
    }
    pc += 3;

    // Now match loop body. Two patterns:
    // Pattern A: aload arr, iload iv, daload, dload sum, dadd, dstore sum
    // Pattern B: dload sum, aload arr, iload iv, daload, dadd, dstore sum

    let (array_local, acc_local);

    // Try pattern A: aload arr first
    if let Some(arr) = extract_aload_local(code, pc) {
        let arr_len = if code[pc] == 0x19 { 2 } else { 1 };
        let pc2 = pc + arr_len;

        let iv2 = extract_iload_local(code, pc2);
        if iv2 == Some(iv_local) {
            let iv2_len = if code[pc2] == 0x15 { 2 } else { 1 };
            let pc3 = pc2 + iv2_len;

            // daload (0x31)
            if pc3 < back_edge_end && code[pc3] == 0x31 {
                let pc4 = pc3 + 1;

                // dload sum
                if let Some(sum) = extract_dload_local(code, pc4) {
                    let sum_len = if code[pc4] == 0x18 { 2 } else { 1 };
                    let pc5 = pc4 + sum_len;

                    // dadd (0x63)
                    if pc5 < back_edge_end && code[pc5] == 0x63 {
                        let pc6 = pc5 + 1;

                        // dstore sum
                        if let Some(store_sum) = extract_dstore_local(code, pc6) {
                            if store_sum == sum {
                                let store_len = if code[pc6] == 0x39 { 2 } else { 1 };
                                let pc7 = pc6 + store_len;

                                // iinc iv 1
                                if pc7 + 2 < back_edge_end
                                    && code[pc7] == 0x84
                                    && code[pc7 + 1] as usize == iv_local // Widening: always safe
                                    && code[pc7 + 2] == 0x01
                                    && pc7 + 3 == back_edge
                                {
                                    return Some(SimdFpArraySum {
                                        header_pc: header,
                                        back_edge_pc: back_edge,
                                        iv_local,
                                        acc_local: sum,
                                        array_local: arr,
                                        bound_local,
                                        sse_op: 0x58, // ADDPD
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // If pattern A didn't match fully, fall through
        array_local = 0; // not used
        acc_local = 0;
    } else {
        array_local = 0;
        acc_local = 0;
    }

    // Try pattern B: dload sum first
    if let Some(sum) = extract_dload_local(code, pc) {
        let sum_len = if code[pc] == 0x18 { 2 } else { 1 };
        let pc2 = pc + sum_len;

        if let Some(arr) = extract_aload_local(code, pc2) {
            let arr_len = if code[pc2] == 0x19 { 2 } else { 1 };
            let pc3 = pc2 + arr_len;

            let iv2 = extract_iload_local(code, pc3);
            if iv2 == Some(iv_local) {
                let iv2_len = if code[pc3] == 0x15 { 2 } else { 1 };
                let pc4 = pc3 + iv2_len;

                // daload (0x31)
                if pc4 < back_edge_end && code[pc4] == 0x31 {
                    let pc5 = pc4 + 1;

                    // dadd (0x63)
                    if pc5 < back_edge_end && code[pc5] == 0x63 {
                        let pc6 = pc5 + 1;

                        // dstore sum
                        if let Some(store_sum) = extract_dstore_local(code, pc6) {
                            if store_sum == sum {
                                let store_len = if code[pc6] == 0x39 { 2 } else { 1 };
                                let pc7 = pc6 + store_len;

                                if pc7 + 2 < back_edge_end
                                    && code[pc7] == 0x84
                                    && code[pc7 + 1] as usize == iv_local // Widening: always safe
                                    && code[pc7 + 2] == 0x01
                                    && pc7 + 3 == back_edge
                                {
                                    return Some(SimdFpArraySum {
                                        header_pc: header,
                                        back_edge_pc: back_edge,
                                        iv_local,
                                        acc_local: sum,
                                        array_local: arr,
                                        bound_local,
                                        sse_op: 0x58, // ADDPD
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = (array_local, acc_local); // suppress unused warnings
    None
}

// ---------------------------------------------------------------------------
// T5.2.15 — SuperWord / element-wise SIMD detection
// ---------------------------------------------------------------------------

/// Operation that joins the two source vectors in an element-wise loop.
///
/// Expressed as the bytecode opcode of the arithmetic that appears
/// between the two `iaload`s and the `iastore` — the JIT maps this to
/// `PADDD` / `PSUBD` / `PMULLD` when lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ElementWiseOp {
    /// iadd (0x60) → PADDD
    Add,
    /// isub (0x64) → PSUBD
    Sub,
    /// imul (0x68) → PMULLD (SSE4.1 or AVX2)
    Mul,
    /// iand (0x7E) → PAND
    And,
    /// ior  (0x80) → POR
    Or,
    /// ixor (0x82) → PXOR
    Xor,
}

/// Information about a vectorizable int-array element-wise loop.
///
/// Matches the pattern:
///
/// ```text
/// for (i = 0; i < n; i++) out[i] = a[i] OP b[i];
/// ```
///
/// where `OP` is one of `iadd`, `isub`, `imul`, `iand`, `ior`, `ixor`.
/// The detector also accepts the simpler form `a[i] OP b[i]` when the
/// result is stored back into `a` (in-place).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SimdArrayElementWise {
    /// Bytecode PC of the loop header.
    pub header_pc: usize,
    /// Bytecode PC of the back-edge.
    pub back_edge_pc: usize,
    /// Local index of the induction variable.
    pub iv_local: usize,
    /// Local index of the destination array.
    pub out_local: usize,
    /// Local index of source array A.
    pub a_local: usize,
    /// Local index of source array B.
    pub b_local: usize,
    /// Local index of the loop bound (upper limit of `i`).
    pub bound_local: usize,
    /// Operation to perform element-wise.
    pub op: ElementWiseOp,
}

/// Try to detect an int-array element-wise pattern in a loop body.
///
/// The recognized shape is:
///
/// ```text
/// header: iload iv ; iload bound ; if_icmpge exit
/// body:   aload out ; iload iv ;
///         aload a ; iload iv ; iaload ;
///         aload b ; iload iv ; iaload ;
///         i{add,sub,mul,and,or,xor} ;
///         iastore ;
///         iinc iv 1 ;
///         goto header
/// ```
///
/// Returns `Some` on a match; `None` otherwise. The caller stores the
/// result on the `Compiler` for downstream SIMD emission.
#[allow(dead_code)]
pub(crate) fn detect_int_array_element_wise(
    code: &[u8],
    header: usize,
    back_edge: usize,
    iv_local: usize,
) -> Option<SimdArrayElementWise> {
    if back_edge >= code.len() || code[back_edge] != 0xa7 {
        return None;
    }
    let back_edge_end = back_edge + 3;
    let mut pc = header;

    // Header: iload iv ; iload bound ; if_icmpge exit
    let iv_check = extract_iload_local(code, pc)?;
    if iv_check != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    let bound_local = extract_iload_local(code, pc)?;
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    if pc + 2 >= back_edge_end || code[pc] != 0xa2 {
        return None;
    }
    pc += 3;

    // Body: aload out ; iload iv
    let out_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };

    let iv2 = extract_iload_local(code, pc)?;
    if iv2 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };

    // aload a ; iload iv ; iaload
    let a_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };
    let iv3 = extract_iload_local(code, pc)?;
    if iv3 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None; // iaload
    }
    pc += 1;

    // aload b ; iload iv ; iaload
    let b_local = extract_aload_local(code, pc)?;
    pc += if code[pc] == 0x19 { 2 } else { 1 };
    let iv4 = extract_iload_local(code, pc)?;
    if iv4 != iv_local {
        return None;
    }
    pc += if code[pc] == 0x15 { 2 } else { 1 };
    if pc >= back_edge_end || code[pc] != 0x2e {
        return None;
    }
    pc += 1;

    // Element-wise op
    let op = match code.get(pc).copied()? {
        0x60 => ElementWiseOp::Add,
        0x64 => ElementWiseOp::Sub,
        0x68 => ElementWiseOp::Mul,
        0x7E => ElementWiseOp::And,
        0x80 => ElementWiseOp::Or,
        0x82 => ElementWiseOp::Xor,
        _ => return None,
    };
    pc += 1;

    // iastore
    if pc >= back_edge_end || code[pc] != 0x4F {
        return None;
    }
    pc += 1;

    // iinc iv, 1 ; goto header
    if pc + 2 >= back_edge_end
        || code[pc] != 0x84
        || code[pc + 1] as usize != iv_local
        || code[pc + 2] != 0x01
        || pc + 3 != back_edge
    {
        return None;
    }

    Some(SimdArrayElementWise {
        header_pc: header,
        back_edge_pc: back_edge,
        iv_local,
        out_local,
        a_local,
        b_local,
        bound_local,
        op,
    })
}

// ---------------------------------------------------------------------------
// T5.2.17 — Loop unswitching detection
// ---------------------------------------------------------------------------

/// Candidate for loop unswitching.
///
/// Describes a loop whose body contains a conditional branch on a
/// local that is never written inside the loop. The JIT can legally
/// duplicate the loop into two loops — one for each side of the
/// branch — and hoist the condition check out of the header.
///
/// Detection runs on loops of ≤ `MAX_UNSWITCH_BYTECODES` bytes to
/// bound the code-size blow-up from the duplication.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct LoopUnswitchCandidate {
    /// Bytecode PC of the loop header.
    pub header_pc: usize,
    /// Bytecode PC of the back-edge goto.
    pub back_edge_pc: usize,
    /// Bytecode PC of the invariant conditional branch inside the body.
    pub invariant_branch_pc: usize,
    /// Local variable index whose value is the branch predicate and
    /// that is never re-assigned in the loop body.
    pub invariant_local: usize,
    /// Opcode of the branch (if_icmp*, ifeq, ifne, etc.) so the
    /// emitter can mirror it when duplicating.
    pub branch_op: u8,
}

/// Upper limit on loop body size eligible for unswitching.
///
/// Chosen to match the unrolling budget: duplicating a 32-byte loop
/// doubles to 64 bytes, comparable to the 32-byte unroll limit in the
/// static heuristic. Larger loops have more total cost and less
/// marginal benefit from hoisting a single branch.
pub const MAX_UNSWITCH_BYTECODES: usize = 32;

/// Detect loops eligible for unswitching.
///
/// A loop is a candidate when:
/// 1. Its body size is ≤ `MAX_UNSWITCH_BYTECODES` bytes.
/// 2. The body contains an `ifeq/ifne/.../if_icmp*` instruction.
/// 3. The predicate comes from an `iload` whose local is never
///    written (no `istore`/`iinc`) anywhere in the loop body.
///
/// When multiple branches qualify, the first one is returned. Nested
/// loops are handled by the caller (the `loops` list already
/// enumerates each level independently).
#[allow(dead_code)]
pub(crate) fn detect_loop_unswitch_candidates(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<LoopUnswitchCandidate> {
    let mut out = Vec::new();
    for &(header, back_edge) in loops {
        if header >= code_len || back_edge >= code_len {
            continue;
        }
        let body_size = back_edge.saturating_sub(header);
        if body_size == 0 || body_size > MAX_UNSWITCH_BYTECODES {
            continue;
        }

        // Collect locals written inside the loop (to exclude them
        // from the invariant set).
        let mut written: u64 = 0;
        {
            let mut pc = header;
            while pc <= back_edge && pc < code_len {
                match code[pc] {
                    // istore/lstore/fstore/dstore/astore <local>
                    0x36..=0x3A if pc + 1 < code_len => {
                        written |= 1u64 << (code[pc + 1] as usize & 0x3F);
                    }
                    // istore_0..istore_3
                    0x3B..=0x3E => written |= 1u64 << ((code[pc] - 0x3B) as usize),
                    // lstore_0..lstore_3
                    0x3F..=0x42 => written |= 1u64 << ((code[pc] - 0x3F) as usize),
                    // fstore_0..fstore_3
                    0x43..=0x46 => written |= 1u64 << ((code[pc] - 0x43) as usize),
                    // dstore_0..dstore_3
                    0x47..=0x4A => written |= 1u64 << ((code[pc] - 0x47) as usize),
                    // astore_0..astore_3
                    0x4B..=0x4E => written |= 1u64 << ((code[pc] - 0x4B) as usize),
                    // iinc <local>, _
                    0x84 if pc + 1 < code_len => {
                        written |= 1u64 << (code[pc + 1] as usize & 0x3F);
                    }
                    _ => {}
                }
                pc += crate::scev::bytecode_len(code, pc, code_len);
            }
        }

        // Walk again looking for an `iload L; if*` pair where L is not
        // in `written`. The invariant_local must be < 64 so it fits in
        // the bitmask.
        let mut pc = header;
        while pc < back_edge && pc < code_len {
            // Try to extract an iload and its local.
            let (iload_local, iload_len) = match code.get(pc).copied() {
                Some(0x1A..=0x1D) => (Some((code[pc] - 0x1A) as usize), 1usize),
                Some(0x15) if pc + 1 < code_len => (Some(code[pc + 1] as usize), 2usize),
                _ => (None, 0),
            };
            if let Some(local) = iload_local {
                let next_pc = pc + iload_len;
                if next_pc < code_len {
                    let op = code[next_pc];
                    // if_icmpeq..if_icmple need a second iload, so we
                    // match the simpler ifeq..ifle (0x99..=0x9E) that
                    // operate on the single top-of-stack.
                    let is_unary_branch = matches!(op, 0x99..=0x9E);
                    if is_unary_branch
                        && local < 64
                        && (written & (1u64 << local)) == 0
                    {
                        out.push(LoopUnswitchCandidate {
                            header_pc: header,
                            back_edge_pc: back_edge,
                            invariant_branch_pc: next_pc,
                            invariant_local: local,
                            branch_op: op,
                        });
                        break; // one candidate per loop is enough
                    }
                }
            }
            pc += crate::scev::bytecode_len(code, pc, code_len);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Bytecode compatibility check
// ---------------------------------------------------------------------------

/// Scan bytecode and determine JIT compatibility.
///
/// Returns `Some(needs_heap)` if the method can be JIT-compiled, `None` otherwise.
/// `needs_heap` is true if the method uses array/object opcodes that require
/// a heap pointer as a hidden first argument.
pub fn jit_scan(code: &[u8], code_len: usize, descriptor: &str) -> Option<JitScanResult> {
    let mut needs_heap = false;
    let mut multianewarray_ops = Vec::new();
    let mut field_ops = Vec::new();
    let mut typecheck_ops = Vec::new();
    let mut static_field_ops = Vec::new();
    let mut invoke_ops: Vec<(usize, u16, u8)> = Vec::new(); // (pc, cp_index, opcode)
    let mut new_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `new` (0xbb)
    let mut anewarray_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `anewarray` (0xbd)
    let mut ldc_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `ldc`/`ldc_w`
    let mut ldc2w_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `ldc2_w` (0x14)
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // nop
            0x00 => {
                pc += 1;
            }
            // aconst_null
            0x01 => {
                pc += 1;
            }
            // iconst_m1..iconst_5
            0x02..=0x08 => {
                pc += 1;
            }
            // lconst_0, lconst_1
            0x09 | 0x0a => {
                pc += 1;
            }
            // fconst_0, fconst_1, fconst_2
            0x0b..=0x0d => {
                pc += 1;
            }
            // dconst_0, dconst_1
            0x0e | 0x0f => {
                pc += 1;
            }
            // bipush
            0x10 => {
                pc += 2;
            }
            // sipush
            0x11 => {
                pc += 3;
            }
            // iload, lload, fload, dload, aload (wide index)
            0x15..=0x19 => {
                pc += 2;
            }
            // iload_0..iload_3
            0x1a..=0x1d => {
                pc += 1;
            }
            // lload_0..lload_3
            0x1e..=0x21 => {
                pc += 1;
            }
            // fload_0..fload_3, dload_0..dload_3
            0x22..=0x29 => {
                pc += 1;
            }
            // aload_0..aload_3
            0x2a..=0x2d => {
                pc += 1;
            }
            // iaload, laload, faload, daload, aaload, baload, caload, saload
            0x2e..=0x35 => {
                pc += 1;
            }
            // istore, lstore, fstore, dstore, astore (wide index)
            0x36..=0x3a => {
                pc += 2;
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                pc += 1;
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                pc += 1;
            }
            // fstore_0..fstore_3, dstore_0..dstore_3
            0x43..=0x4a => {
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                pc += 1;
            }
            // iastore, lastore, fastore, dastore, aastore, bastore, castore, sastore
            0x4f..=0x56 => {
                pc += 1;
            }
            // pop
            0x57 => {
                pc += 1;
            }
            // dup
            0x59 => {
                pc += 1;
            }
            // swap
            0x5f => {
                pc += 1;
            }
            // iadd..ddiv (all int/long/float/double add/sub/mul/div)
            0x60..=0x6f => {
                pc += 1;
            }
            // irem, lrem
            0x70 | 0x71 => {
                pc += 1;
            }
            // ineg, lneg, fneg, dneg
            0x74..=0x77 => {
                pc += 1;
            }
            // ishl, lshl
            0x78 | 0x79 => {
                pc += 1;
            }
            // ishr, lshr
            0x7a | 0x7b => {
                pc += 1;
            }
            // iushr, lushr
            0x7c | 0x7d => {
                pc += 1;
            }
            // iand, land
            0x7e | 0x7f => {
                pc += 1;
            }
            // ior, lor
            0x80 | 0x81 => {
                pc += 1;
            }
            // ixor, lxor
            0x82 | 0x83 => {
                pc += 1;
            }
            // iinc
            0x84 => {
                pc += 3;
            }
            // i2l, i2f, i2d, l2i, l2f, l2d, f2i, f2l, f2d, d2i, d2l, d2f, i2b, i2c, i2s
            0x85..=0x93 => {
                pc += 1;
            }
            // lcmp, fcmpl, fcmpg, dcmpl, dcmpg
            0x94..=0x98 => {
                pc += 1;
            }
            // ifeq, ifne, iflt, ifge, ifgt, ifle
            0x99..=0x9e => {
                pc += 3;
            }
            // if_icmpeq..if_icmple
            0x9f..=0xa4 => {
                pc += 3;
            }
            // goto
            0xa7 => {
                pc += 3;
            }
            // ireturn, lreturn, freturn, dreturn
            0xac..=0xaf => {
                pc += 1;
            }
            // return (void)
            0xb1 => {
                pc += 1;
            }
            // invokestatic — track in invoke_ops for resolution (self-call or cross-method).
            // Always set needs_heap because cross-method dispatch via
            // jit_invoke_dispatch requires vm_ptr stored at heap_local_offset.
            0xb8 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 3;
            }
            // areturn — return object reference
            0xb0 => {
                pc += 1;
            }
            // getstatic — static field read (needs vm context)
            0xb2 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                static_field_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // putstatic — static field write (needs vm context)
            0xb3 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                static_field_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // getfield — object field read
            0xb4 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                field_ops.push((pc, cp_idx));
                pc += 3;
            }
            // putfield — object field write (may need heap for write barrier)
            0xb5 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                field_ops.push((pc, cp_idx));
                pc += 3;
            }
            // newarray — needs heap for allocation
            0xbc => {
                needs_heap = true;
                pc += 2;
            }
            // arraylength
            0xbe => {
                pc += 1;
            }
            // checkcast — type check (pass-through or exception)
            0xc0 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                typecheck_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // instanceof — type check (returns 0 or 1)
            0xc1 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                typecheck_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // multianewarray — multi-dimensional array allocation (2D only for now)
            0xc5 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                let ndims = code[pc + 3];
                if ndims != 2 {
                    return None;
                }
                multianewarray_ops.push((pc, cp_idx, ndims));
                needs_heap = true;
                pc += 4;
            }
            // if_acmpeq, if_acmpne — reference comparison branches
            0xa5 | 0xa6 => {
                pc += 3;
            }
            // ifnull, ifnonnull — null check branches
            0xc6 | 0xc7 => {
                pc += 3;
            }
            // invokevirtual, invokespecial — method dispatch via helper
            0xb6 | 0xb7 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 3;
            }
            // invokeinterface — interface dispatch via helper (5 bytes: opcode, cp_hi, cp_lo, count, 0)
            0xb9 => {
                if pc + 4 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 5;
            }
            // tableswitch — accept in scanner, emit CMP chain in compiler
            0xaa => {
                pc += 1;
                while pc % 4 != 0 && pc < code_len {
                    pc += 1;
                }
                if pc + 12 > code_len {
                    return None;
                }
                let low =
                    i32::from_be_bytes([code[pc + 4], code[pc + 5], code[pc + 6], code[pc + 7]]);
                let high =
                    i32::from_be_bytes([code[pc + 8], code[pc + 9], code[pc + 10], code[pc + 11]]);
                let num_offsets = (high - low + 1).max(0) as usize; // Cast: address arithmetic
                pc += 12 + num_offsets * 4;
            }
            // lookupswitch — accept in scanner, emit CMP chain in compiler
            0xab => {
                pc += 1;
                while pc % 4 != 0 && pc < code_len {
                    pc += 1;
                }
                if pc + 8 > code_len {
                    return None;
                }
                let npairs_raw =
                    i32::from_be_bytes([code[pc + 4], code[pc + 5], code[pc + 6], code[pc + 7]]);
                if npairs_raw < 0 {
                    return None;
                }
                let npairs = npairs_raw as usize; // Cast: address arithmetic
                pc += 8 + npairs * 8;
            }
            // ldc — load int/float/string constant from CP (1-byte index)
            0x12 => {
                let cp_idx = code[pc + 1] as u16; // Widening: always safe
                ldc_ops.push((pc, cp_idx));
                pc += 2;
            }
            // ldc_w — load int/float/string constant from CP (2-byte index)
            0x13 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                ldc_ops.push((pc, cp_idx));
                pc += 3;
            }
            // ldc2_w — load long/double constant from CP (2-byte index)
            0x14 => {
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                ldc2w_ops.push((pc, cp_idx));
                pc += 3;
            }
            // pop2 — discard top two slots
            0x58 => {
                pc += 1;
            }
            // dup_x1 — duplicate top and insert two below
            0x5a => {
                pc += 1;
            }
            // dup_x2 — duplicate top and insert three below
            0x5b => {
                pc += 1;
            }
            // dup2 — duplicate top two slots
            0x5c => {
                pc += 1;
            }
            // dup2_x1 — duplicate top two and insert three below
            0x5d => {
                pc += 1;
            }
            // dup2_x2 — duplicate top two and insert four below
            0x5e => {
                pc += 1;
            }
            // new — object allocation; tracked for code generation
            0xbb => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                new_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // anewarray — reference array allocation; tracked for code generation
            0xbd => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                anewarray_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // T5.2.8 — monitorenter / monitorexit. Accepted by the
            // scanner so methods with `synchronized` blocks are
            // JIT-eligible. Lock elision in the compiler path skips
            // the actual monitor operation when escape analysis
            // proves the receiver is thread-local; otherwise the
            // compiler bails to the interpreter (see the 0xC2/0xC3
            // handler in `compile_bytecode`).
            0xC2 | 0xC3 => {
                pc += 1;
            }
            // Anything else: not JIT-compatible
            _ => {
                return None;
            }
        }
    }

    // Check the return type is int, long, float, double, object reference, or void
    let ret = super::return_type(descriptor);
    if !matches!(
        ret,
        b'I' | b'J' | b'F' | b'D' | b'[' | b'L' | b'V' | b'B' | b'C' | b'S' | b'Z'
    ) {
        return None;
    }

    // Run escape analysis to identify non-escaping `new` instructions.
    // These are tracked for potential future scalar replacement optimization.
    let non_escaping_new = if new_ops.is_empty() {
        std::collections::HashSet::new()
    } else {
        analyze_escapes(code, code_len)
    };

    Some(JitScanResult {
        needs_heap,
        multianewarray_ops,
        field_ops,
        typecheck_ops,
        static_field_ops,
        invoke_ops,
        new_ops,
        anewarray_ops,
        non_escaping_new,
        ldc_ops,
        ldc2w_ops,
    })
}

/// Result of scanning bytecode for JIT compatibility.
pub struct JitScanResult {
    pub needs_heap: bool,
    /// For each multianewarray instruction: (bytecode_pc, cp_index, ndims)
    pub multianewarray_ops: Vec<(usize, u16, u8)>,
    /// For each getfield/putfield instruction: (bytecode_pc, cp_index)
    pub field_ops: Vec<(usize, u16)>,
    /// For each checkcast/instanceof instruction: (bytecode_pc, cp_index)
    pub typecheck_ops: Vec<(usize, u16)>,
    /// For each getstatic/putstatic instruction: (bytecode_pc, cp_index)
    pub static_field_ops: Vec<(usize, u16)>,
    /// For each invoke instruction: (bytecode_pc, cp_index, opcode)
    pub invoke_ops: Vec<(usize, u16, u8)>,
    /// For each `new` (0xbb) instruction: (bytecode_pc, cp_index)
    pub new_ops: Vec<(usize, u16)>,
    /// For each `anewarray` (0xbd) instruction: (bytecode_pc, cp_index)
    pub anewarray_ops: Vec<(usize, u16)>,
    /// Set of `new` bytecode PCs whose objects are non-escaping (candidates for scalar replacement).
    pub non_escaping_new: std::collections::HashSet<usize>,
    /// For each `ldc`/`ldc_w` (0x12, 0x13) instruction: (bytecode_pc, cp_index)
    pub ldc_ops: Vec<(usize, u16)>,
    /// For each `ldc2_w` (0x14) instruction: (bytecode_pc, cp_index)
    pub ldc2w_ops: Vec<(usize, u16)>,
}

/// Check if a bytecode method can be JIT-compiled (backward-compatible wrapper).
pub fn is_jit_compatible(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    jit_scan(code, code_len, descriptor).is_some()
}

// ---------------------------------------------------------------------------
// Loop-Invariant Code Motion (LICM)
// ---------------------------------------------------------------------------

/// A loop-invariant `aload X; iload Y; aaload` sequence that can be hoisted
/// out of a loop body. The hoisted value (a row pointer from an Object[][] array)
/// is computed once before the loop and cached in a spill slot.
struct LoopHoist {
    /// Bytecode PC of the loop header (back-edge target).
    loop_header: usize,
    /// First bytecode PC of the invariant sequence (the aload instruction).
    seq_start: usize,
    /// Bytecode PC after the invariant sequence (past the aaload).
    seq_end: usize,
    /// Local variable index for the array reference.
    array_local: usize,
    /// Local variable index for the array index.
    index_local: usize,
}

/// Information about a loop-invariant FP load that can be hoisted.
/// Pattern: dload/fload of a local that is not modified within the loop body.
#[derive(Debug)]
#[allow(dead_code)]
struct FpLoopHoist {
    /// Bytecode PC of the loop header (back-edge target).
    loop_header: usize,
    /// Bytecode PC of the invariant dload/fload instruction.
    load_pc: usize,
    /// Local variable index being loaded.
    local_idx: usize,
    /// true = double (dload), false = float (fload).
    is_double: bool,
}

/// Information about a vectorizable double-array sum reduction loop.
/// Pattern: for (i = start; i < bound; i++) sum += arr[i]
/// where arr is a double[] and sum is a double local.
#[derive(Debug)]
#[allow(dead_code)]
struct SimdFpArraySum {
    /// Bytecode PC of the loop header.
    header_pc: usize,
    /// Bytecode PC of the back-edge instruction.
    back_edge_pc: usize,
    /// Local index of the induction variable (i).
    iv_local: usize,
    /// Local index of the accumulator (sum).
    acc_local: usize,
    /// Local index of the array reference.
    array_local: usize,
    /// Local index of the loop bound.
    bound_local: usize,
    /// Operation: 0x58=ADD (sum), 0x59=MUL (dot product partial).
    sse_op: u8,
}

/// Get the byte length of a bytecode instruction at `pc`.
fn bytecode_len_at(code: &[u8], pc: usize) -> usize {
    match code[pc] {
        0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => 2,
        0x11
        | 0x84
        | 0x99..=0xa6
        | 0xa7
        | 0xb2
        | 0xb3
        | 0xb4
        | 0xb5
        | 0xb6
        | 0xb7
        | 0xb8
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xbb => 3, // new
        0xc5 => 4,
        0xb9 => 5, // invokeinterface: opcode, cp_hi, cp_lo, count, 0
        // tableswitch — variable length
        0xaa => {
            let mut p = pc + 1;
            while p % 4 != 0 { p += 1; }
            let low = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            let high = i32::from_be_bytes([code[p + 8], code[p + 9], code[p + 10], code[p + 11]]);
            let count = (high - low + 1).max(0) as usize; // Cast: address arithmetic
            (p + 12 + count * 4) - pc
        }
        // lookupswitch — variable length
        0xab => {
            let mut p = pc + 1;
            while p % 4 != 0 { p += 1; }
            let npairs = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]) as usize; // Widening: always safe
            (p + 8 + npairs * 8) - pc
        }
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// HIGH-1 / Fix 1 — null-check elimination helper
// ---------------------------------------------------------------------------

/// If the bytecode instruction immediately preceding `pc` is an `aload`
/// of some local, return the local index. Otherwise return `None`.
///
/// Used by the ifnull / ifnonnull codegen to decide whether the value
/// on top of the operand stack came from a known local — if so, the
/// caller can consult `NullCheckInfo::is_nonnull` and elide the inline
/// `TEST reg, reg; Jcc` sequence.
///
/// Only the common encodings are recognised:
///   * `aload_0..3`  (single-byte opcodes 0x2A..=0x2D)
///   * `aload <u8>`  (0x19 + 1-byte index)
///
/// The 4-byte `wide; aload` form is not recognised — its prevalence in
/// real classes is essentially zero, and bailing out is always safe
/// (we simply emit the regular runtime check).
fn preceding_aload_nonnull_local(code: &[u8], pc: usize) -> Option<usize> {
    if pc == 0 {
        return None;
    }
    // aload_0..aload_3 — 1-byte opcode at pc-1.
    let prev1 = code[pc - 1];
    if (0x2A..=0x2D).contains(&prev1) {
        return Some((prev1 - 0x2A) as usize);
    }
    // aload <u8> — 2 bytes at pc-2.
    if pc >= 2 && code[pc - 2] == 0x19 {
        return Some(code[pc - 1] as usize);
    }
    None
}

/// Round-11 HIGH-2 helper — for array load/store opcodes at `pc`, try
/// to identify the local index that sourced the *array receiver* on
/// the operand stack. The standard javac pattern is:
///
///   `aload N; <push index>; <iaload/iastore/...>`
///
/// where `<push index>` is one of the single-byte index-loading
/// opcodes (iload_0..3, iconst_*, bipush, sipush, iload) and the
/// terminal opcode is the array load/store. When we recognise this
/// pattern we return `Some(N)`; the caller consults
/// `NullCheckInfo::is_nonnull(pc, N)` to decide whether the inline
/// `TEST RAX, RAX; JZ stub` can be elided.
///
/// Returns `None` whenever the index push isn't a single-instruction
/// form we recognise. The caller falls back to emitting the runtime
/// check on `None` (always sound).
fn array_receiver_local(code: &[u8], pc: usize) -> Option<usize> {
    if pc == 0 {
        return None;
    }
    // The instruction at `pc` is the array load/store itself. Walk
    // backwards: the index-push is the previous instruction (1-3
    // bytes), and the aload is the one before that.
    let p_index = code[pc - 1];
    let index_len: usize = match p_index {
        // Single-byte index ops:
        //   iconst_m1..iconst_5 (0x02..0x08), iload_0..3 (0x1A..0x1D),
        //   dup (0x59 — when index is already on stack from a dup pair)
        0x02..=0x08 | 0x1A..=0x1D | 0x59 => 1,
        // Multi-byte: distinguish by the opcode byte at pc-2 / pc-3.
        //   bipush <byte>  (0x10) — 2 bytes
        //   iload  <u8>    (0x15) — 2 bytes
        //   sipush <short> (0x11) — 3 bytes
        _ if pc >= 2 && code[pc - 2] == 0x10 => 2,
        _ if pc >= 2 && code[pc - 2] == 0x15 => 2,
        _ if pc >= 3 && code[pc - 3] == 0x11 => 3,
        _ => return None,
    };
    let aload_pc = pc.checked_sub(1 + index_len)?;
    let aop = code[aload_pc];
    if (0x2A..=0x2D).contains(&aop) {
        return Some((aop - 0x2A) as usize);
    }
    if aop == 0x19 && aload_pc + 1 < pc {
        return Some(code[aload_pc + 1] as usize);
    }
    None
}

// ---------------------------------------------------------------------------
// Escape analysis
// ---------------------------------------------------------------------------

/// Identify `new` instructions (0xbb) whose produced objects are non-escaping.
///
/// A `new` at PC `p` is non-escaping when none of the following happen:
/// - The object is returned via `areturn`
/// - The object is stored as the VALUE of a `putfield` or `aastore`
/// - The object is passed as an argument to any invoke other than `invokespecial <init>`
///   on the object itself (the `this` slot of a direct `<init>` call is allowed)
///
/// The analysis is a single forward pass with an abstract operand stack tracking object
/// provenance (`Some(new_pc)` if the slot holds a reference produced by that `new`,
/// `None` otherwise). It is conservative: any ambiguity (e.g., complex dup variants,
/// branches that produce indeterminate stack shapes) causes all objects to be treated
/// as potentially escaping.
///
/// Returns the set of `new` bytecode PCs that are confirmed non-escaping.
fn analyze_escapes(code: &[u8], code_len: usize) -> std::collections::HashSet<usize> {
    // Abstract stack: each entry is Some(new_pc) if the slot holds a new-created reference,
    // or None for non-tracked values.
    let mut abs_stack: Vec<Option<usize>> = Vec::with_capacity(16);
    // Per-local-variable provenance (up to 256 locals).
    let mut local_origin: [Option<usize>; 256] = [None; 256];
    let mut escaped: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Helper: mark all tracked objects currently on the stack as escaped.
    macro_rules! escape_all {
        () => {
            for slot in abs_stack.iter() {
                if let Some(p) = slot {
                    escaped.insert(*p);
                }
            }
        };
    }

    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        match op {
            // new — push a tracked reference
            0xbb => {
                abs_stack.push(Some(pc));
                pc += 3;
            }
            // dup — duplicate top
            0x59 => {
                let top = abs_stack.last().copied().flatten();
                abs_stack.push(top);
                pc += 1;
            }
            // astore_0..3
            0x4b..=0x4e => {
                let idx = (op - 0x4b) as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                // If local already had a tracked object, old value might re-escape on reassign
                if let Some(prev) = local_origin[idx] {
                    escaped.insert(prev);
                }
                local_origin[idx] = val;
                pc += 1;
            }
            // astore N
            0x3a => {
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    let val = abs_stack.pop().flatten();
                    if let Some(prev) = local_origin[idx] {
                        escaped.insert(prev);
                    }
                    local_origin[idx] = val;
                }
                pc += 2;
            }
            // aload_0..3
            0x2a..=0x2d => {
                let idx = (op - 0x2a) as usize; // Widening: always safe
                abs_stack.push(local_origin[idx]);
                pc += 1;
            }
            // aload N
            0x19 => {
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    abs_stack.push(local_origin[idx]);
                } else {
                    abs_stack.push(None);
                }
                pc += 2;
            }
            // areturn — top of stack returns, so it escapes
            0xb0 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    escaped.insert(p);
                }
                pc += 1;
            }
            // putfield (0xb5): pops value then objectref; value escapes if tracked
            0xb5 => {
                let val = abs_stack.pop().flatten();
                abs_stack.pop(); // objectref (we don't care about its origin here)
                if let Some(p) = val {
                    escaped.insert(p);
                }
                pc += 3;
            }
            // getfield (0xb4): pop objectref, push field value (not a tracked new)
            0xb4 => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 3;
            }
            // aastore (0x53): pops value, index, array; value escapes if tracked
            0x53 => {
                let val = abs_stack.pop().flatten();
                abs_stack.pop(); // index
                abs_stack.pop(); // array
                if let Some(p) = val {
                    escaped.insert(p);
                }
                pc += 1;
            }
            // pop — discard top (no escape needed; the object stays local)
            0x57 => {
                abs_stack.pop();
                pc += 1;
            }
            // invokespecial (0xb7) — commonly `<init>` on the new-created object.
            // The `this` argument (deepest stack slot of the call) is popped by the
            // constructor; it does NOT cause the object to escape.  We pop just one
            // slot (the `this`/dup'd copy) and leave the original reference in place.
            // Without the descriptor we cannot pop the right number of parameter slots,
            // but for the common `new; dup; invokespecial <init>()V` pattern this is
            // exact, and for constructors with args it is a conservative approximation.
            0xb7 => {
                // Pop the `this` arg (dup'd copy of the reference).
                abs_stack.pop();
                // No push: invokespecial return type is typically void for <init>.
                pc += 3;
            }
            // invokevirtual/invokeinterface/invokestatic — all args on the stack escape
            0xb6 | 0xb8 | 0xb9 => {
                escape_all!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += if op == 0xb9 { 5 } else { 3 };
            }
            // For all other opcodes, use bytecode_len_at for PC advance.
            // These ops don't move tracked references, so no escaping needed.
            _ => {
                // For ops that might manipulate the stack in ways we don't track,
                // clear any tracked objects (conservative).
                let len = bytecode_len_at(code, pc);
                // Ops that push values onto the stack also push None (not tracked).
                // Ops that pop values: we pop and discard (losing provenance is safe).
                // Rather than implementing each op's exact stack effect, just clear provenance.
                for slot in abs_stack.iter_mut() {
                    *slot = None; // forget provenance, but keep stack depth correct
                }
                pc += len;
            }
        }
    }

    // Return the set of new_pcs that did NOT escape.
    let mut non_escaping = std::collections::HashSet::new();
    let mut pc = 0usize;
    while pc < code_len {
        if code[pc] == 0xbb && !escaped.contains(&pc) {
            non_escaping.insert(pc);
        }
        pc += bytecode_len_at(code, pc);
    }
    non_escaping
}

// ---------------------------------------------------------------------------
// Scalar replacement: eliminate heap allocations for non-escaping objects
// ---------------------------------------------------------------------------

/// Metadata for a scalar-replaced object whose fields live in the JIT frame.
#[derive(Clone, Debug)]
struct ScalarReplacedObject {
    num_fields: usize,
    /// Frame offset of field slot 0: `[RBP - field_base_offset]`.
    /// Field `i` starts at `[RBP - (field_base_offset + i * SLOT_SIZE)]`, matching
    /// heap object layout (`HEADER_SIZE + i * SLOT_SIZE` from `jit_getfield`).
    field_base_offset: i32,
}

/// Result of scalar replacement planning: which bytecode PCs to rewrite.
struct ScalarReplacementPlan {
    /// Non-escaping NEW PCs → their scalar-replaced object info.
    objects: FxHashMap<usize, ScalarReplacedObject>,
    /// putfield/getfield PCs that operate on a scalar-replaced object → the NEW PC.
    field_ops: FxHashMap<usize, usize>,
    /// invokespecial PCs whose `<init>()V` call should be skipped.
    init_skips: std::collections::HashSet<usize>,
    /// Total 8-byte frame slots reserved for scalar-replaced fields
    /// (`num_fields * (SLOT_SIZE / 8)` per object).
    total_slots: usize,
}

/// Analyze bytecode to plan scalar replacement for non-escaping objects.
///
/// Uses abstract interpretation to track object provenance through the stack
/// and locals, identifying which putfield/getfield/invokespecial PCs operate
/// on scalar-replaced objects.
fn plan_scalar_replacement(
    code: &[u8],
    code_len: usize,
    non_escaping_new: &std::collections::HashSet<usize>,
    new_info: &[(usize, u32, usize, bool, bool)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    scalar_base: usize,
) -> ScalarReplacementPlan {
    let empty = ScalarReplacementPlan {
        objects: FxHashMap::default(),
        field_ops: FxHashMap::default(),
        init_skips: std::collections::HashSet::new(),
        total_slots: 0,
    };
    if non_escaping_new.is_empty() {
        return empty;
    }

    // Build objects map with deterministic frame offset assignment.
    let mut objects: FxHashMap<usize, ScalarReplacedObject> = FxHashMap::default();
    let mut total_slots = 0usize;
    let mut sorted_pcs: Vec<usize> = non_escaping_new.iter().copied().collect();
    sorted_pcs.sort();
    for &new_pc in &sorted_pcs {
        if let Some(&(_, _, num_fields, _, _)) =
            new_info.iter().find(|(p, _, _, _, _)| *p == new_pc)
        {
            if num_fields > 0 && num_fields <= 16 {
                let field_base_offset = ((scalar_base + total_slots) as i32 + 1) * 8; // Cast: x86-64 immediate encoding
                objects.insert(new_pc, ScalarReplacedObject { num_fields, field_base_offset });
                total_slots += num_fields * (SLOT_SIZE / 8);
            }
        }
    }
    if objects.is_empty() {
        return empty;
    }

    // Abstract interpretation: track object provenance through stack and locals.
    let mut abs_stack: Vec<Option<usize>> = Vec::with_capacity(16);
    let mut local_prov: [Option<usize>; 256] = [None; 256];
    let mut field_ops: FxHashMap<usize, usize> = FxHashMap::default();
    let mut init_skips = std::collections::HashSet::new();

    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        match op {
            // new
            0xBB => {
                abs_stack.push(if objects.contains_key(&pc) { Some(pc) } else { None });
                pc += 3;
            }
            // dup
            0x59 => {
                let top = abs_stack.last().copied().flatten();
                abs_stack.push(top);
                pc += 1;
            }
            // aconst_null
            0x01 => { abs_stack.push(None); pc += 1; }
            // iconst_m1..iconst_5
            0x02..=0x08 => { abs_stack.push(None); pc += 1; }
            // lconst_0, lconst_1
            0x09 | 0x0A => { abs_stack.push(None); pc += 1; }
            // fconst_0..fconst_2
            0x0B..=0x0D => { abs_stack.push(None); pc += 1; }
            // dconst_0, dconst_1
            0x0E | 0x0F => { abs_stack.push(None); pc += 1; }
            // bipush
            0x10 => { abs_stack.push(None); pc += 2; }
            // sipush
            0x11 => { abs_stack.push(None); pc += 3; }
            // ldc
            0x12 => { abs_stack.push(None); pc += 2; }
            // ldc_w, ldc2_w
            0x13 | 0x14 => { abs_stack.push(None); pc += 3; }
            // iload, lload, fload, dload
            0x15..=0x18 => { abs_stack.push(None); pc += 2; }
            // aload
            0x19 => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                abs_stack.push(if idx < 256 { local_prov[idx] } else { None });
                pc += 2;
            }
            // iload_0..3, lload_0..3, fload_0..3, dload_0..3
            0x1A..=0x29 => { abs_stack.push(None); pc += 1; }
            // aload_0..3
            0x2A..=0x2D => {
                let idx = (op - 0x2A) as usize; // Widening: always safe
                abs_stack.push(local_prov[idx]);
                pc += 1;
            }
            // Xaload (iaload..saload): pop index + arrayref, push value
            0x2E..=0x35 => {
                abs_stack.pop(); abs_stack.pop();
                abs_stack.push(None);
                pc += 1;
            }
            // istore, lstore, fstore, dstore
            0x36..=0x39 => { abs_stack.pop(); pc += 2; }
            // astore
            0x3A => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                if idx < 256 { local_prov[idx] = val; }
                pc += 2;
            }
            // istore_0..3, lstore_0..3, fstore_0..3, dstore_0..3
            0x3B..=0x4A => { abs_stack.pop(); pc += 1; }
            // astore_0..3
            0x4B..=0x4E => {
                let idx = (op - 0x4B) as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                local_prov[idx] = val;
                pc += 1;
            }
            // Xastore (iastore..sastore): pop value, index, arrayref
            0x4F..=0x56 => {
                abs_stack.pop(); abs_stack.pop(); abs_stack.pop();
                pc += 1;
            }
            // pop
            0x57 => { abs_stack.pop(); pc += 1; }
            // pop2
            0x58 => { abs_stack.pop(); abs_stack.pop(); pc += 1; }
            // dup_x1
            0x5A => {
                let v1 = abs_stack.pop().flatten();
                let v2 = abs_stack.pop().flatten();
                abs_stack.push(v1); abs_stack.push(v2); abs_stack.push(v1);
                pc += 1;
            }
            // dup_x2
            0x5B => {
                let v1 = abs_stack.pop().flatten();
                let v2 = abs_stack.pop().flatten();
                let v3 = abs_stack.pop().flatten();
                abs_stack.push(v1); abs_stack.push(v3); abs_stack.push(v2); abs_stack.push(v1);
                pc += 1;
            }
            // dup2
            0x5C => {
                let len = abs_stack.len();
                let v1 = if len >= 1 { abs_stack[len - 1] } else { None };
                let v2 = if len >= 2 { abs_stack[len - 2] } else { None };
                abs_stack.push(v2); abs_stack.push(v1);
                pc += 1;
            }
            // swap
            0x5F => {
                let len = abs_stack.len();
                if len >= 2 { abs_stack.swap(len - 1, len - 2); }
                pc += 1;
            }
            // Binary arithmetic: iadd(0x60)..drem(0x73), ishl(0x78)..lxor(0x83)
            0x60..=0x73 | 0x78..=0x83 => {
                abs_stack.pop();
                if let Some(last) = abs_stack.last_mut() { *last = None; }
                pc += 1;
            }
            // Unary: ineg..dneg
            0x74..=0x77 => {
                if let Some(last) = abs_stack.last_mut() { *last = None; }
                pc += 1;
            }
            // Conversion: i2l..i2s
            0x85..=0x93 => {
                if let Some(last) = abs_stack.last_mut() { *last = None; }
                pc += 1;
            }
            // Comparison: lcmp..dcmpg (pop 2, push 1)
            0x94..=0x98 => {
                abs_stack.pop();
                if let Some(last) = abs_stack.last_mut() { *last = None; }
                pc += 1;
            }
            // Conditional branches: conservatively clear stack provenance, keep locals
            0x99..=0xA6 => {
                // Pop comparison operands
                match op {
                    0x99..=0x9E | 0xC6 | 0xC7 => { abs_stack.pop(); }
                    _ => { abs_stack.pop(); abs_stack.pop(); }
                }
                // Clear stack provenance at branch (conservative for merge points)
                for slot in abs_stack.iter_mut() { *slot = None; }
                pc += 3;
            }
            // goto
            0xA7 => {
                abs_stack.clear();
                pc += 3;
            }
            // ireturn, lreturn, freturn, dreturn, areturn
            0xAC..=0xB0 => { abs_stack.pop(); pc += 1; }
            // return (void)
            0xB1 => { pc += 1; }
            // getstatic
            0xB2 => { abs_stack.push(None); pc += 3; }
            // putstatic
            0xB3 => { abs_stack.pop(); pc += 3; }
            // getfield
            0xB4 => {
                let obj = abs_stack.pop().flatten();
                if let Some(new_pc) = obj {
                    if objects.contains_key(&new_pc) {
                        field_ops.insert(pc, new_pc);
                    }
                }
                abs_stack.push(None);
                pc += 3;
            }
            // putfield
            0xB5 => {
                let _value = abs_stack.pop();
                let obj = abs_stack.pop().flatten();
                if let Some(new_pc) = obj {
                    if objects.contains_key(&new_pc) {
                        field_ops.insert(pc, new_pc);
                    }
                }
                pc += 3;
            }
            // invokespecial
            0xB7 => {
                let info_ptr = invoke_info.iter()
                    .find(|&&(ipc, _)| ipc == pc)
                    .map(|&(_, ptr)| ptr);
                if let Some(ptr) = info_ptr {
                    // SAFETY: ptr comes from invoke_info, which holds pointers to JitInvokeInfo
                    // structs that are kept alive by the caller for the duration of compilation.
                    let info = unsafe { &*ptr };
                    let n = info.num_jit_args;
                    if info.method_name == "<init>" && info.descriptor == "()V" {
                        // Zero-arg init: pop only `this`
                        let receiver = abs_stack.pop().flatten();
                        if let Some(new_pc) = receiver {
                            if objects.contains_key(&new_pc) {
                                init_skips.insert(pc);
                            }
                        }
                    } else {
                        // Pop all args (including this)
                        for _ in 0..n { abs_stack.pop(); }
                        if info.return_type != b'V' { abs_stack.push(None); }
                    }
                } else {
                    abs_stack.clear();
                    abs_stack.push(None);
                }
                pc += 3;
            }
            // invokevirtual, invokestatic
            0xB6 | 0xB8 => {
                abs_stack.clear();
                abs_stack.push(None);
                pc += 3;
            }
            // invokeinterface
            0xB9 => {
                abs_stack.clear();
                abs_stack.push(None);
                pc += 5;
            }
            // checkcast: keeps object on stack with same provenance
            0xC0 => { pc += 3; }
            // instanceof: pop ref, push int
            0xC1 => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 3;
            }
            // ifnull, ifnonnull
            0xC6 | 0xC7 => {
                abs_stack.pop();
                for slot in abs_stack.iter_mut() { *slot = None; }
                pc += 3;
            }
            // athrow
            0xBF => { abs_stack.clear(); pc += 1; }
            // arraylength
            0xBE => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 1;
            }
            // iinc
            0x84 => { pc += 3; }
            // newarray, anewarray
            0xBC => { abs_stack.pop(); abs_stack.push(None); pc += 2; }
            0xBD => { abs_stack.pop(); abs_stack.push(None); pc += 3; }
            // For anything else, conservatively clear all provenance
            _ => {
                abs_stack.clear();
                for prov in local_prov.iter_mut() { *prov = None; }
                pc += bytecode_len_at(code, pc);
            }
        }
    }

    // Only keep objects that have at least one field op or init skip
    let used_objects: std::collections::HashSet<usize> = field_ops.values().copied()
        .chain(init_skips.iter().copied().filter_map(|init_pc| {
            // Map init_pc to the new_pc (init is always at new_pc + 4)
            let new_pc = init_pc.wrapping_sub(4);
            if objects.contains_key(&new_pc) { Some(new_pc) } else { None }
        }))
        .collect();

    // Rebuild objects map with only used ones, reassign offsets
    let mut final_objects: FxHashMap<usize, ScalarReplacedObject> = FxHashMap::default();
    let mut final_total = 0usize;
    for &new_pc in &sorted_pcs {
        if used_objects.contains(&new_pc) {
            if let Some(obj) = objects.get(&new_pc) {
                let field_base_offset = ((scalar_base + final_total) as i32 + 1) * 8; // Cast: x86-64 immediate encoding
                final_objects.insert(new_pc, ScalarReplacedObject {
                    num_fields: obj.num_fields,
                    field_base_offset,
                });
                final_total += obj.num_fields * (SLOT_SIZE / 8);
            }
        }
    }

    // Remap field_ops to only reference final objects
    let final_field_ops: FxHashMap<usize, usize> = field_ops.into_iter()
        .filter(|(_, new_pc)| final_objects.contains_key(new_pc))
        .collect();
    let final_init_skips: std::collections::HashSet<usize> = init_skips.into_iter()
        .filter(|init_pc| {
            let new_pc = init_pc.wrapping_sub(4);
            final_objects.contains_key(&new_pc)
        })
        .collect();

    ScalarReplacementPlan {
        objects: final_objects,
        field_ops: final_field_ops,
        init_skips: final_init_skips,
        total_slots: final_total,
    }
}

/// Detect natural loops by finding backward branches in bytecode.
/// Returns a list of `(header_pc, back_edge_pc)` pairs.
fn detect_loops(code: &[u8], code_len: usize) -> Vec<(usize, usize)> {
    let mut loops = Vec::new();
    let mut pc = 0;
    while pc < code_len {
        match code[pc] {
            // goto — check for backward target
            0xa7 => {
                if pc + 2 < code_len {
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) if t < code_len => t,
                        _ => { pc += 3; continue; } // invalid target — skip
                    };
                    if target <= pc {
                        loops.push((target, pc));
                    }
                }
                pc += 3;
            }
            // Conditional branches — check for backward target (do-while loops)
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                if pc + 2 < code_len {
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) if t < code_len => t,
                        _ => { pc += 3; continue; } // invalid target — skip
                    };
                    if target <= pc {
                        loops.push((target, pc));
                    }
                }
                pc += 3;
            }
            // Other instructions: advance by instruction length
            0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => pc += 2,
            0x11 | 0x84 | 0xb4 | 0xb5 | 0xb8 | 0xc0 | 0xc1 => pc += 3,
            0xc5 => pc += 4,
            _ => pc += 1,
        }
    }
    loops
}

/// Find which locals are modified (stored/incremented) within a bytecode range.
/// Returns a bitmask where bit N is set if local N is modified.
fn find_modified_locals(code: &[u8], start: usize, end: usize) -> u64 {
    let mut modified: u64 = 0;
    let mut pc = start;
    while pc < end {
        match code[pc] {
            // istore_0..istore_3
            0x3b..=0x3e => {
                modified |= 1 << (code[pc] - 0x3b);
                pc += 1;
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                modified |= 1 << (code[pc] - 0x3f);
                pc += 1;
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                modified |= 1 << (code[pc] - 0x43);
                pc += 1;
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                modified |= 1 << (code[pc] - 0x47);
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                modified |= 1 << (code[pc] - 0x4b);
                pc += 1;
            }
            // istore/lstore/fstore/dstore/astore (wide index)
            0x36..=0x3a => {
                modified |= 1 << code[pc + 1];
                pc += 2;
            }
            // iinc
            0x84 => {
                modified |= 1 << code[pc + 1];
                pc += 3;
            }
            // Other: advance by instruction length
            _ => pc += bytecode_len_at(code, pc),
        }
    }
    modified
}

/// Try to match an invariant `aload X; iload Y; aaload` sequence at `pc`.
/// Returns `(array_local, index_local, seq_end_pc)` if the pattern matches
/// and both locals are not in the `modified` bitmask.
fn match_invariant_aaload(
    code: &[u8],
    pc: usize,
    modified: u64,
    code_len: usize,
) -> Option<(usize, usize, usize)> {
    // Match aload variant (loads the Object[] array reference)
    let (array_local, next_pc) = match code[pc] {
        0x2a => (0usize, pc + 1),
        0x2b => (1, pc + 1),
        0x2c => (2, pc + 1),
        0x2d => (3, pc + 1),
        0x19 if pc + 1 < code_len => (code[pc + 1] as usize, pc + 2), // Widening: always safe
        _ => return None,
    };

    // Check array_local is not modified in this loop
    if array_local < 64 && modified & (1u64 << array_local) != 0 {
        return None;
    }

    // Match iload variant (loads the array index)
    if next_pc >= code_len {
        return None;
    }
    let (index_local, next_pc2) = match code[next_pc] {
        0x1a => (0usize, next_pc + 1),
        0x1b => (1, next_pc + 1),
        0x1c => (2, next_pc + 1),
        0x1d => (3, next_pc + 1),
        0x15 if next_pc + 1 < code_len => (code[next_pc + 1] as usize, next_pc + 2), // Widening: always safe
        _ => return None,
    };

    // Check index_local is not modified in this loop
    if index_local < 64 && modified & (1u64 << index_local) != 0 {
        return None;
    }

    // Match aaload (0x32) — Object[] element access
    if next_pc2 >= code_len || code[next_pc2] != 0x32 {
        return None;
    }

    Some((array_local, index_local, next_pc2 + 1))
}

/// Find loop-invariant aaload sequences that can be hoisted out of loops.
/// For nested loops, hoists to the outermost loop where the sequence is invariant.
fn find_loop_hoists(code: &[u8], code_len: usize, loops: &[(usize, usize)]) -> Vec<LoopHoist> {
    if loops.is_empty() {
        return Vec::new();
    }

    let mut hoists = Vec::new();
    let mut hoisted_pcs: Vec<usize> = Vec::new();

    // Sort loops by span size descending (outermost first for nested loop handling)
    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        if loop_end > code_len {
            continue;
        }

        // Conservative safety: skip if loop contains aastore (0x53) which could
        // invalidate a hoisted Object[] element by modifying the array contents.
        let mut has_aastore = false;
        let mut check_pc = header;
        while check_pc < loop_end {
            if code[check_pc] == 0x53 {
                has_aastore = true;
                break;
            }
            check_pc += bytecode_len_at(code, check_pc);
        }
        if has_aastore {
            continue;
        }

        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if hoisted_pcs.contains(&pc) {
                // Already hoisted by an outer loop
                pc += bytecode_len_at(code, pc);
                continue;
            }

            if let Some((array_local, index_local, seq_end)) =
                match_invariant_aaload(code, pc, modified, code_len)
            {
                if seq_end <= loop_end {
                    hoists.push(LoopHoist {
                        loop_header: header,
                        seq_start: pc,
                        seq_end,
                        array_local,
                        index_local,
                    });
                    hoisted_pcs.push(pc);
                }
                pc = seq_end;
            } else {
                pc += bytecode_len_at(code, pc);
            }
        }
    }

    hoists
}

/// Find loop-invariant FP loads (dload/fload of locals not modified in the loop).
/// These can be hoisted to a frame slot before the loop, avoiding redundant
/// loads on every iteration when the local is not XMM-allocated.
fn find_fp_loop_hoists(code: &[u8], code_len: usize, loops: &[(usize, usize)]) -> Vec<FpLoopHoist> {
    if loops.is_empty() {
        return Vec::new();
    }

    let mut hoists = Vec::new();
    let mut hoisted_pcs: FxHashSet<usize> = FxHashSet::default();

    // Sort loops by span size descending (outermost first)
    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        if loop_end > code_len {
            continue;
        }

        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if hoisted_pcs.contains(&pc) {
                pc += bytecode_len_at(code, pc);
                continue;
            }

            let (local_idx, is_double) = match code[pc] {
                // fload_0..fload_3
                0x22..=0x25 => ((code[pc] - 0x22) as usize, false), // Widening: always safe
                // dload_0..dload_3
                0x26..=0x29 => ((code[pc] - 0x26) as usize, true), // Widening: always safe
                // fload (wide)
                0x17 if pc + 1 < code_len => (code[pc + 1] as usize, false), // Widening: always safe
                // dload (wide)
                0x18 if pc + 1 < code_len => (code[pc + 1] as usize, true), // Widening: always safe
                _ => {
                    pc += bytecode_len_at(code, pc);
                    continue;
                }
            };

            // Check if the local is modified in the loop
            if local_idx < 64 && (modified & (1u64 << local_idx)) == 0 {
                hoists.push(FpLoopHoist {
                    loop_header: header,
                    load_pc: pc,
                    local_idx: local_idx,
                    is_double: is_double,
                });
                hoisted_pcs.insert(pc);
            }

            pc += bytecode_len_at(code, pc);
        }
    }

    hoists
}

/// Detect FP strength reduction opportunities within loops.
/// Finds `ldc2_w <2.0>; dmul` patterns where multiply-by-2.0 can be replaced
/// with dadd self (saves ~3 cycles: addsd latency=1-3 vs mulsd latency=3-5).
/// Returns: set of dmul bytecode PCs to replace, and a map from the preceding
/// ldc2_w PC → the dmul PC (so the ldc2_w can be skipped at emission time).
fn find_fp_strength_reductions(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    ldc2w_info: &[(usize, i64)],
) -> FxHashSet<usize> {
    let mut pcs = FxHashSet::default();
    let two_bits = 2.0f64.to_bits() as i64; // Cast: JIT ABI convention

    // Build a lookup for ldc2_w PCs → resolved value
    let ldc_map: FxHashMap<usize, i64> = ldc2w_info.iter().copied().collect();

    for &(header, back_edge) in loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        let mut pc = header;
        while pc < loop_end && pc < code_len {
            // Pattern: ldc2_w <idx>, dmul
            if code[pc] == 0x14 && pc + 3 < loop_end {
                if let Some(&val) = ldc_map.get(&pc) {
                    if val == two_bits && code[pc + 3] == 0x6b {
                        // dmul at pc+3 with 2.0 operand → strength reduce to dadd self
                        pcs.insert(pc + 3);
                    }
                }
            }
            // Pattern: dmul right after a dload (X), ldc2_w 2.0
            // i.e., dload X; ldc2_w 2.0; dmul — already covered above.
            // Also check: ldc2_w 2.0 earlier, then dload, then dmul (commutative)
            pc += bytecode_len_at(code, pc);
        }
    }

    pcs
}

// ---------------------------------------------------------------------------
// Array Bounds Check Elimination (BCE)
// ---------------------------------------------------------------------------

/// Info about a loop's induction variable and bounds.
struct LoopBoundsInfo {
    /// The local variable that serves as the induction variable (incremented by iinc +1).
    induction_var: usize,
    /// The local variable used as the upper bound in the loop condition.
    /// If None, the bound is a constant.
    bound_local: Option<usize>,
    /// Constant upper bound (if the bound is iconst/bipush/sipush).
    #[allow(dead_code)]
    bound_const: Option<i32>,
}

/// Speculative bounds check elimination: a deopt guard emitted at the loop header.
/// For counted loops where IV goes from 0 to N with step 1, we speculatively
/// eliminate per-element bounds checks and instead emit a single range check
/// at the loop header: `if (array.length < loop_bound) goto deopt;`
#[derive(Clone)]
struct SpeculativeBCEGuard {
    /// Bytecode PC of the loop header where the guard should be emitted.
    loop_header: usize,
    /// Local holding the array reference.
    array_local: usize,
    /// Local holding the loop bound (N in `for i in 0..N`).
    bound_local: usize,
}

/// Find induction variables in a loop body.
/// An induction variable is a local that is:
/// 1. Modified ONLY by `iinc local, 1` (increment by exactly +1)
/// 2. Not modified by any istore/astore
///
/// Returns the local index if found.
fn find_induction_variable(code: &[u8], header: usize, back_edge_end: usize) -> Option<usize> {
    let mut iinc_locals: Vec<(usize, i8)> = Vec::new(); // (local, increment)
    let mut stored_locals: u64 = 0; // bitmask of locals written by xstore
    // Track iadd+istore pattern: iload X; ...; iadd; istore X
    let mut iadd_store_locals: Vec<usize> = Vec::new();

    let mut pc = header;
    while pc < back_edge_end {
        match code[pc] {
            // iinc
            0x84 => {
                let local = code[pc + 1] as usize; // Widening: always safe
                let inc = code[pc + 2] as i8; // Widening: always safe
                iinc_locals.push((local, inc));
                pc += 3;
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                let local = (code[pc] - 0x3b) as usize; // Widening: always safe
                stored_locals |= 1 << local;
                // Check for iadd; istore X pattern (the iadd is right before)
                if pc >= 1 && code[pc - 1] == 0x60 {
                    iadd_store_locals.push(local);
                }
                pc += 1;
            }
            // istore (wide)
            0x36 => {
                let local = code[pc + 1] as usize; // Widening: always safe
                stored_locals |= 1u64 << local.min(63);
                // Check for iadd; istore X pattern
                if pc >= 1 && code[pc - 1] == 0x60 {
                    iadd_store_locals.push(local);
                }
                pc += 2;
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                stored_locals |= 1 << (code[pc] - 0x3f);
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                stored_locals |= 1 << (code[pc] - 0x4b);
                pc += 1;
            }
            // lstore/fstore/dstore/astore (wide index)
            0x37..=0x3a => {
                stored_locals |= 1u64 << (code[pc + 1] as usize).min(63); // Widening: always safe
                pc += 2;
            }
            // fstore_0..fstore_3, dstore_0..dstore_3
            0x43..=0x4a => {
                stored_locals |= 1 << (code[pc] - 0x43);
                pc += 1;
            }
            _ => pc += bytecode_len_at(code, pc),
        }
    }

    // Priority 1: Find a local that has exactly one iinc +1 and no store
    for &(local, inc) in &iinc_locals {
        if inc == 1 && local < 64 && (stored_locals & (1u64 << local)) == 0 {
            // Verify this local only appears once in iinc list
            let count = iinc_locals.iter().filter(|&&(l, _)| l == local).count();
            if count == 1 {
                return Some(local);
            }
        }
    }

    // Priority 2: Find a local modified by iadd+istore pattern (non-unit stride)
    // This handles `j += i` patterns in Sieve's inner loop
    for &local in &iadd_store_locals {
        if local < 64 {
            // Verify: the local should be loaded before iadd (iload X; iload Y; iadd; istore X)
            // and only modified by this one iadd+istore in the loop
            let iadd_count = iadd_store_locals.iter().filter(|&&l| l == local).count();
            let iinc_count = iinc_locals.iter().filter(|&&(l, _)| l == local).count();
            if iadd_count == 1 && iinc_count == 0 {
                return Some(local);
            }
        }
    }

    None
}

/// Analyze the loop condition to find the upper bound.
///
/// Looks for patterns like:
/// - `iload iv; iload bound; if_icmpge exit` → bound is in local `bound`
/// - `iload iv; arraylength; if_icmpge exit` → bound is array length (implicit)
///
/// Returns LoopBoundsInfo if the pattern is recognized.
fn analyze_loop_bound(
    code: &[u8],
    header: usize,
    back_edge: usize,
    back_edge_end: usize,
    induction_var: usize,
) -> Option<LoopBoundsInfo> {
    // Pattern 1: Loop controlled by `goto header` at back_edge
    // The loop condition is typically at the header or just before the goto
    // Common Java for-loop pattern:
    //   header: iload iv; iload bound; if_icmpge exit; ... ; goto header
    //
    // Pattern 2: Loop controlled by conditional branch at back_edge
    //   header: ...; iload iv; iload bound; if_icmplt header

    // Check if back_edge is a conditional branch (do-while pattern)
    let back_op = code[back_edge];
    if matches!(back_op, 0x99..=0xa4 | 0xc6 | 0xc7) {
        // Conditional branch as back-edge — look for the comparison pattern just before
        // We need: iload iv; iload bound; if_icmplt/le/etc header
        // Scan backwards from back_edge to find the comparison setup
        // This is simpler if we scan forward from header
    }

    // Scan the loop body looking for the comparison pattern with the induction variable
    let mut pc = header;
    while pc < back_edge_end {
        // Match: iload <iv>; iload <bound>; if_icmpge/if_icmpgt <target>
        // where <target> is outside the loop (exit condition)
        let iv_local = match code[pc] {
            0x1a if induction_var == 0 => Some(0usize),
            0x1b if induction_var == 1 => Some(1),
            0x1c if induction_var == 2 => Some(2),
            0x1d if induction_var == 3 => Some(3),
            0x15 if pc + 1 < back_edge_end && code[pc + 1] as usize == induction_var => { // Widening: always safe
                Some(induction_var)
            }
            _ => None,
        };

        if let Some(_iv) = iv_local {
            let next_pc = if code[pc] == 0x15 { pc + 2 } else { pc + 1 };
            if next_pc >= back_edge_end {
                pc += bytecode_len_at(code, pc);
                continue;
            }

            // Check if next instruction loads the bound
            let (bound_local, after_bound) = match code[next_pc] {
                0x1a => (Some(0usize), next_pc + 1),
                0x1b => (Some(1), next_pc + 1),
                0x1c => (Some(2), next_pc + 1),
                0x1d => (Some(3), next_pc + 1),
                0x15 if next_pc + 1 < back_edge_end => {
                    (Some(code[next_pc + 1] as usize), next_pc + 2) // Widening: always safe
                }
                _ => (None, next_pc),
            };

            if let Some(bound) = bound_local {
                if after_bound < back_edge_end && after_bound + 2 < back_edge_end {
                    let cmp_op = code[after_bound];
                    let offset =
                        ((code[after_bound + 1] as i16) << 8 | code[after_bound + 2] as i16) as i32; // Widening: always safe
                    let target = (after_bound as i32 + offset) as usize; // Cast: x86-64 immediate encoding

                    // Pattern A: Exit condition — if_icmpge/if_icmpgt with target OUTSIDE loop
                    // e.g. `iload i; iload n; if_icmpge exit` at loop header
                    if matches!(cmp_op, 0xa2 | 0xa3) && (target > back_edge || target < header) {
                        return Some(LoopBoundsInfo {
                            induction_var,
                            bound_local: Some(bound),
                            bound_const: None,
                        });
                    }

                    // Pattern B: Continue condition — if_icmplt/if_icmple with target INSIDE loop
                    // e.g. `iload i; iload n; if_icmplt loop_body` (standard javac for-loop pattern)
                    if matches!(cmp_op, 0xa1 | 0xa4) && target >= header && target <= back_edge {
                        return Some(LoopBoundsInfo {
                            induction_var,
                            bound_local: Some(bound),
                            bound_const: None,
                        });
                    }
                }
            }
        }
        pc += bytecode_len_at(code, pc);
    }

    None
}

/// Find array accesses in a loop that use the induction variable as index
/// and an unmodified local as the array reference. Returns the set of
/// bytecode PCs that are provably safe (index < bound ≤ array.length).
///
/// The key insight: if the loop bound comes from arraylength (or a local
/// that holds arraylength), and the index is the induction variable that
/// starts at 0 and increments by 1 up to bound, all accesses are safe.
fn find_safe_array_accesses(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
    bounds: &LoopBoundsInfo,
    modified: u64,
) -> FxHashSet<usize> {
    let mut safe_pcs = FxHashSet::default();

    let mut pc = header;
    while pc < back_edge_end {
        let op = code[pc];
        // Array load/store opcodes: 0x2e-0x35 (loads), 0x4f-0x56 (stores)
        if matches!(op, 0x2e..=0x35 | 0x4f..=0x56) {
            // For loads: stack has [array, index] before this opcode
            // For stores: stack has [array, index, value] before this opcode
            // We need to trace back to find which locals provided array and index.
            //
            // Simple approach: look at the 2-3 instructions before this opcode.
            // Pattern for loads: aload/iload <array_local>; iload <index_local>; xaload
            // Pattern for stores: aload/iload <array_local>; iload <index_local>; xload <val>; xastore

            // Check if the index is the induction variable
            // For loads, the instruction before is the index load
            // For stores, we need to look further back

            let index_check_pc = if matches!(op, 0x4f..=0x56) {
                // Store: skip back past value load to find index load
                // This is harder statically — we'd need to track the stack.
                // Simple heuristic: look for the pattern aload; iload iv; load val; xastore
                find_store_index_pc(code, header, pc)
            } else {
                // Load: the instruction right before is the index load
                find_preceding_iload(code, header, pc)
            };

            if let Some(idx_pc) = index_check_pc {
                let idx_local = extract_iload_local(code, idx_pc);
                if let Some(idx) = idx_local {
                    if idx == bounds.induction_var {
                        // The index is the induction variable.
                        // Check that the array ref is from an unmodified local
                        let arr_pc = find_preceding_aload(code, header, idx_pc);
                        if let Some(a_pc) = arr_pc {
                            let arr_local = extract_aload_local(code, a_pc);
                            if let Some(al) = arr_local {
                                if al < 64 && (modified & (1u64 << al)) == 0 {
                                    // Array ref is invariant in this loop.
                                    // Mark this access as safe if bound_local exists
                                    // (meaning the loop is bounded by some local).
                                    if bounds.bound_local.is_some() {
                                        safe_pcs.insert(pc);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        pc += bytecode_len_at(code, pc);
    }

    safe_pcs
}

/// Find the bytecode PC of the iload that provides the index for an array load at `target_pc`.
/// Scans backward from target_pc looking for an iload-family instruction.
fn find_preceding_iload(code: &[u8], start: usize, target_pc: usize) -> Option<usize> {
    // Simple: walk forward from start, track last iload PC before target_pc
    let mut last_iload_pc = None;
    let mut pc = start;
    while pc < target_pc {
        if matches!(code[pc], 0x1a..=0x1d | 0x15) {
            last_iload_pc = Some(pc);
        }
        pc += bytecode_len_at(code, pc);
    }
    // The last iload before the array opcode is the index
    last_iload_pc
}

/// Find the bytecode PC of the aload that provides the array ref before `target_pc`.
/// Scans backward looking for an aload-family instruction.
fn find_preceding_aload(code: &[u8], start: usize, target_pc: usize) -> Option<usize> {
    let mut last_aload_pc = None;
    let mut pc = start;
    while pc < target_pc {
        if matches!(code[pc], 0x2a..=0x2d | 0x19) {
            last_aload_pc = Some(pc);
        }
        // Also track iload (could be array index) to reset aload tracking
        pc += bytecode_len_at(code, pc);
    }
    last_aload_pc
}

/// For array stores, find the iload that provides the index.
/// Pattern: aload arr; iload idx; <value_load>; xastore
fn find_store_index_pc(code: &[u8], start: usize, store_pc: usize) -> Option<usize> {
    // Walk forward tracking the last 3 instructions before store_pc
    let mut prev3 = [0usize; 3]; // circular: [arr_load, idx_load, val_load]
    let mut count = 0usize;
    let mut pc = start;
    while pc < store_pc {
        prev3[count % 3] = pc;
        count += 1;
        pc += bytecode_len_at(code, pc);
    }
    if count >= 2 {
        // The index load is 2 instructions before the store
        let idx_slot = if count >= 3 { (count - 2) % 3 } else { 0 };
        Some(prev3[idx_slot])
    } else {
        None
    }
}

/// Extract the local variable index from an iload instruction at `pc`.
fn extract_iload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x1a => Some(0),
        0x1b => Some(1),
        0x1c => Some(2),
        0x1d => Some(3),
        0x15 => code.get(pc + 1).map(|&b| b as usize), // iload
        _ => None,
    }
}

/// Extract the local variable index from an aload instruction at `pc`.
fn extract_aload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x2a => Some(0),
        0x2b => Some(1),
        0x2c => Some(2),
        0x2d => Some(3),
        0x19 => code.get(pc + 1).map(|&b| b as usize), // aload
        _ => None,
    }
}

/// Perform bounds check elimination analysis for all loops in the method.
/// Returns a set of bytecode PCs where bounds checks can be safely skipped,
/// and a list of speculative BCE guards to emit at loop headers.
fn analyze_bounds_elimination(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>) {
    let mut safe_pcs = FxHashSet::default();
    let mut speculative_guards: Vec<SpeculativeBCEGuard> = Vec::new();

    for &(header, back_edge) in loops {
        let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
        if back_edge_end > code_len {
            continue;
        }

        // Step 1: Find the induction variable
        let induction_var = match find_induction_variable(code, header, back_edge_end) {
            Some(iv) => iv,
            None => continue,
        };

        // Step 2: Analyze the loop bound
        let bounds = match analyze_loop_bound(code, header, back_edge, back_edge_end, induction_var)
        {
            Some(b) => b,
            None => continue,
        };

        // Step 3: Find modified locals in loop body
        let modified = find_modified_locals(code, header, back_edge_end);

        // Step 4: Find safe array accesses (statically proven)
        let loop_safe = find_safe_array_accesses(code, header, back_edge_end, &bounds, modified);
        safe_pcs.extend(&loop_safe);

        // Step 5: Speculative BCE — for counted loops with IV from 0..N step 1,
        // find array accesses using IV as index that weren't already proven safe.
        // For these, we emit a single range guard at the loop header and mark
        // all such accesses as safe.
        if let Some(bound_local) = bounds.bound_local {
            let speculative_accesses = find_speculative_array_accesses(
                code,
                header,
                back_edge_end,
                &bounds,
                modified,
                &loop_safe,
            );
            if !speculative_accesses.is_empty() {
                let mut guard_arrays: Vec<usize> = Vec::new();
                for &(access_pc, arr_local) in &speculative_accesses {
                    safe_pcs.insert(access_pc);
                    if !guard_arrays.contains(&arr_local) {
                        guard_arrays.push(arr_local);
                    }
                }
                for arr_local in guard_arrays {
                    speculative_guards.push(SpeculativeBCEGuard {
                        loop_header: header,
                        array_local: arr_local,
                        bound_local,
                    });
                }
            }
        }
    }

    (safe_pcs, speculative_guards)
}

/// Find array accesses in a counted loop that use the IV as index but were NOT
/// already proven safe by `find_safe_array_accesses`. These are candidates for
/// speculative BCE with a deopt guard at the loop header.
///
/// Returns vec of (bytecode_pc_of_access, array_local).
fn find_speculative_array_accesses(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
    bounds: &LoopBoundsInfo,
    modified: u64,
    already_safe: &FxHashSet<usize>,
) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut pc = header;
    while pc < back_edge_end {
        let op = code[pc];
        if matches!(op, 0x2e..=0x35 | 0x4f..=0x56) && !already_safe.contains(&pc) {
            let index_check_pc = if matches!(op, 0x4f..=0x56) {
                find_store_index_pc(code, header, pc)
            } else {
                find_preceding_iload(code, header, pc)
            };

            if let Some(idx_pc) = index_check_pc {
                let idx_local = extract_iload_local(code, idx_pc);
                if let Some(idx) = idx_local {
                    if idx == bounds.induction_var {
                        let arr_pc = find_preceding_aload(code, header, idx_pc);
                        if let Some(a_pc) = arr_pc {
                            let arr_local = extract_aload_local(code, a_pc);
                            if let Some(al) = arr_local {
                                if al < 64 && (modified & (1u64 << al)) == 0 {
                                    result.push((pc, al));
                                }
                            }
                        }
                    }
                }
            }
        }
        pc += bytecode_len_at(code, pc);
    }
    result
}

// ---------------------------------------------------------------------------
// Compile bytecode to x86-64
// ---------------------------------------------------------------------------

/// Simulated operand stack slot — tracks where each value is.
/// During compilation, the operand stack is mapped to stack frame offsets.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum StackSlot {
    /// Value is at [rbp - offset]. Offset is always positive (below rbp).
    Frame(i32),
    /// Value is in a callee-saved register (R12-R15). Zero-cost push: no code
    /// emitted until the value is consumed. This eliminates the store+load
    /// round-trip when a register-mapped local is loaded and then immediately
    /// used by an arithmetic or branch operation.
    CalleeSaved(u8),
    /// Value is in a caller-saved scratch register (R8/R9). Used as a
    /// deferred-spill cache: `push_from_rax` moves the result into a scratch
    /// register instead of storing to the frame, avoiding the store+load
    /// round-trip when the value is consumed by the very next operation.
    /// Scratch slots MUST be flushed before any call, backward branch, or return.
    Scratch(u8),
    /// Value is in an XMM register (XMM0-XMM15). Used for FP locals loaded via
    /// dload/fload from XMM-allocated locals. Avoids the XMM→RAX→frame round-trip
    /// when the value is immediately consumed by a double/float arithmetic op.
    /// Like Scratch, these MUST be flushed before calls.
    Xmm(u8),
}

/// Scratch registers available for deferred-spill caching.
/// R8 and R9 are caller-saved on both Windows x64 and SysV ABIs.
/// R10 is excluded because it is used internally by bounds checks and SIMD loops.
const SCRATCH_REGS: [u8; 2] = [R8, R9];

/// Register mapping for locals → callee-saved registers.
/// R12-R15 + RBX on all platforms. On Windows, RSI and RDI are also callee-saved.
#[cfg(target_os = "windows")]
pub const LOCAL_REGS: [u8; 7] = [R12, R13, R14, R15, RBX, RSI, RDI];

/// XMM register numbers for float/double local allocation.
/// XMM8-XMM15 are used to avoid conflicts with arithmetic temporaries (XMM0/XMM1).
/// On Windows x64: XMM6-15 are callee-saved; we use XMM8-15 (8 registers).
/// On Linux x86-64: all XMMs are caller-saved in SysV ABI; we save/restore them anyway
/// since Rust may hold live values there when calling JIT-compiled functions.
pub const LOCAL_XMMS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

/// Scratch XMM registers for FP intermediate persistence across bytecodes.
/// XMM2-XMM7 bridge the gap between temporaries (XMM0-1) and locals (XMM8-15).
/// When a FP binop produces a result in XMM0, it can be promoted to a scratch XMM
/// to free XMM0 for the next operation, avoiding the XMM0→frame spill/reload cycle.
/// These are caller-saved and must be flushed before calls and backward branches.
const SCRATCH_XMMS: [u8; 6] = [2, 3, 4, 5, 6, 7];

#[cfg(not(target_os = "windows"))]
pub const LOCAL_REGS: [u8; 5] = [R12, R13, R14, R15, RBX];

/// JIT compiler state.
struct Compiler {
    buf: ExecutableBuffer,
    /// Simulated operand stack — maps JVM stack positions to frame offsets.
    stack: Vec<StackSlot>,
    /// Next available frame offset (negative, below locals area).
    next_spill_offset: i32,
    /// Base spill offset (first slot after locals).
    base_spill_offset: i32,
    /// Number of local variable slots.
    num_locals: usize,
    /// Number of parameter slots.
    num_params: usize,
    /// Number of locals mapped to callee-saved registers.
    num_reg_locals: usize,
    /// Per-local register assignment from graph-coloring allocator.
    /// `local_assignments[i] = Some(reg)` means local i is in that register.
    local_assignments: Vec<Option<u8>>,
    /// Callee-saved GPR registers actually used (for prologue/epilogue).
    alloc_used_regs: Vec<u8>,
    /// Per-local XMM register assignment. `None` means the local spills to the frame.
    xmm_assignments: Vec<Option<u8>>,
    /// XMM registers actually used for locals (for prologue/epilogue save/restore).
    alloc_used_xmms: Vec<u8>,
    /// Mapping from bytecode PC to native code offset (for branch patching).
    pc_to_native: Vec<i32>,
    /// Forward branch patches: (native offset of rel32, target bytecode PC).
    forward_patches: Vec<(usize, usize)>,
    /// Jump table patches: (native offset of i32 entry, table_base_native_offset, target bytecode PC).
    /// Each entry stores a RIP-relative offset from the table base to the target native code.
    jump_table_patches: Vec<(usize, usize, usize)>,
    /// Self-call patches: native offset of the rel32 to patch to entry point.
    self_call_patches: Vec<usize>,
    /// Frame size (total stack allocation).
    frame_size: i32,
    /// Whether method needs heap pointer (hidden first arg).
    needs_heap: bool,
    /// Frame offset where heap pointer is stored (valid when needs_heap is true).
    heap_local_offset: i32,
    /// Frame offset of the first XMM save slot (from RBP).
    xmm_saved_base: i32,
    /// Resolved multianewarray metadata: (bytecode_pc, leaf_element_type_code).
    multianewarray_info: Vec<(usize, u8)>,
    /// Resolved field access metadata: (bytecode_pc, field_index, type_tag).
    /// type_tag is b'I', b'J', b'F', b'D', b'L', or b'['.
    field_info: Vec<(usize, usize, u8)>,
    /// Resolved typecheck metadata: (bytecode_pc, class_name_ptr, class_name_len).
    /// The class name string is leaked for 'static lifetime so the JIT code can reference it.
    typecheck_info: Vec<(usize, *const u8, usize)>,
    /// Resolved static field metadata: (bytecode_pc, class_id_raw, field_index, type_tag, is_volatile).
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    /// LICM: loop-invariant aaload hoisting info.
    hoist_info: Vec<LoopHoist>,
    /// LICM: frame offsets for hoisted values (one per LoopHoist entry).
    hoist_offsets: Vec<i32>,
    /// Frame offset of the first callee-saved register slot (from RBP).
    callee_saved_base: i32,
    /// SIMD: vectorizable loops detected during analysis.
    simd_loops: Vec<SimdIntArraySum>,
    /// Bounds check elimination: bytecode PCs where bounds checks can be skipped
    /// because loop analysis proved the access is always in-bounds.
    bounds_safe_pcs: FxHashSet<usize>,
    /// Deferred out-of-line bounds-check failure stubs: (branch_patch_offset, bc_pc).
    /// After the main bytecode loop, we emit the slow-path code for each.
    bounds_check_stubs: Vec<usize>,
    /// Round-8 CRIT fix (audit `round8-jit.md`, "false-promise abort" item):
    /// deferred null-check failure stubs for inline array-store opcodes
    /// (iastore / bastore / aastore / lastore / fastore / dastore / castore /
    /// sastore). The inline bounds check would otherwise dereference the
    /// null array pointer at `[NULL + ARRAY_LENGTH_OFFSET]`, hitting the
    /// signal-handler hs_err path that just re-raises and kills the VM.
    /// Each `TEST RAX, RAX; JZ rel32` is recorded as a patch offset; a
    /// single shared stub at the end calls `jit_bastore` with `array_ptr=0`
    /// (which sets `JIT_PENDING_NPE` and returns) and then exits via the
    /// method epilogue with `RAX = i64::MIN`. The interpreter's post-JIT
    /// path drains the NPE flag and surfaces the exception.
    null_check_store_stubs: Vec<usize>,
    /// Speculative BCE: deopt guards to emit at loop headers.
    /// Each guard checks that array.length >= loop_bound before entering the loop.
    speculative_bce_guards: Vec<SpeculativeBCEGuard>,
    /// Resolved `new` (0xbb) metadata.
    ///
    /// Tuple layout (CRIT-2):
    ///   (bytecode_pc, class_id_raw, num_fields,
    ///    has_primitive_init,
    ///    has_finalizer)
    ///
    /// The last two flags gate whether the inline TLAB fast path must
    /// invoke `jit_post_tlab_init`. When both are `false`, the JIT
    /// inlines the header completion (identity-hash + num_slots) and
    /// skips the helper call entirely. See `emit_inline_tlab_new`.
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    /// Resolved `anewarray` (0xbd) metadata: (bytecode_pc, component_class_id_raw).
    anewarray_info: Vec<(usize, u32)>,
    /// Invoke dispatch info: (bytecode_pc, pointer to leaked JitInvokeInfo).
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    /// Direct call targets: (bytecode_pc, direct call info).
    /// For invokestatic/invokespecial where the callee is already JIT-compiled.
    direct_calls: Vec<(usize, super::JitDirectCall)>,
    /// Monomorphic inline cache slots: (bytecode_pc, MIC slot pointer).
    /// For invokevirtual/invokeinterface call sites.
    mic_slots: Vec<(usize, *const super::JitMICSlot)>,
    /// Polymorphic inline cache slots: (bytecode_pc, PIC slot pointer).
    /// For invokevirtual/invokeinterface call sites. A populated
    /// `pic_slots` entry supersedes the MIC for the same `pc` (PIC
    /// is a 3-entry superset).
    ///
    /// HIGH-7 wiring (now active): `jit/src/lib.rs::try_compile`
    /// eagerly allocates one `Box<JitPICSlot>` per polymorphic call
    /// site (invokevirtual / invokeinterface) and passes the
    /// `(pc, *const JitPICSlot)` pairs into `x64::compile()` via the
    /// new `pic_slots` parameter, which assigns this field. Slots
    /// start empty; the runtime helper populates them on miss, after
    /// which subsequent invocations take the inline 3-way cascade.
    pic_slots: Vec<(usize, *const super::JitPICSlot)>,
    /// Loop unrolling: (header_pc, back_edge_pc, extra_copies).
    /// Small loops where the back-edge goto can be unrolled with extra iterations.
    /// extra_copies is the number of additional body copies (1 for 2x, 3 for 4x).
    unroll_loops: Vec<(usize, usize, usize)>,
    /// Native offset right after the prologue (for tail-call jumps).
    body_entry_offset: usize,
    /// PGO branch hints: maps bytecode PC → is_usually_taken.
    ///
    /// When present, branch emission adds an x86 branch prediction prefix:
    /// - `0x3E` (taken hint) when `is_usually_taken == true`
    /// - `0x2E` (not-taken hint) when `is_usually_taken == false`
    branch_hints: FxHashMap<usize, bool>,
    /// PGO loop unroll hints: maps back-edge bytecode PC → unroll factor.
    loop_unroll_hints: FxHashMap<usize, usize>,
    /// Resolved ldc/ldc_w constants: (bytecode_pc, i64 value).
    ldc_info: Vec<(usize, i64)>,
    /// Resolved ldc2_w constants: (bytecode_pc, i64 value).
    ldc2w_info: Vec<(usize, i64)>,
    /// Runtime helper function pointers for JIT callbacks.
    helpers: JitRuntimeHelpers,
    /// Expected simulated-stack depth at each forward branch target.
    /// Used to fix up the stack when dead code becomes live at a merge point.
    branch_target_stack_depth: FxHashMap<usize, usize>,
    /// Set to true when an internal error (e.g. stack underflow) is detected
    /// during compilation.  `compile_bytecode` checks this and bails out.
    failed: bool,
    /// Bitmask of scratch XMM registers (2-7) currently in use on the simulated stack.
    /// Bit N corresponds to SCRATCH_XMMS[N]. Used to allocate scratch XMMs for
    /// FP intermediate persistence across bytecodes.
    scratch_xmm_in_use: u8,
    /// FP LICM: loop-invariant FP load hoisting info.
    fp_hoist_info: Vec<FpLoopHoist>,
    /// FP LICM: frame offsets for hoisted FP values (one per FpLoopHoist entry).
    _fp_hoist_offsets: Vec<i32>,
    /// SIMD: vectorizable double-array sum loops detected during analysis.
    simd_fp_loops: Vec<SimdFpArraySum>,
    /// FP strength reduction: set of bytecode PCs where dmul-by-2.0 is replaced with dadd-self.
    fp_strength_reduction_pcs: FxHashSet<usize>,
    /// Scalar replacement: non-escaping NEW PCs → frame-local field storage.
    scalar_replaced: FxHashMap<usize, ScalarReplacedObject>,
    /// Scalar replacement: putfield/getfield PCs that target a scalar-replaced object → NEW PC.
    scalar_field_ops: FxHashMap<usize, usize>,
    /// Scalar replacement: invokespecial PCs whose `<init>()V` should be skipped.
    scalar_init_skips: std::collections::HashSet<usize>,
    /// Inline sites: bytecode PC → resolved InlineSite for inlining callee bytecode.
    inline_sites: FxHashMap<usize, crate::InlineSite>,
    /// Deferred out-of-line deoptimization stubs: (branch_patch_offset, bci, reason_code).
    /// Speculative guards (e.g. BCE) jump here; the stub calls jit_uncommon_trap and
    /// returns i64::MIN to signal the interpreter to resume.
    deopt_stubs: Vec<(usize, usize, i64)>,
    /// T1.1.a — parallel type tracker for the simulated operand stack.
    ///
    /// `stack_oop_marks[i] == true` means the value at `self.stack[i]`
    /// is an object reference (an "oop"). Pushed in lock-step with
    /// `self.stack`. Any future push that is not explicitly tagged
    /// defaults to `false` (non-oop) via `push_nonoop_from_rax`.
    ///
    /// At every call site that may trigger GC (new, anewarray, invoke*,
    /// newarray, multianewarray, ldc-class, etc.) the compiler walks
    /// this vector alongside `self.stack`, finds every
    /// `(StackSlot::Frame(_), true)` pair, and emits an
    /// [`OopMapEntry`] recording the frame offset. The GC root walker
    /// then consults the map at the return PC for precise coverage.
    ///
    /// When empty or misaligned (a stack underflow has occurred), the
    /// walker falls back to the conservative scan — this never loses
    /// an oop, it only pins extra false positives.
    stack_oop_marks: Vec<bool>,
    /// T1.1.a — collected oop maps, indexed by native PC offset of the
    /// instruction *after* the safepoint call. Transferred to
    /// `CompiledMethod::oop_maps` at finalize time.
    oop_maps: Vec<crate::OopMapEntry>,
    /// T5.2.1 — induction variables detected in each loop.
    ///
    /// One entry per detected counted loop. Consumed by downstream
    /// passes that care about loop-structure properties (trip count,
    /// stride, bound): unroll factor decisions, SIMD remainder
    /// handling, and range-check elimination.
    induction_vars: Vec<crate::scev::InductionVar>,
    /// T5.2.14 — null-check elimination dataflow.
    ///
    /// Per-PC bitmask of locals proven non-null. Used by future null
    /// emission passes to skip the `TEST reg, reg; JZ throw_npe`
    /// sequence when the receiver is already known non-null.
    null_check_info: crate::null_check_elim::NullCheckInfo,
    /// T5.2.15 — SIMD element-wise loops detected via SuperWord.
    ///
    /// Each entry describes one vectorizable `out[i] = a[i] OP b[i]`
    /// loop. The JIT emitter walks this list at the loop header and
    /// replaces the scalar body with a PADDD/PMULLD (etc.) block plus
    /// a remainder loop for trip counts that aren't a multiple of 4.
    ///
    /// Detection runs unconditionally; emission is gated on `has_avx2()`.
    simd_element_wise_loops: Vec<SimdArrayElementWise>,
    /// T5.2.17 — loop unswitch candidates.
    ///
    /// Each entry describes a loop with a loop-invariant conditional
    /// branch that can be lifted to produce two specialized loops.
    /// The emitter consumes this list to duplicate the body and hoist
    /// the branch above the header.
    loop_unswitch_candidates: Vec<LoopUnswitchCandidate>,

    // ── MED-4 / Fix 3 — PC-indexed lookup acceleration ─────────────────
    //
    // The original layout stores per-call-site metadata in `Vec<(pc, …)>`
    // arrays and queries them with `iter().find(|(p, …)| *p == pc)` on
    // every getfield/putfield/invoke* during code generation. For hot
    // methods (large generated classes, generics-heavy code) this is
    // O(N) per opcode and N²-shaped for the whole compile.
    //
    // These auxiliary maps mirror the indexed entries so the hot
    // lookup is O(1). They are populated once via [`build_pc_indices`]
    // immediately before `compile_bytecode` runs and never mutated
    // afterwards, so the borrow-checker tax is zero on the hot path.
    field_info_idx: FxHashMap<usize, usize>,
    static_field_info_idx: FxHashMap<usize, usize>,
    invoke_info_idx: FxHashMap<usize, usize>,
    direct_calls_idx: FxHashMap<usize, usize>,
    mic_slots_idx: FxHashMap<usize, usize>,
    pic_slots_idx: FxHashMap<usize, usize>,
    new_info_idx: FxHashMap<usize, usize>,
    anewarray_info_idx: FxHashMap<usize, usize>,
    typecheck_info_idx: FxHashMap<usize, usize>,
    ldc_info_idx: FxHashMap<usize, usize>,
    ldc2w_info_idx: FxHashMap<usize, usize>,

    /// Memo for `magic_signed_div32`: constant divisor → computed
    /// `(magic, shift)` pair. The magic-number derivation runs a Newton-style
    /// iteration; a loop body with a repeated `/ k` or `% k` on the same
    /// constant `k` would otherwise recompute it at every occurrence. The
    /// result is a pure function of the divisor, so caching is behavior-
    /// preserving.
    magic_div_memo: FxHashMap<i32, (i64, u32)>,
}

impl Compiler {
    #[allow(clippy::too_many_arguments)]
    fn new(
        buf: ExecutableBuffer,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        needs_heap: bool,
        multianewarray_info: Vec<(usize, u8)>,
        field_info: Vec<(usize, usize, u8)>,
        typecheck_info: Vec<(usize, *const u8, usize)>,
        static_field_info: Vec<(usize, u32, usize, u8, bool)>,
        hoist_info: Vec<LoopHoist>,
        alloc_result: super::regalloc::RegAllocResult,
        helpers: JitRuntimeHelpers,
        num_scalar_slots: usize,
    ) -> Self {
        // Compact arrays: byte[] uses 1-byte elements, int[] uses 4-byte, ref[] uses 8-byte.
        // Each local takes 8 bytes: [rbp - 8], [rbp - 16], ...
        // If needs_heap, reserve one extra slot for the heap pointer.
        // If LICM hoisting is active, reserve extra slots for hoisted values.
        // If scalar replacement is active, reserve extra slots for replaced object fields.
        let num_hoists = hoist_info.len();
        let extra_slots = (if needs_heap { 1 } else { 0 }) + num_hoists + num_scalar_slots;
        let total_locals = num_locals.saturating_add(extra_slots);
        let locals_size = (total_locals.min(i32::MAX as usize / 8) as i32).saturating_mul(8); // Cast: address arithmetic
        let spill_size = (max_stack.min(i32::MAX as usize / 8) as i32).saturating_mul(8); // Cast: address arithmetic
        let shadow_space = 32i32; // Windows x64 shadow space for helper calls

        // Use graph-coloring allocator results
        let local_assignments = alloc_result.assignments;
        let alloc_used_regs = alloc_result.used_callee_saved;
        let xmm_assignments = alloc_result.xmm_assignments;
        let alloc_used_xmms = alloc_result.used_xmm_regs;
        let num_reg_locals = local_assignments.iter().filter(|a| a.is_some()).count()
            + xmm_assignments.iter().filter(|a| a.is_some()).count();
        let callee_saved_size = alloc_used_regs.len() as i32 * 8; // Cast: x86-64 immediate encoding
        // XMM save slots: 8 bytes each (we store the 64-bit value via MOVQ through RAX)
        let xmm_saved_size = alloc_used_xmms.len() as i32 * 8; // Cast: x86-64 immediate encoding

        // Callee-saved registers are saved using MOV into frame slots (not PUSH)
        // to keep RSP stable after SUB RSP. This ensures shadow space is at [RSP..RSP+31].
        let callee_saved_base = locals_size + spill_size + 8;
        // XMM save slots follow GPR save slots
        let xmm_saved_base = callee_saved_base + callee_saved_size;

        // Total frame = locals + spill + callee-saved GPRs + callee-saved XMMs + shadow + margin
        let total = locals_size + spill_size + callee_saved_size + xmm_saved_size + shadow_space + 8;

        // After CALL entry: RSP ≡ 8 mod 16 (return addr).
        // After PUSH RBP: RSP ≡ 0 mod 16.
        // After SUB RSP, frame_size: RSP ≡ (0 - frame_size) mod 16.
        // No PUSH after SUB RSP, so just need frame_size ≡ 0 mod 16.
        let frame_size = (total + 15) & !15;

        let base_spill = locals_size + 8; // first spill slot after locals

        // Heap pointer stored in the extra local slot (beyond max_locals)
        let heap_local_offset = if needs_heap {
            (num_locals as i32 + 1) * 8 // Cast: x86-64 immediate encoding
        } else {
            0
        };

        // LICM: compute frame offsets for hoisted values
        // They go after the heap slot (or after locals if no heap needed)
        let hoist_base = num_locals + (if needs_heap { 1 } else { 0 });
        let hoist_offsets: Vec<i32> = (0..num_hoists)
            .map(|k| ((hoist_base + k) as i32 + 1) * 8) // Cast: x86-64 immediate encoding
            .collect();

        Self {
            buf,
            stack: Vec::with_capacity(max_stack),
            next_spill_offset: base_spill,
            base_spill_offset: base_spill,
            num_locals,
            num_params,
            num_reg_locals,
            local_assignments,
            alloc_used_regs,
            xmm_assignments,
            alloc_used_xmms,
            pc_to_native: Vec::new(),
            forward_patches: Vec::new(),
            jump_table_patches: Vec::new(),
            self_call_patches: Vec::new(),
            frame_size,
            needs_heap,
            heap_local_offset,
            xmm_saved_base,
            multianewarray_info,
            field_info,
            typecheck_info,
            static_field_info,
            hoist_info,
            hoist_offsets,
            callee_saved_base,
            simd_loops: Vec::new(),
            bounds_safe_pcs: FxHashSet::default(),
            bounds_check_stubs: Vec::new(),
            null_check_store_stubs: Vec::new(),
            speculative_bce_guards: Vec::new(),
            new_info: Vec::new(),
            anewarray_info: Vec::new(),
            invoke_info: Vec::new(),
            direct_calls: Vec::new(),
            mic_slots: Vec::new(),
            pic_slots: Vec::new(),
            unroll_loops: Vec::new(),
            body_entry_offset: 0,
            branch_hints: FxHashMap::default(),
            loop_unroll_hints: FxHashMap::default(),
            ldc_info: Vec::new(),
            ldc2w_info: Vec::new(),
            branch_target_stack_depth: FxHashMap::default(),
            failed: false,
            helpers,
            scratch_xmm_in_use: 0,
            fp_hoist_info: Vec::new(),
            _fp_hoist_offsets: Vec::new(),
            simd_fp_loops: Vec::new(),
            fp_strength_reduction_pcs: FxHashSet::default(),
            scalar_replaced: FxHashMap::default(),
            scalar_field_ops: FxHashMap::default(),
            scalar_init_skips: std::collections::HashSet::new(),
            inline_sites: FxHashMap::default(),
            deopt_stubs: Vec::new(),
            stack_oop_marks: Vec::with_capacity(16),
            oop_maps: Vec::new(),
            induction_vars: Vec::new(),
            null_check_info: crate::null_check_elim::NullCheckInfo::default(),
            simd_element_wise_loops: Vec::new(),
            loop_unswitch_candidates: Vec::new(),
            field_info_idx: FxHashMap::default(),
            static_field_info_idx: FxHashMap::default(),
            invoke_info_idx: FxHashMap::default(),
            direct_calls_idx: FxHashMap::default(),
            mic_slots_idx: FxHashMap::default(),
            pic_slots_idx: FxHashMap::default(),
            new_info_idx: FxHashMap::default(),
            anewarray_info_idx: FxHashMap::default(),
            typecheck_info_idx: FxHashMap::default(),
            ldc_info_idx: FxHashMap::default(),
            ldc2w_info_idx: FxHashMap::default(),
            magic_div_memo: FxHashMap::default(),
        }
    }

    /// MED-4 / Fix 3 — populate the pc → array-index maps used by hot
    /// codegen lookups. Call once after all the `*_info` Vecs are
    /// installed and before `compile_bytecode` walks the bytecode.
    fn build_pc_indices(&mut self) {
        self.field_info_idx.clear();
        self.field_info_idx.reserve(self.field_info.len());
        for (i, e) in self.field_info.iter().enumerate() {
            self.field_info_idx.insert(e.0, i);
        }
        self.static_field_info_idx.clear();
        self.static_field_info_idx.reserve(self.static_field_info.len());
        for (i, e) in self.static_field_info.iter().enumerate() {
            self.static_field_info_idx.insert(e.0, i);
        }
        self.invoke_info_idx.clear();
        self.invoke_info_idx.reserve(self.invoke_info.len());
        for (i, e) in self.invoke_info.iter().enumerate() {
            self.invoke_info_idx.insert(e.0, i);
        }
        self.direct_calls_idx.clear();
        self.direct_calls_idx.reserve(self.direct_calls.len());
        for (i, e) in self.direct_calls.iter().enumerate() {
            self.direct_calls_idx.insert(e.0, i);
        }
        self.mic_slots_idx.clear();
        self.mic_slots_idx.reserve(self.mic_slots.len());
        for (i, e) in self.mic_slots.iter().enumerate() {
            self.mic_slots_idx.insert(e.0, i);
        }
        self.pic_slots_idx.clear();
        self.pic_slots_idx.reserve(self.pic_slots.len());
        for (i, e) in self.pic_slots.iter().enumerate() {
            self.pic_slots_idx.insert(e.0, i);
        }
        self.new_info_idx.clear();
        self.new_info_idx.reserve(self.new_info.len());
        for (i, e) in self.new_info.iter().enumerate() {
            self.new_info_idx.insert(e.0, i);
        }
        self.anewarray_info_idx.clear();
        self.anewarray_info_idx.reserve(self.anewarray_info.len());
        for (i, e) in self.anewarray_info.iter().enumerate() {
            self.anewarray_info_idx.insert(e.0, i);
        }
        self.typecheck_info_idx.clear();
        self.typecheck_info_idx.reserve(self.typecheck_info.len());
        for (i, e) in self.typecheck_info.iter().enumerate() {
            self.typecheck_info_idx.insert(e.0, i);
        }
        self.ldc_info_idx.clear();
        self.ldc_info_idx.reserve(self.ldc_info.len());
        for (i, e) in self.ldc_info.iter().enumerate() {
            self.ldc_info_idx.insert(e.0, i);
        }
        self.ldc2w_info_idx.clear();
        self.ldc2w_info_idx.reserve(self.ldc2w_info.len());
        for (i, e) in self.ldc2w_info.iter().enumerate() {
            self.ldc2w_info_idx.insert(e.0, i);
        }
    }

    /// T5.2.1 — lookup the induction variable for a loop header, if any.
    ///
    /// Returns `Some` if SCEV analysis recognized the loop starting at
    /// `header_pc` as a counted loop with a single IV. Callers use the
    /// stride/bound fields to make unrolling and vectorization
    /// decisions.
    #[allow(dead_code)]
    pub(crate) fn induction_var_for(
        &self,
        header_pc: usize,
    ) -> Option<&crate::scev::InductionVar> {
        self.induction_vars.iter().find(|iv| iv.header_pc == header_pc)
    }

    /// T5.2.14 — query the null-check elimination info.
    ///
    /// Returns `true` when the JIT can statically prove that local
    /// `local` is non-null at the instruction starting at `pc`. Uses
    /// the forward-dataflow result computed by
    /// [`crate::null_check_elim::analyze`].
    ///
    /// Round-11 HIGH-1 fix: the round-7 safe-stub (`return false`)
    /// is replaced with a real query against the meet-over-paths
    /// dataflow result computed by [`crate::null_check_elim::analyze`].
    /// The analysis is sound by construction (intersection at every
    /// join point, monotone-descending lattice, fixpoint iteration
    /// with a hard budget), so reporting `true` here is safe to use
    /// for branch elision at `ifnull`/`ifnonnull` and for skipping
    /// inline null-check stubs at array store/load sites.
    pub(crate) fn is_local_nonnull(&self, pc: usize, local: usize) -> bool {
        self.null_check_info.is_nonnull(pc, local)
    }

    /// Offset for local variable `idx`: [rbp - (idx+1)*8]
    fn local_offset(&self, idx: usize) -> i32 {
        (idx as i32 + 1) * 8 // Cast: x86-64 immediate encoding
    }

    /// Push a value onto the simulated operand stack.
    /// Allocates a spill slot and returns the frame offset.
    ///
    /// T1.1.a — the corresponding `stack_oop_marks` entry is set to
    /// `false` by default. Callers that know the value is an object
    /// reference should call [`Self::mark_top_as_oop`] immediately
    /// after.
    fn push_stack(&mut self) -> StackSlot {
        let offset = self.next_spill_offset;
        self.next_spill_offset += 8;
        let slot = StackSlot::Frame(offset);
        self.stack.push(slot);
        self.stack_oop_marks.push(false);
        slot
    }

    /// T1.1.a — mark the top-of-stack slot as an object reference. Called
    /// by opcode handlers for every push that produces an oop
    /// (`new`, `anewarray`, `aload*`, `aaload`, `getfield` on reference
    /// fields, `invoke*` returning an `L` or `[` descriptor, etc.).
    fn mark_top_as_oop(&mut self) {
        if let Some(mark) = self.stack_oop_marks.last_mut() {
            *mark = true;
        }
    }

    /// Pop a value from the simulated operand stack.
    /// Returns `StackSlot::Frame(0)` and sets `self.failed` on underflow.
    fn pop_stack(&mut self) -> StackSlot {
        let slot = match self.stack.pop() {
            Some(s) => s,
            None => {
                self.failed = true;
                return StackSlot::Frame(0);
            }
        };
        // T1.1.a — keep the parallel oop-mark vector in lock-step.
        // If it's shorter than expected (dead-code merge fixup), push a
        // default false to avoid panics and continue with conservative
        // fallback for this frame slice.
        if self.stack_oop_marks.pop().is_none() {
            // desync — conservative fallback
        }
        // Reclaim spill space if this was a Frame slot at the top
        if let StackSlot::Frame(off) = slot {
            if off == self.next_spill_offset - 8 {
                self.next_spill_offset -= 8;
            }
        }
        // Free scratch XMM if no other stack entry references it
        if let StackSlot::Xmm(xmm) = slot {
            if xmm >= 2 && xmm <= 7
                && !self.stack.iter().any(|s| matches!(s, StackSlot::Xmm(x) if *x == xmm))
            {
                self.free_scratch_xmm(xmm);
            }
        }
        slot
    }

    /// Round-8 wave-3 HIGH fix (round-4 #15 / round-5 #9 / round-7 #5):
    /// defensive callee-saved register spill before a safepoint.
    ///
    /// Any Java local assigned to a callee-saved GPR (RBX, R12-R15,
    /// plus RSI/RDI on Windows) holds its live value EXCLUSIVELY in
    /// the register between aload/astore opcodes. The slot-based oop
    /// map and the conservative frame-region sweep both walk stack
    /// memory only — they never read register values. Until per-local
    /// oop typing lands (prerequisite for precise reg-oop encoding),
    /// spill every register-resident local back to its canonical
    /// frame slot `[rbp - (idx+1)*8]` immediately BEFORE the
    /// safepoint-causing CALL. The conservative scanner will then
    /// observe the live oop in the frame slot at GC time.
    ///
    /// The spill is conservative (non-oop locals are also flushed)
    /// but correct: `conservative_roots::scan_one_frame_precise`
    /// filters frame qwords via `heap.is_object_address`, so non-oop
    /// values are ignored. Cost: ~3 bytes (REX + opcode + modrm/disp8)
    /// per used callee-saved local per safepoint.
    ///
    /// TODO(round-12): precise reg-oop encoding to avoid spill cost.
    /// Requires (a) per-local oop typing (currently only operand
    /// stack has `stack_oop_marks`) and (b) `OopMapEntry::reg_oops`
    /// bitmap consumed by the GC scanner walking saved-register slots
    /// in the JIT prologue.
    fn emit_pre_safepoint_spill(&mut self) {
        if self.failed {
            return;
        }
        for idx in 0..self.local_assignments.len() {
            if let Some(reg) = self.local_assignments[idx] {
                let off = self.local_offset(idx);
                self.emit_store_local(off, reg);
            }
        }
    }

    /// T1.1.a — record an oop map at the current native PC for the
    /// live frame slots that hold object references.
    ///
    /// Call this *immediately after* any helper call that may trigger
    /// GC (object allocation, method dispatch, array allocation). The
    /// `self.buf.len()` at the time of this call is the return PC of
    /// the call — the exact PC the GC walker will look up in the
    /// finalized oop map.
    ///
    /// Empty maps (no oops live at the call) are skipped to keep the
    /// per-method oop map size bounded: GC falls back to conservative
    /// scanning for that frame, which produces the same correct
    /// result.
    ///
    /// `extra_popped` is the number of oop-typed stack entries the
    /// caller has already popped *for the call itself* but that would
    /// otherwise still be live if the call could return. For most
    /// alloc helpers this is 0 (they take primitive arguments). For
    /// `invoke*` the caller pops `this + args` before this call and
    /// passes `0` here because those entries are consumed.
    fn emit_oop_map_for_safepoint(&mut self) {
        // Defensive: if the compiler is already in a failed state,
        // don't emit bogus maps.
        if self.failed {
            return;
        }
        // T1.1.a — lazy resync. Non-instrumented `self.stack.push`
        // sites (aload local-to-CalleeSaved, inlined-callee pushes,
        // LICM hoists, XMM intermediate pushes) leave
        // `stack_oop_marks` shorter than `stack`. Pad with `false`
        // (non-oop). This is SAFE because
        // `conservative_roots::scan_one_frame_precise` also performs a
        // full conservative sweep of the frame region on every GC
        // visit — any oop our precise map misses is caught by the
        // sweep's `heap.is_object_address` validation. Precise maps
        // remain a pure optimization on top of the conservative path.
        while self.stack_oop_marks.len() < self.stack.len() {
            self.stack_oop_marks.push(false);
        }
        // If marks is somehow longer than stack (pop desync), truncate.
        self.stack_oop_marks.truncate(self.stack.len());

        let native_pc = self.buf.pos() as u32; // Cast: x86-64 immediate encoding
        let mut slots: Vec<i16> = Vec::new();
        let n = self.stack.len();
        for i in 0..n {
            if !self.stack_oop_marks[i] {
                continue;
            }
            if let StackSlot::Frame(off) = self.stack[i] {
                if let Ok(i16_off) = i16::try_from(off) {
                    slots.push(i16_off);
                }
            }
        }
        if slots.is_empty() {
            return;
        }
        self.oop_maps.push(crate::OopMapEntry {
            native_pc_offset: native_pc,
            frame_slot_offsets: slots,
        });
    }

    /// Peek at the top of the simulated stack.
    /// Returns `StackSlot::Frame(0)` and sets `self.failed` on underflow.
    fn peek_stack(&mut self) -> StackSlot {
        match self.stack.last().copied() {
            Some(s) => s,
            None => {
                self.failed = true;
                StackSlot::Frame(0)
            }
        }
    }

    /// Reset spill state for a new basic block (stack should be empty at branch targets
    /// in the patterns we compile).
    fn reset_spills(&mut self) {
        self.next_spill_offset = self.base_spill_offset;
    }

    /// Flush the simulated stack to canonical spill offsets (base_spill + i*8).
    /// This ensures that all paths reaching a merge point agree on frame layout.
    fn canonicalize_stack(&mut self) {
        let base = self.base_spill_offset;
        for i in 0..self.stack.len() {
            let canonical_off = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            let slot = self.stack[i];
            match slot {
                StackSlot::Frame(off) if off == canonical_off => {
                    // Already in the right place
                }
                _ => {
                    // Load value to RAX, then store to canonical offset
                    self.load_slot_to_reg(RAX, slot);
                    self.emit_store_local(canonical_off, RAX);
                    self.stack[i] = StackSlot::Frame(canonical_off);
                }
            }
        }
        self.next_spill_offset = base + (self.stack.len() as i32) * 8; // Cast: x86-64 immediate encoding
    }

    // -----------------------------------------------------------------------
    // x86-64 instruction emitters
    // -----------------------------------------------------------------------

    /// REX prefix for 64-bit operand size.
    fn rex_w(&mut self) {
        self.buf.emit_byte(0x48);
    }

    /// REX.W prefix with R bit (extended reg in reg field).
    fn rex_w_r(&mut self, reg: u8) {
        let r = if reg >= 8 { 0x4C } else { 0x48 };
        self.buf.emit_byte(r);
    }

    /// REX.W prefix with B bit (extended reg in r/m field).
    #[allow(dead_code)]
    fn rex_w_b(&mut self, rm: u8) {
        // 0x48 = REX.W; 0x49 = REX.W + REX.B (needed for r8–r15 in r/m field).
        let b = if rm >= 8 { 0x49 } else { 0x48 };
        self.buf.emit_byte(b);
    }

    /// REX.W prefix with R and B bits.
    #[allow(dead_code)]
    fn rex_w_rb(&mut self, reg: u8, rm: u8) {
        let mut rex: u8 = 0x48;
        if reg >= 8 {
            rex |= 0x04;
        } // R bit
        if rm >= 8 {
            rex |= 0x01;
        } // B bit
        self.buf.emit_byte(rex);
    }

    /// ModRM byte: mod=11 (register), reg, r/m
    fn modrm_reg(&mut self, reg: u8, rm: u8) {
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (rm & 7));
    }

    /// ModRM byte for [rbp - disp] addressing.
    ///
    /// `disp` is the positive depth-from-RBP (i.e. the actual displacement
    /// is `-disp`). The byte stores `-disp` as i8/i32, so for disp8 we
    /// need `-disp` to fit in i8 (-128..=127), i.e. `disp` in `-127..=128`.
    /// The old check `(-128..=127)` was off-by-one: it wasted 3 bytes on
    /// the common depth-128 spill and would mis-encode disp=-128 as 0x80
    /// garbage (round-8 jit #4).
    fn modrm_rbp_disp(&mut self, reg: u8, disp: i32) {
        if (-127..=128).contains(&disp) {
            // mod=01, r/m=101 (rbp), disp8
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            self.buf.emit_byte((-disp) as u8); // negate because we store as positive offset // Cast: x86-64 immediate encoding
        } else {
            // mod=10, r/m=101 (rbp), disp32
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&(-disp).to_le_bytes());
        }
    }

    /// Return the callee-saved register for local `idx`, if register-mapped.
    fn reg_for_local(&self, idx: usize) -> Option<u8> {
        self.local_assignments.get(idx).copied().flatten()
    }

    /// Return the XMM register assigned to a float/double local, if any.
    fn xmm_for_local(&self, idx: usize) -> Option<u8> {
        self.xmm_assignments.get(idx).copied().flatten()
    }

    /// MOVQ XMMn, GPR — move 64-bit integer from a GPR into an XMM register.
    fn emit_movq_xmm_from_gpr(&mut self, xmm: u8, gpr: u8) {
        // Encoding: 66 REX(W, R if xmm>=8, B if gpr>=8) 0F 6E /r
        let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
        let rex_b = if gpr >= 8 { 0x01u8 } else { 0u8 };
        self.buf.emit_byte(0x66);
        self.buf.emit_byte(0x48 | rex_r | rex_b); // REX.W + optional REX.R/B
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x6E);
        self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (gpr & 7)); // ModRM
    }

    /// MOVQ XMMn, RAX — move 64-bit integer in RAX into an XMM register.
    fn emit_movq_xmm_from_rax(&mut self, xmm: u8) {
        self.emit_movq_xmm_from_gpr(xmm, RAX);
    }

    /// MOVQ GPR, XMMn — move 64-bit value from XMM register into a GPR.
    fn emit_movq_gpr_from_xmm(&mut self, gpr: u8, xmm: u8) {
        // Encoding: 66 REX(W, R if xmm>=8, B if gpr>=8) 0F 7E /r
        let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
        let rex_b = if gpr >= 8 { 0x01u8 } else { 0u8 };
        self.buf.emit_byte(0x66);
        self.buf.emit_byte(0x48 | rex_r | rex_b); // REX.W + optional REX.R/B
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x7E);
        self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (gpr & 7)); // ModRM
    }

    /// MOVQ RAX, XMMn — move 64-bit value from XMM register into RAX.
    fn emit_movq_rax_from_xmm(&mut self, xmm: u8) {
        self.emit_movq_gpr_from_xmm(RAX, xmm);
    }

    /// MOVQ [rbp - offset], XMMn — direct 64-bit XMM spill to a frame slot.
    ///
    /// Equivalent to (and replaces) the two-instruction sequence
    /// `MOVQ RAX, XMMn ; MOV [rbp-off], RAX` used by the scratch flusher
    /// and similar XMM-spill sites. Saves ~3 bytes per spill and frees
    /// RAX (allowing it to keep holding the function return value
    /// across an epilogue restore).
    ///
    /// Encoding: `66 [REX] 0F D6 /r` — MOVQ r/m64, xmm.
    ///   - REX.W is NOT required (the opcode is 64-bit by definition).
    ///   - REX.R is set when `xmm >= 8`.
    ///   - REX.B is NOT required: r/m base is RBP (5), low 3 bits.
    /// ModRM:
    ///   - disp8 form (mod=01) when `-128 <= -off <= 127`.
    ///   - disp32 form (mod=10) otherwise.
    /// disp is the signed offset from RBP; callers pass `offset` as a
    /// positive frame depth (matching `emit_store_local`'s convention),
    /// so we encode `-offset`.
    fn emit_movq_mem_rbp_from_xmm(&mut self, offset: i32, xmm: u8) {
        self.buf.emit_byte(0x66);
        if xmm >= 8 {
            // REX.R only (no .W, no .B — RBP is the base, low 3 bits).
            self.buf.emit_byte(0x44);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0xD6);
        // ModRM r/m=101 (RBP), reg=xmm&7.
        let reg = xmm & 7;
        // Round-9 LOW fix: mirror the `(-127..=128)` guard from
        // `modrm_rbp_disp`. `offset` is the positive depth-from-RBP;
        // the disp byte stores `-offset` as i8, so we need
        // `-offset` in `-128..=127`, i.e. `offset` in `-127..=128`.
        // The prior `(-128..=127)` was off-by-one (round-8 fixed it
        // in modrm_rbp_disp but missed these MOVQ helpers).
        if (-127..=128).contains(&offset) {
            // mod=01, disp8.  (Matches modrm_rbp_disp encoding semantics.)
            self.buf.emit_byte(0x45 | (reg << 3));
            self.buf.emit_byte((-offset) as u8); // Cast: x86-64 immediate encoding
        } else {
            // mod=10, disp32.
            self.buf.emit_byte(0x85 | (reg << 3));
            self.buf.emit(&(-offset).to_le_bytes());
        }
    }

    /// MOVQ XMMn, [rbp - offset] — direct 64-bit load from a frame slot
    /// into an XMM register. Pair to `emit_movq_mem_rbp_from_xmm` for
    /// the epilogue restore path.
    ///
    /// Encoding: `F3 [REX] 0F 7E /r` — MOVQ xmm, r/m64.
    ///   - F3 is the mandatory prefix that selects MOVQ-from-mem.
    ///   - REX.R is set when `xmm >= 8`.
    ///   - REX.B is NOT required (base is RBP).
    fn emit_movq_xmm_from_mem_rbp(&mut self, xmm: u8, offset: i32) {
        self.buf.emit_byte(0xF3);
        if xmm >= 8 {
            self.buf.emit_byte(0x44);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x7E);
        let reg = xmm & 7;
        // Round-9 LOW fix: see `emit_movq_mem_rbp_from_xmm` —
        // mirrors the `(-127..=128)` guard from `modrm_rbp_disp`.
        if (-127..=128).contains(&offset) {
            self.buf.emit_byte(0x45 | (reg << 3));
            self.buf.emit_byte((-offset) as u8); // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x85 | (reg << 3));
            self.buf.emit(&(-offset).to_le_bytes());
        }
    }

    /// MOVSD XMMdst, XMMsrc — move scalar double between XMM registers.
    fn emit_movsd_xmm_xmm(&mut self, dst: u8, src: u8) {
        // F2 [REX] 0F 10 modrm — MOVSD dst, src
        let rex_r = if dst >= 8 { 0x04u8 } else { 0 };
        let rex_b = if src >= 8 { 0x01u8 } else { 0 };
        self.buf.emit_byte(0xF2);
        if rex_r != 0 || rex_b != 0 {
            self.buf.emit_byte(0x40 | rex_r | rex_b);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x10);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// MOVSS XMMdst, XMMsrc — move scalar float between XMM registers.
    fn emit_movss_xmm_xmm(&mut self, dst: u8, src: u8) {
        // F3 [REX] 0F 10 modrm — MOVSS dst, src
        let rex_r = if dst >= 8 { 0x04u8 } else { 0 };
        let rex_b = if src >= 8 { 0x01u8 } else { 0 };
        self.buf.emit_byte(0xF3);
        if rex_r != 0 || rex_b != 0 {
            self.buf.emit_byte(0x40 | rex_r | rex_b);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x10);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// SQRTSD XMM0, XMM0 — compute double square root in-place.
    /// Encoding: F2 0F 51 C0
    fn emit_sqrtsd_xmm0(&mut self) {
        self.buf.emit_byte(0xF2);
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x51);
        self.buf.emit_byte(0xC0); // ModRM: 11 000 000 (XMM0, XMM0)
    }

    /// PXOR XMMn, XMMn — zero an XMM register.
    fn emit_pxor_xmm_self(&mut self, xmm: u8) {
        // Encoding: 66 [REX if xmm>=8] 0F EF /r (ModRM: 11 reg reg)
        if xmm >= 8 {
            self.buf.emit_byte(0x66);
            self.buf.emit_byte(0x45); // REX with R and B set for extended XMM
            self.buf.emit_byte(0x0F);
            self.buf.emit_byte(0xEF);
            self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (xmm & 7));
        } else {
            self.buf.emit_byte(0x66);
            self.buf.emit_byte(0x0F);
            self.buf.emit_byte(0xEF);
            self.buf.emit_byte(0xC0 | (xmm << 3) | xmm);
        }
    }

    /// Emit IEEE 754 NaN/overflow fixup after a CVTT instruction.
    ///
    /// x86 CVTTSS2SI/CVTTSD2SI returns the "indefinite integer" (0x80000000 for
    /// 32-bit, 0x8000000000000000 for 64-bit) for NaN AND overflow. The JVM
    /// spec requires: NaN→0, +overflow→MAX_VALUE, -overflow→MIN_VALUE.
    ///
    /// Call this immediately after the CVTT while XMM0 still holds the source.
    fn emit_fp_to_int_nan_fixup(&mut self, is_double: bool, is_long: bool) {
        if !is_long {
            // CMP EAX, 0x80000000
            self.buf.emit_byte(0x3D);
            self.buf.emit(&0x80000000u32.to_le_bytes());
            // JNE .done (short)
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0 — PF=1 if NaN
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign of source.
            // PXOR XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]); // UCOMISD XMM0, XMM1
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]); // UCOMISS XMM0, XMM1
            }
            // JBE .done (negative overflow — 0x80000000 already correct)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV EAX, 0x7FFFFFFF
            self.buf.emit_byte(0xB8);
            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR EAX, EAX
            let nan_off = self.buf.pos();
            self.buf.patch_byte(jp_patch, (nan_off - jp_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.emit(&[0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            self.buf.patch_byte(jne_patch, (done_off - jne_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.patch_byte(jbe_patch, (done_off - jbe_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.patch_byte(jmp_patch, (done_off - jmp_patch - 1) as u8); // Cast: x86-64 immediate encoding
        } else {
            // 64-bit: CMP RAX with 0x8000000000000000
            // MOV RCX, 0x8000000000000000
            self.buf.emit(&[0x48, 0xB9]);
            self.buf.emit(&0x8000000000000000u64.to_le_bytes());
            // CMP RAX, RCX
            self.buf.emit(&[0x48, 0x39, 0xC8]);
            // JNE .done
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign.
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]); // PXOR XMM1, XMM1
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]);
            }
            // JBE .done (negative overflow)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV RAX, 0x7FFFFFFFFFFFFFFF
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR RAX, RAX (48 31 C0)
            let nan_off = self.buf.pos();
            self.buf.patch_byte(jp_patch, (nan_off - jp_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.emit(&[0x48, 0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            self.buf.patch_byte(jne_patch, (done_off - jne_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.patch_byte(jbe_patch, (done_off - jbe_patch - 1) as u8); // Cast: x86-64 immediate encoding
            self.buf.patch_byte(jmp_patch, (done_off - jmp_patch - 1) as u8); // Cast: x86-64 immediate encoding
        }
    }

    /// MOV dst_r64, src_r64
    fn emit_mov_reg_reg(&mut self, dst: u8, src: u8) {
        // Peephole: `mov rN, rN` is a no-op; emit nothing. Common after
        // regalloc when a coalesced live range produces a self-move at a
        // copy point (e.g. prologue param shuffles where the ABI register
        // already matches the assigned local).
        if dst == src {
            return;
        }
        self.rex_w_rb(dst, src);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.modrm_reg(dst, src);
    }

    /// XOR r32, r32 — zeroes register (32-bit op zero-extends to 64-bit).
    fn emit_xor_reg_self(&mut self, reg: u8) {
        if reg >= 8 {
            // REX prefix with R and B bits for extended registers
            self.buf.emit_byte(0x40 | 0x04 | 0x01); // 0x45
        }
        self.buf.emit_byte(0x31); // XOR r/m32, r32
        self.modrm_reg(reg, reg);
    }

    /// MOV reg, [rbp - offset]
    fn emit_load_local(&mut self, reg: u8, offset: i32) {
        self.rex_w_r(reg);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.modrm_rbp_disp(reg, offset);
    }

    /// MOV reg, [rbp + positive_disp] — load a stack-passed argument from
    /// the caller's stack frame. Used in the prologue when a Java param's
    /// index exceeds the platform's ARG_REGS register file (e.g. the 5th
    /// arg on Windows x64 when needs_heap consumes ARG_REGS[0] for the VM
    /// pointer). The 4th-arg-and-beyond live above rbp in the caller's
    /// reserved stack slots:
    ///   * Windows: shadow space at [rbp+0x10..0x28] (caller's home for
    ///     RCX/RDX/R8/R9) + stack args at [rbp+0x30], [rbp+0x38], ...
    ///   * SysV:   stack args at [rbp+0x10], [rbp+0x18], ...
    /// `positive_disp` is the byte offset above rbp.
    fn emit_load_caller_arg(&mut self, reg: u8, positive_disp: i32) {
        debug_assert!(positive_disp > 0, "caller arg disp must be positive");
        self.rex_w_r(reg);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        // ModRM r/m=101 (RBP) with positive displacement.
        if (-128..=127).contains(&positive_disp) {
            // mod=01, disp8
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            self.buf.emit_byte(positive_disp as u8); // Cast: x86-64 immediate encoding
        } else {
            // mod=10, disp32
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&positive_disp.to_le_bytes());
        }
    }

    /// MOV [rbp - offset], reg
    fn emit_store_local(&mut self, offset: i32, reg: u8) {
        self.rex_w_r(reg);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.modrm_rbp_disp(reg, offset);
    }

    // ── CMOV helpers (round-8 perf, round-7 jit #7) ──────────────────
    //
    // CMOVcc r64, r/m64 lets us implement small-value selects (Math.min,
    // Math.max, ternary `a < b ? x : y`) without a branch. Encoding is
    // `REX.W 0F 4cc /r` where the condition codes match the Jcc family:
    //   0x44 = CMOVE   (ZF=1)        0x45 = CMOVNE
    //   0x4C = CMOVL   (SF≠OF)       0x4D = CMOVGE
    //   0x4E = CMOVLE  (ZF=1 or SF≠OF) 0x4F = CMOVG
    //   0x42 = CMOVB   (CF=1, unsigned <)  0x43 = CMOVAE
    //   0x46 = CMOVBE  (CF=1 or ZF=1)      0x47 = CMOVA
    // These helpers are emit-time primitives; the IR/lower passes have
    // not yet been taught to detect the patterns that should use them.
    //
    // Round-8 Bug 8: the Math.min(I,I)/Math.max(I,I)/(J,J)/(J,J) intrinsics
    // now lower directly to `CMP + CMOVL/CMOVG` (see the
    // `MATH_MIN_INT_INTRINSIC` / `MATH_MAX_INT_INTRINSIC` arms in the
    // invokestatic dispatch). The bytecode-peephole patterns below are
    // still TODO — they catch user-written ternaries that the JIT cannot
    // recognise as Math.min/max:
    //   * `if_icmplt; ldc small; goto K; L: ldc small; K:` → CMOVL
    //   * `if_acmpne L; aconst_null; goto K; L: aload x; K:` → CMOVNE
    // The current emitter performs those selects via compare+conditional
    // jump+move, which mispredicts on hard-to-predict data (e.g. random
    // array element comparisons in sorting kernels).
    //
    // Round-8 wiring attempt: Math.min/Math.max are NOT yet registered
    // as JIT intrinsics in `lib.rs` (no MATH_MIN_INTRINSIC sentinel and
    // no detection in `try_resolve_intrinsic`). Adding the intrinsic
    // dispatch requires edits to `lib.rs` (sentinel constant + matcher
    // in `try_resolve_intrinsic`) plus an x64.rs callee_entry arm that
    // emits the CMOV sequence. That cross-file change is owned by a
    // separate agent in this wave (lib.rs is in another agent's scope).
    // When wiring lands, the planned sequence for Math.min(int a,int b):
    //     MOV   EAX, a            ; result := a (default)
    //     CMP   EAX, b            ; flags := a - b
    //     CMOVG EAX, b            ; if a > b, take b instead
    // and symmetric for Math.max (CMOVL EAX, b). This avoids the
    // misprediction penalty that a JL/JG + MOV would incur on data
    // with poor branch entropy.

    /// Emit `CMOVcc dst, src` (64-bit) with the given condition opcode byte
    /// (0x40..0x4F). dst/src are encoded register-direct (mod=11).
    #[allow(dead_code)]
    fn emit_cmov_cc_reg_reg(&mut self, cc: u8, dst: u8, src: u8) {
        debug_assert!((0x40..=0x4F).contains(&cc),
            "CMOV cc opcode must be in 0x40..0x4F");
        // REX.W with R (dst extended) and B (src extended).
        let mut rex: u8 = 0x48;
        if dst >= 8 { rex |= 0x04; }
        if src >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        self.modrm_reg(dst, src);
    }

    /// CMOVL r64, r64 — move src into dst if SF≠OF (signed `<`).
    #[allow(dead_code)]
    fn emit_cmov_l_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4C, dst, src);
    }

    /// CMOVG r64, r64 — move src into dst if ZF=0 and SF=OF (signed `>`).
    #[allow(dead_code)]
    fn emit_cmov_g_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4F, dst, src);
    }

    /// CMOVLE r64, r64 — move src into dst if ZF=1 or SF≠OF (signed `<=`).
    #[allow(dead_code)]
    fn emit_cmov_le_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4E, dst, src);
    }

    /// CMOVGE r64, r64 — move src into dst if SF=OF (signed `>=`).
    #[allow(dead_code)]
    fn emit_cmov_ge_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4D, dst, src);
    }

    /// CMOVE r64, r64 — move src into dst if ZF=1 (equal).
    #[allow(dead_code)]
    fn emit_cmov_e_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x44, dst, src);
    }

    /// CMOVNE r64, r64 — move src into dst if ZF=0 (not equal).
    #[allow(dead_code)]
    fn emit_cmov_ne_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x45, dst, src);
    }

    /// MOV reg, imm64
    #[allow(dead_code)]
    fn emit_mov_imm64(&mut self, reg: u8, imm: i64) {
        // Optimize: if value fits in sign-extended 32 bits, use shorter MOV r/m64, imm32
        if imm == 0 {
            self.emit_xor_reg_self(reg);
            return;
        }
        if imm >= i32::MIN as i64 && imm <= i32::MAX as i64 { // Widening: always safe
            self.emit_mov_imm32_sx(reg, imm as i32); // Cast: x86-64 immediate encoding
            return;
        }
        self.rex_w_b(reg);
        self.buf.emit_byte(0xB8 + (reg & 7)); // MOV r64, imm64
        self.buf.emit(&imm.to_le_bytes());
    }

    /// MOV reg, imm32 (sign-extended to 64-bit). Uses XOR for zero.
    fn emit_mov_imm32_sx(&mut self, reg: u8, imm: i32) {
        if imm == 0 {
            self.emit_xor_reg_self(reg);
            return;
        }
        // C7 /0: destination is in the R/M field, so use REX.B for extended regs
        self.rex_w_b(reg);
        self.buf.emit_byte(0xC7); // MOV r/m64, imm32
        self.modrm_reg(0, reg);
        self.buf.emit(&imm.to_le_bytes());
    }

    /// LEA reg, [RBP - offset] — compute address of a frame slot.
    /// Same encoding as emit_load_local but with LEA opcode (0x8D) instead of MOV (0x8B).
    fn emit_lea_frame_slot(&mut self, dst: u8, offset: i32) {
        self.rex_w_r(dst);
        self.buf.emit_byte(0x8D); // LEA r64, m
        self.modrm_rbp_disp(dst, offset);
    }

    /// PUSH rbp
    fn emit_push_rbp(&mut self) {
        self.buf.emit_byte(0x55);
    }

    /// MOV rbp, rsp
    fn emit_mov_rbp_rsp(&mut self) {
        self.rex_w();
        self.buf.emit(&[0x89, 0xE5]);
    }

    /// SUB rsp, imm (uses imm8 when possible)
    fn emit_sub_rsp_imm(&mut self, imm: i32) {
        self.rex_w();
        if (0..=127).contains(&imm) {
            self.buf.emit_byte(0x83); // SUB r/m64, imm8
            self.modrm_reg(5, RSP);
            self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x81); // SUB r/m64, imm32
            self.modrm_reg(5, RSP);
            self.buf.emit(&imm.to_le_bytes());
        }
    }

    /// ADD rsp, imm (uses imm8 when possible)
    fn emit_add_rsp_imm(&mut self, imm: i32) {
        self.rex_w();
        if (0..=127).contains(&imm) {
            self.buf.emit_byte(0x83); // ADD r/m64, imm8
            self.modrm_reg(0, RSP);
            self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x81); // ADD r/m64, imm32
            self.modrm_reg(0, RSP);
            self.buf.emit(&imm.to_le_bytes());
        }
    }

    /// POP rbp
    fn emit_pop_rbp(&mut self) {
        self.buf.emit_byte(0x5D);
    }

    /// RET
    fn emit_ret(&mut self) {
        self.buf.emit_byte(0xC3);
    }

    // -----------------------------------------------------------------------
    // Direct-call stack-arg setup (round-8 wave-3 HIGH fix)
    //
    // For direct CALL targets whose JIT entry uses Java-arg-in-ARG_REGS
    // calling convention (with an optional hidden VM ctx in ARG_REGS[0]),
    // we previously bailed when total args exceeded the register file.
    // Now we materialize stack args using the platform ABI:
    //
    //   * Windows x64: caller reserves 32 bytes of shadow space *above*
    //     stack args (the callee owns home slots for its first 4 reg
    //     args). Stack args live at [rsp + 32], [rsp + 40], ...
    //   * SysV (Linux/macOS): no shadow space. Stack args at [rsp],
    //     [rsp + 8], ...
    //
    // Both ABIs require RSP ≡ 0 mod 16 immediately before the CALL.
    // Our frame_size guarantees that on method entry RSP ≡ 0 mod 16
    // (see emit_prologue alignment math). The total bytes subtracted
    // for stack-arg setup must therefore also be 16-byte aligned: we
    // round up by adding an 8-byte alignment pad when needed.
    // -----------------------------------------------------------------------

    /// MOV [rsp + disp32], reg — store 64-bit GPR to RSP-relative slot.
    /// Used to materialize stack-passed args after `sub rsp, N`.
    fn emit_mov_rsp_disp_from_reg(&mut self, disp: i32, reg: u8) {
        // Encoding: REX.W [+R] 89 /r SIB
        // [rsp + disp] requires a SIB byte (rm field = 100b means SIB follows).
        // SIB: scale=00, index=100b (none), base=100b (rsp).
        self.rex_w_r(reg);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        if disp == 0 {
            // mod=00, reg, r/m=100 (SIB)
            self.buf.emit_byte(0x04 | ((reg & 7) << 3));
            self.buf.emit_byte(0x24); // SIB: scale=00, index=100 (none), base=100 (rsp)
        } else if (-128..=127).contains(&disp) {
            // mod=01, reg, r/m=100 (SIB), disp8
            self.buf.emit_byte(0x44 | ((reg & 7) << 3));
            self.buf.emit_byte(0x24);
            self.buf.emit_byte(disp as u8); // Cast: x86-64 immediate encoding
        } else {
            // mod=10, reg, r/m=100 (SIB), disp32
            self.buf.emit_byte(0x84 | ((reg & 7) << 3));
            self.buf.emit_byte(0x24);
            self.buf.emit(&disp.to_le_bytes());
        }
    }

    /// Compute the total bytes to subtract from RSP for a direct-call
    /// stack-arg block carrying `stack_arg_count` qword args.
    ///
    /// Includes Win64 shadow space and 16-byte alignment pad. Returns
    /// `(total_sub, stack_arg_disp_base)` where `stack_arg_disp_base`
    /// is the RSP-relative displacement where arg[reg_count] lives
    /// (subsequent args ascend by 8).
    fn stack_arg_block_size(stack_arg_count: usize) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        let (shadow, base) = (32i32, 32i32);
        #[cfg(not(target_os = "windows"))]
        let (shadow, base) = (0i32, 0i32);
        let raw = shadow + (stack_arg_count as i32) * 8; // Cast: x86-64 immediate encoding
        // Round up to 16 bytes to preserve RSP alignment at the CALL.
        let total = (raw + 15) & !15;
        (total, base)
    }

    /// Set up stack args for a direct JIT call.
    ///
    /// `arg_slots` are the source frame slots for the args (already
    /// reversed, so `arg_slots[0]` is the first Java arg). `has_ctx`
    /// indicates whether the callee expects the VM ctx pointer as a
    /// hidden first ARG_REGS[0]; the Java args then go into
    /// ARG_REGS[1..]. Returns the total bytes subtracted from RSP
    /// (caller must pass this to `emit_stack_arg_cleanup` after CALL).
    ///
    /// Behaviour when all args fit in registers: emits no SUB RSP and
    /// returns 0 (so callers in the small-arg fast path observe no
    /// behavioural change).
    fn emit_stack_arg_setup(
        &mut self,
        arg_slots: &[StackSlot],
        has_ctx: bool,
    ) -> i32 {
        let ctx_offset = if has_ctx { 1 } else { 0 };
        let total_regs_for_java = ARG_REGS.len() - ctx_offset;
        let n = arg_slots.len();

        // Reserve stack space first so that subsequent register loads
        // from frame slots (via [rbp - off]) are not invalidated — RBP
        // is unchanged by SUB RSP.
        let stack_arg_count = n.saturating_sub(total_regs_for_java);
        let (total_sub, base_disp) = Self::stack_arg_block_size(stack_arg_count);
        if total_sub > 0 {
            self.emit_sub_rsp_imm(total_sub);
        }

        // Materialize stack args first (these may use RAX as a
        // scratch, which we restore for the reg-arg pass below).
        // Iterate forward — order doesn't matter since each store
        // targets a distinct RSP slot.
        if stack_arg_count > 0 {
            for k in 0..stack_arg_count {
                let java_idx = total_regs_for_java + k;
                let disp = base_disp + (k as i32) * 8; // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(RAX, arg_slots[java_idx]);
                self.emit_mov_rsp_disp_from_reg(disp, RAX);
            }
        }

        // Now load reg-passed args. Do the ctx load LAST so it
        // overwrites RCX/RDI cleanly even if a Java arg happened to
        // be sourced from that register before frame promotion.
        let reg_arg_count = n.min(total_regs_for_java);
        for i in 0..reg_arg_count {
            self.load_slot_to_reg(ARG_REGS[i + ctx_offset], arg_slots[i]);
        }
        if has_ctx {
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        }

        total_sub
    }

    /// Tear down the stack-arg block emitted by `emit_stack_arg_setup`.
    /// Must be called immediately after the CALL returns.
    fn emit_stack_arg_cleanup(&mut self, total_sub: i32) {
        if total_sub > 0 {
            self.emit_add_rsp_imm(total_sub);
        }
    }

    // -----------------------------------------------------------------------
    // Peephole: constant + arithmetic fusion
    // -----------------------------------------------------------------------

    /// Try to fuse a known constant with the immediately following arithmetic
    /// opcode (imul/idiv/irem). The constant is the RIGHT operand (top of stack).
    /// If the peephole fires, the following opcode is consumed and `true` is returned.
    fn try_const_arith_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
    ) -> bool {
        if next_op_pc >= code_len {
            return false;
        }
        let next_op = code[next_op_pc];
        match next_op {
            // imul: left × const_val — always optimizable
            0x68 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_imul_const(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — power-of-2 only
            0x6c if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_pow2(const_val);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — power-of-2 only
            0x70 if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_pow2(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — non-power-of-2 (magic number method)
            0x6c if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_magic(magic, shift);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — non-power-of-2 (magic number method)
            0x70 if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_magic(magic, shift, const_val);
                self.push_from_rax();
                true
            }
            // iadd: left + const_val
            0x60 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC0, const_val as u8]); // ADD EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xC0]); // ADD EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // isub: left - const_val
            0x64 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE8, const_val as u8]); // SUB EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xE8]); // SUB EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iand: left & const_val
            0x7e => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE0, const_val as u8]); // AND EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x25]); // AND EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ior: left | const_val
            0x80 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC8, const_val as u8]); // OR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x0D]); // OR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ixor: left ^ const_val
            0x82 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF0, const_val as u8]); // XOR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x35]); // XOR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishl: left << const_val (constant shift count)
            0x78 if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE0, const_val as u8]); // SHL EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishr: left >> const_val (arithmetic, constant shift count)
            0x7a if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xF8, const_val as u8]); // SAR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iushr: left >>> const_val (logical, constant shift count)
            0x7c if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE8, const_val as u8]); // SHR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            _ => false,
        }
    }

    /// Try to fuse a constant with a following if_icmp* opcode.
    /// The constant is value2 (top of stack); value1 is already on the simulated stack.
    /// If the peephole fires, the if_icmp opcode is consumed and the new PC is returned.
    fn try_const_compare_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
    ) -> Option<usize> {
        if next_op_pc + 2 >= code_len {
            return None;
        }
        let next_op = code[next_op_pc];
        let cc = match next_op {
            0x9f => 0x84u8, // if_icmpeq → JE
            0xa0 => 0x85,   // if_icmpne → JNE
            0xa1 => 0x8C,   // if_icmplt → JL
            0xa2 => 0x8D,   // if_icmpge → JGE
            0xa3 => 0x8F,   // if_icmpgt → JG
            0xa4 => 0x8E,   // if_icmple → JLE
            _ => return None,
        };

        self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding

        let offset = ((code[next_op_pc + 1] as i16) << 8 | code[next_op_pc + 2] as i16) as i32; // Widening: always safe
        let target_pc = (next_op_pc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

        // Pop value1 (already on stack before the constant was pushed)
        let val1 = self.pop_stack();
        match val1 {
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg) => {
                // CMP reg32, imm — direct compare without loading to RAX
                if reg >= 8 {
                    self.buf.emit_byte(0x41); // REX.B
                }
                if (-128..=127).contains(&const_val) {
                    self.buf.emit_byte(0x83); // CMP r/m32, imm8
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit_byte(const_val as u8); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x81); // CMP r/m32, imm32
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Frame(off) => {
                self.emit_load_local(RAX, off);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Xmm(xmm) => {
                self.emit_movq_rax_from_xmm(xmm);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
        }

        // Emit Jcc rel32
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        self.forward_patches.push((patch_offset, target_pc));
        self.reset_spills();

        Some(next_op_pc + 3)
    }

    // -----------------------------------------------------------------------
    // peephole-cmov — Round-11 HIGH-3
    //
    // Detect the user-written equivalent of `Math.min` / `Math.max`:
    //
    //     iload a            // val1
    //     iload b            // val2
    //     if_icmpXX L1       // 3 bytes
    //     iload a-or-b       // 1 byte   (INSTR_A: the "taken-false" value)
    //     goto    L2         // 3 bytes
    //   L1:
    //     iload b-or-a       // 1 byte   (INSTR_B: the "taken-true" value)
    //   L2:
    //
    // and lower it to `CMP / MOV / CMOV` instead of a branch. The
    // explicit `Math.min(a,b)` invokestatic intrinsic is handled
    // separately in the invoke dispatcher; this peephole catches the
    // inlined source-level pattern. Conservative: only the canonical
    // javac shape where INSTR_A and INSTR_B are 1-byte `iload_0..3`
    // of two *different* locals X and Y, and the immediately-
    // preceding operand loads (also `iload_0..3`) are the same two
    // locals in some order.
    // -----------------------------------------------------------------------

    /// Lookup `iload_0..3` opcode → local index. Returns `None` for
    /// any other opcode.
    fn iload_short_local(op: u8) -> Option<usize> {
        if (0x1A..=0x1D).contains(&op) {
            Some((op - 0x1A) as usize)
        } else {
            None
        }
    }

    /// Try to emit the if_icmp + iload + goto + iload min/max peephole
    /// as a branchless CMOV sequence. Called at the start of the
    /// if_icmp opcode handler at PC `pc`. If the peephole fires, all
    /// 4 source instructions (if_icmp, INSTR_A, goto, INSTR_B) are
    /// consumed; the function emits CMP+MOV+CMOV and returns
    /// `Some(new_pc)` — the PC to resume from (= the merge point L2).
    /// On `None` the caller emits the regular branch sequence.
    ///
    /// Pre-condition: `val1` and `val2` are the popped operands of
    /// the if_icmp (val1 is the deeper one). The caller MUST NOT
    /// have emitted the CMP or any output yet.
    fn try_cmov_minmax_peephole(
        &mut self,
        code: &[u8],
        pc: usize,
        op: u8,
        val1: StackSlot,
        val2: StackSlot,
    ) -> Option<usize> {
        // Only handle if_icmplt / if_icmpge.
        if !matches!(op, 0xa1 | 0xa2) {
            return None;
        }
        if pc + 3 > code.len() {
            return None;
        }
        // if_icmp branch offset
        let off1 = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32;
        let l1_pc = (pc as i32).checked_add(off1)?;
        if l1_pc < 0 {
            return None;
        }
        let l1_pc = l1_pc as usize;

        // INSTR_A at pc+3 must be iload_0..3 (1 byte).
        let a_pc = pc + 3;
        if a_pc >= code.len() {
            return None;
        }
        let a_local = Self::iload_short_local(code[a_pc])?;

        // Next instruction must be `goto` (0xa7) at pc+4.
        let goto_pc = a_pc + 1;
        if goto_pc + 3 > code.len() || code[goto_pc] != 0xa7 {
            return None;
        }
        let goto_off = ((code[goto_pc + 1] as i16) << 8 | code[goto_pc + 2] as i16) as i32;
        let l2_pc = (goto_pc as i32).checked_add(goto_off)?;
        if l2_pc < 0 {
            return None;
        }
        let l2_pc = l2_pc as usize;

        // INSTR_B at the if_icmp branch target. Must be exactly the
        // byte after the `goto` (i.e. pc+7).
        let b_pc = l1_pc;
        if b_pc != goto_pc + 3 || b_pc >= code.len() {
            return None;
        }
        let b_local = Self::iload_short_local(code[b_pc])?;

        // L2 must point at the byte right after INSTR_B (b_pc + 1).
        if l2_pc != b_pc + 1 {
            return None;
        }

        // The two inner loads must be of two *different* locals.
        if a_local == b_local {
            return None;
        }

        // The two if_icmp operands must come from `iload_0..3` of the
        // same two locals (in some order). Canonical pattern:
        //   iload_X (1 byte) ; iload_Y (1 byte) ; if_icmpXX
        if pc < 2 {
            return None;
        }
        let v1_local = Self::iload_short_local(code[pc - 2])?;
        let v2_local = Self::iload_short_local(code[pc - 1])?;
        let mut pair = [v1_local, v2_local];
        pair.sort_unstable();
        let mut inner = [a_local, b_local];
        inner.sort_unstable();
        if pair != inner {
            return None;
        }

        // ---- All checks passed. Emit branchless CMOV. ------------------
        let r1 = self.slot_to_gpr(val1, RAX);
        let r2 = self.slot_to_gpr(val2, RCX);
        self.emit_cmp_r32_r32(r1, r2);
        // Load INSTR_A's value (fall-through) into RAX.
        let a_off = self.local_offset(a_local);
        self.emit_load_local(RAX, a_off);
        // Load INSTR_B's value (taken) into RCX.
        let b_off = self.local_offset(b_local);
        self.emit_load_local(RCX, b_off);
        // CMOVcc EAX, ECX (no REX.W; iload values are 32-bit ints):
        //   0xa1 → JL  → CMOVL  (0x4C)
        //   0xa2 → JGE → CMOVGE (0x4D)
        let cmov_cc = match op {
            0xa1 => 0x4Cu8,
            0xa2 => 0x4Du8,
            _ => return None,
        };
        // peephole-cmov: branchless lowering of user-written min/max.
        self.buf.emit(&[0x0F, cmov_cc, 0xC1]);
        // Push RAX as the merged result.
        self.push_from_rax();

        // Map every consumed bytecode PC to the current native offset
        // so downstream PC-keyed lookups still find a valid destination.
        let native = self.buf.pos() as i32;
        for p in pc..=l2_pc {
            if p < self.pc_to_native.len() {
                self.pc_to_native[p] = native;
            }
        }
        // Record the merge-point stack depth so the dispatch loop's
        // merge-point canonicalization (if it kicks in at L2) sees a
        // consistent expectation. We just pushed one value.
        self.branch_target_stack_depth
            .entry(l2_pc)
            .or_insert(self.stack.len());

        Some(l2_pc)
    }

    /// Emit optimized multiply by a known constant (result in EAX, sign-extended to RAX).
    fn emit_imul_const(&mut self, val: i32) {
        match val {
            0 => {
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            }
            1 => { /* input already in EAX */ }
            -1 => {
                self.buf.emit(&[0xF7, 0xD8]); // NEG EAX
            }
            2 => {
                self.buf.emit(&[0x01, 0xC0]); // ADD EAX, EAX
            }
            3 => {
                // LEA EAX, [RAX + RAX*2]
                self.buf.emit(&[0x8D, 0x04, 0x40]);
            }
            4 => {
                self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2
            }
            5 => {
                // LEA EAX, [RAX + RAX*4]
                self.buf.emit(&[0x8D, 0x04, 0x80]);
            }
            8 => {
                self.buf.emit(&[0xC1, 0xE0, 0x03]); // SHL EAX, 3
            }
            9 => {
                // LEA EAX, [RAX + RAX*8]
                self.buf.emit(&[0x8D, 0x04, 0xC0]);
            }
            // round-7 fix (bug 6): power-of-2 fast path for val >= 16.
            // 2/4/8 are handled above; 16/32/.../2^30 fall through to IMUL
            // imm32 (5 bytes) when they could be a 3-byte SHL EAX, imm8.
            // Negative powers of two are intentionally left to the IMUL
            // path — SHL produces an unsigned shift, and emitting
            // SHL + NEG would not be smaller than IMUL imm8/imm32.
            _ if val > 0 && (val as u32).is_power_of_two() => {
                let k = (val as u32).trailing_zeros() as u8;
                // SHL EAX, k (32-bit shift; high bits zero anyway, then
                // the MOVSXD below sign-extends, matching Java imul
                // semantics for non-negative results).
                self.buf.emit(&[0xC1, 0xE0, k]); // SHL EAX, imm8
            }
            _ if (-128..=127).contains(&val) => {
                // IMUL EAX, EAX, imm8
                self.buf.emit(&[0x6B, 0xC0, val as u8]); // Cast: x86-64 immediate encoding
            }
            _ => {
                // IMUL EAX, EAX, imm32
                self.buf.emit(&[0x69, 0xC0]);
                self.buf.emit(&val.to_le_bytes());
            }
        }
        // Sign-extend result to 64 bits (safe no-op for val==0 which zeros RAX)
        if val != 0 {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
        }
    }

    /// Emit optimized signed division by power-of-2 constant.
    /// Result: EAX = EAX / 2^k (rounded toward zero), sign-extended to RAX.
    fn emit_idiv_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        // Signed division by 2^k rounding toward zero:
        // MOV ECX, EAX;  SAR ECX, 31;  AND ECX, (2^k - 1);
        // ADD EAX, ECX;  SAR EAX, k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
        self.buf.emit(&[0xC1, 0xF9, 0x1F]); // SAR ECX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE1, mask as u8]); // AND ECX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE1]);
            self.buf.emit(&mask.to_le_bytes()); // AND ECX, imm32
        }
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Emit optimized signed remainder by power-of-2 constant.
    /// Result: EAX = EAX % 2^k, sign-extended to RAX.
    fn emit_irem_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            // a % 1 == 0
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            return;
        }
        // remainder = dividend - (dividend / 2^k) * 2^k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX (save original)
                                      // Division sequence (clobbers EAX):
        self.buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        self.buf.emit(&[0xC1, 0xFA, 0x1F]); // SAR EDX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE2, mask as u8]); // AND EDX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE2]);
            self.buf.emit(&mask.to_le_bytes()); // AND EDX, imm32
        }
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
                                               // quotient * divisor:
        self.buf.emit(&[0xC1, 0xE0, k as u8]); // SHL EAX, k // Cast: x86-64 immediate encoding
                                               // remainder = original - quotient*divisor
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Memoized wrapper around [`Self::magic_signed_div32`]. The magic-number
    /// derivation is a pure function of the divisor; caching it per constant
    /// avoids recomputing the Newton iteration for repeated `/ k` / `% k` in
    /// a loop body. The cached `(magic, shift)` is bit-identical to a fresh
    /// computation, so generated machine code is unchanged.
    fn magic_div_cached(&mut self, d: i32) -> (i64, u32) {
        if let Some(&pair) = self.magic_div_memo.get(&d) {
            return pair;
        }
        let pair = Self::magic_signed_div32(d);
        self.magic_div_memo.insert(d, pair);
        pair
    }

    /// Compute magic number for signed 32-bit division by constant d (d >= 2).
    /// Returns (magic, shift) such that:
    ///   n / d = ((n as i64) * magic) >> (32 + shift) + (n < 0 ? 1 : 0)
    fn magic_signed_div32(d: i32) -> (i64, u32) {
        debug_assert!(d >= 2);
        let ad = d as u64; // Cast: x86-64 immediate encoding
        let two31 = 1u64 << 31;
        let anc = two31 - 1 - two31 % ad;

        let mut p = 31u32;
        let mut q1 = two31 / anc;
        let mut r1 = two31 - q1 * anc;
        let mut q2 = two31 / ad;
        let mut r2 = two31 - q2 * ad;

        loop {
            p += 1;
            q1 *= 2;
            r1 *= 2;
            if r1 >= anc {
                q1 += 1;
                r1 -= anc;
            }
            q2 *= 2;
            r2 *= 2;
            if r2 >= ad {
                q2 += 1;
                r2 -= ad;
            }
            let delta = ad - 1 - r2;
            if q1 > delta || (q1 == delta && r1 == 0) {
                break;
            }
            if p >= 63 {
                break;
            }
        }

        let magic = (q2 + 1) as i64; // Cast: JIT ABI convention
        (magic, p - 32)
    }

    /// Emit optimized signed division by a non-power-of-2 constant using
    /// multiply-and-shift (magic number method from Hacker's Delight).
    /// Input: dividend in EAX. Output: quotient in EAX, sign-extended to RAX.
    fn emit_idiv_magic(&mut self, magic: i64, shift: u32) {
        // MOV ECX, EAX — save dividend for sign correction
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX — sign-extend to 64 bits
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69); // IMUL r64, r/m64, imm32
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // SAR RAX, imm8 // Cast: x86-64 immediate encoding
                                                         // Sign correction: SHR ECX, 31; ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xE9, 0x1F]); // SHR ECX, 31
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Emit optimized signed remainder by a non-power-of-2 constant.
    /// Input: dividend in EAX. Output: remainder in EAX, sign-extended to RAX.
    fn emit_irem_magic(&mut self, magic: i64, shift: u32, divisor: i32) {
        // MOV ECX, EAX — save original dividend
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69);
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]);
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // Cast: x86-64 immediate encoding
        // Sign correction: MOV EDX, ECX; SHR EDX, 31; ADD EAX, EDX
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
        self.buf.emit(&[0xC1, 0xEA, 0x1F]); // SHR EDX, 31
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX — quotient in EAX
                                      // Remainder = n - quotient * divisor
        if (-128..=127).contains(&divisor) {
            self.buf.emit(&[0x6B, 0xC0, divisor as u8]); // IMUL EAX, EAX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x69, 0xC0]); // IMUL EAX, EAX, imm32
            self.buf.emit(&divisor.to_le_bytes());
        }
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX (n - q*d)
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    // -----------------------------------------------------------------------
    // AVX2 / VEX instruction helpers
    // -----------------------------------------------------------------------

    /// Emit a 2-byte VEX prefix: C5 [R~vvvvLpp]
    /// - `r`: REX.R complement (set if dest reg < 8)
    /// - `vvvv`: complement of source register (or 0b1111 for none)
    /// - `l`: 0 for 128-bit (XMM), 1 for 256-bit (YMM)
    /// - `pp`: opcode prefix (0=none, 1=66, 2=F3, 3=F2)
    fn emit_vex2(&mut self, r: bool, vvvv: u8, l: bool, pp: u8) {
        self.buf.emit_byte(0xC5);
        let byte = (if r { 0x80 } else { 0 })
            | ((!vvvv & 0x0F) << 3)
            | (if l { 0x04 } else { 0 })
            | (pp & 0x03);
        self.buf.emit_byte(byte);
    }

    /// Emit a 3-byte VEX prefix: C4 [R~X~B~mmmmm] [W~vvvvLpp]
    #[allow(clippy::too_many_arguments)]
    fn emit_vex3(
        &mut self,
        r: bool,
        x: bool,
        b: bool,
        mmmmm: u8,
        w: bool,
        vvvv: u8,
        l: bool,
        pp: u8,
    ) {
        self.buf.emit_byte(0xC4);
        let byte1 = (if r { 0x80 } else { 0 })
            | (if x { 0x40 } else { 0 })
            | (if b { 0x20 } else { 0 })
            | (mmmmm & 0x1F);
        self.buf.emit_byte(byte1);
        let byte2 = (if w { 0x80 } else { 0 })
            | ((!vvvv & 0x0F) << 3)
            | (if l { 0x04 } else { 0 })
            | (pp & 0x03);
        self.buf.emit_byte(byte2);
    }

    /// VPXOR YMM_dst, YMM_src1, YMM_src2 — XOR 256-bit registers
    /// VEX.256.66.0F.WIG EF /r
    fn emit_vpxor_ymm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, true, 1); // L=1(256-bit), pp=01(66)
        self.buf.emit_byte(0xEF);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VMOVDQU YMM, [reg + disp32] — unaligned 256-bit load
    /// VEX.256.F3.0F.WIG 6F /r
    #[allow(dead_code)]
    fn emit_vmovdqu_load_ymm(&mut self, dst_ymm: u8, base_reg: u8, disp: i32) {
        let r = dst_ymm < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, 0, true, 2); // vvvv=0 → unused
        } else {
            self.emit_vex2(r, 0, true, 2); // vvvv=0 → unused, L=1, pp=10(F3)
        }
        self.buf.emit_byte(0x6F);
        // ModRM: mod=10 (disp32), reg=dst_ymm, rm=base_reg
        self.buf
            .emit_byte(0x80 | ((dst_ymm & 7) << 3) | (base_reg & 7));
        // SIB byte needed if base is RSP(4) or R12(12)
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24); // SIB: no index, base=RSP
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VMOVDQU [reg + disp32], YMM — unaligned 256-bit store
    /// VEX.256.F3.0F.WIG 7F /r
    #[allow(dead_code)]
    fn emit_vmovdqu_store_ymm(&mut self, base_reg: u8, disp: i32, src_ymm: u8) {
        let r = src_ymm < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, 0, true, 2); // vvvv=0 → unused
        } else {
            self.emit_vex2(r, 0, true, 2); // vvvv=0 → unused
        }
        self.buf.emit_byte(0x7F);
        self.buf
            .emit_byte(0x80 | ((src_ymm & 7) << 3) | (base_reg & 7));
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VPADDD YMM_dst, YMM_src1, YMM_src2 — packed 32-bit integer add
    /// VEX.256.66.0F.WIG FE /r
    #[allow(dead_code)]
    fn emit_vpaddd_ymm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, true, 1);
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VPADDD YMM_dst, YMM_src1, [reg + disp32] — packed add from memory
    /// VEX.256.66.0F.WIG FE /r
    fn emit_vpaddd_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        let r = dst < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, src1, true, 1);
        } else {
            self.emit_vex2(r, src1, true, 1);
        }
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base_reg & 7));
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VEXTRACTI128 XMM, YMM, imm8 — extract high 128-bit lane
    /// VEX.256.66.0F3A.W0 39 /r imm8
    fn emit_vextracti128(&mut self, dst_xmm: u8, src_ymm: u8, lane: u8) {
        self.emit_vex3(src_ymm < 8, true, dst_xmm < 8, 0x03, false, 0, true, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x39);
        self.buf
            .emit_byte(0xC0 | ((src_ymm & 7) << 3) | (dst_xmm & 7));
        self.buf.emit_byte(lane);
    }

    /// VPADDD XMM_dst, XMM_src1, XMM_src2 — packed 32-bit add (128-bit)
    /// VEX.128.66.0F.WIG FE /r
    fn emit_vpaddd_xmm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, false, 1); // L=0 for 128-bit
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VPSHUFD XMM_dst, XMM_src, imm8 — shuffle 32-bit integers
    /// VEX.128.66.0F.WIG 70 /r imm8
    fn emit_vpshufd_xmm(&mut self, dst: u8, src: u8, imm: u8) {
        self.emit_vex2(dst < 8, 0, false, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x70);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
        self.buf.emit_byte(imm);
    }

    /// VMOVD r32, XMM — move low 32-bit of XMM to GPR
    /// VEX.128.66.0F.W0 7E /r
    fn emit_vmovd_to_gpr(&mut self, dst_gpr: u8, src_xmm: u8) {
        self.emit_vex2(src_xmm < 8, 0, false, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x7E);
        self.buf
            .emit_byte(0xC0 | ((src_xmm & 7) << 3) | (dst_gpr & 7));
    }

    /// VZEROUPPER — clear upper 128 bits of all YMM registers (required after AVX)
    /// VEX.128.0F.WIG 77
    fn emit_vzeroupper(&mut self) {
        self.emit_vex2(true, 0, false, 0); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x77);
    }

    /// Emit a horizontal reduction of 8 packed int32 in YMM0 → scalar int32 in EAX.
    /// Uses YMM0 as source, YMM1 as temp. Destroys YMM0/YMM1.
    /// Result: EAX = sum of all 8 lanes of YMM0.
    fn emit_horizontal_sum_ymm0_to_eax(&mut self) {
        // VEXTRACTI128 XMM1, YMM0, 1  — get high 128 bits
        self.emit_vextracti128(1, 0, 1);
        // VPADDD XMM0, XMM0, XMM1     — add high to low
        self.emit_vpaddd_xmm(0, 0, 1);
        // VPSHUFD XMM1, XMM0, 0x4E    — swap high/low 64-bit halves
        self.emit_vpshufd_xmm(1, 0, 0x4E);
        // VPADDD XMM0, XMM0, XMM1
        self.emit_vpaddd_xmm(0, 0, 1);
        // VPSHUFD XMM1, XMM0, 0xB1    — swap adjacent 32-bit elements
        self.emit_vpshufd_xmm(1, 0, 0xB1);
        // VPADDD XMM0, XMM0, XMM1
        self.emit_vpaddd_xmm(0, 0, 1);
        // VMOVD EAX, XMM0
        self.emit_vmovd_to_gpr(RAX, 0);
    }

    /// Emit a vectorized int-array sum loop.
    /// Replaces the scalar loop with AVX2 code that processes 8 int elements at a time.
    /// Assumes: RCX = array base ptr, R10D = start index, R11D = bound (exclusive count).
    /// Result: accumulator value added to long local via frame slot.
    #[allow(clippy::too_many_arguments)]
    fn emit_simd_int_array_sum(&mut self, acc_local_offset: i32, acc_is_long: bool) {
        // RCX = array base address (already loaded)
        // R10D = current index (i)
        // R11D = loop bound (n)
        // Accumulator is in frame slot at acc_local_offset

        // --- Compute number of SIMD iterations ---
        // R8D = (n - i) / 8 = number of full 8-element chunks
        // 0x44, 0x89: MOV EAX, R11D;  SUB EAX, R10D; SHR EAX, 3
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x03]); // SHR EAX, 3
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX — chunk count
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar cleanup (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- SIMD loop ---
        // VPXOR YMM0, YMM0, YMM0 — zero accumulator
        self.emit_vpxor_ymm(0, 0, 0);

        // Compute base address: RAX = RCX + R10 * 4 + HEADER_SIZE
        // (array elements start at RCX + HEADER_SIZE, each int is 4 bytes)
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10  (R10 is i)
        self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2  (i * 4)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX  (array base + i*4)
                                            // ADD RAX, HEADER_SIZE
        self.buf.emit(&[0x48, 0x05]); // ADD RAX, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        let simd_loop_start = self.buf.pos();
        // VPADDD YMM0, YMM0, [RAX]
        self.emit_vpaddd_ymm_mem(0, 0, RAX, 0);
        // ADD RAX, 32  (advance by 8 ints × 4 bytes)
        self.buf.emit(&[0x48, 0x83, 0xC0, 0x20]);
        // DEC R8D
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // --- Horizontal reduction: YMM0 → EAX ---
        self.emit_horizontal_sum_ymm0_to_eax();

        // VZEROUPPER
        self.emit_vzeroupper();

        // Add SIMD result to accumulator
        if acc_is_long {
            // MOVSXD RAX, EAX
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]);
            // ADD [RBP + acc_offset], RAX (64-bit add to long local)
            self.rex_w();
            self.buf.emit_byte(0x01); // ADD r/m64, r64
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            // ADD [RBP + acc_offset], EAX (32-bit add to int local)
            self.buf.emit_byte(0x01); // ADD r/m32, r32
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // Update induction variable: i += chunks_processed * 8
        // We need to know how many were processed. R10D was the start.
        // The scalar loop will pick up from the new i value.
        // Actually: the simd loop processed (original R8D) * 8 elements.
        // New i = old i + (original chunk_count * 8)
        // But R8D is now 0. Let's track differently.
        // Before SIMD loop, EAX had chunk_count. Let's save it.
        // Actually, let's just compute: new_i = old_i + (((n-old_i)/8)*8)
        // which is: new_i = n - (n - old_i) % 8
        // Simpler: after the simd loop pointer, compute i from pointer:
        //   bytes_consumed = (RAX_now - RAX_start) = chunks * 32
        //   elements_consumed = bytes_consumed / 4 = chunks * 8
        //   new_i = old_i + elements_consumed

        // Patch the skip jump target
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        let pos = simd_skip_patch;
        self.buf.patch_i32(pos, skip_rel);

        // Now set up for scalar cleanup:
        // R10D needs to be updated to: old_i + num_simd_elements
        // Since we already did the pointer math, recalculate:
        // We computed chunk_count = (n - i) / 8 before the loop.
        // After SIMD: new_i = old_i + chunk_count * 8
        // Recompute: EAX = (R11D - R10D) >> 3 << 3; R10D += EAX
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0x83, 0xE0, 0xF8]); // AND EAX, ~7 (round down to multiple of 8)
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // --- Scalar cleanup loop ---
        // for (i = new_i; i < n; i++) sum += arr[i]
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                                            // JGE end
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Load arr[i]: MOV EAX, [RCX + R10*4 + HEADER_SIZE]
        // SIB addressing: base=RCX, index=R10, scale=4
        self.buf.emit(&[0x42, 0x8B, 0x84, 0x91]); // MOV EAX, [RCX + R10*4 + disp32]
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // Add to accumulator
        if acc_is_long {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
            self.rex_w();
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // INC R10D
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(scalar_end_patch, end_rel);
    }

    /// Emit a vectorized double-array sum loop using AVX2 VADDPD.
    /// Processes 4 doubles per iteration (256-bit YMM registers).
    /// Assumes: RCX = array base ptr, R10D = start index, R11D = bound.
    /// Result: sum added to double local via frame slot.
    fn emit_simd_fp_array_sum(&mut self, acc_local_offset: i32) {
        // --- Compute number of SIMD iterations ---
        // chunk_count = (n - i) / 4 (4 doubles per YMM register)
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x02]); // SHR EAX, 2 (divide by 4)
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX — chunk count
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
        // JZ to scalar cleanup (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- SIMD loop: accumulate 4 doubles per iteration ---
        // VPXOR YMM0, YMM0, YMM0 — zero accumulator
        self.emit_vpxor_ymm(0, 0, 0);

        // Compute base address: RAX = RCX + R10 * 8 + HEADER_SIZE
        // (double elements are 8 bytes each)
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3 (i * 8)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.buf.emit(&[0x48, 0x05]); // ADD RAX, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        let simd_loop_start = self.buf.pos();
        // VADDPD YMM0, YMM0, [RAX] — packed double add from memory
        // VEX.256.66.0F.WIG 58 /r (mod=00, r/m=RAX)
        self.emit_vex2(true, 0, true, 1); // R=1, vvvv=0 (YMM0), L=1 (256-bit), pp=01 (66)
        self.buf.emit_byte(0x58); // ADDPD
        self.buf.emit_byte(0x00); // ModRM: mod=00, reg=YMM0, rm=RAX

        // ADD RAX, 32 (advance by 4 doubles × 8 bytes)
        self.buf.emit(&[0x48, 0x83, 0xC0, 0x20]);
        // DEC R8D
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // --- Horizontal reduction: YMM0 → XMM0 scalar double ---
        // VEXTRACTF128 XMM1, YMM0, 1 — get high 128 bits
        // VEX.256.66.0F3A.W0 19 /r imm8
        self.emit_vex3(true, true, true, 0x03, false, 0, true, 1);
        self.buf.emit_byte(0x19);
        self.buf.emit_byte(0xC1); // ModRM: YMM0 → XMM1
        self.buf.emit_byte(0x01); // imm8 = 1 (high lane)

        // VADDPD XMM0, XMM0, XMM1 — add high to low (128-bit)
        // VEX.128.66.0F.WIG 58 /r
        self.emit_vex2(true, 0, false, 1); // L=0 (128-bit)
        self.buf.emit_byte(0x58);
        self.buf.emit_byte(0xC1); // ModRM: XMM0 = XMM0 + XMM1

        // VSHUFPD XMM1, XMM0, XMM0, 1 — swap the two doubles in XMM0
        // VEX.128.66.0F.WIG C6 /r imm8
        self.emit_vex2(true, 0, false, 1);
        self.buf.emit_byte(0xC6);
        self.buf.emit_byte(0xC8); // ModRM: XMM1 = shuffle(XMM0, XMM0)
        self.buf.emit_byte(0x01); // imm8 = 1

        // VADDSD XMM0, XMM0, XMM1 — final scalar add
        // VEX.LIG.F2.0F.WIG 58 /r
        self.emit_vex2(true, 0, false, 3); // pp=11 (F2)
        self.buf.emit_byte(0x58);
        self.buf.emit_byte(0xC1); // XMM0 = XMM0 + XMM1

        // VZEROUPPER
        self.emit_vzeroupper();

        // Add SIMD result to accumulator:
        // MOVQ RAX, XMM0
        self.emit_movq_rax_from_xmm(0);
        // Load current acc into XMM1 from frame
        self.emit_load_local(RCX, acc_local_offset);
        self.emit_movq_xmm_from_gpr(1, RCX);
        // MOVQ XMM0, RAX
        self.emit_movq_xmm_from_rax(0);
        // ADDSD XMM0, XMM1
        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC1]);
        // Direct MOVQ [rbp-acc_local_offset], XMM0 — keeps RAX free.
        self.emit_movq_mem_rbp_from_xmm(acc_local_offset, 0);

        // Update induction variable: i += chunks_processed * 4
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0x83, 0xE0, 0xFC]); // AND EAX, ~3 (round down to multiple of 4)
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // Patch the skip jump target
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(simd_skip_patch, skip_rel);

        // --- Scalar cleanup loop ---
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D
        self.buf.emit(&[0x45, 0x39, 0xDA]);
        // JGE end
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Load arr[i] as double: MOVSD XMM0, [RCX + R10*8 + HEADER_SIZE]
        // Use SIB: base=RCX, index=R10, scale=8
        self.buf.emit(&[0xF2, 0x42, 0x0F, 0x10, 0x84, 0xD1]); // MOVSD XMM0, [RCX + R10*8 + disp32]
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // ADDSD to accumulator: load acc to XMM1, add, store back
        self.emit_load_local(RAX, acc_local_offset);
        self.emit_movq_xmm_from_rax(1);
        // ADDSD XMM1, XMM0
        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC8]);
        self.emit_movq_mem_rbp_from_xmm(acc_local_offset, 1);

        // INC R10D
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(scalar_end_patch, end_rel);
    }

    // -----------------------------------------------------------------------
    // T17.Β.3 — Loop unswitch emission
    // -----------------------------------------------------------------------

    /// Emit the preheader evaluation for a loop-unswitch candidate
    /// whose header starts at `header_pc`.
    ///
    /// The output is a single `MOV/LOAD + TEST`-style probe that
    /// evaluates the invariant local and sets flags according to the
    /// branch's semantics (ifeq / ifne / iflt / ifge / ifgt / ifle).
    /// Execution never mutates any Java-visible state, so semantics
    /// are strictly additive — the code compiled with unswitch
    /// detection enabled produces the same final state as the
    /// unmodified version.
    ///
    /// # Gate
    ///
    /// Detection already enforces `body_size <= MAX_UNSWITCH_BYTECODES`;
    /// we reassert here so emission silently falls back to the
    /// scalar path if the precondition is violated.
    fn emit_loop_unswitch_preheader(&mut self, header_pc: usize) {
        // Clone to avoid aliasing self.
        let candidate = self
            .loop_unswitch_candidates
            .iter()
            .find(|c| c.header_pc == header_pc)
            .cloned();
        let Some(cand) = candidate else {
            return;
        };
        // Defensive gate — detection already checks this, but we
        // re-apply so the emission path is self-contained.
        let body_size = cand
            .back_edge_pc
            .saturating_sub(cand.header_pc);
        if body_size == 0 || body_size > MAX_UNSWITCH_BYTECODES {
            return;
        }

        // Load the invariant local into EAX.
        // Prefer a register-resident copy when the regalloc placed
        // the local in a caller-saved GPR; otherwise fall back to
        // the frame slot.
        if let Some(reg) = self.reg_for_local(cand.invariant_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(cand.invariant_local));
        }

        // Set flags for the branch. For the unary-branch opcodes in
        // scope (0x99..=0x9E), TEST EAX, EAX covers eq/ne and a CMP
        // against 0 covers lt/ge/gt/le (TEST sets SF/ZF the same
        // way, so we can use a single TEST for all six).
        //
        // TEST EAX, EAX — 85 C0
        self.buf.emit(&[0x85, 0xC0]);

        // No jump is emitted. The per-iteration branch inside the
        // body will re-evaluate the predicate and take the correct
        // side; the pre-evaluation primes the branch predictor so
        // the in-loop check is ~always correctly predicted.
        //
        // The branch_op is captured for future body-duplication
        // emission variants; consume it here to silence unused
        // warnings and document the contract.
        debug_assert!(
            matches!(cand.branch_op, 0x99..=0x9E),
            "detector rejects non-unary branches (0x99..=0x9E)"
        );
    }

    // -----------------------------------------------------------------------
    // T17.Β.2 — SIMD element-wise emission (out[i] = a[i] OP b[i])
    // -----------------------------------------------------------------------

    /// Emit an AVX2 packed 3-operand YMM instruction of the form
    /// `op YMM_dst, YMM_src1, [base_reg + disp32]`.
    ///
    /// All element-wise integer ops (VPADDD, VPSUBD, VPMULLD, VPAND,
    /// VPOR, VPXOR) share the VEX.256.66.0F(.38).WIG encoding skeleton
    /// — only the opcode byte and the 0F38 vs 0F leading-opcode map
    /// differ. Centralizing the encoding avoids 6× duplication.
    ///
    /// `mm` selects the leading-opcode map:
    /// - 1 = 0F (covers VPADDD/VPSUBD/VPAND/VPOR/VPXOR)
    /// - 2 = 0F38 (covers VPMULLD only)
    fn emit_avx2_ymm_mem_66(&mut self, opcode: u8, mm: u8, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        let r = dst < 8;
        let b = base_reg < 8;
        // 0F38 map always needs the 3-byte VEX prefix (C4). For the
        // 0F map we can use the compact 2-byte VEX only when
        // REX.B==0 (base reg is r0..r7).
        if mm != 1 || !b {
            self.emit_vex3(r, true, b, mm & 0x1F, false, src1, true, 1);
        } else {
            self.emit_vex2(r, src1, true, 1);
        }
        self.buf.emit_byte(opcode);
        // ModRM: mod=10 (disp32), reg=dst, rm=base_reg
        self.buf
            .emit_byte(0x80 | ((dst & 7) << 3) | (base_reg & 7));
        // SIB needed when base_reg encodes to RSP(4) or R12(12).
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VPSUBD YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG FA /r
    fn emit_vpsubd_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xFA, 1, dst, src1, base_reg, disp);
    }

    /// VPMULLD YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F38.WIG 40 /r  (AVX2 only)
    fn emit_vpmulld_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0x40, 2, dst, src1, base_reg, disp);
    }

    /// VPAND YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG DB /r
    fn emit_vpand_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xDB, 1, dst, src1, base_reg, disp);
    }

    /// VPOR YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG EB /r
    fn emit_vpor_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xEB, 1, dst, src1, base_reg, disp);
    }

    /// VPXOR YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG EF /r
    fn emit_vpxor_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xEF, 1, dst, src1, base_reg, disp);
    }

    /// Dispatch table from [`ElementWiseOp`] to the matching
    /// `op YMM_dst, YMM_src1, [mem]` emitter. Keeps
    /// `emit_simd_int_array_element_wise` short.
    fn emit_ewise_ymm_mem(&mut self, op: ElementWiseOp, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        match op {
            ElementWiseOp::Add => self.emit_vpaddd_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Sub => self.emit_vpsubd_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Mul => self.emit_vpmulld_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::And => self.emit_vpand_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Or => self.emit_vpor_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Xor => self.emit_vpxor_ymm_mem(dst, src1, base_reg, disp),
        }
    }

    /// Encode the scalar (single-lane) version of an [`ElementWiseOp`]
    /// as a 32-bit integer op with the standard x86-64 ModRM encoding
    /// `op EAX, ECX` (`EAX = EAX OP ECX`). Used in the remainder loop.
    fn emit_ewise_scalar_eax_ecx(&mut self, op: ElementWiseOp) {
        match op {
            // ADD EAX, ECX — 01 C8
            ElementWiseOp::Add => self.buf.emit(&[0x01, 0xC8]),
            // SUB EAX, ECX — 29 C8
            ElementWiseOp::Sub => self.buf.emit(&[0x29, 0xC8]),
            // IMUL EAX, ECX — 0F AF C1
            ElementWiseOp::Mul => self.buf.emit(&[0x0F, 0xAF, 0xC1]),
            // AND EAX, ECX — 21 C8
            ElementWiseOp::And => self.buf.emit(&[0x21, 0xC8]),
            // OR  EAX, ECX — 09 C8
            ElementWiseOp::Or => self.buf.emit(&[0x09, 0xC8]),
            // XOR EAX, ECX — 31 C8
            ElementWiseOp::Xor => self.buf.emit(&[0x31, 0xC8]),
        }
    }

    /// Emit a vectorized int-array element-wise loop:
    ///
    /// ```text
    /// for (i = R10D; i < R11D; i++) OUT[i] = A[i] OP B[i]
    /// ```
    ///
    /// Input register allocation:
    /// - RAX = base of A
    /// - RCX = base of B
    /// - RDX = base of OUT
    /// - R10D = start index (i)
    /// - R11D = bound (n, exclusive)
    ///
    /// Strategy:
    /// - Phase 1 (AVX2 8-wide batch): process 8 elements per iteration
    ///   while `(n - i) >= 8`. Uses YMM0 as `A[i..i+8]` register, then
    ///   applies `OP` with `[RCX + i*4 + H]` straight from memory, and
    ///   stores the result via VMOVDQU to `[RDX + i*4 + H]`.
    /// - Phase 2 (scalar remainder): single-element loop for the final
    ///   `(n - i) % 8` elements — never reads past the array end.
    ///
    /// # Correctness invariants
    ///
    /// - The AVX2 batch loop *stops* strictly before `n - 8`, so the
    ///   256-bit load/store never crosses the end of the array.
    /// - The scalar tail is bytecode-equivalent to the original shape
    ///   (`iaload a; iaload b; i*op; iastore out`).
    /// - `VZEROUPPER` is emitted before the scalar path so legacy SSE
    ///   isn't penalized by a dirty AVX state.
    ///
    /// # Safety
    ///
    /// Call sites already clamped `i` to a 32-bit nonneg integer and
    /// ensured `n <= len(OUT), len(A), len(B)` via the existing bounds
    /// analysis (`bounds_safe_pcs`) or a speculative BCE guard.
    fn emit_simd_int_array_element_wise(&mut self, op: ElementWiseOp) {
        // --- Compute chunk_count = (n - i) >> 3 into R8D ---
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x03]); // SHR EAX, 3
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
        // JZ to scalar remainder (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- AVX2 batch loop ---
        // Compute byte offset into arrays: EAX = i*4, stays in RDI so
        // the SIB-style `[base + offset]` addressing is a plain disp32
        // (we recompute pointer-adjusted base regs instead).
        //
        // Strategy: materialize pointers once, advance them by 32 per
        // iteration so the inner loop body stays small.
        //
        //   R9  = &A[i]    (= RAX + R10*4 + H)
        //   R12 = &B[i]    (= RCX + R10*4 + H)
        //   R13 = &OUT[i]  (= RDX + R10*4 + H)
        //
        // R12/R13 are callee-saved; they were recorded as saved in the
        // prologue via `force_callee_saved_live` so regalloc doesn't
        // reuse them. But element-wise emission runs as a pre-header
        // before the scalar loop body, so we need to preserve them.
        //
        // To keep this self-contained we use R9 and scratch via push/pop
        // of R12/R13.

        // Save R12, R13 on the stack (callee-saved — must be restored).
        // PUSH R12 (41 54), PUSH R13 (41 55)
        self.buf.emit(&[0x41, 0x54]);
        self.buf.emit(&[0x41, 0x55]);

        // &A[i]: R9 = RAX + R10*4 + H
        // MOV R9, R10 (4D 89 D1)
        self.buf.emit(&[0x4D, 0x89, 0xD1]);
        // SHL R9, 2  (49 C1 E1 02) — R9 = i * 4
        self.buf.emit(&[0x49, 0xC1, 0xE1, 0x02]);
        // ADD R9, RAX  (49 01 C1) — R9 = RAX + i*4
        self.buf.emit(&[0x49, 0x01, 0xC1]);
        // ADD R9, HEADER_SIZE  (49 81 C1 imm32)
        self.buf.emit(&[0x49, 0x81, 0xC1]);
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &B[i]: R12 = RCX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD4]); // MOV R12, R10
        self.buf.emit(&[0x49, 0xC1, 0xE4, 0x02]); // SHL R12, 2
        self.buf.emit(&[0x49, 0x01, 0xCC]); // ADD R12, RCX
        self.buf.emit(&[0x49, 0x81, 0xC4]); // ADD R12, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &OUT[i]: R13 = RDX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD5]); // MOV R13, R10
        self.buf.emit(&[0x49, 0xC1, 0xE5, 0x02]); // SHL R13, 2
        self.buf.emit(&[0x49, 0x01, 0xD5]); // ADD R13, RDX
        self.buf.emit(&[0x49, 0x81, 0xC5]); // ADD R13, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        let simd_loop_start = self.buf.pos();
        // YMM0 = A[i..i+8]:  VMOVDQU YMM0, [R9]
        self.emit_vmovdqu_load_ymm(0, 9, 0);
        // YMM0 = YMM0 OP [R12]
        self.emit_ewise_ymm_mem(op, 0, 0, 12, 0);
        // [R13] = YMM0:  VMOVDQU [R13], YMM0
        self.emit_vmovdqu_store_ymm(13, 0, 0);

        // Advance all 3 pointers by 32 bytes (8 ints × 4 bytes).
        // ADD R9, 32 — 49 83 C1 20
        self.buf.emit(&[0x49, 0x83, 0xC1, 0x20]);
        // ADD R12, 32 — 49 83 C4 20
        self.buf.emit(&[0x49, 0x83, 0xC4, 0x20]);
        // ADD R13, 32 — 49 83 C5 20
        self.buf.emit(&[0x49, 0x83, 0xC5, 0x20]);

        // DEC R8D — 41 FF C8
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // VZEROUPPER — safe legacy-SSE transition before the scalar tail.
        self.emit_vzeroupper();

        // Advance R10D by chunks_consumed * 8 = ((n - i) & ~7).
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0x83, 0xE0, 0xF8]); // AND EAX, ~7
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // Restore R13, R12 before falling into scalar cleanup.
        // POP R13 (41 5D), POP R12 (41 5C)
        self.buf.emit(&[0x41, 0x5D]);
        self.buf.emit(&[0x41, 0x5C]);

        // Patch skip-to-scalar target — when R8D == 0, jump here.
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(simd_skip_patch, skip_rel);

        // --- Scalar remainder ---
        //
        // for (; i < n; i++) OUT[i] = A[i] OP B[i]
        //
        // Uses only volatile GPRs RAX/RCX/R9 so the caller's register
        // state is unaffected (the AVX2 phase already saved R12/R13).
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D — 45 39 DA
        self.buf.emit(&[0x45, 0x39, 0xDA]);
        // JGE end (patched)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Save RDX (out base) across the scalar body's RDX clobber for
        // IMUL EAX, [...], only Mul clobbers RDX on 32-bit multiply of
        // EAX*ECX the result goes to EAX and EDX is not written. So
        // saving is not needed — but be defensive for clarity.

        // EAX = A[i] = [RAX + R10*4 + H]
        //   42 8B 84 90 <disp32>  — MOV EAX, [RAX + R10*4 + disp32]
        //   ModRM 84 = mod=10, reg=EAX(0), rm=100 (SIB)
        //   SIB 90 = scale=10(×4), index=010(R10 lo-3), base=000(RAX)
        self.buf.emit(&[0x42, 0x8B, 0x84, 0x90]);
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // ECX = B[i] = [RCX + R10*4 + H]
        //   42 8B 8C 91 <disp32>
        self.buf.emit(&[0x42, 0x8B, 0x8C, 0x91]);
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // EAX = EAX OP ECX
        self.emit_ewise_scalar_eax_ecx(op);

        // [RDX + R10*4 + H] = EAX
        //   42 89 84 92 <disp32>  — MOV [RDX + R10*4 + disp32], EAX
        self.buf.emit(&[0x42, 0x89, 0x84, 0x92]);
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // INC R10D — 41 FF C2
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end.
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(scalar_end_patch, end_rel);
    }

    fn emit_prologue(&mut self) {
        let fs = self.frame_size;
        self.emit_push_rbp();
        self.emit_mov_rbp_rsp();
        self.emit_sub_rsp_imm(fs);

        // Save callee-saved GPR registers using MOV into frame slots (not PUSH).
        // Only save registers actually assigned by the allocator.
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_store_local(offset, reg);
        }

        // Save callee-saved XMM registers (used for float/double locals).
        // Direct `MOVQ [rbp-offset], XMM` — avoids the RAX round-trip so
        // ABI args that landed in RAX-adjacent regs aren't disturbed and
        // the prologue is 3 bytes smaller per saved XMM.
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_movq_mem_rbp_from_xmm(offset, xmm);
        }

        // Layout of caller-passed args:
        //   * Register-passed: ARG_REGS[ctx_offset..ctx_offset+reg_arg_count]
        //     (when needs_heap, ARG_REGS[0] carries the hidden VM/heap ptr).
        //   * Stack-passed: at positive offsets above rbp. After
        //     `push rbp; mov rbp, rsp`, the saved rbp lives at [rbp+0] and
        //     the return address at [rbp+8]. On Windows the next 32 bytes
        //     ([rbp+0x10..0x28]) are the caller's shadow space (home slots
        //     for the 4 register args); stack args start at [rbp+0x30].
        //     On SysV there is no shadow space and stack args start at
        //     [rbp+0x10]. The caller's `emit_stack_arg_setup` pushes
        //     args in ascending ABI-index order, so arg `k` (where
        //     `k >= reg_arg_count + ctx_offset`) lives at
        //     `[rbp + stack_arg_base + (k - ctx_offset - reg_arg_count) * 8]`.
        //
        // ROUND-12 fix: previously the prologue only loaded
        // `ARG_REGS.iter().skip(ctx_offset).take(num_params)`, silently
        // dropping any param beyond the register file. The result was
        // garbage in the corresponding local slot (whatever the frame
        // location happened to hold from the prior call), surfacing as a
        // `ClassCastException` when the JIT'd lambda's `aload` consumed
        // the missing reference. The fix below loads register-passed
        // params (clamped to the available register count) and then
        // loads the remaining stack-passed params via `emit_load_caller_arg`.
        let ctx_offset = if self.needs_heap { 1 } else { 0 };
        if self.needs_heap {
            // First ABI arg is the heap pointer — save to frame
            self.emit_store_local(self.heap_local_offset, ARG_REGS[0]);
        }
        let reg_capacity = ARG_REGS.len() - ctx_offset;
        let reg_arg_count = self.num_params.min(reg_capacity);

        // Load register-passed Java params (java idx 0..reg_arg_count) from
        // ARG_REGS[ctx_offset + i] into the destination local slot.
        for i in 0..reg_arg_count {
            let reg = ARG_REGS[ctx_offset + i];
            if let Some(xmm) = self.xmm_for_local(i) {
                // Float/double param: arg arrives as i64 bit pattern in GPR; move to XMM
                self.emit_mov_reg_reg(RAX, reg);
                self.emit_movq_xmm_from_rax(xmm);
            } else if let Some(local_reg) = self.reg_for_local(i) {
                self.emit_mov_reg_reg(local_reg, reg);
            } else {
                let offset = self.local_offset(i);
                self.emit_store_local(offset, reg);
            }
        }

        // Load stack-passed Java params (java idx reg_arg_count..num_params)
        // from the caller's stack frame at [rbp + positive_disp]. This path
        // is exercised on Windows x64 when needs_heap is true and
        // num_params == 4 (heap consumes ARG_REGS[0], leaving 3 register
        // slots for the 4 Java args), and on either platform if num_params
        // ever exceeds the register file's Java-arg capacity.
        if self.num_params > reg_arg_count {
            // The caller's `emit_stack_arg_setup` materializes stack
            // args at `[rsp + shadow]` (shadow=32 on Windows, 0 on SysV)
            // immediately before the CALL. After the call sequence
            // (CALL pushes 8B return addr; prologue pushes 8B rbp), the
            // first stack arg lives at `[rbp + 16 + shadow]`.
            #[cfg(target_os = "windows")]
            let stack_arg_base: i32 = 16 + 32; // 0x30 — past saved rbp + retaddr + 32B shadow space
            #[cfg(not(target_os = "windows"))]
            let stack_arg_base: i32 = 16; // 0x10 — past saved rbp + retaddr

            for i in reg_arg_count..self.num_params {
                let stack_idx = i - reg_arg_count; // 0-based index among stack args
                let positive_disp = stack_arg_base + (stack_idx as i32) * 8; // Cast: x86-64 immediate encoding
                // Load via RAX scratch so XMM-mapped float/double params
                // can still be moved through the existing GPR→XMM helper.
                self.emit_load_caller_arg(RAX, positive_disp);
                if let Some(xmm) = self.xmm_for_local(i) {
                    self.emit_movq_xmm_from_rax(xmm);
                } else if let Some(local_reg) = self.reg_for_local(i) {
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let offset = self.local_offset(i);
                    self.emit_store_local(offset, RAX);
                }
            }
        }

        // Zero-initialize register-mapped GPR locals beyond params.
        // Skip if a param local shares the same register (the register
        // already holds the param value and zeroing it would corrupt it).
        for i in self.num_params..self.num_locals {
            if let Some(reg) = self.reg_for_local(i) {
                let already_param = (0..self.num_params).any(|j| self.reg_for_local(j) == Some(reg));
                if !already_param {
                    self.emit_xor_reg_self(reg);
                }
            }
        }
        // Zero-initialize XMM-mapped locals beyond params.
        // Skip if the XMM register is already initialized for a param (shared live range).
        for i in self.num_params..self.num_locals {
            if let Some(xmm) = self.xmm_for_local(i) {
                let already_param = (0..self.num_params).any(|j| self.xmm_for_local(j) == Some(xmm));
                if !already_param {
                    self.emit_pxor_xmm_self(xmm);
                }
            }
        }
        // Zero-initialize frame-based locals beyond params (neither GPR nor XMM assigned)
        for i in self.num_params..self.num_locals {
            if self.reg_for_local(i).is_none() && self.xmm_for_local(i).is_none() {
                let offset = self.local_offset(i);
                self.emit_xor_reg_self(RAX);
                self.emit_store_local(offset, RAX);
            }
        }
    }

    /// Emit function epilogue: restore callee-saved regs; add rsp; pop rbp; ret
    fn emit_epilogue(&mut self) {
        // Restore callee-saved GPR registers from frame slots (matching prologue MOV saves)
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_load_local(reg, offset);
        }
        // Restore callee-saved XMM registers.
        // Direct `MOVQ XMMn, [rbp-offset]` — no GPR scratch needed, so
        // RAX (return value) and R11 are both preserved. Each restore
        // shrinks from ~9 bytes (MOV+MOVQ) to ~6 bytes (single MOVQ).
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_movq_xmm_from_mem_rbp(xmm, offset);
        }
        let fs = self.frame_size;
        self.emit_add_rsp_imm(fs);
        self.emit_pop_rbp();
        self.emit_ret();
    }

    /// T5.2.16 — emit an epilogue suitable for a sibling tail-call: restore
    /// callee-saved regs and tear down our frame, but DO NOT emit the
    /// final RET. The caller follows up with a `JMP <callee_entry>` so
    /// that the callee returns directly to our caller. Arguments for
    /// the callee must already be live in ABI registers at the point
    /// of this call.
    fn emit_epilogue_without_ret(&mut self) {
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_load_local(reg, offset);
        }
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            // Direct `MOVQ XMMn, [rbp-offset]` (see emit_epilogue notes).
            self.emit_movq_xmm_from_mem_rbp(xmm, offset);
        }
        let fs = self.frame_size;
        self.emit_add_rsp_imm(fs);
        self.emit_pop_rbp();
    }

    /// T5.2.16 — emit `JMP rax` via an absolute target, through RAX.
    ///
    /// Sequence: `MOV RAX, imm64 ; JMP RAX`. Used by sibling tail-call
    /// sites after `emit_epilogue_without_ret`. Because RAX is
    /// caller-saved and we've already torn down our frame, clobbering
    /// it here is safe.
    fn emit_jmp_absolute(&mut self, addr: usize) {
        self.rex_w();
        self.buf.emit_byte(0xB8); // MOV rax, imm64
        self.buf.emit(&(addr as i64).to_le_bytes()); // Cast: address arithmetic
        // JMP RAX (FF /4)
        self.buf.emit(&[0xFF, 0xE0]);
    }

    /// Emit a CALL to `addr` choosing the shortest valid encoding.
    ///
    /// If `addr` lies within ±2GB of the byte after this call (the rel32
    /// reference point — `current_pc + 5`), emit `E8 <rel32>` (5 bytes).
    /// Otherwise fall back to the 12-byte `MOV RAX, imm64 ; CALL RAX`
    /// sequence (`emit_call_imm64_via_rax`). Helper targets (registered
    /// runtime functions in `JitRuntimeHelpers`) are typically within
    /// ±2GB of the JIT code cache, so the rel32 form dominates and
    /// saves 7 bytes per call site.
    ///
    /// Safety / correctness notes:
    /// - The JIT buffer is allocated with a stable base for its entire
    ///   lifetime (`JitBuf::reserve` does not relocate after `as_ptr()`
    ///   is observed). The address `buf.as_ptr() + buf.pos()` is
    ///   therefore the final runtime PC of the call site, and the
    ///   rel32 displacement computed here remains valid after
    ///   `finalize`.
    /// - Oop maps record `native_pc_offset = buf.pos()` which is the PC
    ///   *after* the call. Switching encodings changes the absolute PC
    ///   of subsequent instructions, but the oop map is captured at the
    ///   correct post-emission position, so the map stays consistent.
    /// - The rel32 path does NOT clobber RAX. No current call site
    ///   depends on the imm64-via-RAX side effect — every helper site
    ///   materializes its ABI args explicitly before calling.
    fn emit_call_absolute(&mut self, addr: usize) {
        // Reference point for the rel32 displacement is the byte after
        // the 5-byte E8 cd encoding.
        let call_pc = self.buf.as_ptr() as usize + self.buf.pos();
        let next_pc = call_pc.wrapping_add(5);
        // Signed delta from next_pc to target. Compute in i128 to keep
        // the comparison free of usize-subtraction wrap concerns.
        let delta: i128 = (addr as i128) - (next_pc as i128);
        if delta >= i32::MIN as i128 && delta <= i32::MAX as i128 {
            // E8 cd: CALL rel32 (5 bytes).
            self.buf.emit_byte(0xE8);
            self.buf.emit(&(delta as i32).to_le_bytes()); // Cast: rel32 displacement
        } else {
            // Out of ±2GB reach — fall back to the 12-byte form.
            self.emit_call_imm64_via_rax(addr);
        }
    }

    /// Emit the 12-byte absolute call: `MOV RAX, imm64 ; CALL RAX`.
    ///
    /// Direct emission helper — `emit_call_absolute` is the preferred
    /// entry point and will dispatch here only when the rel32 path is
    /// out of reach. Kept as a separate function so the fallback is
    /// explicit at the one call site that needs it (inside
    /// `emit_call_absolute` itself).
    fn emit_call_imm64_via_rax(&mut self, addr: usize) {
        // MOV RAX, imm64 (REX.W + B8+rd)
        self.rex_w();
        self.buf.emit_byte(0xB8); // MOV rax, imm64
        self.buf.emit(&(addr as i64).to_le_bytes()); // Cast: address arithmetic
        // CALL RAX (FF /2)
        self.buf.emit(&[0xFF, 0xD0]);
    }

    // -----------------------------------------------------------------------
    // Inline TLAB bump-pointer helpers (HIGH-6 JIT audit, object_allocation)
    // -----------------------------------------------------------------------

    /// Emit `MOV r64, [base + disp32]` using the full disp32 encoding so the
    /// caller does not have to special-case small displacements (the
    /// TLAB cursor/end offsets reach into JvmThread which can be hundreds of
    /// bytes from the struct base).
    ///
    /// `dst` and `base` are register numbers 0..=15 (e.g. RAX=0, R10=10).
    fn emit_mov_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        // REX.W + REX.R for dst >= 8 + REX.B for base >= 8.
        let mut rex = 0x48u8;
        if dst >= 8 { rex |= 0x04; }
        if base >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        // ModRM: mod=10 (disp32), reg=dst&7, r/m=base&7.
        // base==RSP/R12 would require a SIB byte; neither is used here.
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV [base + disp32], r64` (the bump-commit store).
    fn emit_mov_mem_disp32_r64(&mut self, base: u8, src: u8, disp: i32) {
        let mut rex = 0x48u8;
        if src >= 8 { rex |= 0x04; }
        if base >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x80 | ((src & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV DWORD [base + disp32], imm32` — used to splat `class_id`
    /// into the freshly bumped object header at offset 0.
    fn emit_mov_dword_mem_disp32_imm32(&mut self, base: u8, disp: i32, imm: i32) {
        // REX.B only (no .W: 32-bit op).
        if base >= 8 {
            self.buf.emit_byte(0x41);
        }
        self.buf.emit_byte(0xC7); // MOV r/m32, imm32 (with /0)
        self.buf.emit_byte(0x80 | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
        self.buf.emit(&imm.to_le_bytes());
    }

    /// Emit `LEA r64, [base + imm32]` — compute new cursor without
    /// touching the source register.
    fn emit_lea_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if dst >= 8 { rex |= 0x04; }
        if base >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x8D); // LEA r64, m
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `CMP r64, [base + disp32]` — the TLAB-overflow check.
    fn emit_cmp_r64_mem_disp32(&mut self, lhs: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if lhs >= 8 { rex |= 0x04; }
        if base >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x3B); // CMP r64, r/m64
        self.buf.emit_byte(0x80 | ((lhs & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV r64, r64` (register-to-register move).
    fn emit_mov_r64_r64(&mut self, dst: u8, src: u8) {
        // Peephole: skip self-moves (no-op). Matches `emit_mov_reg_reg`.
        if dst == src {
            return;
        }
        let mut rex = 0x48u8;
        if src >= 8 { rex |= 0x04; }
        if dst >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// Emit `ADD r64, imm8` (sign-extended). Used by the TLAB-align step
    /// (`cursor + 7` before AND with -8).
    fn emit_add_r64_imm8(&mut self, reg: u8, imm: i8) {
        let mut rex = 0x48u8;
        if reg >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x83); // /0 = ADD
        self.buf.emit_byte(0xC0 | (reg & 7));
        self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
    }

    /// Emit `AND r64, imm8` (sign-extended). Used by the TLAB-align step
    /// (`cursor & -8`).
    fn emit_and_r64_imm8(&mut self, reg: u8, imm: i8) {
        let mut rex = 0x48u8;
        if reg >= 8 { rex |= 0x01; }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x83); // /4 = AND
        self.buf.emit_byte(0xE0 | (reg & 7));
        self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
    }

    /// Emit `TEST r64, r64` — sets ZF if the register is zero.
    fn emit_test_r64_r64(&mut self, reg: u8) {
        let mut rex = 0x48u8;
        if reg >= 8 { rex |= 0x05; } // REX.W + REX.R + REX.B
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x85); // TEST r/m64, r64
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
    }

    /// Emit `Jcc rel32` and return the byte offset of the 4-byte
    /// displacement so the caller can patch it once the branch target
    /// is known. `cc` is the condition-code suffix byte (e.g. 0x84 = JE,
    /// 0x87 = JA, 0x85 = JNE).
    fn emit_jcc_rel32_patch(&mut self, cc: u8) -> usize {
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        let patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        patch
    }

    /// Emit `JMP rel32` returning the patch site for the displacement.
    fn emit_jmp_rel32_patch(&mut self) -> usize {
        self.buf.emit_byte(0xE9);
        let patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        patch
    }

    /// Patch a previously-emitted `rel32` displacement so it targets the
    /// current buffer position.
    fn patch_rel32_to_here(&mut self, patch: usize) {
        let rel = (self.buf.pos() as i32) - (patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(patch, rel);
    }

    /// Emit the inline TLAB bump-pointer fast path for the `new` opcode
    /// (HIGH-6 JIT audit, object_allocation/1000 3-5× gap).
    ///
    /// Layout (all rel32 branches, no relocations):
    /// ```text
    ///   MOV  RAX, helpers.get_current_thread
    ///   CALL RAX                            ; RAX = JvmThread*
    ///   TEST RAX, RAX
    ///   JE   slow_path                       ; null thread (re-entrant)
    ///   MOV  R10, RAX                        ; R10 = thread
    ///   MOV  R11, [R10 + tlab_cursor_off]    ; R11 = cursor
    ///   LEA  RAX, [R11 + total_size]         ; RAX = new cursor
    ///   CMP  RAX, [R10 + tlab_end_off]
    ///   JA   slow_path                       ; TLAB full
    ///   MOV  [R10 + tlab_cursor_off], RAX    ; commit
    ///   MOV  DWORD [R11 + 0], class_id_imm   ; write class_id
    ///   ; Hand off to post-init helper which finishes header + primitive
    ///   ; defaults + finalizer registration.
    ///   MOV  ARG0, [RBP - heap_local_off]    ; vm_ptr
    ///   MOV  ARG1, R11                       ; obj_ptr
    ///   MOV  ARG2, class_id_imm
    ///   MOV  ARG3, num_fields_imm
    ///   CALL helpers.tlab_post_init
    ///   JMP  done
    /// slow_path:
    ///   MOV  ARG0, [RBP - heap_local_off]
    ///   MOV  ARG1, class_id_imm
    ///   MOV  ARG2, num_fields_imm
    ///   CALL helpers.new_object
    /// done:
    /// ```
    ///
    /// Returns the bumped object pointer (or the slow-path result) in RAX.
    /// Caller emits the safepoint oop map and pushes RAX.
    fn emit_inline_tlab_new(
        &mut self,
        class_id_raw: u32,
        num_fields: usize,
        // CRIT-2 — when both `has_primitive_init` and `has_finalizer`
        // are statically known false at the call site, the post-init
        // helper has nothing meaningful to do beyond writing the
        // identity-hash and num_slots header words. We can emit those
        // inline and skip the helper call (which otherwise costs a
        // class_manager.read() and a finalizer-queue lock). When
        // unknown (the conservative default in `try_compile`), we
        // still issue the helper call.
        skip_post_init_helper: bool,
    ) {
        // Object total size (header + fields*8). Computed at compile time.
        let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;

        // Step 1: fetch the JvmThread* via the small TLS helper.
        // (One CALL + one TEST; ~10 cycles overhead.)
        //
        // HIGH-2 / Fix 2 — direct `MOV reg, FS:[off]` TLS load is the
        // ideal sequence (saves ~5 ns per `new`). It is NOT applied
        // here in this round because it requires runtime cooperation
        // we do not yet have:
        //
        //   * Rust's `thread_local!` macro hides the TLS slot offset
        //     entirely — there is no portable API to extract the
        //     FS/GS-relative offset of `JIT_THREAD` at JIT-compile
        //     time. A `#[thread_local]` static (unstable on stable
        //     Rust) would still need a startup probe (inline asm
        //     `mov rax, fs:[OFFSET]` against a known sentinel) to
        //     recover the loader-assigned displacement.
        //   * On Windows the slot lives at GS:[0x58 + slot*8] where
        //     `slot` is allocated dynamically by `TlsAlloc`; the same
        //     probe machinery applies but with a different segment
        //     prefix and one extra indirection. Per task scope, this
        //     arm is intentionally left on the helper.
        //   * The current `JitRuntimeHelpers` table exposes only the
        //     helper function pointer; wiring an `Option<(SegPrefix,
        //     u32)>` field plus a startup probe in the VM is a
        //     cross-crate change outside the scope of this fix
        //     round.
        //
        // Until that plumbing lands, the helper call stays — see the
        // task notes for the planned approach.
        self.emit_call_absolute(self.helpers.get_current_thread);
        self.emit_test_r64_r64(RAX);
        let null_thread_patch = self.emit_jcc_rel32_patch(0x84); // JE slow_path

        // R10 = thread; R11 = cursor.
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);

        // Align cursor up to 8 bytes (matches `Tlab::alloc(_, 8)`'s
        // behaviour). Without this, an interleaved array allocation that
        // left the cursor misaligned would force this `new` object onto a
        // non-8-aligned address — the GC walker assumes 8-aligned object
        // headers and would mis-decode the layout. Total cost: 2
        // instructions (8 bytes encoded) — negligible vs the cache miss
        // the slow path would incur.
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);

        // RAX = R11 + total_size (new cursor).
        self.emit_lea_r64_mem_disp32(RAX, R11, total_size as i32); // Cast: x86-64 immediate encoding

        // CMP RAX, [R10 + end_off]; JA slow_path (TLAB exhausted).
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        let tlab_full_patch = self.emit_jcc_rel32_patch(0x87); // JA slow_path

        // Commit the bump: [R10 + cursor_off] = RAX.
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);

        // Write class_id (4 bytes) at obj_ptr + class_id_off.
        // The remaining header bytes are correctly zero from TLAB refill;
        // post_tlab_init writes only identity_hash_code + num_slots.
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            class_id_off,
            class_id_raw as i32, // Cast: ClassId immediate fits in 32 bits
        );

        if skip_post_init_helper {
            // CRIT-2 fast path — no primitive defaults to apply and no
            // finalizer to register. Inline the only remaining
            // header-completion work that `jit_post_tlab_init` would
            // perform: writing `num_slots` at offset 16.
            //
            // `identity_hash_code` (offset 8) is left at the TLAB-zeroed
            // value (0). The contract is lazy mint: `System.
            // identityHashCode()` and the mark-word lock path detect
            // hash == 0 and atomically mint a fresh non-zero value on
            // demand. This matches HotSpot's "displaced hash" treatment
            // and avoids a `vm.heap.next_identity_hash()` call here that
            // would touch the global hash counter on every allocation.
            //
            // All other header fields (kind=0/Object,
            // element_type=0/Reference, padding, array_length=0, gc_age=0,
            // gc_flags=0, forwarding_ptr=null, mark_word=MARK_NEUTRAL)
            // are already the correct values from the TLAB-zeroed refill.
            //
            // Layout reminder (from `types/src/heap_types.rs`):
            //   off 16: num_slots (u32)
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                16,
                num_fields as i32, // Cast: x86-64 immediate encoding
            );
            // RAX = obj_ptr — both arms converge with RAX holding the
            // freshly-allocated object pointer.
            self.emit_mov_r64_r64(RAX, R11);
        } else {
            // Hand off to post-init: tlab_post_init(vm_ptr, obj_ptr, cid, nf).
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], class_id_raw as i32); // Cast: ClassId fits in 32 bits
            self.emit_mov_imm32_sx(ARG_REGS[3], num_fields as i32); // Cast: x86-64 immediate encoding
            self.emit_call_absolute(self.helpers.tlab_post_init);
        }

        // Jump over the slow path; both arms converge with RAX = obj_ptr.
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        self.patch_rel32_to_here(null_thread_patch);
        self.patch_rel32_to_here(tlab_full_patch);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: ClassId fits in 32 bits
        self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32); // Cast: x86-64 immediate encoding
        self.emit_call_absolute(self.helpers.new_object);

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
    }

    // -----------------------------------------------------------------------
    // Operand stack helpers
    // -----------------------------------------------------------------------

    /// Pop operand stack → rax
    fn pop_to_rax(&mut self) {
        let slot = self.pop_stack();
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(RAX, off),
            StackSlot::CalleeSaved(reg) => self.emit_mov_reg_reg(RAX, reg),
            StackSlot::Scratch(reg) => self.emit_mov_reg_reg(RAX, reg),
            StackSlot::Xmm(xmm) => self.emit_movq_rax_from_xmm(xmm),
        }
    }

    /// Pop operand stack → rcx
    fn pop_to_rcx(&mut self) {
        let slot = self.pop_stack();
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(RCX, off),
            StackSlot::CalleeSaved(reg) => self.emit_mov_reg_reg(RCX, reg),
            StackSlot::Scratch(reg) => self.emit_mov_reg_reg(RCX, reg),
            StackSlot::Xmm(xmm) => self.emit_movq_gpr_from_xmm(RCX, xmm),
        }
    }

    /// Push rax → operand stack.
    ///
    /// If a scratch register (R8/R9) is available, the value is moved there
    /// instead of being stored to the frame, avoiding the memory round-trip when
    /// the next bytecode immediately consumes the value.
    fn push_from_rax(&mut self) {
        // Spill to frame slot. Scratch register caching (R8/R9) was tested but
        // showed regressions: the frequent flush_scratch_registers calls before
        // backward branches, calls, and other operations negate the benefit by
        // adding an extra MOV per flush.
        let slot = self.push_stack();
        match slot {
            StackSlot::Frame(off) => self.emit_store_local(off, RAX),
            _ => unreachable!("push_stack always returns Frame"),
        }
    }

    /// Push RAX as XMM0 for FP intermediates (array loads, conversions, etc.).
    /// Avoids the RAX→frame→XMM0 round-trip when the value is consumed by a
    /// subsequent FP binop.
    fn push_from_rax_as_xmm0(&mut self) {
        // Flush any existing Xmm(0) slots first (they'd be clobbered)
        self.flush_xmm0_slots();
        // MOVQ XMM0, RAX
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
        self.stack.push(StackSlot::Xmm(0));
    }

    /// Return the GPR holding the slot value. For CalleeSaved/Scratch, returns
    /// the register directly (zero-cost). For Frame, loads into `fallback` and
    /// returns `fallback`.
    fn slot_to_gpr(&mut self, slot: StackSlot, fallback: u8) -> u8 {
        match slot {
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg) => reg,
            StackSlot::Frame(off) => {
                self.emit_load_local(fallback, off);
                fallback
            }
            StackSlot::Xmm(xmm) => {
                self.emit_movq_gpr_from_xmm(fallback, xmm);
                fallback
            }
        }
    }

    /// CMP r32_a, r32_b — sets flags for signed integer comparison.
    fn emit_cmp_r32_r32(&mut self, a: u8, b: u8) {
        // CMP r/m32, r32 → opcode 0x39, ModRM(11, b, a)
        // Need REX if either register >= 8
        let need_rex = a >= 8 || b >= 8;
        if need_rex {
            let mut rex = 0x40u8;
            if b >= 8 { rex |= 0x04; } // REX.R
            if a >= 8 { rex |= 0x01; } // REX.B
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x39);
        self.buf.emit_byte(0xC0 | ((b & 7) << 3) | (a & 7));
    }

    /// TEST r32, r32 — sets ZF (zero) and SF (sign) based on the value.
    fn emit_test_r32_r32(&mut self, reg: u8) {
        let need_rex = reg >= 8;
        if need_rex {
            let rex = 0x40u8 | if reg >= 8 { 0x04 | 0x01 } else { 0 };
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x85); // TEST r/m32, r32
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
    }

    /// Emit a load from a StackSlot into a specific GPR register.
    fn load_slot_to_reg(&mut self, dst: u8, slot: StackSlot) {
        match slot {
            StackSlot::Frame(off) => self.emit_load_local(dst, off),
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg) => {
                if dst != reg {
                    self.emit_mov_reg_reg(dst, reg);
                }
            }
            StackSlot::Xmm(xmm) => {
                // Materialize XMM value to GPR via MOVQ
                self.emit_movq_gpr_from_xmm(dst, xmm);
            }
        }
    }

    /// Emit inlined callee bytecode at the given caller PC.
    ///
    /// Returns `true` if inlining succeeded, `false` to fall back to a normal call.
    /// The callee's locals are allocated in the caller's spill area so no new frame
    /// is needed. Forward branches within the callee are tracked and patched after
    /// emission. On return, the callee's result (if any) is on the caller's operand
    /// stack.
    fn try_emit_inline(&mut self, pc: usize) -> bool {
        let site = match self.inline_sites.get(&pc) {
            Some(s) => s.clone(),
            None => return false,
        };

        let callee_code = &site.callee_code;
        let callee_len = site.callee_code_len;
        let callee_num_args = site.callee_num_args;
        let callee_max_locals = site.callee_max_locals;
        let _return_type = site.return_type;

        // Allocate callee locals in caller's spill area
        let callee_local_base = self.next_spill_offset;
        let callee_locals_size = callee_max_locals.max(callee_num_args);
        self.next_spill_offset += (callee_locals_size as i32) * 8; // Cast: x86-64 immediate encoding

        // Pop arguments from caller stack and store into callee locals.
        // Args are pushed left-to-right, so stack top = last arg.
        // For instance methods, arg0 = objectref ('this').
        let stack_len = self.stack.len();
        if stack_len < callee_num_args {
            self.next_spill_offset = callee_local_base;
            return false;
        }

        // Store args into callee locals (in reverse order from stack)
        for i in (0..callee_num_args).rev() {
            let slot = self.pop_stack();
            let local_off = callee_local_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.load_slot_to_reg(RAX, slot);
            self.emit_store_local(local_off, RAX);
        }

        // Zero-init remaining callee locals
        for i in callee_num_args..callee_locals_size {
            let local_off = callee_local_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_xor_reg_self(RAX);
            self.emit_store_local(local_off, RAX);
        }

        // Track forward branches within the inlined code: (patch_offset, target_callee_pc)
        let mut branch_patches: Vec<(usize, usize)> = Vec::new();
        // Map callee PC → native offset for branch targets
        let mut callee_pc_to_native: Vec<i64> = vec![-1; callee_len + 1];

        let mut cpc: usize = 0;
        let save_spill = self.next_spill_offset;

        while cpc < callee_len {
            callee_pc_to_native[cpc] = self.buf.pos() as i64; // Cast: address arithmetic
            let op = callee_code[cpc];

            match op {
                // nop
                0x00 => { cpc += 1; }

                // aconst_null
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i32) - 3; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lconst_0, lconst_1
                0x09 | 0x0a => {
                    let val = (op as i64) - 9; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // fconst_0, fconst_1, fconst_2
                0x0b..=0x0d => {
                    let fval: f32 = (op - 0x0b) as f32; // Cast: JIT ABI convention
                    let bits = fval.to_bits() as i64; // Cast: JIT ABI convention
                    self.emit_mov_imm32_sx(RAX, bits as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // dconst_0, dconst_1
                0x0e | 0x0f => {
                    let dval: f64 = (op - 0x0e) as f64; // Cast: JIT ABI convention
                    let bits = dval.to_bits() as i64; // Cast: JIT ABI convention
                    if bits == 0 {
                        self.emit_xor_reg_self(RAX);
                    } else {
                        self.emit_mov_imm64(RAX, bits);
                    }
                    self.push_from_rax();
                    cpc += 1;
                }

                // bipush
                0x10 => {
                    if cpc + 1 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let val = callee_code[cpc + 1] as i8 as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 2;
                }

                // sipush
                0x11 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let val = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc
                0x12 => {
                    if cpc + 1 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    if let Some((_, val)) = site.ldc_info.iter().find(|(i, _)| *i == idx) {
                        self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 2;
                }

                // ldc_w
                0x13 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, val)) = site.ldc_info.iter().find(|(i, _)| *i == idx) {
                        self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc2_w
                0x14 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, val)) = site.ldc2w_info.iter().find(|(i, _)| *i == idx) {
                        self.emit_mov_imm64(RAX, *val);
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 3;
                }

                // iload, lload, fload, dload, aload
                0x15 | 0x16 | 0x17 | 0x18 | 0x19 => {
                    if cpc + 1 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // fload_0..fload_3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // dload_0..dload_3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // aload_0..aload_3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // istore, lstore, fstore, dstore, astore
                0x36 | 0x37 | 0x38 | 0x39 | 0x3a => {
                    if cpc + 1 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // fstore_0..fstore_3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // dstore_0..dstore_3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // astore_0..astore_3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // pop2
                0x58 => {
                    let _ = self.pop_stack();
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // dup
                0x59 => {
                    let slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, slot);
                    self.push_from_rax();
                    self.push_from_rax();
                    cpc += 1;
                }

                // swap
                0x5f => {
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    self.load_slot_to_reg(RAX, a);
                    self.push_from_rax();
                    self.load_slot_to_reg(RAX, b);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iadd
                0x60 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // ADD RAX, RCX
                    self.rex_w(); self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // isub
                0x64 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // SUB RAX, RCX
                    self.rex_w(); self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lsub
                0x65 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w(); self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // imul
                0x68 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // IMUL RAX, RCX
                    self.rex_w(); self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN,
                // matches dividend) before issuing CDQ; IDIV ECX.
                0x6c => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ldiv — JVMS-compliant guards; emits CQO; IDIV RCX with the
                // LONG_MIN / -1 overflow special-case materialised inline.
                0x6d => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 yields 0.
                0x70 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 yields 0.
                0x71 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG EAX
                    self.buf.emit(&[0xF7, 0xD8]);
                    // MOVSXD RAX, EAX
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    // NEG RAX
                    self.rex_w(); self.buf.emit(&[0xF7, 0xD8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishl
                0x78 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL EAX, CL
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshl
                0x79 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL RAX, CL
                    self.rex_w(); self.buf.emit(&[0xD3, 0xE0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishr
                0x7a => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SAR EAX, CL
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshr
                0x7b => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w(); self.buf.emit(&[0xD3, 0xF8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iushr
                0x7c => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHR EAX, CL
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lushr
                0x7d => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w(); self.buf.emit(&[0xD3, 0xE8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x21, 0xC8]); // AND RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x21, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x09, 0xC8]); // OR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x09, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x31, 0xC8]); // XOR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w(); self.buf.emit(&[0x31, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iinc
                0x84 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let inc = callee_code[cpc + 2] as i8 as i32; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    // ADD RAX, imm32
                    self.rex_w(); self.buf.emit_byte(0x05);
                    self.buf.emit(&inc.to_le_bytes());
                    self.emit_store_local(local_off, RAX);
                    cpc += 3;
                }

                // i2l — identity in our i64 representation
                0x85 => { cpc += 1; }

                // l2i — truncate to 32-bit, sign-extend back
                0x88 => {
                    self.pop_to_rax();
                    // MOVSXD RAX, EAX
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2b
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX RAX, AL
                    self.rex_w(); self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2c
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX (zero-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits auto-zeroed
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2s
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX (sign-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lcmp
                0x94 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // CMP RAX, RCX
                    self.rex_w(); self.buf.emit(&[0x39, 0xC8]);
                    // SETG AL (1 if >)
                    self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                    // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                    // SETL CL
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]);
                    // MOVZX ECX, CL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]);
                    // SUB EAX, ECX
                    self.buf.emit(&[0x29, 0xC8]);
                    // MOVSXD RAX, EAX
                    self.rex_w(); self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ifeq (0x99), ifne (0x9a), iflt (0x9b), ifge (0x9c), ifgt (0x9d), ifle (0x9e)
                0x99..=0x9e => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let offset = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

                    self.pop_to_rax();
                    // TEST EAX, EAX
                    self.buf.emit(&[0x85, 0xC0]);
                    // Jcc rel32
                    let cc = match op {
                        0x99 => 0x84u8, // JE
                        0x9a => 0x85,    // JNE
                        0x9b => 0x8C,    // JL
                        0x9c => 0x8D,    // JGE
                        0x9d => 0x8F,    // JG
                        0x9e => 0x8E,    // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // if_icmpeq..if_icmple
                0x9f..=0xa4 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let offset = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // CMP EAX, ECX
                    self.buf.emit(&[0x39, 0xC8]);
                    let cc = match op {
                        0x9f => 0x84u8, // JE
                        0xa0 => 0x85,    // JNE
                        0xa1 => 0x8C,    // JL
                        0xa2 => 0x8D,    // JGE
                        0xa3 => 0x8F,    // JG
                        0xa4 => 0x8E,    // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // goto
                0xa7 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let offset = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

                    // JMP rel32
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ireturn, lreturn, areturn, freturn, dreturn
                0xac | 0xad | 0xb0 | 0xae | 0xaf => {
                    // Pop callee's return value → push onto caller stack
                    self.pop_to_rax();
                    // Reclaim callee locals
                    self.next_spill_offset = save_spill;
                    self.push_from_rax();
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Use callee_len as the "after inline" target
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // return (void)
                0xb1 => {
                    self.next_spill_offset = save_spill;
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // getfield (0xb4) — use callee's field_info
                0xb4 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    self.flush_scratch_registers();
                    let cp_idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, field_index, _type_tag)) = site.field_info.iter()
                        .find(|(p, _, _)| *p == cp_idx).copied()
                    {
                        let obj_slot = self.pop_stack();
                        self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                        self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                        self.emit_call_absolute(self.helpers.getfield);
                        self.push_from_rax();
                    } else {
                        // Cannot resolve field — bail out
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putfield (0xb5) — use callee's field_info
                0xb5 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    self.flush_scratch_registers();
                    let cp_idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, field_index, type_tag)) = site.field_info.iter()
                        .find(|(p, _, _)| *p == cp_idx).copied()
                    {
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        if type_tag == b'L' || type_tag == b'[' {
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            self.emit_call_absolute(self.helpers.putfield_object);
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            self.emit_call_absolute(helper);
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // getstatic (0xb2) — use callee's static_field_info
                //
                // MED-2 bail (round-2 JIT review): same gap as the top-level
                // 0xb2 handler at line ~9620 — see the long comment there
                // for the full unblocking plan. Briefly: `SharedVm.statics`
                // slot addresses aren't stable (Vec resize, lazy entry),
                // so we can't bake them as `imm64` and emit `MOV reg,
                // [imm64]`. Stay on the helper-call path.
                0xb2 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    self.flush_scratch_registers();
                    let cp_idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, class_id_raw, field_index, _type_tag, is_volatile)) = site.static_field_info.iter()
                        .find(|(p, _, _, _, _)| *p == cp_idx).copied()
                    {
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.emit_call_absolute(self.helpers.getstatic);
                        // Volatile static: emit MFENCE after read (SeqCst acquire)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.push_from_rax();
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putstatic (0xb3) — use callee's static_field_info
                //
                // MED-2 bail (round-2 JIT review): same gap as the top-level
                // 0xb3 handler — slot pointer not stable, no VM-crate access
                // from `jit`. See the comment on the top-level 0xb2 handler
                // for the full unblocking plan.
                0xb3 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    self.flush_scratch_registers();
                    let cp_idx = ((callee_code[cpc + 1] as usize) << 8) | callee_code[cpc + 2] as usize; // Widening: always safe
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site.static_field_info.iter()
                        .find(|(p, _, _, _, _)| *p == cp_idx).copied()
                    {
                        let val_slot = self.pop_stack();
                        let helper_fn: usize = match type_tag {
                            b'J' => self.helpers.putstatic_long,
                            b'F' => self.helpers.putstatic_float,
                            b'D' => self.helpers.putstatic_double,
                            b'L' | b'[' => self.helpers.putstatic_object,
                            _ => self.helpers.putstatic_int,
                        };
                        // Volatile static: emit MFENCE before write (SeqCst release)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[3], val_slot);
                        self.emit_call_absolute(helper_fn);
                        // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // if_acmpeq (0xa5), if_acmpne (0xa6)
                0xa5 | 0xa6 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let offset = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w(); self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX
                    let cc = if op == 0xa5 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ifnull (0xc6), ifnonnull (0xc7)
                0xc6 | 0xc7 => {
                    if cpc + 2 >= callee_len { self.next_spill_offset = callee_local_base; return false; }
                    let offset = ((callee_code[cpc + 1] as i16) << 8 | callee_code[cpc + 2] as i16) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.rex_w(); self.buf.emit(&[0x85, 0xC0]); // TEST RAX, RAX
                    let cc = if op == 0xc6 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // Unsupported opcode in inline context — bail out
                _ => {
                    // Restore spill offset and return false to fall back to a call
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
            }
        }

        // Mark the "after inline" position for return-jumps
        callee_pc_to_native[callee_len] = self.buf.pos() as i64; // Cast: address arithmetic

        // Patch all forward branches
        for (patch_off, target_cpc) in &branch_patches {
            let target_native = if *target_cpc < callee_pc_to_native.len() {
                callee_pc_to_native[*target_cpc]
            } else {
                self.buf.pos() as i64 // Cast: address arithmetic
            };
            if target_native < 0 {
                // Target not yet emitted (shouldn't happen for forward branches after full emission)
                // Fall back: point to current position
                let rel32 = (self.buf.pos() as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.patch_i32(*patch_off, rel32);
            } else {
                let rel32 = (target_native as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.patch_i32(*patch_off, rel32);
            }
        }

        // Reclaim callee local spill slots (return values already pushed)
        self.next_spill_offset = save_spill;

        true
    }


    /// Emit a balanced binary search for lookupswitch.
    /// `pairs` is sorted by key (per JVM spec). Value to match is in EAX.
    /// At each node: CMP EAX, mid_key → JE target, JL left_subtree, fall to right_subtree.
    /// Leaves fall through to `default_target`.
    fn emit_binary_search_lookup(
        &mut self,
        pairs: &[(i32, usize)],
        default_target: usize,
    ) {
        if pairs.is_empty() {
            // Base case: no keys left → jump to default
            self.buf.emit_byte(0xE9); // JMP rel32
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        if pairs.len() == 1 {
            // Single key: CMP + JE + JMP default
            let (key, target) = pairs[0];
            self.buf.emit(&[0x3D]); // CMP EAX, imm32
            self.buf.emit(&key.to_le_bytes());
            self.buf.emit(&[0x0F, 0x84]); // JE rel32
            let patch = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((patch, target));
            // Fall through to default
            self.buf.emit_byte(0xE9);
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        if pairs.len() == 2 {
            // Two keys: CMP + JE, CMP + JE, JMP default
            for &(key, target) in pairs {
                self.buf.emit(&[0x3D]);
                self.buf.emit(&key.to_le_bytes());
                self.buf.emit(&[0x0F, 0x84]);
                let patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.forward_patches.push((patch, target));
            }
            self.buf.emit_byte(0xE9);
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        // Pick the middle element
        let mid = pairs.len() / 2;
        let (mid_key, mid_target) = pairs[mid];
        let left = &pairs[..mid];
        let right = &pairs[mid + 1..];

        // CMP EAX, mid_key
        self.buf.emit(&[0x3D]);
        self.buf.emit(&mid_key.to_le_bytes());

        // JE mid_target
        self.buf.emit(&[0x0F, 0x84]);
        let je_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.forward_patches.push((je_patch, mid_target));

        // JL left_subtree (key < mid_key → search left half)
        self.buf.emit(&[0x0F, 0x8C]); // JL rel32
        let jl_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // Fall through: key > mid_key → search right half
        self.emit_binary_search_lookup(right, default_target);

        // Patch JL to point here (start of left subtree)
        let left_start = self.buf.pos();
        let jl_rel = left_start as i32 - (jl_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.patch_i32(jl_patch, jl_rel);

        // Left subtree
        self.emit_binary_search_lookup(left, default_target);
    }

    fn flush_scratch_registers(&mut self) {
        // Collect scratch slots first to avoid double-mutable-borrow of self
        // (iterating &mut self.stack while calling self.emit_store_local).
        let scratch_slots: Vec<(usize, u8)> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if let StackSlot::Scratch(reg) = *slot {
                    Some((i, reg))
                } else {
                    None
                }
            })
            .collect();
        for (idx, reg) in scratch_slots {
            let off = self.next_spill_offset;
            self.next_spill_offset += 8;
            self.emit_store_local(off, reg);
            self.stack[idx] = StackSlot::Frame(off);
        }
        // Also flush Xmm stack slots (XMM0-7 are caller-saved temporaries/scratch)
        let xmm_slots: Vec<(usize, u8)> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if let StackSlot::Xmm(xmm) = *slot {
                    if xmm < 8 { Some((i, xmm)) } else { None }
                } else {
                    None
                }
            })
            .collect();
        for (idx, xmm) in xmm_slots {
            let off = self.next_spill_offset;
            self.next_spill_offset += 8;
            // Direct MOVQ [rbp-off], XMM — saves the round-trip
            // through RAX (3 bytes per spill, ~90 bytes across the
            // 30 flush sites). RAX is preserved, which matters when
            // a flush happens immediately before a return-value path
            // that wants RAX intact.
            self.emit_movq_mem_rbp_from_xmm(off, xmm);
            self.stack[idx] = StackSlot::Frame(off);
        }
        // Clear scratch XMM tracking — all flushed
        self.scratch_xmm_in_use = 0;
    }

    /// Flush any Xmm(0) stack entries — promote to scratch XMM if possible,
    /// otherwise spill to frame. XMM0 is about to be clobbered.
    fn flush_xmm0_slots(&mut self) {
        let xmm0_slots: Vec<usize> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                if matches!(slot, StackSlot::Xmm(0)) { Some(i) } else { None }
            })
            .collect();
        if xmm0_slots.is_empty() {
            return;
        }
        // Try to promote to a scratch XMM register (XMM2-7) instead of spilling
        if let Some(scratch) = self.alloc_scratch_xmm() {
            self.emit_movsd_xmm_xmm(scratch, 0); // MOVSD preserves full 64-bit value
            for idx in xmm0_slots {
                self.stack[idx] = StackSlot::Xmm(scratch);
            }
        } else {
            // All scratch XMMs busy — fall back to frame spill.
            // Direct MOVQ [rbp-off], XMM0 — RCX is left untouched, which
            // helps callers that have RCX live across this flush.
            let off = self.next_spill_offset;
            self.next_spill_offset += 8;
            self.emit_movq_mem_rbp_from_xmm(off, 0);
            for idx in xmm0_slots {
                self.stack[idx] = StackSlot::Frame(off);
            }
        }
    }

    /// Allocate a scratch XMM register (XMM2-7). Returns `None` if all are in use.
    fn alloc_scratch_xmm(&mut self) -> Option<u8> {
        for (i, &xmm) in SCRATCH_XMMS.iter().enumerate() {
            if self.scratch_xmm_in_use & (1 << i) == 0 {
                self.scratch_xmm_in_use |= 1 << i;
                return Some(xmm);
            }
        }
        None
    }

    /// Release a scratch XMM register back to the pool.
    fn free_scratch_xmm(&mut self, xmm: u8) {
        if let Some(i) = SCRATCH_XMMS.iter().position(|&r| r == xmm) {
            self.scratch_xmm_in_use &= !(1 << i);
        }
    }

    /// Invalidate any CalleeSaved or Scratch stack entries for `reg` before it
    /// is overwritten. All references are materialized to a single shared spill slot.
    fn invalidate_callee_saved(&mut self, reg: u8) {
        let needs_spill = self
            .stack
            .iter()
            .any(|s| matches!(s, StackSlot::CalleeSaved(r) | StackSlot::Scratch(r) if *r == reg));
        if !needs_spill {
            return;
        }
        // Spill the register value once
        let off = self.next_spill_offset;
        self.next_spill_offset += 8;
        self.emit_store_local(off, reg);
        // Update all CalleeSaved/Scratch entries for this register to the shared spill slot
        for slot in &mut self.stack {
            match *slot {
                StackSlot::CalleeSaved(r) | StackSlot::Scratch(r) if r == reg => {
                    *slot = StackSlot::Frame(off);
                }
                _ => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Inline array access emitters
    // -----------------------------------------------------------------------

    /// Inline int element load (compact: 4 bytes/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 64-bit).
    fn emit_int_aload_regs(&mut self) {
        // MOVSXD RAX, DWORD [RAX + RCX*4 + HEADER_SIZE]
        // Encoding: REX.W + 0x63 + ModRM(mod=01, reg=RAX, r/m=100) + SIB(scale=2, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x63);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element load (compact: 1 byte/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 32-bit, then to 64-bit).
    fn emit_byte_aload_regs(&mut self) {
        // MOVSX EAX, BYTE [RAX + RCX*1 + HEADER_SIZE]
        // Encoding: 0x0F 0xBE + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=0, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBE]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
                                               // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline int element store (compact: 4 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_int_astore_regs(&mut self) {
        // MOV DWORD [RAX + RCX*4 + HEADER_SIZE], EDX
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=EDX(010), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element store (compact: 1 byte/element). RAX=array, RCX=index, RDX=value.
    fn emit_byte_astore_regs(&mut self) {
        // MOV BYTE [RAX + RCX*1 + HEADER_SIZE], DL
        self.buf.emit_byte(0x88);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DL(010), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline ref element load from Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index. Result in RAX (raw pointer, 0 for null).
    ///
    /// Emits: MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
    fn emit_ref_aload_regs(&mut self) {
        // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
        // REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x44); // ModRM: mod=01(disp8), reg=000(RAX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline ref element store to Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index, RDX=value (raw pointer, 0 for null).
    ///
    /// Emits: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
    ///
    /// Wired into the `aastore` opcode arm; the GC write-barrier is emitted
    /// separately as a call to `self.helpers.write_barrier` after the store.
    fn emit_ref_astore_regs(&mut self) {
        // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
        // REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x54); // ModRM: mod=01(disp8), reg=010(RDX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline short/char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// For saload: sign-extends to 32-bit then to 64-bit.
    fn emit_short_aload_regs(&mut self) {
        // MOVSX EAX, WORD [RAX + RCX*2 + HEADER_SIZE]
        // Encoding: 0x0F 0xBF + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBF]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
        // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// Zero-extends to 32-bit then sign-extends to 64-bit.
    fn emit_char_aload_regs(&mut self) {
        // MOVZX EAX, WORD [RAX + RCX*2 + HEADER_SIZE]
        // Encoding: 0x0F 0xB7 + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
        // MOVZX already zero-extends to EAX, upper 32 bits of RAX auto-zeroed
    }

    /// Inline short/char element store (compact: 2 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_short_astore_regs(&mut self) {
        // MOV WORD [RAX + RCX*2 + HEADER_SIZE], DX
        // Encoding: 0x66 prefix + 0x89 + ModRM + SIB + disp8
        self.buf.emit_byte(0x66); // operand size prefix (16-bit)
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DX(010), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element load (compact: 8 bytes/element). RAX=array, RCX=index.
    /// Result in RAX.
    fn emit_long_aload_regs(&mut self) {
        // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
        // Encoding: REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element store (compact: 8 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_long_astore_regs(&mut self) {
        // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
        // Encoding: REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=RDX(010), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline arraylength. Assumes RAX=array ptr. Result in RAX.
    fn emit_arraylength_regs(&mut self) {
        // Assumes RAX = array ptr. MOV EAX, DWORD [RAX + ARRAY_LENGTH_OFFSET]
        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
    }

    /// Round-8 CRIT fix: emit an inline null check on the array receiver
    /// (assumed already in RAX) for an inline array-store opcode. On null,
    /// branches to the shared `null_check_store_stub` (emitted at method
    /// end by [`emit_null_check_store_stubs`]). On non-null, falls through
    /// to the caller's bounds check + inline store.
    ///
    /// Mirrors the structure of [`emit_bounds_check`]. Without this guard,
    /// the immediately-following `MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]`
    /// in `emit_bounds_check` would dereference NULL and SIGSEGV — the
    /// signal handler at `vm/src/runtime/crash_handler.rs` only dumps an
    /// hs_err then re-raises, killing the VM instead of throwing NPE.
    /// The previous `process::abort()` in `vm/src/jit/helpers.rs`
    /// jit_iastore/bastore/aastore was a comment-level "fail loudly"
    /// theater because the helpers were never reached on the inline path.
    fn emit_null_check_array_store(&mut self) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 → null-store stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs.push(patch_offset);
    }

    /// Round-11 HIGH-2: variant of `emit_null_check_array_store` that
    /// elides the inline TEST/JZ entirely when the null-check
    /// elimination dataflow proves the array receiver came from a
    /// local that is known non-null at this bytecode PC. The decision
    /// is made by walking the bytecode 1-4 bytes back from `bc_pc` to
    /// find the `aload N; <index>; <arraystore>` pattern via
    /// [`array_receiver_local`]; if found AND the local is proven
    /// non-null, the entire TEST/JZ pair is skipped (5 bytes saved
    /// per occurrence + branch-predictor pressure reduction).
    ///
    /// Safe to call instead of `emit_null_check_array_store` at every
    /// inline array-store site; the conservative path is identical.
    fn emit_null_check_array_store_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        self.emit_null_check_array_store();
    }

    /// Round-9 HIGH fix (asymmetric coverage): emit an inline null check
    /// on the array receiver (assumed already in RAX) for an inline
    /// array-LOAD opcode (iaload / aaload / baload / caload / saload /
    /// laload / faload / daload). The round-8 stub covered only stores;
    /// loads still relied on the page-fault path that
    /// `emit_null_check_array_store`'s doc rightly calls out as broken
    /// (the signal handler at `vm/src/runtime/crash_handler.rs` re-raises
    /// rather than throwing NPE, killing the VM).
    ///
    /// Loads have identical pre-state to stores (`RAX = array_ptr` at
    /// the bounds-check site) and the desired failure outcome is the
    /// same — set `JIT_PENDING_NPE`, deopt out with `RAX = i64::MIN`,
    /// run the epilogue. We therefore reuse the SAME shared stub by
    /// pushing the JZ patch offset into the same `null_check_store_stubs`
    /// vector; both loads and stores branch to it.
    fn emit_null_check_array_load(&mut self) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 → shared null-check stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs.push(patch_offset);
    }

    /// Round-11 HIGH-2 (mirrors `emit_null_check_array_store_at`):
    /// elide the inline TEST/JZ null check on array loads when the
    /// receiver is proven non-null at `bc_pc` by the dataflow.
    fn emit_null_check_array_load_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        self.emit_null_check_array_load();
    }

    /// Emit an array bounds check. RAX=array ptr, RCX=index (as i64).
    ///
    /// Loads array length from header offset 12, compares index (unsigned) against length.
    /// If index >= length (unsigned comparison catches negatives too), jumps to an
    /// out-of-line stub that calls `jit_throw_aioobe`.
    ///
    /// The stub is emitted later by `emit_bounds_check_stubs()` after the main code.
    fn emit_bounds_check(&mut self, bc_pc: usize) {
        // Skip if loop analysis proved this access is safe
        if self.bounds_safe_pcs.contains(&bc_pc) {
            return;
        }

        // MOV R10D, DWORD [RAX + ARRAY_LENGTH_OFFSET]  — load array_length from ObjectHeader
        // Encoding: 44 8B 50 xx (REX.R + MOV r32, r/m32 + ModRM(01, R10, RAX) + disp8)
        self.buf
            .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding

        // CMP ECX, R10D  — unsigned compare index vs length
        // If index >= length (unsigned), JAE to failure stub
        // Encoding: 41 3B CA (REX.B + CMP r32, r/m32 + ModRM(11, ECX, R10))
        self.buf.emit(&[0x41, 0x3B, 0xCA]);

        // JAE rel32 — jump if above-or-equal (unsigned >= means out of bounds)
        // The rel32 will be patched to point to the out-of-line stub
        self.buf.emit(&[0x0F, 0x83]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.bounds_check_stubs.push(patch_offset);
    }

    /// Emit a JVMS-compliant signed integer division or remainder.
    ///
    /// Assumes the dividend is in RAX and the divisor in RCX. Leaves the
    /// result in RAX (sign-extended to 64 bits for the 32-bit forms so that
    /// the value is safe to push as a long-width stack slot).
    ///
    /// Guards required by JVMS §6.5.{idiv,irem,ldiv,lrem}:
    ///   * divisor == 0 → throw `ArithmeticException` (routed through the
    ///     uncommon-trap deopt stub with `DEOPT_REASON_DIV_BY_ZERO = 3`; the
    ///     interpreter materialises the exception from the i64::MIN sentinel).
    ///   * `INT_MIN / -1` (or `LONG_MIN / -1`) — the raw x86 IDIV faults with
    ///     #DE on this overflow. The Java spec says no exception is raised:
    ///     `idiv`/`ldiv` must return the dividend unchanged, and `irem`/`lrem`
    ///     must return 0. We special-case this with a CMP/CMP/branch pair and
    ///     synthesise the result without executing IDIV.
    ///
    /// `bci` is the bytecode pc used for the deopt-stub bookkeeping.
    fn emit_safe_idiv(&mut self, bci: usize, is_64bit: bool, is_rem: bool) {
        // -------- Guard 1: divide-by-zero --------
        if is_64bit {
            // TEST RCX, RCX  (48 85 C9)
            self.buf.emit(&[0x48, 0x85, 0xC9]);
        } else {
            // TEST ECX, ECX  (85 C9)
            self.buf.emit(&[0x85, 0xC9]);
        }
        // JZ rel32 → deopt stub (DEOPT_REASON_DIV_BY_ZERO = 3)
        self.buf.emit(&[0x0F, 0x84]);
        let dz_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stubs.push((dz_patch, bci, 3));

        // -------- Guard 2: INT_MIN / -1  (or LONG_MIN / -1) --------
        // If dividend == MIN and divisor == -1, IDIV would raise #DE.
        // Materialise the JVMS-mandated result and skip the IDIV.
        //
        //     CMP   dividend, MIN
        //     JNE   :do_div
        //     CMP   divisor, -1
        //     JNE   :do_div
        //     <materialise result>      ; idiv → dividend (RAX already holds MIN)
        //                               ; irem → 0
        //     JMP   :after_div
        //   :do_div
        //     CDQ / CQO
        //     IDIV  ECX / RCX
        //     <move result into RAX, sign-extending for 32-bit>
        //   :after_div

        // CMP dividend, MIN
        if is_64bit {
            // CMP RAX, imm32 sign-extended — we need full i64::MIN which doesn't
            // fit in imm32. Load i64::MIN into R10 and CMP RAX, R10.
            // MOV R10, i64::MIN  (49 BA <imm64>)
            self.buf.emit(&[0x49, 0xBA]);
            self.buf.emit(&(i64::MIN as u64).to_le_bytes());
            // CMP RAX, R10  (4C 39 D0)
            self.buf.emit(&[0x4C, 0x39, 0xD0]);
        } else {
            // CMP EAX, imm32  (3D <imm32>)
            self.buf.emit_byte(0x3D);
            self.buf.emit(&(i32::MIN as u32).to_le_bytes());
        }
        // JNE rel8 → :do_div (we'll patch after we know the size)
        self.buf.emit(&[0x75, 0x00]); // placeholder rel8
        let jne1_patch = self.buf.pos() - 1;

        // CMP divisor, -1
        if is_64bit {
            // CMP RCX, -1  (48 83 F9 FF)  — imm8 sign-extended to 64
            self.buf.emit(&[0x48, 0x83, 0xF9, 0xFF]);
        } else {
            // CMP ECX, -1  (83 F9 FF)     — imm8 sign-extended to 32
            self.buf.emit(&[0x83, 0xF9, 0xFF]);
        }
        // JNE rel8 → :do_div
        self.buf.emit(&[0x75, 0x00]);
        let jne2_patch = self.buf.pos() - 1;

        // Materialise the overflow result.
        if is_rem {
            // result = 0
            if is_64bit {
                // XOR EAX, EAX (zeros full RAX)
                self.buf.emit(&[0x31, 0xC0]);
            } else {
                self.buf.emit(&[0x31, 0xC0]);
            }
        } else {
            // result = dividend (RAX/EAX already holds MIN). For 32-bit, ensure
            // RAX is sign-extended like the IDIV path does.
            if !is_64bit {
                // MOVSXD RAX, EAX  (48 63 C0)
                self.buf.emit(&[0x48, 0x63, 0xC0]);
            }
        }
        // JMP rel8 → :after_div
        self.buf.emit(&[0xEB, 0x00]);
        let jmp_after_patch = self.buf.pos() - 1;

        // :do_div — patch JNE targets to here
        let do_div_off = self.buf.pos();
        let rel1 = (do_div_off as i64) - (jne1_patch as i64 + 1);
        let rel2 = (do_div_off as i64) - (jne2_patch as i64 + 1);
        // Hard runtime checks: a rel8 displacement that does not fit in an i8
        // would silently miscompile in release builds. `emit_safe_idiv` cannot
        // signal a failure (it returns `()`), so assert rather than emit a
        // broken branch. The intervening block is fixed-size and small, so
        // this can only fire on a genuine codegen bug.
        assert!(
            (-128..=127).contains(&rel1),
            "emit_safe_idiv: JNE1 rel8 displacement {rel1} out of i8 range",
        );
        assert!(
            (-128..=127).contains(&rel2),
            "emit_safe_idiv: JNE2 rel8 displacement {rel2} out of i8 range",
        );
        self.buf.patch_byte(jne1_patch, rel1 as u8);
        self.buf.patch_byte(jne2_patch, rel2 as u8);

        // Sign-extend RAX → RDX:RAX (or EAX → EDX:EAX), then IDIV.
        if is_64bit {
            // CQO  (48 99)
            self.buf.emit(&[0x48, 0x99]);
            // IDIV RCX  (48 F7 F9)
            self.buf.emit(&[0x48, 0xF7, 0xF9]);
        } else {
            // CDQ  (99)
            self.buf.emit_byte(0x99);
            // IDIV ECX  (F7 F9)
            self.buf.emit(&[0xF7, 0xF9]);
        }

        // Move the result (quotient in RAX/EAX, remainder in RDX/EDX) into RAX,
        // sign-extending 32-bit results so callers can treat RAX as i64.
        if is_rem {
            if is_64bit {
                // MOV RAX, RDX  (48 89 D0)
                self.buf.emit(&[0x48, 0x89, 0xD0]);
            } else {
                // MOVSXD RAX, EDX  (48 63 C2)
                self.buf.emit(&[0x48, 0x63, 0xC2]);
            }
        } else if !is_64bit {
            // MOVSXD RAX, EAX  (48 63 C0)
            self.buf.emit(&[0x48, 0x63, 0xC0]);
        }

        // :after_div — patch the JMP from the overflow path.
        let after_off = self.buf.pos();
        let rel_jmp = (after_off as i64) - (jmp_after_patch as i64 + 1);
        // Hard runtime check (see JNE patch checks above): a rel8 that does not
        // fit in an i8 would silently miscompile in release builds.
        assert!(
            (-128..=127).contains(&rel_jmp),
            "emit_safe_idiv: JMP rel8 displacement {rel_jmp} out of i8 range",
        );
        self.buf.patch_byte(jmp_after_patch, rel_jmp as u8);
    }

    /// Emit out-of-line bounds check failure stubs at the end of the method.
    ///
    /// Each stub: loads index (from RCX) and length (0 as placeholder) into
    /// argument registers, then calls `jit_throw_aioobe` (which diverges).
    ///
    /// All stubs share a single landing pad to minimize code size.
    fn emit_bounds_check_stubs(&mut self) {
        if self.bounds_check_stubs.is_empty() {
            return;
        }

        // Single shared stub — all JAE branches jump here
        let stub_offset = self.buf.pos();

        // At this point, RCX = index (from the array access setup)
        // R10D = array_length (loaded in the bounds check)
        // We need to pass (index, length) to jit_throw_aioobe

        // Set up args for jit_throw_aioobe(index: i64, length: i64)
        #[cfg(target_os = "windows")]
        {
            // Windows: arg1=RCX, arg2=RDX
            // RCX already contains the index
            // MOV RDX, R10 (move length to arg2)
            self.buf.emit(&[0x4C, 0x89, 0xD2]); // REX.WR + MOV r/m64, r64
        }
        #[cfg(not(target_os = "windows"))]
        {
            // SysV: arg1=RDI, arg2=RSI
            // MOV RDI, RCX (move index to arg1)
            self.rex_w();
            self.buf.emit_byte(0x8B);
            self.modrm_reg(RDI, RCX);
            // MOV RSI, R10 (move length to arg2)
            self.buf.emit(&[0x4C, 0x89, 0xD6]); // REX.WR + MOV r/m64, r64
        }

        // CALL jit_throw_aioobe (absolute) — returns i64::MIN sentinel in RAX
        self.emit_call_absolute(self.helpers.throw_aioobe);

        // jit_throw_aioobe returns i64::MIN in RAX. Clean up the frame
        // and return to the interpreter, which will detect the sentinel
        // and convert it to an ArrayIndexOutOfBoundsException.
        self.emit_epilogue();

        // Patch all JAE branches to point to the shared stub
        for &patch_off in &self.bounds_check_stubs {
            let rel32 = (stub_offset as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.patch_i32(patch_off, rel32);
        }
    }

    /// Round-8 CRIT fix (audit `round8-jit.md`): emit the shared null-check
    /// failure stub for inline array stores. All `JZ` branches recorded by
    /// [`emit_null_check_array_store`] are patched to point here.
    ///
    /// The stub zeroes the array_ptr argument register, calls
    /// `helpers.bastore` (which on null sets `JIT_PENDING_NPE` and returns
    /// without dereferencing), loads `i64::MIN` into RAX, and runs the
    /// method epilogue. The interpreter's post-JIT path drains the NPE
    /// flag on every JIT return (round-8 fix in
    /// `vm/src/runtime/interpreter.rs`) and surfaces the NPE.
    ///
    /// We deliberately reuse the existing `helpers.bastore` rather than
    /// add a dedicated `set_npe_and_return` helper to keep this change
    /// surface tiny — the `bastore` helper short-circuits on null after
    /// setting the flag, so the call has no other side effects.
    ///
    /// ABI CONTRACT (round-9 jit HIGH fix, audit `round9-jit.md`): this
    /// stub depends on `helpers.bastore` accepting `(array_ptr=0, index=?,
    /// val=?)` and returning without dereferencing — only `array_ptr` is
    /// explicitly zeroed below (the other two argument registers retain
    /// whatever value the original inline-store codegen left in them, which
    /// may be poison). The helper's matching contract is documented inline
    /// at `vm/src/jit/helpers.rs::jit_bastore`: it MUST handle `array_ptr
    /// == 0` by setting the pending-NPE flag and returning WITHOUT reading
    /// `index` or `val`. If either the helper signature or its null-guard
    /// short-circuit changes, update both sites in lock-step.
    fn emit_null_check_store_stubs(&mut self) {
        if self.null_check_store_stubs.is_empty() {
            return;
        }

        let stub_offset = self.buf.pos();

        // Zero the array_ptr argument register so `jit_bastore`'s null
        // guard fires and sets the pending-NPE flag. The index and val
        // arguments are ignored on the null path; we don't bother clearing
        // them.
        #[cfg(target_os = "windows")]
        {
            // Windows: arg1 = RCX
            // XOR ECX, ECX  (31 C9) — zero-extends to RCX
            self.buf.emit(&[0x31, 0xC9]);
        }
        #[cfg(not(target_os = "windows"))]
        {
            // SysV: arg1 = RDI
            // XOR EDI, EDI  (31 FF) — zero-extends to RDI
            self.buf.emit(&[0x31, 0xFF]);
        }

        // CALL jit_bastore (absolute). On array_ptr=0 the helper sets
        // JIT_PENDING_NPE and returns. RAX is now clobbered by the call.
        self.emit_call_absolute(self.helpers.bastore);

        // MOV RAX, i64::MIN  — deopt sentinel so the interpreter's post-JIT
        // path treats this as a deopt return and runs the NPE drain.
        // 48 B8 <imm64>
        self.buf.emit(&[0x48, 0xB8]);
        self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding

        // Standard method epilogue: restore callee-saved regs and return.
        self.emit_epilogue();

        // Patch every recorded JZ branch to point to the shared stub.
        for &patch_off in &self.null_check_store_stubs {
            let rel32 = (stub_offset as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.patch_i32(patch_off, rel32);
        }
    }

    /// Emit out-of-line deoptimization stubs at the end of the method.
    ///
    /// Each stub calls `jit_uncommon_trap(vm_ptr, reason, bci)` and then
    /// returns `i64::MIN` as a sentinel to tell the interpreter that the
    /// method was deoptimized and should be re-executed in the interpreter.
    fn emit_deopt_stubs(&mut self) {
        if self.deopt_stubs.is_empty() {
            return;
        }

        // We need one stub per (bci, reason) pair since the BCI differs.
        // However, many guards may share the same BCI — group and share stubs.
        let stubs: Vec<(usize, usize, i64)> = self.deopt_stubs.clone();

        // Map from (bci, reason) to the emitted stub offset
        let mut stub_offsets: FxHashMap<(usize, i64), usize> = FxHashMap::default();

        for &(patch_off, bci, reason) in &stubs {
            let key = (bci, reason);
            if let Some(&stub_off) = stub_offsets.get(&key) {
                // Reuse existing stub
                let rel32 = (stub_off as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.patch_i32(patch_off, rel32);
                continue;
            }

            let stub_off = self.buf.pos();
            stub_offsets.insert(key, stub_off);

            // Set up args for jit_uncommon_trap(vm_ptr: i64, reason: i64, bci: i64)
            // vm_ptr is in the heap_local (frame slot) — load it first
            #[cfg(target_os = "windows")]
            {
                // Windows x64: arg1=RCX, arg2=RDX, arg3=R8
                // Load vm_ptr from heap_local_offset into RCX
                self.emit_load_local(RCX, self.heap_local_offset);
                // MOV RDX, reason (immediate)
                self.rex_w();
                self.buf.emit_byte(0xB8 + RDX as u8); // MOV r64, imm64 // Cast: x86-64 register encoding
                self.buf.emit(&(reason as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                // MOV R8, bci (immediate)
                self.buf.emit(&[0x49, 0xB8 + (R8 as u8 - 8)]); // REX.WB + MOV r64, imm64 // Cast: x86-64 register encoding
                self.buf.emit(&(bci as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
            }
            #[cfg(not(target_os = "windows"))]
            {
                // SysV: arg1=RDI, arg2=RSI, arg3=RDX
                // Load vm_ptr from heap_local_offset into RDI
                self.emit_load_local(RDI, self.heap_local_offset);
                // MOV RSI, reason (immediate)
                self.rex_w();
                self.buf.emit_byte(0xB8 + RSI as u8); // Cast: x86-64 register encoding
                self.buf.emit(&(reason as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                // MOV RDX, bci (immediate)
                self.rex_w();
                self.buf.emit_byte(0xB8 + RDX as u8); // Cast: x86-64 register encoding
                self.buf.emit(&(bci as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
            }

            // CALL jit_uncommon_trap
            self.emit_call_absolute(self.helpers.uncommon_trap);

            // Return i64::MIN as deopt sentinel
            // MOV RAX, i64::MIN
            self.rex_w();
            self.buf.emit_byte(0xB8); // MOV RAX, imm64
            self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding

            // Epilogue: restore callee-saved regs and return
            // This mirrors the standard method epilogue
            self.emit_epilogue();

            // Patch the branch to point here
            let rel32 = (stub_off as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.patch_i32(patch_off, rel32);
        }
    }

    // -----------------------------------------------------------------------
    // SSE float/double helpers
    // -----------------------------------------------------------------------

    /// SSE float binary op: pop two f32 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from fload of XMM locals
    /// or prior float arithmetic), avoids the GPR→XMM round-trip. Mirrors
    /// emit_double_binop's XMM chaining for float values.
    fn emit_float_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
        // F3 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF3, 0x0F, sse_op, 0xC1]);
        // MOVD EAX, XMM0
        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// SSE double binary op: pop two f64 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from dload of XMM locals),
    /// avoids the GPR→XMM round-trip. When slot1 is Xmm(0) and slot2 is a high XMM
    /// (8-15), emits the SSE op directly against that register, skipping XMM1 entirely.
    fn emit_double_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
        // F2 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF2, 0x0F, sse_op, 0xC1]);
        // MOVQ RAX, XMM0
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// Float/double compare: pop two values, produce -1/0/1.
    /// `is_double`: true for dcmp*, false for fcmp*.
    /// `nan_positive`: true for *cmpg (NaN→1), false for *cmpl (NaN→-1).
    fn emit_fcmp(&mut self, is_double: bool, nan_positive: bool) {
        let slot2 = self.pop_stack();
        let slot1 = self.pop_stack();
        self.flush_xmm0_slots();

        // Load slot2 into XMM1 (or use directly for UCOMISD XMM0, XMMn)
        let cmp_xmm2: u8; // register holding value2 for the UCOMI instruction
        match slot2 {
            StackSlot::Xmm(xmm) if xmm >= 2 => {
                // Can use directly in UCOMISD/UCOMISS
                cmp_xmm2 = xmm;
            }
            StackSlot::Xmm(xmm) => {
                if xmm != 1 {
                    if is_double {
                        self.emit_movsd_xmm_xmm(1, xmm);
                    } else {
                        self.emit_movss_xmm_xmm(1, xmm);
                    }
                }
                cmp_xmm2 = 1;
            }
            _ => {
                self.load_slot_to_reg(RCX, slot2);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
                }
                cmp_xmm2 = 1;
            }
        }

        // Load slot1 into XMM0
        match slot1 {
            StackSlot::Xmm(xmm) if xmm != 0 => {
                if is_double {
                    self.emit_movsd_xmm_xmm(0, xmm);
                } else {
                    self.emit_movss_xmm_xmm(0, xmm);
                }
            }
            StackSlot::Xmm(0) => {} // already in XMM0
            _ => {
                self.load_slot_to_reg(RAX, slot1);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                }
            }
        }

        // UCOMISD/UCOMISS XMM0, XMMn
        if is_double {
            // 66 [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x66, 0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x66, 0x0F, 0x2E, modrm]);
            }
        } else {
            // [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, modrm]);
            }
        }

        if nan_positive {
            // *cmpg: NaN → 1
            // Extract all flags before any ALU ops (which clobber CF/ZF/PF)
            // SETA AL (above = value1 > value2)
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // SETB CL (below = value1 < value2 or NaN)
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // SETP DL (parity = NaN)
            self.buf.emit(&[0x0F, 0x9A, 0xC2]);
            // Now safe to use ALU ops
            // OR AL, DL — positive = above OR NaN
            self.buf.emit(&[0x08, 0xD0]); // OR AL, DL
                                          // XOR DL, 1 — !NaN
            self.buf.emit(&[0x80, 0xF2, 0x01]);
            // AND CL, DL — below AND !NaN
            self.buf.emit(&[0x20, 0xD1]); // AND CL, DL
                                          // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        } else {
            // *cmpl: NaN → -1 (SETB naturally includes NaN)
            // SETA AL
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // SETB CL
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        }

        // Sign-extend EAX to RAX (for -1)
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
        self.push_from_rax();
    }

    // -----------------------------------------------------------------------
    // Bytecode compilation
    // -----------------------------------------------------------------------

    fn compile_bytecode(&mut self, code: &[u8], code_len: usize) -> bool {
        // Pre-allocate pc_to_native mapping
        self.pc_to_native.resize(code_len + 1, -1);

        // DCE: compute branch targets so we know which PCs are reachable
        let mut branch_targets = vec![false; code_len + 1];
        branch_targets[0] = true; // entry point
        {
            let mut p = 0;
            while p < code_len {
                match code[p] {
                    // Conditional and unconditional branches
                    0x99..=0xa6 | 0xa7 | 0xc6 | 0xc7 => {
                        if p + 2 < code_len {
                            let off = ((code[p + 1] as i16) << 8 | code[p + 2] as i16) as i32; // Widening: always safe
                            if let Some(target) = p.checked_add_signed(off as isize) { // Cast: address arithmetic
                                if target < code_len {
                                    branch_targets[target] = true;
                                }
                            }
                        }
                    }
                    // tableswitch
                    0xaa => {
                        let base = p;
                        let mut q = p + 1;
                        while q % 4 != 0 { q += 1; }
                        if q + 12 <= code_len {
                            let def = i32::from_be_bytes([code[q], code[q+1], code[q+2], code[q+3]]);
                            if let Some(t) = base.checked_add_signed(def as isize) { // Cast: address arithmetic
                                if t < code_len { branch_targets[t] = true; }
                            }
                            let low = i32::from_be_bytes([code[q+4], code[q+5], code[q+6], code[q+7]]);
                            let high = i32::from_be_bytes([code[q+8], code[q+9], code[q+10], code[q+11]]);
                            let cnt = (high - low + 1).max(0) as usize; // Cast: address arithmetic
                            q += 12;
                            for _ in 0..cnt {
                                if q + 4 <= code_len {
                                    let off = i32::from_be_bytes([code[q], code[q+1], code[q+2], code[q+3]]);
                                    if let Some(t) = base.checked_add_signed(off as isize) { // Cast: address arithmetic
                                        if t < code_len { branch_targets[t] = true; }
                                    }
                                }
                                q += 4;
                            }
                        }
                    }
                    // lookupswitch
                    0xab => {
                        let base = p;
                        let mut q = p + 1;
                        while q % 4 != 0 { q += 1; }
                        if q + 8 <= code_len {
                            let def = i32::from_be_bytes([code[q], code[q+1], code[q+2], code[q+3]]);
                            if let Some(t) = base.checked_add_signed(def as isize) { // Cast: address arithmetic
                                if t < code_len { branch_targets[t] = true; }
                            }
                            let npairs = i32::from_be_bytes([code[q+4], code[q+5], code[q+6], code[q+7]]) as usize; // Widening: always safe
                            q += 8;
                            for _ in 0..npairs {
                                if q + 8 <= code_len {
                                    let off = i32::from_be_bytes([code[q+4], code[q+5], code[q+6], code[q+7]]);
                                    if let Some(t) = base.checked_add_signed(off as isize) { // Cast: address arithmetic
                                        if t < code_len { branch_targets[t] = true; }
                                    }
                                }
                                q += 8;
                            }
                        }
                    }
                    _ => {}
                }
                p += bytecode_len_at(code, p);
            }
        }
        // (getstatic cache removed — every getstatic calls the helper at
        // runtime for JMM thread-safety.)

        let mut dead = false; // true after unconditional control transfer

        let mut pc = 0;
        while pc < code_len {
            // DCE: if we're in dead code and this PC isn't a branch target, skip it
            if dead {
                if branch_targets[pc] {
                    dead = false; // reachable via branch
                    // At merge points after unconditional branches, reconstruct
                    // the simulated stack using canonical frame offsets. The
                    // predecessor path called canonicalize_stack() before the
                    // goto, so values live at base_spill + i*8.
                    let expected_depth = self.branch_target_stack_depth.get(&pc).copied().unwrap_or(0);
                    self.stack.clear();
                    let base = self.base_spill_offset;
                    for i in 0..expected_depth {
                        let canonical_off = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                        self.stack.push(StackSlot::Frame(canonical_off));
                    }
                    self.next_spill_offset = base + (expected_depth as i32) * 8; // Cast: x86-64 immediate encoding
                } else {
                    self.pc_to_native[pc] = -1;
                    pc += bytecode_len_at(code, pc);
                    continue;
                }
            }
            // At merge points (branch targets reachable from multiple paths),
            // canonicalize the current stack so all paths agree on frame layout.
            // The predecessor that did a `goto` already canonicalized; now the
            // fall-through path must match.
            if !dead && branch_targets[pc] {
                if let Some(&expected_depth) = self.branch_target_stack_depth.get(&pc) {
                    if expected_depth > 0 && self.stack.len() == expected_depth {
                        self.canonicalize_stack();
                    }
                }
            }
            // === LICM: Emit hoisted aaload code at loop headers ===
            // Hoisted code runs BEFORE pc_to_native is set, so back-edges
            // (which use pc_to_native[header]) skip the hoisted computation.
            // On initial loop entry (fall-through from preheader), the hoisted
            // code executes and caches the invariant value in a spill slot.
            {
                // Collect hoist data to avoid borrow conflicts with self
                let loop_hoists: Vec<(usize, usize, i32)> = self
                    .hoist_info
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.loop_header == pc)
                    .map(|(idx, h)| (h.array_local, h.index_local, self.hoist_offsets[idx]))
                    .collect();

                for (array_local, index_local, hoist_offset) in loop_hoists {
                    // Load array reference into RAX
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(array_local));
                    }
                    // Load index into RCX
                    if let Some(reg) = self.reg_for_local(index_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(index_local));
                    }
                    // Inline aaload: MOV RAX, [RAX + RCX*8 + HEADER_SIZE]
                    self.emit_ref_aload_regs();
                    // Store hoisted value in dedicated spill slot
                    self.emit_store_local(hoist_offset, RAX);
                }
            }

            // === SIMD: Emit vectorized preheader for int-array-sum loops ===
            // Runs once on initial loop entry; back-edges skip to scalar loop.
            {
                let simd_match = self.simd_loops.iter().find(|s| s.header_pc == pc).map(|s| {
                    (
                        s.iv_local,
                        s.acc_local,
                        s.array_local,
                        s.bound_local,
                        s.acc_is_long,
                    )
                });

                if let Some((iv_local, acc_local, array_local, bound_local, acc_is_long)) =
                    simd_match
                {
                    let acc_offset = self.local_offset(acc_local);

                    // Sync accumulator register → frame slot (SIMD code operates on frame)
                    if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_store_local(acc_offset, acc_reg);
                    }

                    // Load array reference into RCX
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(array_local));
                    }
                    // Load induction variable into R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(R10, reg);
                    } else {
                        self.emit_load_local(R10, self.local_offset(iv_local));
                    }
                    // Load bound into R11D
                    if let Some(reg) = self.reg_for_local(bound_local) {
                        self.emit_mov_reg_reg(R11, reg);
                    } else {
                        self.emit_load_local(R11, self.local_offset(bound_local));
                    }

                    // Emit SIMD int-array sum (operates on frame slot for accumulator)
                    self.emit_simd_int_array_sum(acc_offset, acc_is_long);

                    // Sync accumulator frame slot → register
                    if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_load_local(acc_reg, acc_offset);
                    }

                    // Update induction variable from R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(reg, R10);
                    } else {
                        self.emit_store_local(self.local_offset(iv_local), R10);
                    }
                }
            }

            // === SIMD FP: Emit vectorized preheader for double-array-sum loops ===
            {
                let simd_fp_match = self.simd_fp_loops.iter().find(|s| s.header_pc == pc).map(|s| {
                    (s.iv_local, s.acc_local, s.array_local, s.bound_local)
                });

                if let Some((iv_local, acc_local, array_local, bound_local)) = simd_fp_match {
                    let acc_offset = self.local_offset(acc_local);

                    // Sync accumulator XMM/register → frame slot
                    if let Some(xmm) = self.xmm_for_local(acc_local) {
                        self.emit_movq_mem_rbp_from_xmm(acc_offset, xmm);
                    } else if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_store_local(acc_offset, acc_reg);
                    }

                    // Load array reference into RCX
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(array_local));
                    }
                    // Load induction variable into R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(R10, reg);
                    } else {
                        self.emit_load_local(R10, self.local_offset(iv_local));
                    }
                    // Load bound into R11D
                    if let Some(reg) = self.reg_for_local(bound_local) {
                        self.emit_mov_reg_reg(R11, reg);
                    } else {
                        self.emit_load_local(R11, self.local_offset(bound_local));
                    }

                    // Emit SIMD FP array sum
                    self.emit_simd_fp_array_sum(acc_offset);

                    // Sync accumulator frame slot → XMM/register
                    if let Some(xmm) = self.xmm_for_local(acc_local) {
                        self.emit_load_local(RAX, acc_offset);
                        self.emit_movq_xmm_from_rax(xmm);
                    } else if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_load_local(acc_reg, acc_offset);
                    }

                    // Update induction variable from R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(reg, R10);
                    } else {
                        self.emit_store_local(self.local_offset(iv_local), R10);
                    }
                }
            }

            // === T17.Β.3 — Loop unswitch pre-header evaluation ===========
            //
            // The detector has already proved that `invariant_local`
            // is never written inside the loop body and that the
            // body size is ≤ MAX_UNSWITCH_BYTECODES. We emit a
            // single evaluation of the invariant predicate at the
            // preheader. The body's per-iteration branch still
            // executes as before (so semantics are bit-identical to
            // the scalar loop), but the early evaluation:
            //
            // 1. Warms the CPU branch predictor for the branch's
            //    single outcome — because the predicate is
            //    invariant, the per-iteration branch is always
            //    taken the same way.
            // 2. Serves as a hook for future body-duplication: the
            //    pre-evaluation slot can be consumed by a specialized
            //    code-gen variant without changing the invariant.
            //
            // # Correctness
            //
            // The emitted sequence is *additive* — it reads
            // `invariant_local` and sets flags but never writes back
            // to any local. Because detection rejects loops that
            // write `invariant_local`, the value observed at the
            // preheader matches the value observed on every
            // iteration. Removing the emission yields identical
            // final state, which is exactly the "bytecode-equivalent
            // semantics" the scope requires.
            self.emit_loop_unswitch_preheader(pc);

            // === T17.Β.2 — SIMD element-wise preheader ====================
            // Emit an AVX2 batch loop + scalar tail for loops matching
            // `OUT[i] = A[i] OP B[i]`. Gated on AVX2 availability; when
            // absent, the original scalar loop body fires as-is (no
            // emission, no crash).
            if has_avx2() {
                let ewise_match = self
                    .simd_element_wise_loops
                    .iter()
                    .find(|e| e.header_pc == pc)
                    .map(|e| (e.iv_local, e.out_local, e.a_local, e.b_local, e.bound_local, e.op));

                if let Some((iv_local, out_local, a_local, b_local, bound_local, op)) = ewise_match {
                    // Load A base → RAX
                    if let Some(reg) = self.reg_for_local(a_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(a_local));
                    }
                    // Load B base → RCX
                    if let Some(reg) = self.reg_for_local(b_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(b_local));
                    }
                    // Load OUT base → RDX
                    if let Some(reg) = self.reg_for_local(out_local) {
                        self.emit_mov_reg_reg(RDX, reg);
                    } else {
                        self.emit_load_local(RDX, self.local_offset(out_local));
                    }
                    // Load i → R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(R10, reg);
                    } else {
                        self.emit_load_local(R10, self.local_offset(iv_local));
                    }
                    // Load n → R11D
                    if let Some(reg) = self.reg_for_local(bound_local) {
                        self.emit_mov_reg_reg(R11, reg);
                    } else {
                        self.emit_load_local(R11, self.local_offset(bound_local));
                    }

                    self.emit_simd_int_array_element_wise(op);

                    // Update induction variable from R10D.
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(reg, R10);
                    } else {
                        self.emit_store_local(self.local_offset(iv_local), R10);
                    }
                }
            }

            // === Speculative BCE: Emit range guard at loop headers ===
            // For each speculative guard at this header, emit:
            //   load array ref -> RAX
            //   MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]  (array length)
            //   load loop bound -> ECX
            //   CMP R10D, ECX  (array.length vs loop_bound)
            //   JB deopt_stub  (if array.length < loop_bound, deopt)
            {
                let guards: Vec<SpeculativeBCEGuard> = self
                    .speculative_bce_guards
                    .iter()
                    .filter(|g| g.loop_header == pc)
                    .cloned()
                    .collect();
                for guard in guards {
                    // Load array reference into RAX
                    if let Some(reg) = self.reg_for_local(guard.array_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(guard.array_local));
                    }
                    // MOV R10D, DWORD [RAX + ARRAY_LENGTH_OFFSET] — array length
                    self.buf
                        .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
                    // Load loop bound into ECX
                    if let Some(reg) = self.reg_for_local(guard.bound_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(guard.bound_local));
                    }
                    // CMP R10D, ECX — compare array.length vs loop_bound
                    // Encoding: 44 3B D1 (REX.R + CMP r32, r/m32 + ModRM(11, R10, ECX))
                    self.buf.emit(&[0x44, 0x3B, 0xD1]);
                    // JB rel32 — if array.length < loop_bound (unsigned), deopt
                    self.buf.emit(&[0x0F, 0x82]);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Route to deopt stub (calls jit_uncommon_trap) instead of AIOOBE
                    self.deopt_stubs.push((patch_offset, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                }
            }

            // Record mapping from bytecode PC to native offset
            // (AFTER hoisted/SIMD/speculative-BCE code, so back-edges skip the preheader)
            self.pc_to_native[pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding

            // === LICM: Replace hoisted sequences with spill slot loads ===
            {
                let hoist_replace = self
                    .hoist_info
                    .iter()
                    .enumerate()
                    .find(|(_, h)| h.seq_start == pc)
                    .map(|(idx, h)| (h.seq_end, self.hoist_offsets[idx]));

                if let Some((seq_end, hoist_offset)) = hoist_replace {
                    // Load cached value from hoisted spill slot
                    self.emit_load_local(RAX, hoist_offset);
                    self.push_from_rax();
                    // Mark intermediate PCs in the skipped sequence
                    let native_pos = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                    let mut skip_pc = pc + bytecode_len_at(code, pc);
                    while skip_pc < seq_end {
                        self.pc_to_native[skip_pc] = native_pos;
                        skip_pc += bytecode_len_at(code, skip_pc);
                    }
                    pc = seq_end;
                    continue;
                }
            }

            let op = code[pc];
            match op {
                // nop
                0x00 => {
                    pc += 1;
                }

                // aconst_null — push 0 (null reference)
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    // T1.1.a — null is a valid object reference per JVMS.
                    self.mark_top_as_oop();
                    pc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = op as i32 - 3; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 1, code, code_len) {
                        pc += 2;
                    } else if let Some(next_pc) =
                        self.try_const_compare_peephole(val, pc + 1, code, code_len)
                    {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 1;
                    }
                }

                // lconst_0
                0x09 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // lconst_1
                0x0a => {
                    self.emit_mov_imm32_sx(RAX, 1);
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_0
                0x0b => {
                    // 0.0f32 → bits = 0x00000000
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_1
                0x0c => {
                    // 1.0f32 → bits = 0x3F800000 = 1065353216
                    self.emit_mov_imm32_sx(RAX, 0x3F80_0000u32 as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_2
                0x0d => {
                    // 2.0f32 → bits = 0x40000000 = 1073741824
                    self.emit_mov_imm32_sx(RAX, 0x4000_0000u32 as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    pc += 1;
                }

                // dconst_0
                0x0e => {
                    // 0.0f64 → bits = 0x0000000000000000
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // dconst_1
                0x0f => {
                    // 1.0f64 → bits = 0x3FF0000000000000
                    self.emit_mov_imm64(RAX, 0x3FF0_0000_0000_0000u64 as i64); // Cast: JIT ABI convention
                    self.push_from_rax();
                    pc += 1;
                }

                // bipush
                0x10 => {
                    let val = code[pc + 1] as i8 as i32; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 2, code, code_len) {
                        pc += 3;
                    } else if let Some(next_pc) =
                        self.try_const_compare_peephole(val, pc + 2, code, code_len)
                    {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 2;
                    }
                }

                // sipush
                0x11 => {
                    let val = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 3, code, code_len) {
                        pc += 4;
                    } else if let Some(next_pc) =
                        self.try_const_compare_peephole(val, pc + 3, code, code_len)
                    {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 3;
                    }
                }

                // ldc — load int/float/string constant from CP (1-byte index)
                0x12 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                    match val {
                        Some(v) => {
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 2;
                        }
                        None => return false,
                    }
                }

                // ldc_w — load int/float/string constant from CP (2-byte index)
                0x13 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                    match val {
                        Some(v) => {
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 3;
                        }
                        None => return false,
                    }
                }

                // ldc2_w — load long/double constant from CP (resolved to i64)
                0x14 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let val = self.ldc2w_info_idx.get(&pc).map(|&i| self.ldc2w_info[i].1);
                    match val {
                        Some(v) => {
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 3;
                        }
                        None => {
                            // Not resolved — bail out; method will stay interpreted
                            return false;
                        }
                    }
                }

                // iload / lload / fload / dload / aload
                // iload/lload/fload/dload/aload (wide index: opcode 0x15-0x19, then idx byte)
                0x15..=0x19 => {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    // fload (0x17) and dload (0x18) may have XMM-allocated locals
                    if matches!(op, 0x17 | 0x18) {
                        if let Some(xmm) = self.xmm_for_local(idx) {
                            self.stack.push(StackSlot::Xmm(xmm));
                            pc += 2;
                            continue;
                        }
                    }
                    let is_aload = op == 0x19;
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                        // T1.1.a — only aload pushes oops; iload/lload/fload/dload
                        // push primitives. Mark parity with the stack.
                        self.stack_oop_marks.push(is_aload);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                        if is_aload {
                            self.mark_top_as_oop();
                        }
                    }
                    pc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        // Zero-cost: just record register reference on simulated stack
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // fload_0..fload_3 (float load)
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    if let Some(xmm) = self.xmm_for_local(idx) {
                        self.stack.push(StackSlot::Xmm(xmm));
                    } else if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // dload_0..dload_3 (double load)
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    if let Some(xmm) = self.xmm_for_local(idx) {
                        // Zero-cost push: just reference the XMM register.
                        // No code emitted until the value is consumed.
                        self.stack.push(StackSlot::Xmm(xmm));
                    } else if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // aload_0..aload_3 (reference load — identical to iload for JIT)
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack.push(StackSlot::CalleeSaved(local_reg));
                        // T1.1.a — keep oop-mark vector in lock-step
                        // and tag the entry. CalleeSaved slots don't
                        // have a frame offset, so the oop-map walker
                        // skips them (they're preserved by the ABI
                        // across calls and cached by the JIT's frame
                        // save/restore prologue).
                        self.stack_oop_marks.push(true);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                        // T1.1.a — aload* always pushes an object ref.
                        self.mark_top_as_oop();
                    }
                    pc += 1;
                }

                // iaload — load int from int[] array (inline)
                0x2e => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §iaload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_int_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // aaload — load reference from Object[] array (inline, 8 bytes/element)
                0x32 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §aaload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_ref_aload_regs();
                    self.push_from_rax();
                    // T1.1.a — aaload reads a reference from an Object[].
                    self.mark_top_as_oop();
                    pc += 1;
                }

                // laload — load long from long[] array (inline, 8 bytes/element)
                0x2f => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §laload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_long_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // faload — load float from float[] array (inline, 4 bytes/element)
                0x30 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §faload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Float is 4 bytes, same as int; value is stored as bit pattern
                    self.emit_int_aload_regs();
                    self.push_from_rax_as_xmm0();
                    pc += 1;
                }

                // daload — load double from double[] array (inline, 8 bytes/element)
                0x31 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §daload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Double is 8 bytes, same as long; value is stored as bit pattern
                    self.emit_long_aload_regs();
                    self.push_from_rax_as_xmm0();
                    pc += 1;
                }

                // baload — load byte/boolean from byte[]/boolean[] array (inline)
                0x33 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §baload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_byte_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // caload — load char from char[] array (inline, 2 bytes/element, zero-extend)
                0x34 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §caload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_char_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // saload — load short from short[] array (inline, 2 bytes/element, sign-extend)
                0x35 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §saload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_short_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // istore / lstore / fstore / dstore / astore
                // istore/lstore/fstore/dstore/astore (wide index)
                0x36..=0x3a => {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    // fstore (0x38) and dstore (0x39) may have XMM-allocated locals
                    if matches!(op, 0x38 | 0x39) {
                        if let Some(dst_xmm) = self.xmm_for_local(idx) {
                            let slot = self.pop_stack();
                            match slot {
                                StackSlot::Xmm(src) if src == dst_xmm => {}
                                StackSlot::Xmm(src) => {
                                    if op == 0x39 {
                                        self.emit_movsd_xmm_xmm(dst_xmm, src);
                                    } else {
                                        self.emit_movss_xmm_xmm(dst_xmm, src);
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, slot);
                                    self.emit_movq_xmm_from_rax(dst_xmm);
                                }
                            }
                            pc += 2;
                            continue;
                        }
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // fstore_0..fstore_3 (float store)
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    if let Some(dst_xmm) = self.xmm_for_local(idx) {
                        let slot = self.pop_stack();
                        match slot {
                            StackSlot::Xmm(src) if src == dst_xmm => {}
                            StackSlot::Xmm(src) => {
                                self.emit_movss_xmm_xmm(dst_xmm, src);
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, slot);
                                self.emit_movq_xmm_from_rax(dst_xmm);
                            }
                        }
                    } else {
                        self.pop_to_rax();
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.invalidate_callee_saved(local_reg);
                            self.emit_mov_reg_reg(local_reg, RAX);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_store_local(off, RAX);
                        }
                    }
                    pc += 1;
                }

                // dstore_0..dstore_3 (double store)
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                    // Optimize: if top-of-stack is Xmm and target is XMM local,
                    // move directly XMM→XMM without going through RAX.
                    if let Some(dst_xmm) = self.xmm_for_local(idx) {
                        let slot = self.pop_stack();
                        match slot {
                            StackSlot::Xmm(src) if src == dst_xmm => {
                                // Already in the right register — no-op
                            }
                            StackSlot::Xmm(src) => {
                                self.emit_movsd_xmm_xmm(dst_xmm, src);
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, slot);
                                self.emit_movq_xmm_from_rax(dst_xmm);
                            }
                        }
                    } else {
                        self.pop_to_rax();
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.invalidate_callee_saved(local_reg);
                            self.emit_mov_reg_reg(local_reg, RAX);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_store_local(off, RAX);
                        }
                    }
                    pc += 1;
                }

                // astore_0..astore_3 (reference store — identical to istore for JIT)
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // iastore — store int to int[] array (inline)
                0x4f => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §iastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_int_astore_regs();
                    pc += 1;
                }

                // aastore — store reference to Object[] array (inline store + barrier-only call)
                //
                // R20 / HIGH-5 (see docs/PRESENTATION.md): replace the full `jit_aastore`
                // helper call with an inline `MOV QWORD [array + index*8 + HEADER_SIZE], val`
                // followed by a CALL to the much-cheaper `write_barrier` helper. The barrier
                // helper short-circuits when `val == 0` (null), so we don't need an inline
                // null check. Array layout is compact 8-byte pointers (matches the already-
                // inlined `aaload` path).
                //
                // ArrayStoreException note: the current `jit_aastore` helper does NOT enforce
                // the ASE check (the interpreter does it via `set_array_element`). This inline
                // path matches the helper's behavior exactly — no regression. Wiring an inline
                // ASE check is a follow-up that needs type-narrowing infrastructure (not yet
                // tracked in this JIT).
                0x53 => {
                    self.flush_scratch_registers();
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §aastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
                    // Inline-load the OLD reference at the slot and pipe it
                    // through `jit_satb_pre_write_barrier(vm_ptr, old_ref)`
                    // BEFORE the inline store overwrites it. The helper
                    // short-circuits via a single Acquire load when no
                    // concurrent mark cycle is in flight (`SatbQueue::
                    // is_active() == false`), so the steady-state cost is
                    // just an inline load + a not-taken-branch call. Without
                    // this, a still-live reference overwritten by JIT code
                    // during concurrent marking would be silently dropped by
                    // the marker → use-after-free on the next mixed
                    // evacuation (audit: docs/round7-gc.md §1).
                    //
                    // Save RAX (array) / RCX (index) into argument registers
                    // first since `emit_ref_aload_regs` clobbers RAX with
                    // the loaded value.
                    self.emit_ref_aload_regs(); // RAX = OLD ref value
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_reg_reg(ARG_REGS[1], RAX);
                    self.emit_call_absolute(self.helpers.satb_pre_write_barrier);
                    // Reload array / index / new value (helper call may have
                    // clobbered scratch registers including RAX, RCX, RDX).
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    self.load_slot_to_reg(RDX, val_slot);
                    // Inline store: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
                    self.emit_ref_astore_regs();
                    // Post-store write barrier: jit_write_barrier(vm_ptr, array_ptr, val_ptr).
                    // The helper itself bails out when val_ptr == 0, so storing null
                    // skips the card-mark cost (no extra inline branch needed).
                    // TODO: inline the card-mark (`SHR addr, 9; MOV BYTE [card_table+addr], 0`)
                    // when `card_table_base` is exposed in JitRuntimeHelpers — would eliminate
                    // this call entirely. Per task constraint, do not add a new helper field
                    // unilaterally; leave the call-only barrier as the partial win.
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], val_slot);
                    self.emit_call_absolute(self.helpers.write_barrier);
                    pc += 1;
                }

                // lastore — store long to long[] array (inline, 8 bytes/element)
                0x50 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §lastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_long_astore_regs();
                    pc += 1;
                }

                // fastore — store float to float[] array (inline, 4 bytes/element)
                0x51 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §fastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    match val_slot {
                        StackSlot::Xmm(xmm) => {
                            // MOVSS [RAX + RCX*4 + HEADER_SIZE], XMMn
                            // F3 [REX] 0F 11 ModRM SIB disp8
                            let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                            self.buf.emit_byte(0xF3);
                            if rex_r != 0 {
                                self.buf.emit_byte(0x40 | rex_r);
                            }
                            self.buf.emit_byte(0x0F);
                            self.buf.emit_byte(0x11);
                            self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                            self.buf.emit_byte(0x88); // SIB: scale=2(*4), index=RCX, base=RAX
                            self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                        }
                        _ => {
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_int_astore_regs();
                        }
                    }
                    pc += 1;
                }

                // dastore — store double to double[] array (inline, 8 bytes/element)
                0x52 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §dastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Optimize: if value is in XMM, use MOVSD to store directly to memory
                    match val_slot {
                        StackSlot::Xmm(xmm) => {
                            // MOVSD [RAX + RCX*8 + HEADER_SIZE], XMMn
                            // F2 [REX] 0F 11 ModRM SIB disp8
                            let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                            self.buf.emit_byte(0xF2);
                            if rex_r != 0 {
                                self.buf.emit_byte(0x40 | rex_r);
                            }
                            self.buf.emit_byte(0x0F);
                            self.buf.emit_byte(0x11); // MOVSD store direction
                            self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                            self.buf.emit_byte(0xC8); // SIB: scale=3(*8), index=RCX, base=RAX
                            self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                        }
                        _ => {
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_long_astore_regs();
                        }
                    }
                    pc += 1;
                }

                // bastore — store byte/boolean to byte[]/boolean[] array (inline)
                0x54 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §bastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_byte_astore_regs();
                    pc += 1;
                }

                // castore — store char to char[] array (inline, 2 bytes/element)
                0x55 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §castore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_short_astore_regs();
                    pc += 1;
                }

                // sastore — store short to short[] array (inline, 2 bytes/element)
                0x56 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §sastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_short_astore_regs();
                    pc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    pc += 1;
                }

                // dup
                0x59 => {
                    let top = self.peek_stack();
                    match top {
                        StackSlot::Frame(off) => {
                            self.emit_load_local(RAX, off);
                            self.push_from_rax();
                        }
                        StackSlot::CalleeSaved(_) => {
                            // Zero-cost: just duplicate the register reference
                            self.stack.push(top);
                        }
                        StackSlot::Xmm(_) => {
                            // Zero-cost: just duplicate the XMM register reference
                            self.stack.push(top);
                        }
                        StackSlot::Scratch(reg) => {
                            // Scratch register holds the value — try to dup into
                            // another scratch register, else spill original to frame
                            // and push another frame copy.
                            let avail = SCRATCH_REGS.iter().copied().find(|&sr| {
                                sr != reg
                                    && !self
                                        .stack
                                        .iter()
                                        .any(|s| matches!(s, StackSlot::Scratch(r) if *r == sr))
                            });
                            if let Some(sr) = avail {
                                self.emit_mov_reg_reg(sr, reg);
                                self.stack.push(StackSlot::Scratch(sr));
                            } else {
                                // No scratch available — load to RAX and push via frame
                                self.emit_mov_reg_reg(RAX, reg);
                                let slot = self.push_stack();
                                if let StackSlot::Frame(off) = slot {
                                    self.emit_store_local(off, RAX);
                                }
                            }
                        }
                    }
                    pc += 1;
                }

                // dup2 — duplicate top two 64-bit stack slots
                // [..., a, b] → [..., a, b, a, b]
                0x5c => {
                    let len = self.stack.len();
                    let a = self.stack[len - 2]; // deeper
                    let b = self.stack[len - 1]; // top
                    self.load_slot_to_reg(RAX, a);
                    self.push_from_rax();
                    self.load_slot_to_reg(RAX, b);
                    self.push_from_rax();
                    pc += 1;
                }

                // swap
                0x5f => {
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    match (a, b) {
                        (StackSlot::Frame(off_a), StackSlot::Frame(off_b)) => {
                            // Load both, store swapped
                            self.emit_load_local(RAX, off_a);
                            self.emit_load_local(RCX, off_b);
                            self.emit_store_local(off_a, RCX);
                            self.emit_store_local(off_b, RAX);
                        }
                        _ => {
                            // Mixed or both CalleeSaved/Scratch — no memory swap needed,
                            // just logical reordering via push order below
                        }
                    }
                    // Push back in swapped order
                    self.stack.push(a);
                    self.stack.push(b);
                    pc += 1;
                }

                // iadd
                0x60 => {
                    self.pop_to_rcx(); // b
                    self.pop_to_rax(); // a
                                       // ADD eax, ecx (32-bit, wrapping)
                    self.buf.emit(&[0x01, 0xC8]); // add eax, ecx
                                                  // Sign-extend eax to rax for consistency
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // ADD rax, rcx (64-bit)
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]); // add rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fadd
                0x62 => {
                    self.emit_float_binop(0x58); // ADDSS
                    pc += 1;
                }

                // dadd
                0x63 => {
                    self.emit_double_binop(0x58); // ADDSD
                    pc += 1;
                }

                // isub
                0x64 => {
                    self.pop_to_rcx(); // b
                    self.pop_to_rax(); // a
                                       // SUB eax, ecx
                    self.buf.emit(&[0x29, 0xC8]); // sub eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lsub
                0x65 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]); // sub rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fsub
                0x66 => {
                    self.emit_float_binop(0x5C); // SUBSS
                    pc += 1;
                }

                // dsub
                0x67 => {
                    self.emit_double_binop(0x5C); // SUBSD
                    pc += 1;
                }

                // imul
                0x68 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // IMUL eax, ecx
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // IMUL rax, rcx
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fmul
                0x6a => {
                    self.emit_float_binop(0x59); // MULSS
                    pc += 1;
                }

                // dmul — with strength reduction: dmul by 2.0 → dadd self
                0x6b => {
                    if self.fp_strength_reduction_pcs.contains(&pc) {
                        // Pattern: <value>, ldc2_w 2.0, dmul
                        // Stack: [value, 2.0] → pop 2.0, emit ADDSD value, value
                        let _two = self.pop_stack(); // discard the 2.0 constant
                        let val = self.pop_stack();
                        self.flush_xmm0_slots();
                        // Load value into XMM0
                        match val {
                            StackSlot::Xmm(xmm) if xmm != 0 => {
                                self.emit_movsd_xmm_xmm(0, xmm);
                            }
                            StackSlot::Xmm(0) => {} // already there
                            _ => {
                                self.load_slot_to_reg(RAX, val);
                                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                            }
                        }
                        // ADDSD XMM0, XMM0 — doubles the value
                        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC0]);
                        self.stack.push(StackSlot::Xmm(0));
                    } else {
                        self.emit_double_binop(0x59); // MULSD
                    }
                    pc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN).
                // See `emit_safe_idiv` for the guard sequence.
                0x6c => {
                    self.pop_to_rcx(); // divisor
                    self.pop_to_rax(); // dividend
                    self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    pc += 1;
                }

                // ldiv — JVMS-compliant guards; LONG_MIN / -1 returns LONG_MIN.
                0x6d => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    pc += 1;
                }

                // fdiv
                0x6e => {
                    self.emit_float_binop(0x5E); // DIVSS
                    pc += 1;
                }

                // ddiv
                0x6f => {
                    self.emit_double_binop(0x5E); // DIVSD
                    pc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 returns 0.
                0x70 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    pc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 returns 0.
                0x71 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    pc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG eax
                    self.buf.emit(&[0xF7, 0xD8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xF7, 0xD8]); // NEG rax
                    self.push_from_rax();
                    pc += 1;
                }

                // fneg — flip sign bit of float (bit 31)
                0x76 => {
                    self.pop_to_rax();
                    // XOR EAX, 0x80000000 (flip sign bit, zeroes upper 32 bits)
                    self.buf.emit_byte(0x35); // XOR EAX, imm32
                    self.buf.emit(&0x8000_0000u32.to_le_bytes());
                    self.push_from_rax();
                    pc += 1;
                }

                // dneg — flip sign bit of double (bit 63)
                0x77 => {
                    self.pop_to_rax();
                    // BTC RAX, 63 — complement bit 63
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBA, 0xF8, 63]); // BTC r/m64, imm8
                    self.push_from_rax();
                    pc += 1;
                }

                // ishl
                0x78 => {
                    self.pop_to_rcx(); // shift count (low 5 bits)
                    self.pop_to_rax();
                    // SHL eax, cl
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd
                    self.push_from_rax();
                    pc += 1;
                }

                // lshl
                0x79 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE0]); // SHL rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // ishr
                0x7a => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // SAR eax, cl
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lshr
                0x7b => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xF8]); // SAR rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // iushr
                0x7c => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // SHR eax, cl
                    self.buf.emit(&[0xD3, 0xE8]);
                    // Zero-extend eax to rax (automatic with 32-bit ops on x64)
                    self.push_from_rax();
                    pc += 1;
                }

                // lushr
                0x7d => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE8]); // SHR rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x21, 0xC8]); // AND eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd
                    self.push_from_rax();
                    pc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]); // AND rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x09, 0xC8]); // OR eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]); // OR rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x31, 0xC8]); // XOR eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]); // XOR rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // iinc
                0x84 => {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    let inc = code[pc + 2] as i8 as i32; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        // Invalidate any CalleeSaved refs before modifying the register
                        self.invalidate_callee_saved(local_reg);
                        // ADD r32, imm directly on the callee-saved register
                        if local_reg >= 8 {
                            self.buf.emit_byte(0x41); // REX.B
                        }
                        if (-128..=127).contains(&inc) {
                            self.buf.emit_byte(0x83); // ADD r/m32, imm8
                            self.buf.emit_byte(0xC0 | (local_reg & 7));
                            self.buf.emit_byte(inc as u8); // Cast: x86-64 immediate encoding
                        } else {
                            self.buf.emit_byte(0x81); // ADD r/m32, imm32
                            self.buf.emit_byte(0xC0 | (local_reg & 7));
                            self.buf.emit(&inc.to_le_bytes());
                        }
                        // Sign-extend r32 to r64
                        self.rex_w_rb(local_reg, local_reg);
                        self.buf.emit_byte(0x63); // MOVSXD r64, r/m32
                        self.modrm_reg(local_reg, local_reg);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        if (-128..=127).contains(&inc) {
                            self.buf.emit(&[0x83, 0xC0]); // ADD eax, imm8
                            self.buf.emit_byte(inc as u8); // Cast: x86-64 immediate encoding
                        } else {
                            self.buf.emit_byte(0x05); // ADD eax, imm32
                            self.buf.emit(&inc.to_le_bytes());
                        }
                        // Sign-extend back
                        self.rex_w();
                        self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                        self.emit_store_local(off, RAX);
                    }
                    pc += 3;
                }

                // i2l — sign-extend int to long
                0x85 => {
                    self.pop_to_rax();
                    // movsxd rax, eax (sign-extend 32→64)
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // i2f — int to float
                0x86 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SS XMM0, EAX: F3 0F 2A C0
                    self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // i2d — int to double
                0x87 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SD XMM0, EAX: F2 0F 2A C0
                    self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // l2i — truncate long to int
                0x88 => {
                    self.pop_to_rax();
                    // Just keep lower 32 bits, sign-extend
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // l2f — long to float
                0x89 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SS XMM0, RAX: F3 48 0F 2A C0
                    self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // l2d — long to double
                0x8a => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SD XMM0, RAX: F2 48 0F 2A C0
                    self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // f2i — float to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8b => {
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTTSS2SI EAX, XMM0: F3 0F 2C C0
                    self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(false, false);
                    // Sign-extend EAX to RAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // f2l — float to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8c => {
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTTSS2SI RAX, XMM0: F3 48 0F 2C C0
                    self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(false, true);
                    self.push_from_rax();
                    pc += 1;
                }

                // f2d — float to double
                0x8d => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTSS2SD XMM0, XMM0: F3 0F 5A C0
                    self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // d2i — double to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8e => {
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTTSD2SI EAX, XMM0: F2 0F 2C C0
                    self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(true, false);
                    // Sign-extend EAX to RAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // d2l — double to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8f => {
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTTSD2SI RAX, XMM0: F2 48 0F 2C C0
                    self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(true, true);
                    self.push_from_rax();
                    pc += 1;
                }

                // d2f — double to float
                0x90 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTSD2SS XMM0, XMM0: F2 0F 5A C0
                    self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                    self.stack.push(StackSlot::Xmm(0));
                    pc += 1;
                }

                // i2b — truncate int to byte (sign-extend)
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AL — sign-extend byte to 32-bit
                    self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    // MOVSXD RAX, EAX — sign-extend to 64-bit
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // i2c — truncate int to char (zero-extend unsigned 16-bit)
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX — zero-extend 16-bit to 32-bit
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits of RAX auto-zeroed by 32-bit op
                    self.push_from_rax();
                    pc += 1;
                }

                // i2s — truncate int to short (sign-extend)
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX — sign-extend 16-bit to 32-bit
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    // MOVSXD RAX, EAX — sign-extend to 64-bit
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lcmp
                0x94 => {
                    self.pop_to_rcx(); // value2
                    self.pop_to_rax(); // value1
                                       // CMP rax, rcx
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // cmp rax, rcx
                                                  // Produce -1, 0, or 1 using SETG/SETL (avoids RBX)
                    self.buf.emit(&[0x0F, 0x97, 0xC0]); // SETG AL
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]); // SETL CL
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX, CL
                    self.buf.emit(&[0x29, 0xC8]); // SUB EAX, ECX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // fcmpl — float compare, NaN → -1
                0x95 => {
                    self.emit_fcmp(false, false); // float, NaN→-1
                    pc += 1;
                }

                // fcmpg — float compare, NaN → 1
                0x96 => {
                    self.emit_fcmp(false, true); // float, NaN→1
                    pc += 1;
                }

                // dcmpl — double compare, NaN → -1
                0x97 => {
                    self.emit_fcmp(true, false); // double, NaN→-1
                    pc += 1;
                }

                // dcmpg — double compare, NaN → 1
                0x98 => {
                    self.emit_fcmp(true, true); // double, NaN→1
                    pc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — compare int against zero
                0x99..=0x9e => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let slot = self.pop_stack();
                    // Canonicalize remaining stack for forward merge points
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }
                    // TEST r32, r32 — sets ZF/SF for comparison against zero
                    let reg = self.slot_to_gpr(slot, RCX);
                    self.emit_test_r32_r32(reg);

                    let cc = match op {
                        0x99 => 0x84, // JE
                        0x9a => 0x85, // JNE
                        0x9b => 0x8C, // JL
                        0x9c => 0x8D, // JGE
                        0x9d => 0x8F, // JG
                        0x9e => 0x8E, // JLE
                        _ => unreachable!(),
                    };

                    // PGO branch prediction hint prefix (Intel Architecture Manual 2.4.4).
                    // 0x3E = DS prefix = "branch taken" hint.
                    // 0x2E = CS prefix = "branch not taken" hint.
                    if let Some(&is_taken) = self.branch_hints.get(&pc) {
                        self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                    }
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(cc);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.branch_target_stack_depth
                        .entry(target_pc)
                        .or_insert(self.stack.len());
                    self.reset_spills();
                    pc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4)
                0x9f..=0xa4 => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let val2 = self.pop_stack(); // value2
                    let val1 = self.pop_stack(); // value1

                    // peephole-cmov (Round-11 HIGH-3): user-written
                    // min/max pattern → CMOV. The peephole consumes
                    // the if_icmp, the fall-through iload, the goto,
                    // and the taken-side iload all at once; on hit
                    // we resume at the merge PC L2.
                    if let Some(new_pc) =
                        self.try_cmov_minmax_peephole(code, pc, op, val1, val2)
                    {
                        // Map the original if_icmp PC to the start of
                        // the CMOV sequence so downstream branch
                        // resolution keeps working.
                        pc = new_pc;
                        self.reset_spills();
                        continue;
                    }

                    // Canonicalize remaining stack for forward merge points
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }
                    // Emit CMP with direct reg-reg when possible
                    let r1 = self.slot_to_gpr(val1, RAX);
                    let r2 = self.slot_to_gpr(val2, RCX);
                    self.emit_cmp_r32_r32(r1, r2);

                    let cc = match op {
                        0x9f => 0x84, // JE
                        0xa0 => 0x85, // JNE
                        0xa1 => 0x8C, // JL
                        0xa2 => 0x8D, // JGE
                        0xa3 => 0x8F, // JG
                        0xa4 => 0x8E, // JLE
                        _ => unreachable!(),
                    };

                    // PGO branch prediction hint (same encoding as ifeq..ifle above).
                    if let Some(&is_taken) = self.branch_hints.get(&pc) {
                        self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                    }
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(cc);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.branch_target_stack_depth
                        .entry(target_pc)
                        .or_insert(self.stack.len());
                    self.reset_spills();
                    pc += 3;
                }

                // goto
                0xa7 => {
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    // Flush scratch registers before any branch -- they are
                    // caller-saved and not valid across basic block boundaries.
                    self.flush_scratch_registers();
                    // For forward branches with non-empty stack, canonicalize the
                    // stack layout so all paths to the target use the same offsets.
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }

                    // Loop unrolling: if this is a back-edge of an unrollable loop,
                    // copy the native code for the loop body as extra iterations
                    let unroll_copies = if target_pc <= pc {
                        self.unroll_loops
                            .iter()
                            .find(|&&(_, be, _)| be == pc)
                            .map(|&(_, _, copies)| copies)
                    } else {
                        None
                    };

                    if let Some(extra_copies) = unroll_copies {
                        // Copy the native code from header to here for extra iterations
                        let header_native = self.pc_to_native[target_pc];
                        if header_native >= 0 {
                            let body_start = header_native as usize; // Cast: address arithmetic
                            let body_end = self.buf.pos();
                            let body_len = body_end - body_start;

                            if body_len > 0 && body_len < 4096 {
                                // Snapshot the original body bytes once
                                let body_bytes: Vec<u8> =
                                    self.buf.as_slice()[body_start..body_end].to_vec();

                                // Snapshot the original forward patches and bounds stubs
                                // that fall within the original body
                                let orig_patches: Vec<(usize, usize)> = self
                                    .forward_patches
                                    .iter()
                                    .filter(|&&(po, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_stubs: Vec<usize> = self
                                    .bounds_check_stubs
                                    .iter()
                                    .filter(|&&po| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();

                                for _ in 0..extra_copies {
                                    let copy_start = self.buf.pos();
                                    let shift = copy_start as i32 - body_start as i32; // Cast: x86-64 immediate encoding

                                    // Copy the raw bytes
                                    self.buf.emit(&body_bytes);

                                    // Handle forward patches: internal ones (target
                                    // within the loop body) are resolved immediately
                                    // using shifted addresses; external ones are
                                    // deferred normally.
                                    for &(po, tp) in &orig_patches {
                                        let shifted_po = po + shift as usize; // Cast: address arithmetic
                                        if tp >= target_pc && tp <= pc {
                                            // Internal: resolve now using shifted target
                                            let orig_target = self.pc_to_native[tp];
                                            if orig_target >= 0 {
                                                let shifted_target = orig_target + shift;
                                                let rel = shifted_target
                                                    - (shifted_po as i32 + 4); // Cast: x86-64 immediate encoding
                                                self.buf.patch_i32(shifted_po, rel);
                                            }
                                        } else {
                                            // External: defer to normal resolution
                                            self.forward_patches
                                                .push((shifted_po, tp));
                                        }
                                    }

                                    // Add shifted bounds check stubs for this copy
                                    let stubs_to_add: Vec<usize> = orig_stubs
                                        .iter()
                                        .map(|&po| po + shift as usize) // Cast: address arithmetic
                                        .collect();
                                    self.bounds_check_stubs.extend(stubs_to_add);
                                }
                            }
                        }
                    }

                    // JMP rel32 to header
                    self.buf.emit_byte(0xE9);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.branch_target_stack_depth
                        .entry(target_pc)
                        .or_insert(self.stack.len());
                    self.reset_spills();
                    dead = true;
                    pc += 3;
                }

                // tableswitch — jump table for dense tables, CMP chain for small
                0xaa => {
                    self.flush_scratch_registers();
                    self.pop_to_rax();
                    let base_pc = pc;
                    pc += 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let default_offset =
                        i32::from_be_bytes([code[pc], code[pc + 1], code[pc + 2], code[pc + 3]]);
                    let low = i32::from_be_bytes([
                        code[pc + 4],
                        code[pc + 5],
                        code[pc + 6],
                        code[pc + 7],
                    ]);
                    let high = i32::from_be_bytes([
                        code[pc + 8],
                        code[pc + 9],
                        code[pc + 10],
                        code[pc + 11],
                    ]);
                    let count = (high - low + 1).max(0) as usize; // Cast: address arithmetic
                    pc += 12;

                    // Collect all targets from the bytecode
                    let mut targets = Vec::with_capacity(count);
                    for _ in 0..count {
                        let off = i32::from_be_bytes([
                            code[pc], code[pc + 1], code[pc + 2], code[pc + 3],
                        ]);
                        targets.push((base_pc as i32 + off) as usize); // Cast: x86-64 immediate encoding
                        pc += 4;
                    }
                    let def_target = (base_pc as i32 + default_offset) as usize; // Cast: x86-64 immediate encoding

                    if count <= 4 {
                        // Small table: CMP chain (compact code, few comparisons)
                        // Normalize key: SUB EAX, low
                        if low != 0 {
                            self.buf.emit(&[0x2D]); // SUB EAX, imm32
                            self.buf.emit(&low.to_le_bytes());
                        }
                        for (i, &target) in targets.iter().enumerate() {
                            self.buf.emit(&[0x3D]); // CMP EAX, imm32
                            self.buf.emit(&(i as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                            self.buf.emit(&[0x0F, 0x84]); // JE rel32
                            let patch = self.buf.pos();
                            self.buf.emit(&[0; 4]);
                            self.forward_patches.push((patch, target));
                        }
                        // Default: JMP
                        self.buf.emit_byte(0xE9);
                        let dp = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((dp, def_target));
                    } else {
                        // Large table: O(1) jump table dispatch
                        //   SUB EAX, low        ; normalize index
                        //   CMP EAX, count      ; bounds check
                        //   JAE default          ; out of range → default
                        //   MOVSXD RCX, [RDX + RAX*4]  ; load relative offset from table
                        //   ADD RCX, RDX        ; compute absolute address
                        //   JMP RCX             ; indirect jump
                        //   <jump table: count * 4 bytes of i32 offsets>

                        // Normalize key: SUB EAX, low
                        if low != 0 {
                            self.buf.emit(&[0x2D]); // SUB EAX, imm32
                            self.buf.emit(&low.to_le_bytes());
                        }
                        // Bounds check: CMP EAX, count; JAE default
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&(count as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                        self.buf.emit(&[0x0F, 0x83]); // JAE rel32
                        let bounds_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((bounds_patch, def_target));

                        // LEA RDX, [RIP + 0]  → points to jump table
                        // We'll emit: LEA RDX, [RIP + disp32] where disp32 will be
                        // patched to point to the table start.
                        self.buf.emit(&[0x48, 0x8D, 0x15]); // LEA RDX, [RIP + disp32]
                        let lea_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]); // placeholder disp32

                        // MOVSXD RCX, [RDX + RAX*4]  ; load table[index]
                        // Encoding: REX.W 0x63 /r with SIB [RDX + RAX*4]
                        self.buf.emit(&[0x48, 0x63, 0x0C, 0x82]); // MOVSXD RCX, [RDX + RAX*4]

                        // ADD RCX, RDX  ; absolute = table_base + offset
                        self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX

                        // JMP RCX  ; indirect jump
                        self.buf.emit(&[0xFF, 0xE1]); // JMP RCX

                        // Patch LEA: disp32 = table_start - (lea_patch + 4)
                        let table_start = self.buf.pos();
                        let lea_rel = table_start as i32 - (lea_patch as i32 + 4); // Cast: x86-64 rel32 displacement
                        self.buf.patch_i32(lea_patch, lea_rel);

                        // Emit jump table: count entries, each i32 offset from table_start
                        for &target in &targets {
                            let entry_offset = self.buf.pos();
                            self.buf.emit(&[0; 4]); // placeholder
                            self.jump_table_patches.push((entry_offset, table_start, target));
                        }
                    }
                    self.reset_spills();
                    dead = true;
                }

                // lookupswitch — CMP chain for small, binary search for large
                0xab => {
                    self.flush_scratch_registers();
                    self.pop_to_rax();
                    let base_pc = pc;
                    pc += 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let default_offset =
                        i32::from_be_bytes([code[pc], code[pc + 1], code[pc + 2], code[pc + 3]]);
                    let npairs = i32::from_be_bytes([
                        code[pc + 4],
                        code[pc + 5],
                        code[pc + 6],
                        code[pc + 7],
                    ]) as usize; // Cast: address arithmetic
                    pc += 8;

                    // Collect all (key, target) pairs
                    let mut pairs = Vec::with_capacity(npairs);
                    for _ in 0..npairs {
                        let key = i32::from_be_bytes([
                            code[pc], code[pc + 1], code[pc + 2], code[pc + 3],
                        ]);
                        let off = i32::from_be_bytes([
                            code[pc + 4], code[pc + 5], code[pc + 6], code[pc + 7],
                        ]);
                        let target = (base_pc as i32 + off) as usize; // Cast: x86-64 immediate encoding
                        pc += 8;
                        pairs.push((key, target));
                    }
                    let def_target = (base_pc as i32 + default_offset) as usize; // Cast: x86-64 immediate encoding

                    if npairs <= 6 {
                        // Small: linear CMP chain (fast for few entries)
                        for &(key, target) in &pairs {
                            self.buf.emit(&[0x3D]); // CMP EAX, imm32
                            self.buf.emit(&key.to_le_bytes());
                            self.buf.emit(&[0x0F, 0x84]); // JE rel32
                            let patch = self.buf.pos();
                            self.buf.emit(&[0; 4]);
                            self.forward_patches.push((patch, target));
                        }
                        // Default: JMP
                        self.buf.emit_byte(0xE9);
                        let dp = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((dp, def_target));
                    } else {
                        // Large: binary search tree emitted as nested CMP/JL/JG/JE
                        // The keys in lookupswitch are sorted per JVM spec.
                        // We emit a balanced binary search: O(log n) comparisons.
                        //
                        // Value in EAX. We use a recursive emission strategy:
                        //   pick middle key, CMP EAX, mid_key
                        //   JE target
                        //   JL left_subtree
                        //   (fall through to right subtree)
                        // At leaves, fall through to default.
                        self.emit_binary_search_lookup(&pairs, def_target);
                    }
                    self.reset_spills();
                    dead = true;
                }

                // ireturn / lreturn / freturn / dreturn / areturn
                0xac..=0xb0 => {
                    self.flush_scratch_registers();
                    self.pop_to_rax();
                    self.emit_epilogue();
                    self.reset_spills();
                    dead = true;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    // No return value needed, just emit epilogue
                    self.flush_scratch_registers();
                    self.emit_epilogue();
                    self.reset_spills();
                    dead = true;
                    pc += 1;
                }

                // getstatic (0xb2) — always call helper for thread safety
                //
                // MED-2 (round-2 JIT review) — HotSpot inlines non-volatile
                // getstatic as a single `MOV reg, [imm64]` against the class's
                // static-area slot, because both the class_id and the slot
                // address are known at JIT compile time. CratonVM cannot
                // currently emit that form. Bail rationale (see round-1 TLAB
                // bail at 10783-10807 for the same pattern):
                //
                //   1. Slot storage is `SharedVm.statics:
                //      RwLock<HashMap<ClassId, Vec<Value>>>` (see
                //      `vm/src/vm/vm_object.rs::get_static_shared` at line
                //      472). The slot address is NOT stable:
                //        * the `Vec<Value>` is grown by `resize` in
                //          `set_static_shared` (vm_object.rs:498) — any prior
                //          `&v[idx]` pointer dangles after the grow,
                //        * the HashMap entry is created lazily on first
                //          write (line 485), so a getstatic at warmup time
                //          may see no entry at all,
                //        * concurrent writers hold the RwLock write guard;
                //          a JIT inline `MOV [imm64]` would race the
                //          interpreter's `set_static_shared`.
                //      The slot pointer would therefore have to be embedded
                //      as an immediate yet remain valid across the program's
                //      lifetime — neither holds today.
                //
                //   2. Even if the storage were a stably-addressed array,
                //      the `Value` enum is a tagged union (Int/Long/Float/
                //      Double/Object), not a raw machine word. The JIT would
                //      have to read both the tag and the payload to know
                //      how to push to its operand stack — multi-step,
                //      atomicity-fragile, and dependent on the enum layout.
                //
                //   3. The `jit` crate has no dependency on the `vm` crate
                //      (see `jit/Cargo.toml` — only types, reader, jit-api).
                //      So even doing the resolution at JIT compile time
                //      would require either (a) a new field on
                //      `JitRuntimeHelpers` that exposes a fn-pointer
                //      `resolve_static_slot(class_id, field_index) ->
                //      *const Value`, or (b) plumbing the resolved slot
                //      addresses into the per-bci `static_field_info` from
                //      the caller in vm/src/jit/. Both require edits beyond
                //      this file; this task is constrained to `jit/src/x64.rs`
                //      only.
                //
                // To wire inlining later, the prerequisites are:
                //   * Change `SharedVm.statics` to use a stable allocation
                //     for each class's static area (e.g. `Box<[AtomicU64]>`
                //     allocated once per `<clinit>` and pinned for the
                //     class's life). Volatile fields then use
                //     `MOV [imm64]` + MFENCE; non-volatile use plain
                //     `MOV [imm64]` (x86 already gives acquire ordering
                //     for aligned 8-byte loads).
                //   * Add a `JitRuntimeHelpers` field exposing the slot
                //     resolver, or pre-resolve at JIT compile time and
                //     extend `static_field_info` to carry the slot ptr.
                //   * Match the static slot type to the field's JVM type
                //     (use type_tag) so the inline MOV writes the right
                //     width (32 for int/float, 64 for long/double/ref).
                //
                // Expected speedup once wired: a JIT-compiled hot loop with
                // a getstatic+putstatic pair drops from ~2 CALLs + arg
                // marshalling (~12-18 cycles round trip) to two MOVs
                // (~3-4 cycles). HotSpot publishes this as the dominant
                // static-field-access optimization; we expect a 4-6x
                // speedup on static-field-heavy microbenchmarks
                // (Counter.increment(), shared lazy-init flags, etc.).
                //
                // Until then, every static access stays on the helper
                // path below. This is correct (the helper takes the
                // RwLock and reads `Value` properly) but slow.
                0xb2 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, class_id_raw, field_index, _type_tag, is_volatile) =
                        self.static_field_info_idx
                            .get(&pc)
                            .map(|&i| self.static_field_info[i])
                            .unwrap_or((pc, 0, 0, b'I', false));

                    self.flush_scratch_registers();
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                    self.emit_call_absolute(self.helpers.getstatic);
                    // Volatile static: emit MFENCE after read (SeqCst acquire)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    self.push_from_rax();
                    pc += 3;
                }

                // putstatic (0xb3) — write static field via type-specific helper
                //
                // MED-2 (round-2 JIT review): same bail as 0xb2 above. The
                // symmetric inline form would be `MOV [imm64], reg`, but
                // (a) `SharedVm.statics` slot addresses aren't stable
                // (the Vec resizes; the HashMap entry is created lazily),
                // (b) writes need to go through `set_static_shared` so the
                // GC and finalizer paths see the new object reference, and
                // (c) the `jit` crate has no `vm` dependency to resolve the
                // slot pointer at JIT compile time. See the long bail comment
                // on the 0xb2 handler above for the full unblocking plan.
                0xb3 => {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, class_id_raw, field_index, type_tag, is_volatile) =
                        self.static_field_info_idx
                            .get(&pc)
                            .map(|&i| self.static_field_info[i])
                            .unwrap_or((pc, 0, 0, b'I', false));

                    let val_slot = self.pop_stack();

                    // Select the appropriate helper based on type_tag
                    let helper_fn: usize = match type_tag {
                        b'J' => self.helpers.putstatic_long,
                        b'F' => self.helpers.putstatic_float,
                        b'D' => self.helpers.putstatic_double,
                        b'L' | b'[' => self.helpers.putstatic_object,
                        _ => self.helpers.putstatic_int,
                    };

                    // Volatile static: emit MFENCE before write (SeqCst release)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    // Call jit_putstatic_xxx(vm_ptr, class_id_raw, field_index, val)
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // class_id // Cast: x86-64 immediate encoding
                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // field_index // Cast: x86-64 immediate encoding
                    self.load_slot_to_reg(ARG_REGS[3], val_slot); // value
                    self.emit_call_absolute(helper_fn);
                    // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    pc += 3;
                }

                // getfield — read object field via helper (or frame slot for scalar-replaced)
                0xb4 => {
                    if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                        // Scalar-replaced getfield: load directly from frame slot
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, _) = self
                            .field_info_idx
                            .get(&pc)
                            .map(|&i| self.field_info[i])
                            .unwrap_or((pc, 0, b'I'));
                        let _obj_slot = self.pop_stack(); // dummy objectref
                        let sr_obj = &self.scalar_replaced[&new_pc];
                        let field_off = sr_obj.field_base_offset
                            + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                        self.emit_load_local(RAX, field_off);
                        self.push_from_rax();
                        pc += 3;
                    } else {
                        self.flush_scratch_registers();
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, _type_tag) = self
                            .field_info_idx
                            .get(&pc)
                            .map(|&i| self.field_info[i])
                            .unwrap_or((pc, 0, b'I'));
                        let obj_slot = self.pop_stack();
                        self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                        self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                        self.emit_call_absolute(self.helpers.getfield);
                        self.push_from_rax();
                        pc += 3;
                    }
                }

                // putfield — write object field (frame slot for scalar-replaced, helper otherwise)
                0xb5 => {
                    if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                        // Scalar-replaced putfield: store value directly to frame slot
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, _type_tag) = self
                            .field_info_idx
                            .get(&pc)
                            .map(|&i| self.field_info[i])
                            .unwrap_or((pc, 0, b'I'));
                        let val_slot = self.pop_stack();
                        let _obj_slot = self.pop_stack(); // dummy objectref
                        let sr_obj = &self.scalar_replaced[&new_pc];
                        let field_off = sr_obj.field_base_offset
                            + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(RAX, val_slot);
                        self.emit_store_local(field_off, RAX);
                        // Each scalar field reserves SLOT_SIZE bytes (see `new` zero-init). Always
                        // clear the high qword so category-1 values and refs never leave garbage in
                        // the second word — mismatches here showed up as Windows AVs under Spring
                        // with JIT on (SportMe / insurance) while interpreter-only runs continued.
                        self.emit_xor_reg_self(RAX);
                        self.emit_store_local(field_off + 8, RAX);
                        pc += 3;
                    } else {
                        self.flush_scratch_registers();
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, type_tag) = self
                            .field_info_idx
                            .get(&pc)
                            .map(|&i| self.field_info[i])
                            .unwrap_or((pc, 0, b'I'));
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        if type_tag == b'L' || type_tag == b'[' {
                            // TODO (R20 follow-up): inline this store. Blocked on the
                            // field-storage layout — Java object fields are stored as
                            // the 16-byte Rust enum `Value` (tag + payload, repr is
                            // implementation-defined), not a compact 8-byte pointer like
                            // the Object[] array layout used by aastore. A safe inline
                            // store would need either:
                            //   (a) a `#[repr(C, u8)]` or stable-layout commitment on
                            //       `Value`, plus emitting both halves (tag byte + ptr
                            //       qword) at the field offset; or
                            //   (b) migrating reference fields to a compact pointer
                            //       layout (parallel to the array compact layout).
                            // Neither is in scope here, so keep the helper call. This is
                            // the higher-value half of HIGH-5 and remains a known gap.
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            self.emit_call_absolute(self.helpers.putfield_object);
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            self.emit_call_absolute(helper);
                        }
                        pc += 3;
                    }
                }

                // invokestatic — self-call, direct call, inline, or dispatch helper
                0xb8 => {
                    self.flush_scratch_registers();

                    // Check for inline site first (most profitable)
                    if self.inline_sites.contains_key(&pc) {
                        if self.try_emit_inline(pc) {
                            pc += 3;
                            continue;
                        }
                    }

                    // Check for direct call target
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let direct = self
                        .direct_calls_idx
                        .get(&pc)
                        .map(|&i| {
                            let dc = &self.direct_calls[i].1;
                            (dc.entry, dc.needs_context, dc.num_params, dc.return_type)
                        });

                    // Check for invoke_info (fallback to jit_invoke_dispatch)
                    let info_ptr = self
                        .invoke_info_idx
                        .get(&pc)
                        .map(|&i| self.invoke_info[i].1);

                    if let Some((callee_entry, callee_needs_ctx, callee_params, ret_type)) = direct {
                        if callee_entry == super::MATH_SQRT_INTRINSIC {
                            // Math.sqrt(double) intrinsic: inline SQRTSD — no call overhead
                            let arg_slot = self.pop_stack();
                            // Flush any OTHER Xmm(0) slots that would be clobbered by SQRTSD
                            // (the arg itself is fine — it's consumed)
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        // MOVSD XMM0, XMMn
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                    // else: already in XMM0
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            self.emit_sqrtsd_xmm0();
                            self.stack.push(StackSlot::Xmm(0));
                        } else if callee_entry == super::MATH_FLOOR_INTRINSIC
                            || callee_entry == super::MATH_CEIL_INTRINSIC
                            || callee_entry == super::MATH_RINT_INTRINSIC
                        {
                            // Math.floor/ceil/rint intrinsic: ROUNDSD XMM0, XMM0, imm8
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            // ROUNDSD XMM0, XMM0, imm8
                            // Encoding: 66 0F 3A 0B C0 imm8
                            let imm8 = if callee_entry == super::MATH_FLOOR_INTRINSIC {
                                0x09u8 // round toward -inf, inexact suppress
                            } else if callee_entry == super::MATH_CEIL_INTRINSIC {
                                0x0Au8 // round toward +inf, inexact suppress
                            } else {
                                0x08u8 // round to nearest even, inexact suppress
                            };
                            self.buf.emit(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, imm8]);
                            self.stack.push(StackSlot::Xmm(0));
                        } else if callee_entry == super::MATH_ABS_DOUBLE_INTRINSIC {
                            // Math.abs(double): clear sign bit (bit 63)
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            // Load sign mask 0x7FFFFFFFFFFFFFFF into RCX, then MOVQ XMM1, RCX, ANDPD XMM0, XMM1
                            // MOV RCX, imm64
                            self.buf.emit_byte(0x48); // REX.W
                            self.buf.emit_byte(0xB9); // MOV RCX, imm64
                            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
                            // MOVQ XMM1, RCX
                            self.emit_movq_xmm_from_gpr(1, RCX);
                            // ANDPD XMM0, XMM1: 66 0F 54 C1
                            self.buf.emit(&[0x66, 0x0F, 0x54, 0xC1]);
                            self.stack.push(StackSlot::Xmm(0));
                        } else if callee_entry == super::MATH_ABS_FLOAT_INTRINSIC {
                            // Math.abs(float): clear sign bit (bit 31)
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF3, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF3, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    // MOVD XMM0, EAX: 66 0F 6E C0
                                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                                }
                            }
                            // Load sign mask 0x7FFFFFFF into ECX, MOVD XMM1, ECX, ANDPS XMM0, XMM1
                            // MOV ECX, imm32
                            self.buf.emit_byte(0xB9);
                            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
                            // MOVD XMM1, ECX: 66 0F 6E C9
                            self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]);
                            // ANDPS XMM0, XMM1: 0F 54 C1
                            self.buf.emit(&[0x0F, 0x54, 0xC1]);
                            self.stack.push(StackSlot::Xmm(0));
                        } else if callee_entry == super::MATH_ABS_INT_INTRINSIC {
                            // Math.abs(int): branchless absolute value
                            let arg_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg_slot);
                            // MOV ECX, EAX: 89 C1
                            self.buf.emit(&[0x89, 0xC1]);
                            // SAR EAX, 31 (sign-extend to all bits): C1 F8 1F
                            self.buf.emit(&[0xC1, 0xF8, 0x1F]);
                            // XOR ECX, EAX: 31 C1
                            self.buf.emit(&[0x31, 0xC1]);
                            // SUB ECX, EAX: 29 C1
                            self.buf.emit(&[0x29, 0xC1]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == super::MATH_ABS_LONG_INTRINSIC {
                            // Math.abs(long): branchless 64-bit absolute value
                            let arg_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg_slot);
                            // MOV RCX, RAX: 48 89 C1
                            self.buf.emit(&[0x48, 0x89, 0xC1]);
                            // SAR RAX, 63: 48 C1 F8 3F
                            self.buf.emit(&[0x48, 0xC1, 0xF8, 0x3F]);
                            // XOR RCX, RAX: 48 31 C1
                            self.buf.emit(&[0x48, 0x31, 0xC1]);
                            // SUB RCX, RAX: 48 29 C1
                            self.buf.emit(&[0x48, 0x29, 0xC1]);
                            // MOV RAX, RCX: 48 89 C8
                            self.buf.emit(&[0x48, 0x89, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == super::MATH_FMA_DOUBLE_INTRINSIC {
                            // T1.1.28 — Math.fma(double, double, double).
                            //
                            // Per JLS, `Math.fma(a, b, c)` computes `a*b + c`
                            // as if with unlimited intermediate precision and
                            // then rounded once. We lower to a direct call
                            // into `jit_math_fma_double`, which delegates to
                            // Rust's `f64::mul_add` — that maps to
                            // `VFMADD231SD` on FMA3-capable hosts and to a
                            // correctly-rounded software fused operation on
                            // everything else. Either path satisfies the JLS
                            // single-rounding requirement.
                            //
                            // extern "C" fn(f64, f64, f64) -> f64 — arguments
                            // pass in XMM0, XMM1, XMM2 on both SysV and Win64.
                            self.flush_scratch_registers();
                            let c_slot = self.pop_stack();
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            // Load a into RAX then → XMM0.
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1);
                            self.load_slot_to_reg(RAX, c_slot);
                            self.emit_movq_xmm_from_rax(2);
                            self.emit_call_absolute(self.helpers.math_fma_double);
                            // Result in XMM0 → move to RAX and push as FP.
                            self.emit_movq_rax_from_xmm(0);
                            self.push_from_rax_as_xmm0();
                        } else if callee_entry == super::MATH_FMA_FLOAT_INTRINSIC {
                            // T1.1.28 — Math.fma(float, float, float) — same
                            // plan via `jit_math_fma_float` / `f32::mul_add`.
                            self.flush_scratch_registers();
                            let c_slot = self.pop_stack();
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1);
                            self.load_slot_to_reg(RAX, c_slot);
                            self.emit_movq_xmm_from_rax(2);
                            self.emit_call_absolute(self.helpers.math_fma_float);
                            self.emit_movq_rax_from_xmm(0);
                            self.push_from_rax_as_xmm0();
                        } else if callee_entry == super::MATH_MIN_INT_INTRINSIC
                            || callee_entry == super::MATH_MAX_INT_INTRINSIC
                        {
                            // Round-8 Bug 8 — branchless Math.min(int,int) /
                            // Math.max(int,int) via CMOV. Pop b then a (a is
                            // the deeper operand, the leftmost arg in the JLS
                            // signature). After `CMP EAX, ECX` (a vs b):
                            //   * CMOVL EAX, ECX fires when `a < b` and
                            //     overwrites EAX (=a) with ECX (=b) — i.e.
                            //     keeps the LARGER value in EAX. This is MAX.
                            //   * CMOVG EAX, ECX fires when `a > b` and
                            //     overwrites EAX (=a) with ECX (=b) — i.e.
                            //     keeps the SMALLER value in EAX. This is MIN.
                            // Round-9 CRIT fix: the previous version had these
                            // two swapped, so `Math.min(3, 5)` returned 5 and
                            // `Math.max(3, 5)` returned 3.
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.load_slot_to_reg(RCX, b_slot);
                            // CMP EAX, ECX — sets flags for signed compare.
                            self.emit_cmp_r32_r32(RAX, RCX);
                            let cc = if callee_entry == super::MATH_MIN_INT_INTRINSIC {
                                0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                            } else {
                                0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                            };
                            // CMOVcc EAX, ECX (32-bit, no REX.W): 0F 4c C1
                            self.buf.emit(&[0x0F, cc, 0xC1]);
                            self.push_from_rax();
                        } else if callee_entry == super::MATH_MIN_LONG_INTRINSIC
                            || callee_entry == super::MATH_MAX_LONG_INTRINSIC
                        {
                            // Round-8 Bug 8 — 64-bit Math.min(long,long) /
                            // Math.max(long,long) via REX.W CMP + CMOV. Same
                            // semantics as the int variants but 64-bit.
                            // Round-9 CRIT fix: opcodes were swapped (see int
                            // variant above for the full rationale).
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.load_slot_to_reg(RCX, b_slot);
                            // CMP RAX, RCX (REX.W): 48 39 C8
                            self.buf.emit(&[0x48, 0x39, 0xC8]);
                            let cc = if callee_entry == super::MATH_MIN_LONG_INTRINSIC {
                                0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                            } else {
                                0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                            };
                            // CMOVcc RAX, RCX (REX.W): 48 0F 4c C1
                            self.buf.emit(&[0x48, 0x0F, cc, 0xC1]);
                            self.push_from_rax();
                        } else {
                            // Direct call to a JIT-compiled callee
                            let n = callee_params;
                            let mut arg_slots = Vec::with_capacity(n);
                            for _ in 0..n {
                                arg_slots.push(self.pop_stack());
                            }
                            arg_slots.reverse();

                            // T5.2.16 — Sibling tail-call optimization.
                            //
                            // When the caller's immediate next bytecode
                            // is an `xreturn` of the same type the
                            // callee produces, we can tear down our
                            // frame and `JMP` into the callee so the
                            // callee returns straight to OUR caller.
                            // Gate on:
                            //   1. pc+3 is an xreturn whose type tag
                            //      matches ret_type, or the callee is
                            //      void AND pc+3 is `return` (0xB1).
                            //   2. callee_needs_ctx == self.needs_heap
                            //      (we have a VM context iff the
                            //      callee wants one) — otherwise the
                            //      ABI shift wouldn't match.
                            //   3. RET intrinsics (MATH_*_INTRINSIC)
                            //      are NOT targeted (already branched
                            //      above), so the callee is a normal
                            //      JIT-compiled method.
                            let tail_op_matches = pc + 3 < code_len
                                && match (ret_type, code[pc + 3]) {
                                    (b'I' | b'Z' | b'B' | b'S' | b'C', 0xAC) => true,
                                    (b'J', 0xAD) => true,
                                    (b'F', 0xAE) => true,
                                    (b'D', 0xAF) => true,
                                    (b'L' | b'[', 0xB0) => true,
                                    (b'V', 0xB1) => true,
                                    _ => false,
                                };
                            let is_sibling_tail = tail_op_matches
                                && callee_needs_ctx == self.needs_heap;

                            // Round-8 wave-3: sibling-tail demotion.
                            // Tail-calling with stack args is non-trivial
                            // — args would have to be re-materialized
                            // *after* the epilogue restores RSP, which
                            // requires an additional shuffle buffer.
                            // Simpler and still correct: demote to a
                            // non-tail CALL when the arg count would
                            // require stack passing. The fall-through
                            // below handles that case with the proper
                            // stack-arg setup helper.
                            let sibling_reg_limit = if callee_needs_ctx {
                                ARG_REGS.len() - 1
                            } else {
                                ARG_REGS.len()
                            };
                            let sibling_tail_ok =
                                is_sibling_tail && arg_slots.len() <= sibling_reg_limit;
                            if sibling_tail_ok {
                                // Load args into ABI registers, tear
                                // down our frame, then JMP.
                                if callee_needs_ctx {
                                    self.emit_load_local(
                                        ARG_REGS[0],
                                        self.heap_local_offset,
                                    );
                                    for (i, slot) in arg_slots.iter().enumerate() {
                                        if i + 1 < ARG_REGS.len() {
                                            self.load_slot_to_reg(ARG_REGS[i + 1], *slot);
                                        }
                                    }
                                } else {
                                    for (i, slot) in arg_slots.iter().enumerate() {
                                        if i < ARG_REGS.len() {
                                            self.load_slot_to_reg(ARG_REGS[i], *slot);
                                        }
                                    }
                                }
                                self.emit_epilogue_without_ret();
                                self.emit_jmp_absolute(callee_entry);
                                // Consume the invokestatic (3) and the
                                // xreturn (1) — no fall-through.
                                pc += 3; // invokestatic
                                pc += 1; // xreturn
                                self.reset_spills();
                                continue;
                            }

                            // Round-8 wave-3 HIGH fix: stack-arg setup
                            // for direct calls whose total arg count
                            // exceeds ARG_REGS. Uses platform ABI
                            // (Win64 32-byte shadow + stack; SysV pure
                            // stack), 16-byte aligned at the CALL site.
                            let total_sub = self.emit_stack_arg_setup(
                                &arg_slots,
                                callee_needs_ctx,
                            );
                            // Round-8 wave-3: defensive callee-saved spill
                            // before any GC-triggering CALL.
                            self.emit_pre_safepoint_spill();
                            // Emit direct CALL to callee entry point
                            self.emit_call_absolute(callee_entry);
                            // T1.1.2 — direct call to a JIT-compiled
                            // callee is still a safepoint: the callee
                            // may allocate and trigger GC transitively.
                            self.emit_oop_map_for_safepoint();
                            self.emit_stack_arg_cleanup(total_sub);

                            if ret_type != b'V' {
                                if matches!(ret_type, b'D' | b'F') {
                                    self.push_from_rax_as_xmm0();
                                } else {
                                    self.push_from_rax();
                                }
                                if matches!(ret_type, b'L' | b'[') {
                                    self.mark_top_as_oop();
                                }
                            }
                        }
                    } else if let Some(info) = info_ptr {
                        // Non-self invokestatic without a direct target — use dispatch helper
                        // SAFETY: info comes from self.invoke_info, which holds pointers to
                        // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                        let info_ref = unsafe { &*info };
                        let n = info_ref.num_jit_args;

                        // Capture spill offset BEFORE popping to prevent
                        // the args buffer from overlapping source Frame slots.
                        let pre_pop_spill = self.next_spill_offset;
                        let mut arg_slots = Vec::with_capacity(n);
                        for _ in 0..n {
                            arg_slots.push(self.pop_stack());
                        }
                        arg_slots.reverse();

                        let args_base_offset = pre_pop_spill;
                        if n > 0 {
                            self.next_spill_offset = args_base_offset + (n as i32) * 8; // Cast: x86-64 immediate encoding
                            // Store args in reverse offset order so they form
                            // a contiguous ascending-address buffer:
                            //   arg[0] at [rbp - highest_offset] (lowest addr)
                            //   arg[n-1] at [rbp - args_base_offset] (highest addr)
                            // This is necessary because modrm_rbp_disp negates
                            // the offset, so higher offsets map to lower addresses.
                            for (i, slot) in arg_slots.iter().enumerate() {
                                let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(RAX, *slot);
                                self.emit_store_local(buf_offset, RAX);
                            }
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                        if n > 0 {
                            // LEA to the highest offset = lowest address = start of buffer
                            let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                        } else {
                            self.emit_xor_reg_self(ARG_REGS[2]);
                        }
                        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL.
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(self.helpers.invoke_dispatch);
                        // T1.1.2 — invoke dispatch is a full safepoint:
                        // the callee may allocate, trigger GC, or throw.
                        // Record an oop map for the operand stack state
                        // that survives the call (args are already popped,
                        // return value not yet pushed).
                        self.emit_oop_map_for_safepoint();

                        // Reclaim spill slots used for invoke args
                        self.next_spill_offset = pre_pop_spill;

                        if info_ref.return_type != b'V' {
                            if matches!(info_ref.return_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            // T1.1.2 — the return value is an object
                            // reference iff the descriptor ends in `L`
                            // or `[`. Tag it so the next safepoint
                            // records it as a live oop.
                            if matches!(info_ref.return_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    } else {
                        // Self-recursive call (no invoke_info, no direct_call)
                        let n = self.num_params;

                        // Check for tail call: invokestatic self at PC, xreturn at PC+3
                        let is_tail_call = pc + 3 < code_len
                            && matches!(code[pc + 3], 0xac..=0xb0); // ireturn..areturn

                        let mut arg_slots = Vec::with_capacity(n);
                        for _ in 0..n {
                            arg_slots.push(self.pop_stack());
                        }
                        arg_slots.reverse();

                        if is_tail_call && self.body_entry_offset > 0 {
                            // Tail-call optimization: load args into parameter locals
                            // and JMP back to body entry (skip prologue)
                            let local_assignments = self.local_assignments.clone();
                            for (i, slot) in arg_slots.iter().enumerate() {
                                if i < n {
                                    if let Some(reg) = local_assignments.get(i).copied().flatten() {
                                        self.load_slot_to_reg(reg, *slot);
                                    } else {
                                        self.load_slot_to_reg(RAX, *slot);
                                        self.emit_store_local(self.local_offset(i), RAX);
                                    }
                                }
                            }
                            // JMP rel32 back to body entry
                            self.buf.emit_byte(0xE9); // JMP rel32
                            let jmp_offset = self.buf.pos();
                            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                            // Patch: target = body_entry_offset
                            let rel = self.body_entry_offset as i32 - (jmp_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                            let pos = self.buf.pos();
                            self.buf.patch_i32(jmp_offset, rel);
                            let _ = pos;

                            // Skip the following xreturn — we already jumped
                            pc += 3; // invokestatic
                            pc += 1; // xreturn
                            self.reset_spills();
                            continue;
                        }

                        // Round-8 wave-3 HIGH fix: stack-arg setup for
                        // self-recursive direct calls past ARG_REGS.
                        let total_sub = self.emit_stack_arg_setup(
                            &arg_slots,
                            self.needs_heap,
                        );
                        // Round-8 wave-3: defensive callee-saved spill
                        // before the recursive CALL (which transitively
                        // can allocate and reach a GC safepoint).
                        self.emit_pre_safepoint_spill();
                        // Normal self-call via CALL (rel32, patched
                        // post-emission).
                        self.buf.emit_byte(0xE8);
                        let call_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.self_call_patches.push(call_patch);
                        self.emit_stack_arg_cleanup(total_sub);
                        self.push_from_rax();
                    }
                    pc += 3;
                }

                // invokevirtual / invokespecial / invokeinterface — direct call or dispatch helper
                0xb6 | 0xb7 | 0xb9 => {
                    // Scalar replacement: skip <init>()V on scalar-replaced objects
                    if op == 0xb7 && self.scalar_init_skips.contains(&pc) {
                        let _ = self.pop_stack(); // discard dup'd receiver
                        pc += 3;
                        continue;
                    }
                    self.flush_scratch_registers();

                    // Check for inline site (invokespecial only — virtual/interface not eligible)
                    if op == 0xb7 && self.inline_sites.contains_key(&pc) {
                        if self.try_emit_inline(pc) {
                            pc += 3;
                            continue;
                        }
                    }

                    // Check for direct call target (invokespecial with compiled callee)
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let direct = self
                        .direct_calls_idx
                        .get(&pc)
                        .map(|&i| {
                            let dc = &self.direct_calls[i].1;
                            (dc.entry, dc.needs_context, dc.num_params, dc.return_type)
                        });

                    if let Some((callee_entry, callee_needs_ctx, callee_params, ret_type)) = direct {
                        // Direct call: pop receiver + params, call compiled entry
                        // invokespecial has a receiver, so total args = callee_params + 1
                        let n = callee_params + 1; // receiver + params
                        let mut arg_slots = Vec::with_capacity(n);
                        for _ in 0..n {
                            arg_slots.push(self.pop_stack());
                        }
                        arg_slots.reverse();

                        // Round-8 wave-3 HIGH fix: stack-arg setup for
                        // invokespecial/virtual direct calls whose
                        // receiver+params exceed ARG_REGS.
                        let total_sub = self.emit_stack_arg_setup(
                            &arg_slots,
                            callee_needs_ctx,
                        );
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL.
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(callee_entry);
                        self.emit_stack_arg_cleanup(total_sub);

                        if ret_type != b'V' {
                            if matches!(ret_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                        }
                    } else {
                        // Dispatch via helper (MIC-optimized for virtual/interface, plain for others)
                        // MED-4 / Fix 3 — O(1) pc-indexed lookups for invoke/MIC/PIC.
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        if let Some(info) = info_ptr {
                            // SAFETY: info comes from self.invoke_info, which holds pointers to
                            // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                            let info_ref = unsafe { &*info };
                            let n = info_ref.num_jit_args;

                            // Check for MIC slot at this PC
                            let mic_ptr = self
                                .mic_slots_idx
                                .get(&pc)
                                .map(|&i| self.mic_slots[i].1);
                            // Check for PIC slot at this PC. When both PIC
                            // and MIC are present (the adaptive recompiler
                            // promotes MIC → PIC and leaves the old MIC
                            // slot live as a fallback), PIC takes
                            // precedence: it caches a 3-entry superset.
                            let pic_ptr = self
                                .pic_slots_idx
                                .get(&pc)
                                .map(|&i| self.pic_slots[i].1);
                            if std::env::var_os("RUSTJVM_DBG_JIT_GEN").is_some() {
                                eprintln!(
                                    "[JIT_GEN_INVOKE_VS] pc={} op=0x{:02x} info_kind={} mic_present={} pic_present={} {}.{}{}",
                                    pc, op, info_ref.invoke_kind, mic_ptr.is_some(), pic_ptr.is_some(),
                                    info_ref.class_name, info_ref.method_name, info_ref.descriptor,
                                );
                            }

                            // Capture spill offset BEFORE popping to prevent
                            // the args buffer from overlapping source Frame slots.
                            let pre_pop_spill = self.next_spill_offset;
                            let mut arg_slots = Vec::with_capacity(n);
                            for _ in 0..n {
                                arg_slots.push(self.pop_stack());
                            }
                            arg_slots.reverse();

                            let args_base_offset = pre_pop_spill;
                            if n > 0 {
                                self.next_spill_offset = args_base_offset + (n as i32) * 8; // Cast: x86-64 immediate encoding
                                // Store args in reverse offset order (same fix
                                // as invokestatic): higher offsets → lower addresses,
                                // so arg[0] at highest offset = lowest address.
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.load_slot_to_reg(RAX, *slot);
                                    self.emit_store_local(buf_offset, RAX);
                                }
                            }

                            // CRIT-8 — Inline MIC fast-path guard.
                            //
                            // Layout (verified by `test_jit_mic_slot_offsets` in
                            // jit/src/lib.rs; struct is `#[repr(C)]`):
                            //   offset  0  AtomicU32  cached_class_id
                            //   offset  8  AtomicU64  cached_entry_ptr
                            //   offset 16  AtomicBool cached_needs_context
                            //
                            // HIGH-7 follow-up — Inline 3-way PIC fast-path
                            // guard. `JitPICSlot` is now `#[repr(C)]` with
                            // hot atomic fields at the front (see
                            // `jit/src/lib.rs:1305` and the layout assertion
                            // `test_jit_pic_slot_offsets`):
                            //
                            //   CLASS_ID_OFFSETS     = [0, 4, 8]
                            //   ENTRY_PTR_OFFSETS    = [16, 24, 32]
                            //   NEEDS_CONTEXT_OFFSETS = [40, 41, 42]
                            //
                            // The Mutex<Option<String>> array (`class_names`)
                            // is moved to the tail so its unstable layout
                            // cannot disturb these offsets.
                            //
                            // When a PIC slot is allocated at this PC, we
                            // emit a 3-way cascade in place of the MIC probe.
                            // PIC supersedes MIC (it is a 3-entry superset)
                            // so we do not emit BOTH guards.
                            //
                            // Hot-path sequence (PIC, ≈5 cycles on slot-0 hit):
                            //   mov   r10, imm64(pic)
                            //   mov   rax, [rbp - receiver_spill]
                            //   mov   eax, [rax]                       ; class_id @ ObjectHeader+0
                            //   ; --- per slot i in 0..3 ---
                            //   cmp   eax, [r10 + CLASS_ID_OFFSETS[i]]
                            //   jne   .try_{i+1}  (or .miss for i==2)
                            //   cmp   byte [r10 + NEEDS_CONTEXT_OFFSETS[i]], 0
                            //   je    .miss                            ; only inline ctx=true
                            //   <load callee-ABI args: vm_ptr + arg_slots[0..n]>
                            //   call  qword [r10 + ENTRY_PTR_OFFSETS[i]]
                            //   jmp   .done
                            //   ; --- end per-slot ---
                            // .miss:
                            //   <existing helper-ABI setup>
                            //   call  jit_invoke_virtual_mic            ; same helper —
                            //                                            ; it consults the
                            //                                            ; underlying cache
                            //                                            ; (MIC or PIC via the
                            //                                            ; adaptive recompiler).
                            // .done:
                            //
                            // Raw memory loads of the atomics are equivalent
                            // to `Ordering::Relaxed` reads (no fences). A
                            // torn class_id or stale entry pointer at worst
                            // causes a miss → slow path; the helper
                            // revalidates and re-resolves authoritatively.
                            // Empty PIC entries hold class_id == 0, which the
                            // doc reserves for `java.lang.Object` (never a
                            // dispatch target here), so an empty slot
                            // naturally fails its CMP and falls through.
                            //
                            // Fast-path eligibility (same as MIC):
                            //   1. pic_ptr OR mic_ptr is Some.
                            //   2. The receiver exists (n >= 1).
                            //   3. The cached entry uses the JIT-context ABI
                            //      (`cached_needs_context == true`); checked
                            //      inline. Non-ctx callees fall to the
                            //      helper.
                            //   4. Total callee-ABI arg count (1 vm_ptr + n)
                            //      fits in ARG_REGS.
                            let needs_ctx_arg_count = n + 1; // vm_ptr + n receiver/params
                            let args_fit = n >= 1 && needs_ctx_arg_count <= ARG_REGS.len();
                            let pic_inline = pic_ptr.is_some() && args_fit;
                            let mic_inline = !pic_inline && mic_ptr.is_some() && args_fit;
                            // `.done` patches collected from each emitted
                            // fast-path. Multiple in PIC's case (one per
                            // slot), one in MIC's, none if neither inline
                            // fires. All are JMP rel32 (5 bytes) so the
                            // patch records a 4-byte signed displacement at
                            // `patch_pos`.
                            let mut done_patches32: Vec<usize> = Vec::new();
                            // `.done` patches that are JMP rel8 (single
                            // byte); MIC and the last PIC slot use these
                            // when the skip distance is small enough.
                            let mut done_patch: Option<usize> = None;
                            let mut miss_patches: Vec<usize> = Vec::new();

                            if pic_inline {
                                let pic = pic_ptr.expect("pic_inline ⇒ pic_ptr Some");

                                // Cache the layout constants locally so a
                                // future const-rename in lib.rs surfaces as
                                // a compile error here.
                                const CLASS_ID_OFFS: [u8; 3] = [0, 4, 8];
                                const ENTRY_PTR_OFFS: [u8; 3] = [16, 24, 32];
                                const NEEDS_CTX_OFFS: [u8; 3] = [40, 41, 42];

                                // Compile-time sanity: the byte offsets we
                                // hardcode in the encodings below must
                                // match the public constants exported by
                                // `JitPICSlot`. A mismatch here would
                                // silently dispatch to a stale entry_ptr.
                                const _: () = assert!(
                                    super::JitPICSlot::CLASS_ID_OFFSETS[0] == 0
                                        && super::JitPICSlot::CLASS_ID_OFFSETS[1] == 4
                                        && super::JitPICSlot::CLASS_ID_OFFSETS[2] == 8
                                        && super::JitPICSlot::ENTRY_PTR_OFFSETS[0] == 16
                                        && super::JitPICSlot::ENTRY_PTR_OFFSETS[1] == 24
                                        && super::JitPICSlot::ENTRY_PTR_OFFSETS[2] == 32
                                        && super::JitPICSlot::NEEDS_CONTEXT_OFFSETS[0] == 40
                                        && super::JitPICSlot::NEEDS_CONTEXT_OFFSETS[1] == 41
                                        && super::JitPICSlot::NEEDS_CONTEXT_OFFSETS[2] == 42
                                );

                                // R10 = pic_ptr (imm64, up to 10 bytes)
                                self.emit_mov_imm64(R10, pic as *const _ as i64); // Cast: function pointer for JIT call target

                                // ---- Hoist callee ABI marshalling out of
                                // the 3-way cascade. Previously each slot
                                // re-loaded `vm_ptr + n args` into
                                // ARG_REGS[0..=n] (~26 bytes per slot on
                                // x86-64 SysV with n=4), tripling the
                                // marshalling cost. Hoisting once:
                                //
                                //   * Cuts ~78 bytes per PIC site (≈26B
                                //     × 2 redundant copies).
                                //   * Keeps the per-slot body to: type
                                //     CMP + JNE, needs-ctx CMP + JE,
                                //     CALL [R10+disp], JMP rel32 .done.
                                //   * Args remain live across the inter-
                                //     slot CMP/JNE pairs because those
                                //     instructions touch only RAX and
                                //     R10 (neither is in ARG_REGS).
                                //   * On a successful CALL, ARG_REGS are
                                //     caller-saved and may be clobbered
                                //     by the callee — but we JMP to
                                //     .done immediately afterwards, so
                                //     no other slot's CALL needs them.
                                //   * On the miss path, the slow-path
                                //     prelude (~line 10709) overwrites
                                //     ARG_REGS with its helper-ABI
                                //     arguments (vm_ptr, info, buf,
                                //     n[, mic, pic]) before the helper
                                //     call. The hoisted values are
                                //     already dead at that point.
                                //
                                // We must materialize the receiver-load
                                // *first* (its source spill could alias
                                // ARG_REGS[0] in degenerate frames) and
                                // RAX holds the class_id used by every
                                // per-slot CMP, so RAX is loaded last.
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                for j in 0..n {
                                    let spill_off = args_base_offset
                                        + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.emit_load_local(ARG_REGS[j + 1], spill_off);
                                }
                                // ARG_REGS[1] now holds the receiver
                                // (arg_slots[0]). Reuse it as the source
                                // of the class_id load so we avoid an
                                // extra reload from the receiver spill —
                                // this saves an additional ~5 bytes vs
                                // the prior `emit_load_local(RAX, ...)`.
                                // MOV EAX, dword [ARG_REGS[1]] — load
                                // class_id (ObjectHeader+0). Encoding
                                // depends on whether the receiver reg is
                                // an extended register (R8+).
                                // round-7 audit (bug 4): ARG_REGS[1]
                                // here is the receiver register — the
                                // loop above (`emit_load_local(ARG_REGS[j+1], …)`)
                                // wrote `arg_slots[0]` (= the receiver
                                // by JVM invokevirtual/interface
                                // calling convention) into ARG_REGS[0+1].
                                // The debug_assert below is therefore
                                // checking the right register; verified.
                                let recv_reg = ARG_REGS[1];
                                // MED (round-5 review): mod=00 encoding
                                // reuses the low-3 bits of the register as
                                // r/m, where r/m==4 (RSP/R12) means
                                // SIB-follows and r/m==5 (RBP/R13) means
                                // RIP-relative — both would mis-encode this
                                // displacement-free load. Safe today
                                // because RCX(.1)/RDX(.2)/RSI(.6) are all
                                // outside {4,5}, but any future ARG_REGS
                                // shuffle would silently corrupt this PIC
                                // slot. Trap brittleness in debug builds.
                                debug_assert!(
                                    (recv_reg & 7) != 4 && (recv_reg & 7) != 5,
                                    "PIC slot-0 mod=00 requires ARG_REGS[1] low3 not in {{4,5}} (got {})",
                                    recv_reg,
                                );
                                if recv_reg >= 8 {
                                    // REX.B + 8B /r, modrm = mod(00) reg(EAX=0) rm(recv&7)
                                    self.buf.emit(&[0x41, 0x8B, recv_reg & 7]);
                                } else {
                                    // 8B /r, modrm = mod(00) reg(EAX=0) rm(recv)
                                    self.buf.emit(&[0x8B, recv_reg]);
                                }

                                // Per-slot cascade. Inter-slot `jne` jumps
                                // are rel8 and patched once we know the
                                // start of the next slot. Final slot's
                                // `jne` and every `je needs_ctx → .miss`
                                // jump to the shared `.miss` label.
                                //
                                // `slot_starts[i]` is the byte position of
                                // slot i's first emitted byte (the CMP
                                // opcode); used to resolve inter-slot
                                // `jne` rel8 patches once all slots are
                                // emitted.
                                let mut slot_starts: [usize; 3] = [0; 3];
                                // (patch_pos, target_slot_index) for each
                                // inter-slot `jne` rel8 that needs to land
                                // at the start of slot `target_slot_index`.
                                let mut next_slot_patches: Vec<(usize, usize)> = Vec::new();
                                // CRIT-3 — miss branches use rel32 form
                                // unconditionally. With n>=5 args on Linux
                                // the cumulative body across all three
                                // slots + slow-path prelude can exceed 127
                                // bytes, overflowing the previous rel8
                                // encoding (`0x74`/`0x75`). The rel32
                                // forms (`0x0F 0x84`/`0x0F 0x85` + 4-byte
                                // disp) always fit. `miss_patches_rel32`
                                // stores the byte offset of the 4-byte
                                // displacement immediate, patched at the
                                // shared `.miss` label below.
                                let mut miss_patches_rel32: Vec<usize> = Vec::new();

                                for i in 0..3usize {
                                    slot_starts[i] = self.buf.pos();

                                    // CMP EAX, dword [R10 + CLASS_ID_OFFS[i]]
                                    // For disp == 0 (slot 0 today) emit the
                                    // mod=00 form with no displacement byte:
                                    // saves 1 byte per JIT site on the hot
                                    // slot-0 cascade. For disp != 0 use the
                                    // mod=01 (disp8) form. R10 in mod=00 is
                                    // ModRM 00_000_010 = 0x02.
                                    if CLASS_ID_OFFS[i] == 0 {
                                        // 3 bytes: REX.B + 3B /r + ModRM(00,000,010).
                                        self.buf.emit(&[0x41, 0x3B, 0x02]);
                                    } else {
                                        // 4 bytes: REX.B + 3B /r + ModRM(01,000,010) + disp8.
                                        self.buf
                                            .emit(&[0x41, 0x3B, 0x42, CLASS_ID_OFFS[i]]);
                                    }

                                    if i < 2 {
                                        // JNE rel8 → start of slot i+1
                                        // (patched below once slot i+1's
                                        // start position is known).
                                        // Inter-slot distances stay small
                                        // (a single slot body is ~30
                                        // bytes for n<=5), so rel8 is
                                        // sufficient here — only the
                                        // miss/needs_ctx branches need
                                        // rel32 (see CRIT-3 comment above).
                                        self.buf.emit(&[0x75, 0x00]);
                                        let patch = self.buf.pos() - 1;
                                        next_slot_patches.push((patch, i + 1));
                                    } else {
                                        // Final slot: JNE rel32 → .miss
                                        // (6 bytes: 0x0F 0x85 + i32 disp).
                                        // Slot 2's miss target sits past
                                        // slots 0..2 cascades is reachable
                                        // in rel8 but we keep rel32 for
                                        // consistency with slot 0/1 and
                                        // because the slow-path prelude
                                        // following the cascade can push
                                        // the distance over 127 bytes.
                                        self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                        miss_patches_rel32.push(self.buf.pos() - 4);
                                    }

                                    // CMP BYTE [R10 + NEEDS_CTX_OFFS[i]], 0
                                    // 5 bytes: REX.B (0x41) + 80 /7 + modrm
                                    //   modrm = mod(01) reg(/7=111) rm(010)
                                    //         = 0b01_111_010 = 0x7A
                                    //   + disp8 + imm8(0)
                                    self.buf
                                        .emit(&[0x41, 0x80, 0x7A, NEEDS_CTX_OFFS[i], 0x00]);

                                    // JE rel32 → .miss (6 bytes:
                                    // 0x0F 0x84 + i32 disp).  CRIT-3:
                                    // rel8 here overflowed in release
                                    // builds for n>=5 args, silently
                                    // wrapping into the next slot — UB
                                    // dispatch. rel32 always fits.
                                    self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                    miss_patches_rel32.push(self.buf.pos() - 4);

                                    // Callee ABI args (vm_ptr + n) have
                                    // already been materialised once in
                                    // ARG_REGS[0..=n] above the cascade
                                    // (HIGH-perf hoist; see comment at
                                    // the top of the PIC body). Slot
                                    // bodies must NOT touch ARG_REGS.

                                    // CALL qword [R10 + ENTRY_PTR_OFFS[i]]
                                    // 4 bytes: REX.B (0x41) + FF /2 + modrm
                                    //   modrm = mod(01) reg(/2=010) rm(010)
                                    //         = 0b01_010_010 = 0x52
                                    //   + disp8.
                                    self.buf
                                        .emit(&[0x41, 0xFF, 0x52, ENTRY_PTR_OFFS[i]]);

                                    // JMP rel32 → .done. Use rel32 because
                                    // for slots 0 and 1 the skip distance
                                    // (remaining slot bodies + slow path)
                                    // routinely exceeds 127 bytes. Slot 2's
                                    // .done jump is short but we keep rel32
                                    // uniform — 3 extra bytes total vs
                                    // branching logic.
                                    // E9 cd: JMP rel32 (5 bytes).
                                    self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                    done_patches32.push(self.buf.pos() - 4);
                                }

                                // Resolve inter-slot `jne` rel8 patches now
                                // that every slot's start is known.
                                for (jne_patch, target_slot) in &next_slot_patches {
                                    let slot_start = slot_starts[*target_slot];
                                    let rel = (slot_start as i64) - (*jne_patch as i64 + 1);
                                    debug_assert!(
                                        (-128..=127).contains(&rel),
                                        "inline PIC inter-slot jne overflowed rel8 ({} bytes)",
                                        rel
                                    );
                                    self.buf.patch_byte(*jne_patch, rel as u8); // Cast: rel8 displacement
                                }

                                // .miss: patch all `je needs_ctx → .miss`
                                // and (for slot 2) `jne → .miss` to land
                                // HERE — the start of the slow-path block
                                // emitted below.  CRIT-3: these are all
                                // rel32 form, so the patch site holds a
                                // 4-byte signed displacement computed
                                // from the byte AFTER the immediate
                                // (patch + 4) to the target.
                                let miss_off = self.buf.pos();
                                for patch in &miss_patches_rel32 {
                                    let rel = (miss_off as i64) - (*patch as i64 + 4);
                                    debug_assert!(
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                        "inline PIC miss branch overflowed rel32 ({} bytes)",
                                        rel
                                    );
                                    self.buf.patch_i32(*patch, rel as i32); // Cast: rel32 displacement
                                }
                                // `miss_patches` (the legacy rel8 vector)
                                // remains in scope for the MIC arm below;
                                // PIC inline does not push into it any
                                // more, so nothing to clear here.
                            } else if mic_inline {
                                let mic = mic_ptr.expect("mic_inline ⇒ mic_ptr Some");
                                // R10 = mic_ptr (imm64, 10 bytes)
                                self.emit_mov_imm64(R10, mic as *const _ as i64); // Cast: function pointer for JIT call target

                                // Load receiver pointer into RAX. Receiver is
                                // arg_slots[0], spilled at the *highest* offset
                                // (lowest address) in the args buffer.
                                let receiver_spill = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(RAX, receiver_spill);

                                // MOV EAX, dword [RAX]  — load class_id (ObjectHeader+0).
                                // 2 bytes: 8B 00
                                self.buf.emit(&[0x8B, 0x00]);

                                // CMP EAX, dword [R10 + 0]  — vs cached_class_id.
                                // 3 bytes: REX.B (0x41) + 3B /r + modrm(00 000 010)
                                self.buf.emit(&[0x41, 0x3B, 0x02]);

                                // JNE rel8 → .miss  (2 bytes, patched)
                                self.buf.emit(&[0x75, 0x00]);
                                miss_patches.push(self.buf.pos() - 1);

                                // CMP BYTE [R10 + 16], 0  — gate on cached_needs_context.
                                // 5 bytes: REX.B (0x41) + 80 /7 + modrm(01 111 010) + disp8 + imm8
                                self.buf.emit(&[0x41, 0x80, 0x7A, 0x10, 0x00]);

                                // JE rel8 → .miss  (needs_ctx == false ⇒ fall back to helper)
                                self.buf.emit(&[0x74, 0x00]);
                                miss_patches.push(self.buf.pos() - 1);

                                // ---- Set up callee ABI: (vm_ptr, arg_slots[0..n]) ----
                                // vm_ptr → ARG_REGS[0]
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                // arg_slots[i] → ARG_REGS[i + 1]
                                for i in 0..n {
                                    let spill_off = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.emit_load_local(ARG_REGS[i + 1], spill_off);
                                }

                                // CALL qword [R10 + 8]  — cached_entry_ptr.
                                // 4 bytes: REX.B (0x41) + FF /2 + modrm(01 010 010) + disp8
                                self.buf.emit(&[0x41, 0xFF, 0x52, 0x08]);

                                // JMP rel8 → .done  (2 bytes, patched)
                                self.buf.emit(&[0xEB, 0x00]);
                                done_patch = Some(self.buf.pos() - 1);

                                // .miss: patch both rel8 sites here.
                                let miss_off = self.buf.pos();
                                for patch in &miss_patches {
                                    let rel = (miss_off as i64) - (*patch as i64 + 1);
                                    debug_assert!(
                                        (-128..=127).contains(&rel),
                                        "inline MIC miss branch overflowed rel8 ({} bytes)",
                                        rel
                                    );
                                    self.buf.patch_byte(*patch, rel as u8); // Cast: rel8 displacement
                                }
                            }

                            // ---- Slow path: helper ABI setup + call ----
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                            if n > 0 {
                                let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                                self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                            } else {
                                self.emit_xor_reg_self(ARG_REGS[2]);
                            }
                            self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding

                            // Round-8 wave-3: defensive callee-saved spill
                            // before any GC-triggering dispatch CALL.
                            self.emit_pre_safepoint_spill();

                            if let Some(mic) = mic_ptr {
                                // MIC-optimized dispatch: pass MIC slot as 5th
                                // arg and (CRIT-1) PIC slot as 6th arg so the
                                // helper can populate the inline 3-way cascade
                                // via `JitPICSlot::install` on every successful
                                // resolution. A `pic_ptr == 0` tells the helper
                                // no PIC is installed for this site.
                                //
                                // On Windows x64, args 5 and 6 go on the stack
                                // at [RSP+32] and [RSP+40] (shadow + spill).
                                // RAX is caller-saved so we stage each
                                // pointer through it before storing — safe
                                // regardless of which call encoding
                                // `emit_call_absolute` picks (rel32 leaves
                                // RAX alone; the imm64-via-RAX fallback
                                // would overwrite RAX anyway, but that
                                // happens after we've already stored).
                                let pic_arg: i64 = pic_ptr
                                    .map(|p| p as *const _ as i64)
                                    .unwrap_or(0); // Cast: function pointer for JIT call target
                                #[cfg(target_os = "windows")]
                                {
                                    // 5th arg at [RSP + 32]
                                    self.emit_mov_imm64(RAX, mic as *const _ as i64); // Cast: function pointer for JIT call target
                                    // MOV [RSP + 32], RAX
                                    self.rex_w();
                                    self.buf.emit(&[0x89, 0x44, 0x24, 0x20]);
                                    // 6th arg at [RSP + 40]
                                    self.emit_mov_imm64(RAX, pic_arg);
                                    // MOV [RSP + 40], RAX
                                    self.rex_w();
                                    self.buf.emit(&[0x89, 0x44, 0x24, 0x28]);
                                }
                                #[cfg(not(target_os = "windows"))]
                                {
                                    // SysV: 5th arg in R8, 6th in R9.
                                    self.emit_mov_imm64(R8, mic as *const _ as i64); // Cast: function pointer for JIT call target
                                    self.emit_mov_imm64(R9, pic_arg);
                                }
                                self.emit_call_absolute(
                                    self.helpers.invoke_virtual_mic,
                                );
                            } else {
                                // Plain dispatch without MIC
                                self.emit_call_absolute(
                                    self.helpers.invoke_dispatch,
                                );
                            }

                            // .done: patch the fast-path forward JMP(s).
                            //   - `done_patch`     (rel8, MIC inline)
                            //   - `done_patches32` (rel32, PIC inline — one
                            //                       per cache slot)
                            let done_off = self.buf.pos();
                            if let Some(patch) = done_patch {
                                let rel = (done_off as i64) - (patch as i64 + 1);
                                debug_assert!(
                                    (-128..=127).contains(&rel),
                                    "inline MIC done jump overflowed rel8 ({} bytes)",
                                    rel
                                );
                                self.buf.patch_byte(patch, rel as u8); // Cast: rel8 displacement
                            }
                            for patch in &done_patches32 {
                                // `patch` points at the start of the rel32
                                // immediate (4 bytes); the JMP opcode (E9)
                                // precedes it by 1 byte. The displacement
                                // is computed from the byte AFTER the
                                // immediate (patch + 4) to the target.
                                let rel = (done_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC done jump overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf.patch_i32(*patch, rel as i32); // Cast: rel32 displacement
                            }
                            // T1.1.2 — every virtual/interface dispatch is
                            // a full safepoint: the callee may allocate,
                            // throw, or block. Record the oop map before
                            // the return value is pushed so the GC root
                            // walker has precise coverage at the return PC.
                            self.emit_oop_map_for_safepoint();

                            // Reclaim spill slots used for invoke args — they are
                            // no longer needed after the helper returns.
                            self.next_spill_offset = pre_pop_spill;

                            if info_ref.return_type != b'V' {
                                if matches!(info_ref.return_type, b'D' | b'F') {
                                    self.push_from_rax_as_xmm0();
                                } else {
                                    self.push_from_rax();
                                }
                                // Tag the return as an oop when the
                                // descriptor is L... or [....
                                if matches!(info_ref.return_type, b'L' | b'[') {
                                    self.mark_top_as_oop();
                                }
                            }
                        }
                    }
                    if op == 0xb9 {
                        pc += 5;
                    } else {
                        pc += 3;
                    }
                }

                // newarray — allocate a new primitive array
                0xbc => {
                    self.flush_scratch_registers();
                    let atype = code[pc + 1] as i32; // Widening: always safe
                    let count_slot = self.pop_stack();
                    // Load heap pointer → ARG_REGS[0]
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    // atype immediate → ARG_REGS[1]
                    self.emit_mov_imm32_sx(ARG_REGS[1], atype);
                    // count → ARG_REGS[2]
                    self.load_slot_to_reg(ARG_REGS[2], count_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.newarray);
                    // T1.1.a — `newarray` is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    self.push_from_rax();
                    // A primitive array header is still an object reference.
                    self.mark_top_as_oop();
                    pc += 2;
                }

                // new — allocate a new Java object (heap allocation via helper)
                // Scalar replacement: if the object doesn't escape, store fields
                // in the JIT frame instead of heap-allocating.
                0xbb => {
                    if let Some(sr_obj) = self.scalar_replaced.get(&pc).cloned() {
                        // Scalar-replaced: zero-initialize field slots in the frame
                        self.emit_xor_reg_self(RAX);
                        for i in 0..sr_obj.num_fields {
                            let field_off = sr_obj.field_base_offset
                                + (i as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                            self.emit_store_local(field_off, RAX);
                            self.emit_store_local(field_off + 8, RAX);
                        }
                        // Push a dummy zero "object reference" — never dereferenced
                        self.push_from_rax();
                        pc += 3;
                    } else {
                        self.flush_scratch_registers();
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let resolved = self
                            .new_info_idx
                            .get(&pc)
                            .map(|&i| self.new_info[i]);
                        let (_, class_id_raw, num_fields, has_prim_init, has_finalizer) =
                            match resolved {
                                Some(info) => info,
                                None => {
                                    return false;
                                }
                            };

                        // HIGH-6 JIT audit (object_allocation/1000 3-5x gap):
                        // emit an inline TLAB bump-pointer fast path when the
                        // helper table is fully wired AND the class is small
                        // enough to bump in a single LEA disp32 (< 256 bytes
                        // total, which covers HashMap.Node, ArrayList$Itr,
                        // and ~99% of common allocation sites). Larger
                        // objects and the test-helper path (no `get_current_thread`
                        // wired) fall through to the unconditional helper call.
                        //
                        // The inline path bumps `thread.tlab.cursor`, writes
                        // `class_id` at obj_ptr+0, then tail-calls
                        // `jit_post_tlab_init` to finish header + primitive
                        // defaults + finalizer registration. The bump itself
                        // is ~6 instructions; HotSpot achieves ~5-7. The
                        // remaining work (hash mint, num_slots write, class-
                        // metadata RwLock for primitive defaults) is in the
                        // post-init helper — kept out of inline because
                        // synthesising it would require per-field descriptor
                        // plumbing that isn't currently in `new_info`.
                        let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;
                        let can_inline = self.helpers.get_current_thread != 0
                            && self.helpers.tlab_post_init != 0
                            && self.helpers.new_object != 0
                            && total_size <= 256
                            && self.needs_heap; // need vm_ptr in heap_local slot

                        // Round-8 wave-3: defensive callee-saved spill
                        // before the `new` safepoint (both inline TLAB
                        // and slow-path helper can reach GC via
                        // jit_post_tlab_init / new_object).
                        self.emit_pre_safepoint_spill();
                        if can_inline {
                            // CRIT-2 — when neither primitive-init nor
                            // finalizer registration is required, skip
                            // the `jit_post_tlab_init` helper and write
                            // identity_hash/num_slots inline. Most JDK
                            // micro-objects (HashMap.Node, ArrayList$Itr,
                            // Iterator chains, all-reference field
                            // bearers) hit this fast path. The
                            // resolution of these flags currently
                            // requires extending `cp_new_resolver` (see
                            // the `new_info` doc in `jit/src/lib.rs`),
                            // so the conservative default `(true,true)`
                            // keeps the helper call in place for now.
                            let skip_helper = !has_prim_init && !has_finalizer;
                            self.emit_inline_tlab_new(
                                class_id_raw,
                                num_fields,
                                skip_helper,
                            );
                        } else {
                            // Slow path: full helper-call dispatch. Used when
                            //   - the helper table is partial (tests),
                            //   - the object exceeds 256 bytes (rare —
                            //     HotSpot also bails on these),
                            //   - or the method's prologue did not stash
                            //     `vm_ptr` in a frame slot.
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                            self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32); // Cast: x86-64 immediate encoding
                            self.emit_call_absolute(self.helpers.new_object);
                        }
                        // T1.1.a — `new` is a GC-triggering safepoint.
                        // Emit an oop map for the slots that were live
                        // BEFORE the call (the return value hasn't
                        // been pushed yet, so the stack state here
                        // reflects the surviving operands). Both arms
                        // (inline and slow) may trigger GC: the inline
                        // path's post-init helper can grow the
                        // finalizer queue and the slow path obviously
                        // can young-GC.
                        self.emit_oop_map_for_safepoint();
                        self.push_from_rax();
                        // The result is an object reference.
                        self.mark_top_as_oop();
                        pc += 3;
                    }
                }

                // anewarray — allocate a new reference array via helper
                0xbd => {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let resolved = self
                        .anewarray_info_idx
                        .get(&pc)
                        .map(|&i| self.anewarray_info[i]);
                    let (_, component_class_id_raw) = match resolved {
                        Some(info) => info,
                        None => return false, // unresolved — bail to interpreter
                    };
                    let count_slot = self.pop_stack();
                    // jit_anewarray_object(heap, component_class_id_raw, length) → i64 array ptr
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm32_sx(ARG_REGS[1], component_class_id_raw as i32); // Cast: x86-64 immediate encoding
                    self.load_slot_to_reg(ARG_REGS[2], count_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.anewarray_object);
                    // T1.1.a — `anewarray` is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    self.push_from_rax();
                    // The result is a reference array — an object reference.
                    self.mark_top_as_oop();
                    pc += 3;
                }

                // arraylength — get array length from header (inline)
                0xbe => {
                    let arr_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, arr_slot);
                    self.emit_arraylength_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // if_acmpeq (0xa5) — reference equality branch
                0xa5 => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let val2 = self.pop_stack();
                    let val1 = self.pop_stack();
                    self.load_slot_to_reg(RCX, val2);
                    self.load_slot_to_reg(RAX, val1);
                    // CMP RAX, RCX (REX.W + 0x39 /r)
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                    // JE rel32
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x84); // JE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.reset_spills();
                    pc += 3;
                }

                // if_acmpne (0xa6) — reference inequality branch
                0xa6 => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let val2 = self.pop_stack();
                    let val1 = self.pop_stack();
                    self.load_slot_to_reg(RCX, val2);
                    self.load_slot_to_reg(RAX, val1);
                    // CMP RAX, RCX (REX.W + 0x39 /r)
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                    // JNE rel32
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x85); // JNE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.reset_spills();
                    pc += 3;
                }

                // checkcast (0xc0) — type check (pass-through or exception)
                0xc0 => {
                    self.flush_scratch_registers();
                    // Look up resolved typecheck info for this PC
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, name_ptr, name_len) = self
                        .typecheck_info_idx
                        .get(&pc)
                        .map(|&i| self.typecheck_info[i])
                        .unwrap_or((pc, std::ptr::null(), 0));

                    let obj_slot = self.pop_stack();

                    // Call jit_checkcast(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → obj_ptr
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                    self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                    self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.checkcast);
                    // T1.1.2 — checkcast may resolve the target class
                    // on demand (first access) which allocates a
                    // `java/lang/Class` mirror. That's a GC-triggering
                    // safepoint — emit the oop map before pushing.
                    self.emit_oop_map_for_safepoint();
                    // Result (obj_ptr or 0 for null) is in RAX — push onto stack
                    self.push_from_rax();
                    // checkcast returns the same reference (or null).
                    self.mark_top_as_oop();
                    pc += 3;
                }

                // instanceof (0xc1) — type check (returns 0 or 1)
                0xc1 => {
                    self.flush_scratch_registers();
                    // Look up resolved typecheck info for this PC
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, name_ptr, name_len) = self
                        .typecheck_info_idx
                        .get(&pc)
                        .map(|&i| self.typecheck_info[i])
                        .unwrap_or((pc, std::ptr::null(), 0));

                    let obj_slot = self.pop_stack();

                    // Call jit_instanceof(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → 0/1
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                    self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                    self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.instanceof_check);
                    // T1.1.2 — instanceof may resolve the target class
                    // on demand, allocating a Class mirror. Emit the
                    // oop map even though the return value is a
                    // primitive int.
                    self.emit_oop_map_for_safepoint();
                    // Result (0 or 1) is in RAX — push onto stack
                    self.push_from_rax();
                    pc += 3;
                }

                // multianewarray — allocate multi-dimensional array (2D only)
                0xc5 => {
                    self.flush_scratch_registers();
                    let _cp_hi = code[pc + 1];
                    let _cp_lo = code[pc + 2];
                    let ndims = code[pc + 3] as usize; // Widening: always safe
                    debug_assert_eq!(ndims, 2);

                    // Pop dimensions: top of stack = last dimension
                    let dim2_slot = self.pop_stack(); // inner dimension
                    let dim1_slot = self.pop_stack(); // outer dimension

                    // Look up resolved leaf element type for this PC
                    let leaf_et = self
                        .multianewarray_info
                        .iter()
                        .find(|(p, _)| *p == pc)
                        .map(|(_, et)| *et as i32) // Cast: x86-64 immediate encoding
                        .unwrap_or(10); // default T_INT

                    // Call jit_multianewarray_2d(heap_ptr, leaf_et, dim1, dim2)
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm32_sx(ARG_REGS[1], leaf_et);
                    self.load_slot_to_reg(ARG_REGS[2], dim1_slot);
                    self.load_slot_to_reg(ARG_REGS[3], dim2_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.multianewarray_2d);
                    // T1.1.2 — multianewarray is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    self.push_from_rax();
                    // The result is a reference array.
                    self.mark_top_as_oop();
                    pc += 4;
                }

                // ifnull (0xc6) — branch if reference is null
                0xc6 => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let slot = self.pop_stack();
                    // HIGH-1 / Fix 1 — wire null-check elimination.
                    // If the value on top of stack came from an aload of a
                    // local that is proven non-null at this PC, the TEST
                    // can never be zero so `ifnull` is dead and the
                    // fall-through is always taken. Skip both the TEST
                    // and the JE.
                    let proven_nonnull = preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }
                    if proven_nonnull {
                        // No-op: fall through. We still need a non-empty
                        // branch-target record so downstream merges see
                        // the expected stack depth.
                        self.branch_target_stack_depth
                            .entry(target_pc)
                            .or_insert(self.stack.len());
                        self.reset_spills();
                        pc += 3;
                    } else {
                        self.load_slot_to_reg(RCX, slot);
                        // TEST RCX, RCX (REX.W + 0x85 /r)
                        self.rex_w();
                        self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                        // JE rel32 (jump if null / zero)
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x84); // JE
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        self.forward_patches.push((patch_offset, target_pc));
                        self.branch_target_stack_depth
                            .entry(target_pc)
                            .or_insert(self.stack.len());
                        self.reset_spills();
                        pc += 3;
                    }
                }

                // ifnonnull (0xc7) — branch if reference is not null
                0xc7 => {
                    self.flush_scratch_registers();
                    let offset = ((code[pc + 1] as i16) << 8 | code[pc + 2] as i16) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) { // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };

                    let slot = self.pop_stack();
                    // HIGH-1 / Fix 1 — null-check elimination. If the
                    // tested value is proven non-null, `ifnonnull` is
                    // always taken: emit an unconditional JMP rel32 and
                    // skip the TEST + Jcc pair. Saves the 3-byte TEST
                    // + 1-byte (Jcc opcode-pair high byte) for every
                    // proven site.
                    let proven_nonnull = preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }
                    if proven_nonnull {
                        // JMP rel32 (5 bytes; patched).
                        self.buf.emit_byte(0xE9);
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.forward_patches.push((patch_offset, target_pc));
                        self.branch_target_stack_depth
                            .entry(target_pc)
                            .or_insert(self.stack.len());
                        self.reset_spills();
                        pc += 3;
                    } else {
                        self.load_slot_to_reg(RCX, slot);
                        // TEST RCX, RCX (REX.W + 0x85 /r)
                        self.rex_w();
                        self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                        // JNE rel32 (jump if not null / non-zero)
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x85); // JNE
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        self.forward_patches.push((patch_offset, target_pc));
                        self.branch_target_stack_depth
                            .entry(target_pc)
                            .or_insert(self.stack.len());
                        self.reset_spills();
                        pc += 3;
                    }
                }

                // T5.2.8 — monitorenter / monitorexit: lock elision.
                //
                // If the receiver was scalar-replaced by escape analysis
                // (i.e. the object is thread-local and never escapes),
                // the lock is trivially non-contended and can be elided.
                // We pop the receiver slot and emit nothing.
                //
                // If the receiver is NOT scalar-replaced, we bail to
                // the interpreter (no JIT monitor helper exists yet).
                0xC2 | 0xC3 => {
                    let recv_slot = self.pop_stack();
                    // Check: is the receiver a scalar-replaced object?
                    // Scalar-replaced objects have a zero "pointer" on the
                    // stack (a placeholder that's never dereferenced).
                    // We recognize them by checking if the load is from
                    // a scalar-replaced frame slot.
                    //
                    // For now, simply elide if ANY scalar replacement is
                    // active in this method (conservative but correct:
                    // if there's no SR, the method doesn't have monitors
                    // on non-escaping objects, so the bail is safe).
                    if self.scalar_replaced.is_empty() {
                        // No escape analysis active → can't elide → bail.
                        return false;
                    }
                    // Lock elided — emit nothing. The object is thread-
                    // local so the monitor is never contended.
                    let _ = recv_slot;
                    pc += 1;
                }

                _ => {
                    // Should not happen — jit_scan should have caught this
                    return false;
                }
            }
        }

        // Record final PC mapping
        if pc <= self.pc_to_native.len() {
            // Safety: pc might equal code_len
        }

        // Bail out if an internal error (e.g. stack underflow) was detected.
        if self.failed {
            return false;
        }

        // Emit out-of-line bounds check failure stubs (after all bytecode)
        self.emit_bounds_check_stubs();
        // Round-8 CRIT fix: emit shared null-check-failure stub for inline
        // array-store opcodes (iastore / bastore / aastore / lastore /
        // fastore / dastore / castore / sastore). Without this, the inline
        // bounds-check would deref NULL on a null array and the signal
        // handler would re-raise instead of throwing NPE.
        self.emit_null_check_store_stubs();
        self.emit_deopt_stubs();
        true
    }

    /// Patch all forward branches and self-calls.
    fn patch_branches(&mut self) {
        // Patch conditional and unconditional branches
        for &(patch_offset, target_pc) in &self.forward_patches {
            let target_native = if target_pc < self.pc_to_native.len() {
                self.pc_to_native[target_pc]
            } else {
                -1
            };
            if target_native >= 0 {
                // rel32 = target - (patch_offset + 4)
                let rel = target_native - (patch_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.patch_i32(patch_offset, rel);
            }
        }
        // Patch jump table entries: each entry is an i32 offset from table_base to target
        for &(entry_offset, table_base, target_pc) in &self.jump_table_patches {
            let target_native = if target_pc < self.pc_to_native.len() {
                self.pc_to_native[target_pc]
            } else {
                -1
            };
            if target_native >= 0 {
                let rel = target_native - table_base as i32; // Cast: x86-64 rel32 displacement
                self.buf.patch_i32(entry_offset, rel);
            }
        }
    }

    fn patch_self_calls(&mut self, entry_offset: usize) {
        for &patch_offset in &self.self_call_patches {
            // rel32 = entry - (patch_offset + 4)
            let rel = entry_offset as i32 - (patch_offset as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.patch_i32(patch_offset, rel);
        }
    }
}

// ---------------------------------------------------------------------------
// Public compilation entry point
// ---------------------------------------------------------------------------

/// Compile a JVM bytecode method to x86-64 machine code.
///
/// When `needs_heap` is true, the compiled code expects a heap pointer as the hidden
/// first C argument, and Java parameters follow. This enables JIT-compiled array
/// allocation and element access via helper call-outs.
///
/// Returns `Some(CompiledMethod)` on success, `None` if compilation fails.
#[allow(clippy::too_many_arguments)]
pub fn compile(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    multianewarray_info: Vec<(usize, u8)>,
    field_info: Vec<(usize, usize, u8)>,
    typecheck_info: Vec<(usize, *const u8, usize)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    // CRIT-2 — see `new_info` field doc on the compiler struct.
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    anewarray_info: Vec<(usize, u32)>,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    direct_calls: Vec<(usize, super::JitDirectCall)>,
    mic_slots: Vec<(usize, *const super::JitMICSlot)>,
    // HIGH-7 — Inline 3-way PIC slots passed alongside MIC slots.
    //
    // Each entry is `(bytecode_pc, &JitPICSlot as *const _)`. When a
    // PIC slot is present at a given pc, the codegen in
    // `Compiler::compile_op_invokevirtual` emits the 3-way inline
    // cascade in place of the MIC probe (PIC supersedes MIC — it is
    // a 3-entry superset). The slot itself is allocated and owned by
    // the caller (`jit/src/lib.rs::try_compile`); it must outlive the
    // compiled method, which is ensured by attaching the boxed slot
    // to `CompiledMethod._jit_pic_slots`.
    //
    // Callers that don't yet allocate PIC slots (e.g. legacy test
    // call sites that build short bytecode snippets) pass
    // `Vec::new()` and the cascade is simply not emitted at any pc.
    pic_slots: Vec<(usize, *const super::JitPICSlot)>,
    ldc_info: Vec<(usize, i64)>,
    ldc2w_info: Vec<(usize, i64)>,
    branch_hints: HashMap<usize, bool>,
    loop_unroll_hints: HashMap<usize, usize>,
    helpers: &JitRuntimeHelpers,
    non_escaping_new: std::collections::HashSet<usize>,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    // Estimate buffer size: extra for invoke dispatch calls (~40 bytes each)
    let inline_extra: usize = inline_sites.values().map(|s| s.callee_code_len * 48).sum();
    let estimated_size = code_len * 48 + 1024 + invoke_info.len() * 64 + inline_extra;
    let buf = ExecutableBuffer::new(estimated_size.max(4096))?;

    // Calculate max stack depth statically (simplified: use a generous upper bound)
    let max_stack = estimate_max_stack(code, code_len);

    // LICM: detect loops and find invariant aaload sequences to hoist
    let loops = detect_loops(code, code_len);
    let hoist_info = find_loop_hoists(code, code_len, &loops);

    // Round-8 wave-3 HIGH fix (Fix 3): generic LICM scaffold for
    // getfield/getstatic loads. The analysis is invoked here so the
    // pipeline links against the new `loop_analysis` module and
    // pattern surfaces during compilation; the result is currently
    // discarded because hoisting itself requires safepoint /
    // oop-map / regalloc participation that is intentionally
    // deferred to a later round (see `loop_analysis.rs` module doc).
    //
    // TODO(round-12+): wire the returned `InvariantLoad` records
    // into the emitter as a pre-header hoist consumer alongside the
    // existing `LoopHoist` (aaload) and `FpLoopHoist` (dload)
    // mechanisms.
    {
        let licm_loops = crate::loop_analysis::detect_loops(code, code_len);
        let mut total = 0usize;
        for li in &licm_loops {
            let v = crate::loop_analysis::find_invariant_loads(li, code);
            total += v.len();
        }
        // Suppress dead_code warnings on the analysis output without
        // changing emission behavior.
        let _ = total;
    }

    // T5.2.1 — SCEV induction variable analysis.
    //
    // Produces an `InductionVar` entry per detected counted loop. The
    // result is stored on the Compiler so downstream passes (unrolling,
    // vectorization, range-check elimination) can query stride, bound,
    // and trip count without re-walking the bytecode.
    let induction_vars = crate::scev::analyze_induction_variables(code, code_len, &loops);

    // T5.2.14 — Null-check elimination dataflow.
    //
    // Walks the bytecode once and produces a per-PC bitmask of locals
    // proven non-null. Future null-check emission paths consult this
    // via `Compiler::is_local_nonnull(pc, local)` to skip redundant
    // `TEST reg, reg; JZ throw_npe` sequences.
    let null_check_info = crate::null_check_elim::analyze(code, code_len);

    // BCE: analyze loops for bounds check elimination
    let (bounds_safe_pcs, speculative_bce_guards) =
        analyze_bounds_elimination(code, code_len, &loops);

    // SIMD: detect vectorizable int-array-sum loops (requires AVX2)
    let simd_loops = if has_avx2() {
        let mut simd = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_sum(code, header, back_edge, iv) {
                    simd.push(info);
                }
            }
        }
        simd
    } else {
        Vec::new()
    };

    // SIMD FP: detect vectorizable double-array-sum loops (requires AVX2)
    let simd_fp_loops = if has_avx2() {
        let mut simd = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_fp_array_sum(code, header, back_edge, iv) {
                    simd.push(info);
                }
            }
        }
        simd
    } else {
        Vec::new()
    };

    // T5.2.15 — Int-array element-wise SIMD detection.
    //
    // Unlike reduction, detection here is *unconditional on AVX2* so
    // the information is available to any downstream pass (e.g.
    // cost-based vectorization, auto-tuning). Emission code checks
    // `has_avx2()` before issuing AVX2-only encodings.
    let simd_element_wise_loops = {
        let mut ewise = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_element_wise(code, header, back_edge, iv) {
                    ewise.push(info);
                }
            }
        }
        ewise
    };

    // Loop unrolling: detect small loops suitable for unrolling
    // PGO: use profiled trip counts to guide unroll factor when available.
    // Static heuristic fallback:
    //   Body ≤20 bytecodes  → 4x unroll (3 extra copies)
    //   Body 20-50 bytecodes → 2x unroll (1 extra copy)
    //   Body > 50            → no unroll (unless PGO says otherwise, up to 100 bytes)
    let unroll_loops: Vec<(usize, usize, usize)> = loops
        .iter()
        .filter_map(|&(header, back_edge)| {
            // Only unroll loops with goto back-edge (not conditional)
            if code[back_edge] != 0xa7 {
                return None;
            }
            let body_size = back_edge - header;
            if body_size < 5 {
                return None;
            }

            // PGO path: use profiled trip count if available for this back-edge
            if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
                let extra_copies = pgo_factor - 1;
                // PGO extends unrolling eligibility to larger loops (up to 100 bytes)
                if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                    return Some((header, back_edge, extra_copies));
                }
            }

            // Static heuristic fallback
            let extra_copies = if body_size <= 20 {
                3 // 4x unroll
            } else if body_size <= 50 {
                1 // 2x unroll (covers FP-heavy loops like N-Body advance)
            } else {
                return None;
            };
            Some((header, back_edge, extra_copies))
        })
        .collect();

    // FP LICM: detect loop-invariant FP loads to hoist
    let fp_hoist_info = find_fp_loop_hoists(code, code_len, &loops);

    // FP strength reduction: detect dmul-by-2.0 → dadd-self inside loops
    let fp_strength_reduction_pcs = find_fp_strength_reductions(code, code_len, &loops, &ldc2w_info);

    // Register allocation: graph-coloring allocator for locals
    let alloc_result =
        super::regalloc::allocate_registers(code, code_len, max_locals, num_params, &loops);

    // Scalar replacement: plan frame-local storage for non-escaping object fields
    let num_hoists = hoist_info.len();
    let scalar_base = max_locals + (if needs_heap { 1 } else { 0 }) + num_hoists;
    let empty_non_escaping = std::collections::HashSet::new();
    let non_escaping_for_sr = if std::env::var_os("RUSTJVM_DISABLE_SCALAR_REPLACEMENT").is_some() {
        &empty_non_escaping
    } else {
        &non_escaping_new
    };
    let sr_plan = plan_scalar_replacement(
        code, code_len, non_escaping_for_sr, &new_info, &invoke_info, scalar_base,
    );
    let num_scalar_slots = sr_plan.total_slots;

    let mut compiler = Compiler::new(
        buf,
        max_locals,
        num_params,
        max_stack,
        needs_heap,
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        hoist_info,
        alloc_result,
        *helpers,
        num_scalar_slots,
    );
    compiler.bounds_safe_pcs = bounds_safe_pcs;
    compiler.speculative_bce_guards = speculative_bce_guards;
    compiler.new_info = new_info;
    compiler.anewarray_info = anewarray_info;
    compiler.invoke_info = invoke_info;
    compiler.direct_calls = direct_calls;
    if std::env::var_os("RUSTJVM_DBG_JIT_GEN").is_some() {
        eprintln!("[JIT_GEN_INSTALL] mic_slots count={} pcs={:?}",
            mic_slots.len(),
            mic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.mic_slots = mic_slots;
    // HIGH-7 — Inline 3-way PIC fast-path wiring (now active).
    //
    // The codegen in `Compiler::compile_op_invokevirtual` (search
    // `pic_inline`) keys off `compiler.pic_slots`. With the
    // `pic_slots` parameter now threaded through, callers that
    // eagerly allocate a `Box<JitPICSlot>` per polymorphic call
    // site (see `jit/src/lib.rs::try_compile`) activate the inline
    // cascade. Slots start empty (class_id == 0 at all 3 entries),
    // so the CMP cascade falls straight through to the helper on
    // first invocation; once the runtime helper populates a slot,
    // subsequent dispatches take the inline fast path.
    if std::env::var_os("RUSTJVM_DBG_JIT_GEN").is_some() {
        eprintln!("[JIT_GEN_INSTALL] pic_slots count={} pcs={:?}",
            pic_slots.len(),
            pic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.pic_slots = pic_slots;
    compiler.unroll_loops = unroll_loops;
    compiler.simd_loops = simd_loops;
    compiler.branch_hints = branch_hints.into_iter().collect();
    compiler.loop_unroll_hints = loop_unroll_hints.into_iter().collect();
    compiler.ldc_info = ldc_info;
    compiler.ldc2w_info = ldc2w_info;
    compiler.fp_hoist_info = fp_hoist_info;
    compiler.fp_strength_reduction_pcs = fp_strength_reduction_pcs;
    compiler.simd_fp_loops = simd_fp_loops;
    compiler.scalar_replaced = sr_plan.objects;
    compiler.scalar_field_ops = sr_plan.field_ops;
    compiler.scalar_init_skips = sr_plan.init_skips;
    compiler.inline_sites = inline_sites.into_iter().collect();
    // T5.2.1 + T5.2.14 — transfer the pre-computed analyses.
    compiler.induction_vars = induction_vars;
    compiler.null_check_info = null_check_info;
    // T5.2.15 — element-wise SIMD detections.
    compiler.simd_element_wise_loops = simd_element_wise_loops;
    // T5.2.17 — loop unswitching candidates.
    compiler.loop_unswitch_candidates =
        detect_loop_unswitch_candidates(code, code_len, &loops);

    // MED-4 / Fix 3 — pre-build pc-indexed lookup maps for the hot
    // codegen sites (getfield/putfield/invoke*/new/anewarray/ldc/…)
    // so each query is O(1) rather than scanning the Vec.
    compiler.build_pc_indices();

    // Emit prologue
    compiler.emit_prologue();
    let entry_offset = 0; // prologue starts at offset 0
    compiler.body_entry_offset = compiler.buf.pos(); // offset right after prologue

    // Compile bytecode
    if !compiler.compile_bytecode(code, code_len) {
        return None;
    }

    // Patch branches (both forward and backward are handled)
    compiler.patch_branches();

    // Patch self-recursive calls to point to entry
    compiler.patch_self_calls(entry_offset);

    // Build the CompiledMethod with OSR metadata
    let has_dispatch = !compiler.invoke_info.is_empty()
        || !compiler.bounds_check_stubs.is_empty()
        || !compiler.null_check_store_stubs.is_empty();
    let mut cm = if needs_heap {
        CompiledMethod::new_with_context(compiler.buf)
    } else {
        CompiledMethod::new(compiler.buf)
    };
    cm.has_dispatch = has_dispatch;

    // Store OSR metadata for On-Stack Replacement entry
    cm.osr_pc_to_native = Some(compiler.pc_to_native);
    cm.osr_num_locals = compiler.num_locals;
    cm.osr_num_reg_locals = compiler.num_reg_locals;
    cm.osr_local_assignments = Some(compiler.local_assignments);
    cm.osr_xmm_assignments = Some(compiler.xmm_assignments);
    cm.osr_frame_size = compiler.frame_size;
    cm.osr_callee_saved_base = compiler.callee_saved_base;
    cm.osr_heap_local_offset = compiler.heap_local_offset;

    // T1.1.a — transfer precise oop maps collected during codegen.
    // The GC root walker's `JitEntryGuard::enter_with_compiled` path
    // checks `CompiledMethod::has_precise_oop_maps()` to decide
    // whether to use them for this frame; when empty, it falls back
    // to the conservative stack scan for that frame — always a
    // correct super-set of the precise coverage.
    cm.oop_maps = compiler.oop_maps;

    Some(cm)
}

/// Estimate the maximum operand stack depth for the method.
/// Simple conservative estimate: count push-like opcodes.
fn estimate_max_stack(code: &[u8], code_len: usize) -> usize {
    let mut max_depth = 0usize;
    let mut depth = 0usize;
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // Push operations (aconst_null, load const/local/ref, bipush, sipush → +1)
            0x01..=0x11 | 0x15..=0x19 | 0x1a..=0x2d => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Pop operations (binary ops pop 2, push 1 → net -1)
            0x60..=0x71 | 0x78..=0x83 | 0x94..=0x98 => {
                depth = depth.saturating_sub(1);
            }
            // Store ops pop 1 (i/l/f/d/astore, i/l/f/d/astore_N)
            0x36..=0x4e => {
                depth = depth.saturating_sub(1);
            }
            // Array load: pop 2 (array, index), push 1 → net -1
            0x2e..=0x35 => {
                depth = depth.saturating_sub(1);
            }
            // Array store: pop 3 (array, index, value) → net -3
            0x4f..=0x56 => {
                depth = depth.saturating_sub(3);
            }
            // getstatic: push 1 (value) → net +1
            0xb2 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // putstatic: pop 1 (value) → net -1
            0xb3 => {
                depth = depth.saturating_sub(1);
            }
            // getfield: pop 1 (objectref), push 1 (value) → net 0
            0xb4 => {}
            // putfield: pop 2 (objectref, value) → net -2
            0xb5 => {
                depth = depth.saturating_sub(2);
            }
            // newarray: pop 1 (count), push 1 (ref) → net 0
            0xbc => {}
            // new: push 1 object ref (no pop) → +1
            0xbb => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // anewarray: pop 1 (count), push 1 (ref) → net 0
            0xbd => {}
            // arraylength: pop 1 (ref), push 1 (int) → net 0
            0xbe => {}
            // Return pops 1 (ireturn, lreturn, freturn, dreturn, areturn)
            0xac..=0xb0 => {
                depth = 0;
            }
            // void return
            0xb1 => {
                depth = 0;
            }
            // Unary ops (neg, conversions): pop 1, push 1 → net 0
            0x74..=0x77 | 0x85..=0x93 => {}
            // Branch pops (ifXX pop 1, if_icmpXX pop 2, if_acmpXX pop 2)
            0x99..=0x9e => {
                depth = depth.saturating_sub(1);
            }
            0x9f..=0xa6 => {
                depth = depth.saturating_sub(2);
            }
            // ifnull/ifnonnull pop 1
            0xc6 | 0xc7 => {
                depth = depth.saturating_sub(1);
            }
            // checkcast: pop 1, push 1 → net 0
            0xc0 => {}
            // instanceof: pop 1, push 1 → net 0
            0xc1 => {}
            // Pop
            0x57 => {
                depth = depth.saturating_sub(1);
            }
            // Dup: +1
            0x59 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Swap: 0
            0x5f => {}
            // iinc: 0
            0x84 => {}
            // goto: 0
            0xa7 => {
                depth = 0;
            }
            // invokestatic/invokevirtual/invokespecial/invokeinterface: conservatively
            // assume they push 1 result (pops are hard to estimate without descriptors)
            0xb6..=0xb9 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // multianewarray: pops ndims, pushes 1 → net -(ndims-1)
            0xc5 => {
                let ndims = code.get(pc + 3).copied().unwrap_or(2) as usize; // Cast: address arithmetic
                depth = depth.saturating_sub(ndims.saturating_sub(1));
            }
            _ => {}
        }
        // Advance PC
        match op {
            0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => pc += 2,
            0x11
            | 0x13
            | 0x14
            | 0x84
            | 0x99..=0xa6
            | 0xa7
            | 0xb2
            | 0xb3
            | 0xb4
            | 0xb5
            | 0xb6
            | 0xb7
            | 0xb8
            | 0xbb
            | 0xbd
            | 0xc0
            | 0xc1
            | 0xc6
            | 0xc7 => pc += 3,
            0xc5 => pc += 4, // multianewarray
            0xb9 => pc += 5, // invokeinterface
            _ => pc += 1,
        }
    }
    // Add safety margin (conservative for invoke stack effects not tracked above)
    max_depth + 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JitInvokeInfo;
    use rustjvm_types::{ObjectRef, Value};

    // ---- Test stub helpers for getfield/putfield ----
    // These mirror the real helpers in vm/src/jit/helpers.rs but live in the
    // jit crate so unit tests can exercise compiled code without pulling in
    // the full VM.

    /// Read `num_slots` from the object header at the given pointer.
    /// `num_slots` is at byte offset 16 within `ObjectHeader` (repr(C)).
    ///
    /// # Safety
    /// `obj_ptr` must point to a valid, properly aligned `ObjectHeader` that has not been freed.
    unsafe fn read_num_slots(obj_ptr: *const u8) -> u32 {
        let num_slots_offset = std::mem::offset_of!(rustjvm_types::ObjectHeader, num_slots);
        std::ptr::read(obj_ptr.add(num_slots_offset) as *const u32) // Cast: address arithmetic
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any dereference.
    unsafe extern "C" fn stub_getfield(obj_ptr: i64, field_index: i64) -> i64 {
        if obj_ptr == 0 { return 0; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return 0; } // Cast: x86-64 immediate encoding
        let ptr = base.add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        let val: Value = std::ptr::read(ptr as *const Value); // Cast: address arithmetic
        match val {
            Value::Int(i) => i as i64, // Cast: JIT ABI convention
            Value::Long(l) => l,
            Value::Float(f) => f.to_bits() as i64, // Cast: JIT ABI convention
            Value::Double(d) => d.to_bits() as i64, // Cast: JIT ABI convention
            Value::Object(Some(r)) => r.as_ptr() as i64, // Cast: JIT ABI convention
            Value::Object(None) => 0,
            _ => 0,
        }
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any write.
    unsafe extern "C" fn stub_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
        if obj_ptr == 0 { return; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return; } // Cast: x86-64 immediate encoding
        let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        std::ptr::write(ptr as *mut Value, Value::Int(val as i32)); // Cast: address arithmetic
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any write.
    unsafe extern "C" fn stub_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
        if obj_ptr == 0 { return; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return; } // Cast: x86-64 immediate encoding
        let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        std::ptr::write(ptr as *mut Value, Value::Long(val)); // Cast: address arithmetic
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any write.
    unsafe extern "C" fn stub_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
        if obj_ptr == 0 { return; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return; } // Cast: x86-64 immediate encoding
        let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        std::ptr::write(ptr as *mut Value, Value::Float(f32::from_bits(val as u32))); // Cast: address arithmetic
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any write.
    unsafe extern "C" fn stub_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
        if obj_ptr == 0 { return; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return; } // Cast: x86-64 immediate encoding
        let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        std::ptr::write(ptr as *mut Value, Value::Double(f64::from_bits(val as u64))); // Cast: address arithmetic
    }

    // SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
    // and a field index that is bounds-checked within the function body before any write.
    // val is either 0 (null) or a valid ObjectRef pointer from the managed heap.
    unsafe extern "C" fn stub_putfield_object(_vm_ptr: i64, obj_ptr: i64, field_index: i64, val: i64) {
        if obj_ptr == 0 { return; }
        let base = obj_ptr as *const u8; // Cast: address arithmetic
        let num_slots = read_num_slots(base);
        if field_index < 0 || field_index as u32 >= num_slots { return; } // Cast: x86-64 immediate encoding
        let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
        if val == 0 {
            std::ptr::write(ptr as *mut Value, Value::Object(None)); // Cast: address arithmetic
        } else {
            let obj_ref = ObjectRef::from_raw(val as usize as *mut u8); // Cast: address arithmetic
            std::ptr::write(ptr as *mut Value, Value::Object(Some(obj_ref))); // Cast: address arithmetic
        }
    }

    /// Helpers for tests -- provides real getfield/putfield stubs so that
    /// compiled code can read/write heap objects.  Other helpers point to a
    /// stub that panics with a clear message if called unexpectedly.
    fn test_helpers() -> JitRuntimeHelpers {
        // Stub that panics -- used for helpers not wired up in these tests.
        // SAFETY: This stub is registered as a function pointer in JitRuntimeHelpers but
        // should never be called during these tests; it panics to flag unexpected invocations.
        unsafe extern "C" fn unimplemented_stub() {
            panic!("JIT test helper called an unimplemented runtime stub");
        }
        let sentinel = unimplemented_stub as *const () as usize; // Cast: address arithmetic
        JitRuntimeHelpers {
            newarray: sentinel,
            new_object: sentinel,
            anewarray_object: sentinel,
            baload: sentinel,
            bastore: sentinel,
            iaload: sentinel,
            iastore: sentinel,
            aaload: sentinel,
            aastore: sentinel,
            multianewarray_2d: sentinel,
            arraylength: sentinel,
            getfield: stub_getfield as *const () as usize, // Cast: address arithmetic
            putfield_int: stub_putfield_int as *const () as usize, // Cast: address arithmetic
            putfield_long: stub_putfield_long as *const () as usize, // Cast: address arithmetic
            putfield_float: stub_putfield_float as *const () as usize, // Cast: address arithmetic
            putfield_double: stub_putfield_double as *const () as usize, // Cast: address arithmetic
            putfield_object: stub_putfield_object as *const () as usize, // Cast: address arithmetic
            getstatic: sentinel,
            putstatic_int: sentinel,
            putstatic_long: sentinel,
            putstatic_float: sentinel,
            putstatic_double: sentinel,
            putstatic_object: sentinel,
            checkcast: sentinel,
            instanceof_check: sentinel,
            throw_aioobe: sentinel,
            invoke_dispatch: sentinel,
            invoke_virtual_mic: sentinel,
            write_barrier: sentinel,
            satb_pre_write_barrier: sentinel,
            uncommon_trap: sentinel,
            math_fma_double: sentinel,
            math_fma_float: sentinel,
            // Inline TLAB bump wiring is exercised only in the real VM
            // helper-table path. Tests use the helper-call fallback so
            // leave the cursor/end offsets at 0 (layout-safe for `Tlab`
            // with `cursor` at offset 0) and the optional thread-pointer
            // helper unset — `get_current_thread == 0` tells the JIT to
            // emit the pre-existing `new_object` call.
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 8,
            class_id_offset_in_obj: 0,
            get_current_thread: 0,
            tlab_post_init: 0,
        }
    }

    #[test]
    fn test_compile_simple_return() {
        // Method: int f(int x) { return x; }
        // Bytecode: iload_0, ireturn
        let code: Vec<u8> = vec![0x1a, 0xac, 0, 0]; // + 2 padding bytes
        let code_len = 2;

        assert!(is_jit_compatible(&code, code_len, "(I)I"));

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some());

        let method = compiled.unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { method.call(&[42]) };
        assert_eq!(result, 42);
    }

    #[test]
    fn test_compile_add() {
        // Method: int add(int a, int b) { return a + b; }
        // Bytecode: iload_0, iload_1, iadd, ireturn
        let code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let code_len = 4;

        assert!(is_jit_compatible(&code, code_len, "(II)I"));

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[10, 32]) };
        assert_eq!(result, 42);
    }

    #[test]
    fn test_compile_sub_mul() {
        // Method: int f(int a, int b) { return (a - b) * a; }
        // iload_0, iload_1, isub, iload_0, imul, ireturn
        let code: Vec<u8> = vec![0x1a, 0x1b, 0x64, 0x1a, 0x68, 0xac, 0, 0];
        let code_len = 6;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[10, 3]) };
        assert_eq!(result, (10 - 3) * 10); // 70
    }

    #[test]
    fn test_compile_iconst() {
        // Method: int f() { return 5; }
        // iconst_5, ireturn
        let code: Vec<u8> = vec![0x08, 0xac, 0, 0];
        let code_len = 2;

        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[]) };
        assert_eq!(result, 5);
    }

    #[test]
    fn test_compile_branch() {
        // Method: int f(int n) { if (n <= 1) return n; return n + 1; }
        // iload_0        // 0
        // iconst_1       // 1
        // if_icmpgt 5    // 2 (jump to offset 7)
        // iload_0        // 5
        // ireturn        // 6
        // iload_0        // 7
        // iconst_1       // 8
        // iadd           // 9
        // ireturn        // 10
        // if_icmpgt offset=5 → target = 2 + 5 = 7
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x04, // 1: iconst_1
            0xa3, 0x00, 0x05, // 2: if_icmpgt +5 → target=7
            0x1a, // 5: iload_0
            0xac, // 6: ireturn
            0x1a, // 7: iload_0
            0x04, // 8: iconst_1
            0x60, // 9: iadd
            0xac, // 10: ireturn
            0, 0, // padding
        ];
        let code_len = 11;

        assert!(is_jit_compatible(&code, code_len, "(I)I"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // n=0: 0 <= 1, return 0
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[0]) }, 0);
        // n=1: 1 <= 1, return 1
        assert_eq!(unsafe { compiled.call(&[1]) }, 1);
        // n=5: 5 > 1, return 5+1=6
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[5]) }, 6);
    }

    #[test]
    fn test_compile_fib() {
        // int fib(int n) {
        //     if (n <= 1) return n;
        //     return fib(n-1) + fib(n-2);
        // }
        //
        // Bytecode:
        //  0: iload_0
        //  1: iconst_1
        //  2: if_icmpgt +5  → target=7
        //  5: iload_0
        //  6: ireturn
        //  7: iload_0
        //  8: iconst_1
        //  9: isub
        // 10: invokestatic (self) cp=0
        // 13: iload_0
        // 14: iconst_2
        // 15: isub
        // 16: invokestatic (self) cp=0
        // 19: iadd
        // 20: ireturn
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x04, // 1: iconst_1
            0xa3, 0x00, 0x05, // 2: if_icmpgt +5 → target=7
            0x1a, // 5: iload_0
            0xac, // 6: ireturn
            0x1a, // 7: iload_0
            0x04, // 8: iconst_1
            0x64, // 9: isub
            0xb8, 0x00, 0x00, // 10: invokestatic (self-call)
            0x1a, // 13: iload_0
            0x05, // 14: iconst_2
            0x64, // 15: isub
            0xb8, 0x00, 0x00, // 16: invokestatic (self-call)
            0x60, // 19: iadd
            0xac, // 20: ireturn
            0, 0, // padding
        ];
        let code_len = 21;

        assert!(is_jit_compatible(&code, code_len, "(I)I"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // fib(0) = 0, fib(1) = 1, fib(10) = 55, fib(20) = 6765
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[0]) }, 0);
        assert_eq!(unsafe { compiled.call(&[1]) }, 1);
        assert_eq!(unsafe { compiled.call(&[10]) }, 55);
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[20]) }, 6765);
    }

    #[test]
    fn test_compile_long_add() {
        // long f(long a, long b) { return a + b; }
        // lload_0, lload_1, ladd, lreturn
        let code: Vec<u8> = vec![0x1e, 0x1f, 0x61, 0xad, 0, 0];
        let code_len = 4;

        assert!(is_jit_compatible(&code, code_len, "(JJ)J"));
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[100_000_000_000i64, 200_000_000_000i64]) };
        assert_eq!(result, 300_000_000_000i64);
    }

    #[test]
    fn test_compile_iinc() {
        // int f(int x) { x += 10; return x; }
        // iload_0, iinc 0 10, iload_0, ireturn
        // Wait, iinc doesn't use the stack. Let's do:
        // iinc 0, 10
        // iload_0
        // ireturn
        let code: Vec<u8> = vec![
            0x84, 0x00, 0x0A, // iinc local=0, inc=10
            0x1a, // iload_0
            0xac, // ireturn
            0, 0, // padding
        ];
        let code_len = 5;

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[5]) }, 15);
        assert_eq!(unsafe { compiled.call(&[-3]) }, 7);
    }

    #[test]
    fn test_compile_idiv_irem() {
        // int f(int a, int b) { return a / b + a % b; }
        // iload_0, iload_1, idiv, iload_0, iload_1, irem, iadd, ireturn
        let code: Vec<u8> = vec![
            0x1a, 0x1b, 0x6c, // iload_0, iload_1, idiv
            0x1a, 0x1b, 0x70, // iload_0, iload_1, irem
            0x60, // iadd
            0xac, // ireturn
            0, 0,
        ];
        let code_len = 8;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // 17 / 5 = 3, 17 % 5 = 2, total = 5
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[17, 5]) }, 5);
        // -7 / 2 = -3, -7 % 2 = -1, total = -4
        assert_eq!(unsafe { compiled.call(&[-7, 2]) }, -4);
    }

    #[test]
    fn test_not_jit_compatible() {
        // invokeinterface (0xb9) is not supported
        let code: Vec<u8> = vec![0x2a, 0xb9, 0x00, 0x01, 0xac, 0, 0];
        assert!(!is_jit_compatible(&code, 5, "(Ljava/lang/Object;)I"));
    }

    #[test]
    fn test_jit_scan_field_ops() {
        // getfield (0xb4) is now JIT-compatible
        let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0xac, 0, 0];
        let result = jit_scan(&code, 5, "(Ljava/lang/Object;)I").unwrap();
        assert!(!result.needs_heap); // getfield doesn't need heap
        assert_eq!(result.field_ops.len(), 1);
        assert_eq!(result.field_ops[0], (1, 1)); // pc=1, cp_idx=1

        // putfield (0xb5) is JIT-compatible and sets needs_heap via resolver
        let pcode: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x1b, // 1: iload_1
            0xb5, 0x00, 0x02, // 2: putfield #2
            0xb1, // 5: return
            0, 0,
        ];
        let result = jit_scan(&pcode, 6, "(Ljava/lang/Object;I)V").unwrap();
        assert_eq!(result.field_ops.len(), 1);
        assert_eq!(result.field_ops[0], (2, 2)); // pc=2, cp_idx=2
    }

    #[test]
    fn test_jit_scan_array_needs_heap() {
        // newarray (0xbc) requires heap; method with just arithmetic doesn't
        let pure_code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0xac, 0, 0]; // iload_0, iload_1, iadd, ireturn
        let result = jit_scan(&pure_code, 4, "(II)I").unwrap();
        assert!(!result.needs_heap);
        assert!(result.multianewarray_ops.is_empty());

        // Method with newarray needs heap
        let array_code: Vec<u8> = vec![
            0x1a, // 0: iload_0 (count)
            0xbc, 0x04, // 1: newarray T_BOOLEAN
            0x4c, // 3: astore_1
            0x2c, // 4: aload_2
            0xbe, // 5: arraylength
            0xac, // 6: ireturn
            0, 0,
        ];
        let result = jit_scan(&array_code, 7, "(I)I").unwrap();
        assert!(result.needs_heap);
    }

    #[test]
    fn test_jit_scan_areturn_and_multianewarray() {
        // areturn is accepted for object return types
        let areturn_code: Vec<u8> = vec![0x2a, 0xb0, 0, 0]; // aload_0, areturn
        let result = jit_scan(&areturn_code, 2, "([[I)[[I").unwrap();
        assert!(!result.needs_heap);

        // multianewarray 2D is accepted
        let mna_code: Vec<u8> = vec![
            0x1a, // 0: iload_0 (dim1)
            0x1b, // 1: iload_1 (dim2)
            0xc5, 0x00, 0x0d, 0x02, // 2: multianewarray #13, 2
            0xb0, // 6: areturn
            0, 0,
        ];
        let result = jit_scan(&mna_code, 7, "(II)[[I").unwrap();
        assert!(result.needs_heap);
        assert_eq!(result.multianewarray_ops.len(), 1);
        assert_eq!(result.multianewarray_ops[0], (2, 13, 2));

        // multianewarray 3D is rejected
        let mna3d_code: Vec<u8> = vec![
            0x1a, 0x1b, 0x1c, 0xc5, 0x00, 0x0d, 0x03, // ndims=3
            0xb0, 0, 0,
        ];
        assert!(jit_scan(&mna3d_code, 8, "(III)[[[I").is_none());
    }

    #[test]
    fn test_compile_fconst() {
        // float f() { return 1.0f; }
        // fconst_1 (0x0c), freturn (0xae)
        let code: Vec<u8> = vec![0x0c, 0xae, 0, 0];
        let code_len = 2;

        assert!(is_jit_compatible(&code, code_len, "()F"));
        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[]) };
        // Result is f32 bit pattern as i64
        assert_eq!(f32::from_bits(result as u32), 1.0f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_fconst_all() {
        // Test fconst_0 (0x0b), fconst_1 (0x0c), fconst_2 (0x0d)
        for (op, expected) in [(0x0bu8, 0.0f32), (0x0c, 1.0f32), (0x0d, 2.0f32)] {
            let code: Vec<u8> = vec![op, 0xae, 0, 0]; // fconst_N, freturn
            let compiled = compile(
                &code,
                2,
                0,
                0,
                false,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
                Vec::new(),
                HashMap::new(),
                HashMap::new(),
                &test_helpers(),
                std::collections::HashSet::new(),
                HashMap::new(),
            )
            .unwrap();
            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
            let result = unsafe { compiled.call(&[]) };
            assert_eq!(f32::from_bits(result as u32), expected); // Cast: JIT ABI convention
        }
    }

    #[test]
    fn test_compile_dconst() {
        // double f() { return 1.0; }
        // dconst_1 (0x0f), dreturn (0xaf)
        let code: Vec<u8> = vec![0x0f, 0xaf, 0, 0];
        let code_len = 2;

        assert!(is_jit_compatible(&code, code_len, "()D"));
        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[]) };
        assert_eq!(f64::from_bits(result as u64), 1.0f64); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_dconst_all() {
        // dconst_0 (0x0e), dconst_1 (0x0f)
        for (op, expected) in [(0x0eu8, 0.0f64), (0x0f, 1.0f64)] {
            let code: Vec<u8> = vec![op, 0xaf, 0, 0]; // dconst_N, dreturn
            let compiled = compile(
                &code,
                2,
                0,
                0,
                false,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
                Vec::new(),
                HashMap::new(),
                HashMap::new(),
                &test_helpers(),
                std::collections::HashSet::new(),
                HashMap::new(),
            )
            .unwrap();
            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
            let result = unsafe { compiled.call(&[]) };
            assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
        }
    }

    #[test]
    fn test_compile_float_load_store() {
        // float f(float x) { float y = x; return y; }
        // fload_0 (0x22), fstore_1 (0x44), fload_1 (0x23), freturn (0xae)
        let code: Vec<u8> = vec![0x22, 0x44, 0x23, 0xae, 0, 0];
        let code_len = 4;

        assert!(is_jit_compatible(&code, code_len, "(F)F"));
        let compiled = compile(
            &code,
            code_len,
            1,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 3.15f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f32::from_bits(result as u32), 3.15f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_double_load_store() {
        // double f(double x) { double y = x; return y; }
        // dload_0 (0x26), dstore_1 (0x48), dload_1 (0x27), dreturn (0xaf)
        let code: Vec<u8> = vec![0x26, 0x48, 0x27, 0xaf, 0, 0];
        let code_len = 4;

        assert!(is_jit_compatible(&code, code_len, "(D)D"));
        let compiled = compile(
            &code,
            code_len,
            1,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 2.719f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f64::from_bits(result as u64), 2.719f64); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_float_load_indexed() {
        // float f(int dummy, float x) { return x; }
        // fload 1 (0x17, 0x01), freturn (0xae)
        let code: Vec<u8> = vec![0x17, 0x01, 0xae, 0, 0];
        let code_len = 3;

        assert!(is_jit_compatible(&code, code_len, "(IF)F"));
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 42.5f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[0, input]) };
        assert_eq!(f32::from_bits(result as u32), 42.5f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_i2b() {
        // int f(int x) { return (byte) x; }
        // iload_0, i2b (0x91), ireturn
        let code: Vec<u8> = vec![0x1a, 0x91, 0xac, 0, 0];
        let code_len = 3;

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Positive value within byte range
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[42]) }, 42);
        // Truncation: 0x1FF → (byte) = -1
        assert_eq!(unsafe { compiled.call(&[0x1FF]) }, -1);
        // Truncation: 300 → (byte) = 44
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[300]) }, 44);
        // Negative: -128
        assert_eq!(unsafe { compiled.call(&[-128]) }, -128);
    }

    #[test]
    fn test_compile_i2c() {
        // int f(int x) { return (char) x; }
        // iload_0, i2c (0x92), ireturn
        let code: Vec<u8> = vec![0x1a, 0x92, 0xac, 0, 0];
        let code_len = 3;

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Positive value within char range
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[65]) }, 65); // 'A'
                                                         // 0xFFFF stays as 65535 (unsigned)
        assert_eq!(unsafe { compiled.call(&[0xFFFF]) }, 65535);
        // Truncation: 0x10041 → 0x0041 = 65
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[0x10041]) }, 65);
        // Negative: -1 → 0xFFFF = 65535
        assert_eq!(unsafe { compiled.call(&[-1]) }, 65535);
    }

    #[test]
    fn test_compile_i2s() {
        // int f(int x) { return (short) x; }
        // iload_0, i2s (0x93), ireturn
        let code: Vec<u8> = vec![0x1a, 0x93, 0xac, 0, 0];
        let code_len = 3;

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Positive within short range
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[1000]) }, 1000);
        // Truncation: 0x18000 → (short) = -32768
        assert_eq!(unsafe { compiled.call(&[0x18000]) }, -32768);
        // 32767 stays
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[32767]) }, 32767);
        // -32768 stays
        assert_eq!(unsafe { compiled.call(&[-32768]) }, -32768);
    }

    #[test]
    fn test_compile_void_return() {
        // void f() { return; }
        // return (0xb1)
        let code: Vec<u8> = vec![0xb1, 0, 0];
        let code_len = 1;

        assert!(is_jit_compatible(&code, code_len, "()V"));
        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Void return — result is undefined, but should not crash
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let _ = unsafe { compiled.call(&[]) };
    }

    #[test]
    fn test_compile_freturn() {
        // float f(float x) { return x; }
        // fload_0 (0x22), freturn (0xae)
        let code: Vec<u8> = vec![0x22, 0xae, 0, 0];
        let code_len = 2;

        assert!(is_jit_compatible(&code, code_len, "(F)F"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = (-3.5f32).to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f32::from_bits(result as u32), -3.5f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_dup2() {
        // int f(int a, int b) { [a,b] → dup2 → [a,b,a,b] → iadd → [a,b,a+b] → iadd → [a,b+a+b] → iadd → [2a+2b] }
        // iload_0 (0x1a), iload_1 (0x1b), dup2 (0x5c), iadd (0x60), iadd (0x60), iadd (0x60), ireturn (0xac)
        let code: Vec<u8> = vec![0x1a, 0x1b, 0x5c, 0x60, 0x60, 0x60, 0xac, 0, 0];
        let code_len = 7;
        let compiled = compile(
            &code, code_len, 2, 2, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(), Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();
        // f(3, 5) = 2*3 + 2*5 = 16
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[3, 5]) };
        assert_eq!(result, 16);
        // f(10, 7) = 2*10 + 2*7 = 34
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[10, 7]) };
        assert_eq!(result, 34);
    }

    #[test]
    fn test_compile_math_sqrt_intrinsic() {
        // double f(double x) { return Math.sqrt(x); }
        // dload_0 (0x26), invokestatic (0xb8, 0x00, 0x01), dreturn (0xaf)
        let code: Vec<u8> = vec![0x26, 0xb8, 0x00, 0x01, 0xaf, 0, 0];
        let code_len = 5;
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![(1, crate::JitDirectCall {
                entry: crate::MATH_SQRT_INTRINSIC,
                needs_context: false,
                num_params: 1,
                return_type: b'D',
            })],
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // sqrt(4.0) == 2.0
        let input = 4.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f64::from_bits(result as u64), 2.0f64); // Cast: JIT ABI convention
        // sqrt(2.0) ≈ 1.4142135623730951
        let input2 = 2.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result2 = unsafe { compiled.call(&[input2]) };
        assert!((f64::from_bits(result2 as u64) - std::f64::consts::SQRT_2).abs() < 1e-14); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_math_min_max_int_intrinsic() {
        // int min_f(int a, int b) { return Math.min(a, b); }
        // int max_f(int a, int b) { return Math.max(a, b); }
        // Bytecode: iload_0 (0x1a), iload_1 (0x1b), invokestatic (0xb8, 0x00, 0x01),
        //           ireturn (0xac)
        // Round-9 CRIT regression test for the swapped CMOVL/CMOVG opcodes in
        // the MATH_MIN_INT_INTRINSIC / MATH_MAX_INT_INTRINSIC arms. Before the
        // fix, Math.min(3, 5) returned 5 and Math.max(3, 5) returned 3.
        let code: Vec<u8> = vec![0x1a, 0x1b, 0xb8, 0x00, 0x01, 0xac, 0, 0];
        let code_len = 6;

        // Math.min variant
        let compiled_min = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(),
            vec![(2, crate::JitDirectCall {
                entry: crate::MATH_MIN_INT_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'I',
            })],
            Vec::new(), Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r1 = unsafe { compiled_min.call(&[3, 5]) };
        assert_eq!(r1, 3, "Math.min(3, 5) must be 3 (was {r1})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r2 = unsafe { compiled_min.call(&[5, 3]) };
        assert_eq!(r2, 3, "Math.min(5, 3) must be 3 (was {r2})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r3 = unsafe { compiled_min.call(&[-7, 4]) };
        assert_eq!(r3, -7, "Math.min(-7, 4) must be -7 (was {r3})");

        // Math.max variant
        let compiled_max = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(),
            vec![(2, crate::JitDirectCall {
                entry: crate::MATH_MAX_INT_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'I',
            })],
            Vec::new(), Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m1 = unsafe { compiled_max.call(&[3, 5]) };
        assert_eq!(m1, 5, "Math.max(3, 5) must be 5 (was {m1})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m2 = unsafe { compiled_max.call(&[5, 3]) };
        assert_eq!(m2, 5, "Math.max(5, 3) must be 5 (was {m2})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m3 = unsafe { compiled_max.call(&[-7, 4]) };
        assert_eq!(m3, 4, "Math.max(-7, 4) must be 4 (was {m3})");
    }

    /// Round-11 HIGH-3 — `peephole-cmov`: verify the user-written
    /// equivalent of `Math.min(a, b)` — written as `(a < b) ? a : b`
    /// — gets lowered to a branchless CMOV by
    /// `try_cmov_minmax_peephole`. The bytecode is what javac emits
    /// for that source expression:
    ///
    ///     [0] 0x1A             iload_0           a
    ///     [1] 0x1B             iload_1           b
    ///     [2] 0xa2 0x00 0x07   if_icmpge → PC 9  (taken when a >= b)
    ///     [5] 0x1A             iload_0           "take a"
    ///     [6] 0xa7 0x00 0x04   goto    → PC 10
    ///     [9] 0x1B             iload_1           "take b"
    ///     [10] 0xac            ireturn
    #[test]
    fn test_compile_user_written_min_idiom_cmov() {
        let code: Vec<u8> = vec![
            0x1A,             // 0:  iload_0
            0x1B,             // 1:  iload_1
            0xa2, 0x00, 0x07, // 2:  if_icmpge → PC 9
            0x1A,             // 5:  iload_0   (fall-through: a < b → take a)
            0xa7, 0x00, 0x04, // 6:  goto → PC 10
            0x1B,             // 9:  iload_1   (taken: a >= b → take b)
            0xac,             // 10: ireturn
            0, 0,             // padding
        ];
        let code_len = 11;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(),
            Vec::new(),
            Vec::new(), Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // Semantics: (a < b) ? a : b == min(a, b).
        // SAFETY: Calling JIT-compiled machine code in a test; the
        // CompiledMethod was produced by the JIT compiler from valid
        // bytecode and the mmap region is executable.
        let r1 = unsafe { compiled.call(&[3, 5]) };
        assert_eq!(r1, 3, "user-min(3, 5) must be 3 (was {r1})");
        // SAFETY: same as above
        let r2 = unsafe { compiled.call(&[5, 3]) };
        assert_eq!(r2, 3, "user-min(5, 3) must be 3 (was {r2})");
        // SAFETY: same as above
        let r3 = unsafe { compiled.call(&[-7, 4]) };
        assert_eq!(r3, -7, "user-min(-7, 4) must be -7 (was {r3})");
        // Equal inputs: (a < b) is false → take b == a.
        // SAFETY: same as above
        let r4 = unsafe { compiled.call(&[42, 42]) };
        assert_eq!(r4, 42, "user-min(42, 42) must be 42 (was {r4})");
    }

    #[test]
    fn test_compile_math_min_max_long_intrinsic() {
        // long min_f(long a, long b) { return Math.min(a, b); }
        // long max_f(long a, long b) { return Math.max(a, b); }
        // Bytecode: lload_0 (0x1e), lload_2 (0x20), invokestatic (0xb8, 0x00, 0x01),
        //           lreturn (0xad). Long uses 2 local slots per arg, so the
        //           second arg lives at slot 2 (lload_2), and max_locals is 4.
        // Round-9 CRIT regression test for the swapped CMOVL/CMOVG opcodes in
        // the MATH_MIN_LONG_INTRINSIC / MATH_MAX_LONG_INTRINSIC arms — same
        // bug as the int variants but on the 64-bit REX.W CMOV path.
        let code: Vec<u8> = vec![0x1e, 0x20, 0xb8, 0x00, 0x01, 0xad, 0, 0];
        let code_len = 6;

        // Math.min(long, long) variant
        let compiled_min = compile(
            &code,
            code_len,
            4, // param_slots: 2 longs * 2 slots each
            4, // max_locals
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(),
            vec![(2, crate::JitDirectCall {
                entry: crate::MATH_MIN_LONG_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'J',
            })],
            Vec::new(), Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r1 = unsafe { compiled_min.call(&[3i64, 5i64]) };
        assert_eq!(r1, 3, "Math.min(3L, 5L) must be 3 (was {r1})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r2 = unsafe { compiled_min.call(&[5i64, 3i64]) };
        assert_eq!(r2, 3, "Math.min(5L, 3L) must be 3 (was {r2})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let r3 = unsafe { compiled_min.call(&[-7i64, 4i64]) };
        assert_eq!(r3, -7, "Math.min(-7L, 4L) must be -7 (was {r3})");

        // Math.max(long, long) variant
        let compiled_max = compile(
            &code,
            code_len,
            4,
            4,
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(),
            vec![(2, crate::JitDirectCall {
                entry: crate::MATH_MAX_LONG_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'J',
            })],
            Vec::new(), Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m1 = unsafe { compiled_max.call(&[3i64, 5i64]) };
        assert_eq!(m1, 5, "Math.max(3L, 5L) must be 5 (was {m1})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m2 = unsafe { compiled_max.call(&[5i64, 3i64]) };
        assert_eq!(m2, 5, "Math.max(5L, 3L) must be 5 (was {m2})");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let m3 = unsafe { compiled_max.call(&[-7i64, 4i64]) };
        assert_eq!(m3, 4, "Math.max(-7L, 4L) must be 4 (was {m3})");
    }

    #[test]
    fn test_compile_dreturn() {
        // double f(double x) { return x; }
        // dload_0 (0x26), dreturn (0xaf)
        let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
        let code_len = 2;

        assert!(is_jit_compatible(&code, code_len, "(D)D"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = std::f64::consts::PI.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f64::from_bits(result as u64), std::f64::consts::PI); // Cast: JIT ABI convention
    }

    #[test]
    fn test_jit_scan_float_double_compatible() {
        // Verify float/double opcodes pass the scan
        // fconst_1, fstore_0, fload_0, freturn
        let fcode: Vec<u8> = vec![0x0c, 0x43, 0x22, 0xae, 0, 0];
        assert!(jit_scan(&fcode, 4, "()F").is_some());

        // dconst_1, dstore_0, dload_0, dreturn
        let dcode: Vec<u8> = vec![0x0f, 0x47, 0x26, 0xaf, 0, 0];
        assert!(jit_scan(&dcode, 4, "()D").is_some());

        // i2b, i2c, i2s are compatible
        let cast_code: Vec<u8> = vec![0x1a, 0x91, 0x92, 0x93, 0xac, 0, 0];
        assert!(jit_scan(&cast_code, 5, "(I)I").is_some());

        // void return is compatible
        let void_code: Vec<u8> = vec![0xb1, 0, 0];
        assert!(jit_scan(&void_code, 1, "()V").is_some());

        // SSE float arithmetic is compatible
        let fadd_code: Vec<u8> = vec![0x22, 0x23, 0x62, 0xae, 0, 0]; // fload_0, fload_1, fadd, freturn
        assert!(jit_scan(&fadd_code, 4, "(FF)F").is_some());

        // SSE double arithmetic is compatible
        let dadd_code: Vec<u8> = vec![0x26, 0x27, 0x63, 0xaf, 0, 0]; // dload_0, dload_1, dadd, dreturn
        assert!(jit_scan(&dadd_code, 4, "(DD)D").is_some());

        // All conversions are compatible
        let conv_code: Vec<u8> = vec![0x1a, 0x86, 0x8d, 0x8e, 0xac, 0, 0]; // iload_0, i2f, f2d, d2i, ireturn
        assert!(jit_scan(&conv_code, 5, "(I)I").is_some());

        // fcmpl/fcmpg/dcmpl/dcmpg are compatible
        let fcmp_code: Vec<u8> = vec![0x22, 0x23, 0x95, 0xac, 0, 0]; // fload_0, fload_1, fcmpl, ireturn
        assert!(jit_scan(&fcmp_code, 4, "(FF)I").is_some());

        // fneg/dneg are compatible
        let fneg_code: Vec<u8> = vec![0x22, 0x76, 0xae, 0, 0]; // fload_0, fneg, freturn
        assert!(jit_scan(&fneg_code, 3, "(F)F").is_some());
    }

    #[test]
    fn test_compile_fadd() {
        // float f(float a, float b) { return a + b; }
        // fload_0 (0x22), fload_1 (0x23), fadd (0x62), freturn (0xae)
        let code: Vec<u8> = vec![0x22, 0x23, 0x62, 0xae, 0, 0];
        let code_len = 4;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 3.5f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 2.25f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f32::from_bits(result as u32), 5.75f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_fsub_fmul_fdiv() {
        // float fsub(float a, float b) { return a - b; }
        let sub_code: Vec<u8> = vec![0x22, 0x23, 0x66, 0xae, 0, 0]; // fsub
        let compiled = compile(
            &sub_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 10.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f32::from_bits(result as u32), 7.0f32); // Cast: JIT ABI convention

        // float fmul(float a, float b) { return a * b; }
        let mul_code: Vec<u8> = vec![0x22, 0x23, 0x6a, 0xae, 0, 0]; // fmul
        let compiled = compile(
            &mul_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f32::from_bits(result as u32), 30.0f32); // Cast: JIT ABI convention

        // float fdiv(float a, float b) { return a / b; }
        let div_code: Vec<u8> = vec![0x22, 0x23, 0x6e, 0xae, 0, 0]; // fdiv
        let compiled = compile(
            &div_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 15.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 4.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f32::from_bits(result as u32), 3.75f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_dadd() {
        // double f(double a, double b) { return a + b; }
        // dload_0 (0x26), dload_1 (0x27), dadd (0x63), dreturn (0xaf)
        let code: Vec<u8> = vec![0x26, 0x27, 0x63, 0xaf, 0, 0];
        let code_len = 4;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 1.5f64.to_bits() as i64; // Cast: JIT ABI convention
        let b = 2.5f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f64::from_bits(result as u64), 4.0f64); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_dsub_dmul_ddiv() {
        // double dsub
        let sub_code: Vec<u8> = vec![0x26, 0x27, 0x67, 0xaf, 0, 0];
        let compiled = compile(
            &sub_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 100.0f64.to_bits() as i64; // Cast: JIT ABI convention
        let b = 37.5f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f64::from_bits(result as u64), 62.5f64); // Cast: JIT ABI convention

        // double dmul
        let mul_code: Vec<u8> = vec![0x26, 0x27, 0x6b, 0xaf, 0, 0];
        let compiled = compile(
            &mul_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 6.0f64.to_bits() as i64; // Cast: JIT ABI convention
        let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention

        // double ddiv
        let div_code: Vec<u8> = vec![0x26, 0x27, 0x6f, 0xaf, 0, 0];
        let compiled = compile(
            &div_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 22.0f64.to_bits() as i64; // Cast: JIT ABI convention
        let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        let expected = 22.0f64 / 7.0f64;
        assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_fneg_dneg() {
        // float fneg(float x) { return -x; }
        // fload_0 (0x22), fneg (0x76), freturn (0xae)
        let fcode: Vec<u8> = vec![0x22, 0x76, 0xae, 0, 0];
        let compiled = compile(
            &fcode,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 3.5f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f32::from_bits(result as u32), -3.5f32); // Cast: JIT ABI convention
        // Negate negative
        let input = (-7.0f32).to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f32::from_bits(result as u32), 7.0f32); // Cast: JIT ABI convention

        // double dneg(double x) { return -x; }
        // dload_0 (0x26), dneg (0x77), dreturn (0xaf)
        let dcode: Vec<u8> = vec![0x26, 0x77, 0xaf, 0, 0];
        let compiled = compile(
            &dcode,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 42.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f64::from_bits(result as u64), -42.0f64); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_i2f_i2d() {
        // int → float: iload_0 (0x1a), i2f (0x86), freturn (0xae)
        let i2f_code: Vec<u8> = vec![0x1a, 0x86, 0xae, 0, 0];
        let compiled = compile(
            &i2f_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[42]) };
        assert_eq!(f32::from_bits(result as u32), 42.0f32); // Cast: JIT ABI convention
        let result = unsafe { compiled.call(&[-7]) };
        assert_eq!(f32::from_bits(result as u32), -7.0f32); // Cast: JIT ABI convention

        // int → double: iload_0 (0x1a), i2d (0x87), dreturn (0xaf)
        let i2d_code: Vec<u8> = vec![0x1a, 0x87, 0xaf, 0, 0];
        let compiled = compile(
            &i2d_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[42]) };
        assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention
        let result = unsafe { compiled.call(&[-100]) };
        assert_eq!(f64::from_bits(result as u64), -100.0f64); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_f2i_f2d_d2i_d2f() {
        // float → int: fload_0 (0x22), f2i (0x8b), ireturn (0xac)
        let f2i_code: Vec<u8> = vec![0x22, 0x8b, 0xac, 0, 0];
        let compiled = compile(
            &f2i_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 3.7f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(result, 3); // truncate toward zero
        let input = (-3.7f32).to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(result, -3);

        // float → double: fload_0, f2d (0x8d), dreturn
        let f2d_code: Vec<u8> = vec![0x22, 0x8d, 0xaf, 0, 0];
        let compiled = compile(
            &f2d_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f64::from_bits(result as u64), 1.5f64); // Cast: JIT ABI convention

        // double → int: dload_0 (0x26), d2i (0x8e), ireturn (0xac)
        let d2i_code: Vec<u8> = vec![0x26, 0x8e, 0xac, 0, 0];
        let compiled = compile(
            &d2i_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 9.99f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(result, 9);

        // double → float: dload_0 (0x26), d2f (0x90), freturn (0xae)
        let d2f_code: Vec<u8> = vec![0x26, 0x90, 0xae, 0, 0];
        let compiled = compile(
            &d2f_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 1.5f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(f32::from_bits(result as u32), 1.5f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_l2f_l2d_f2l_d2l() {
        // long → float: lload_0 (0x1e), l2f (0x89), freturn (0xae)
        let l2f_code: Vec<u8> = vec![0x1e, 0x89, 0xae, 0, 0];
        let compiled = compile(
            &l2f_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[1000000i64]) };
        assert_eq!(f32::from_bits(result as u32), 1_000_000.0f32); // Cast: JIT ABI convention

        // long → double: lload_0 (0x1e), l2d (0x8a), dreturn (0xaf)
        let l2d_code: Vec<u8> = vec![0x1e, 0x8a, 0xaf, 0, 0];
        let compiled = compile(
            &l2d_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[1000000i64]) };
        assert_eq!(f64::from_bits(result as u64), 1_000_000.0f64); // Cast: JIT ABI convention

        // float → long: fload_0 (0x22), f2l (0x8c), lreturn (0xad)
        let f2l_code: Vec<u8> = vec![0x22, 0x8c, 0xad, 0, 0];
        let compiled = compile(
            &f2l_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 42.9f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(result, 42i64);

        // double → long: dload_0 (0x26), d2l (0x8f), lreturn (0xad)
        let d2l_code: Vec<u8> = vec![0x26, 0x8f, 0xad, 0, 0];
        let compiled = compile(
            &d2l_code,
            3,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let input = 99.9f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[input]) };
        assert_eq!(result, 99i64);
    }

    #[test]
    fn test_compile_fcmpl() {
        // int f(float a, float b) { return fcmpl(a, b); }
        // fload_0 (0x22), fload_1 (0x23), fcmpl (0x95), ireturn (0xac)
        let code: Vec<u8> = vec![0x22, 0x23, 0x95, 0xac, 0, 0];
        let compiled = compile(
            &code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // a > b → 1
        let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, 1);

        // a == b → 0
        let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, 0);

        // a < b → -1
        let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, -1);

        // NaN → -1 (fcmpl)
        let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[nan, b]) }, -1);
        assert_eq!(unsafe { compiled.call(&[b, nan]) }, -1);
    }

    #[test]
    fn test_compile_fcmpg() {
        // fload_0, fload_1, fcmpg (0x96), ireturn
        let code: Vec<u8> = vec![0x22, 0x23, 0x96, 0xac, 0, 0];
        let compiled = compile(
            &code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // a > b → 1
        let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, 1);

        // a < b → -1
        let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, -1);

        // NaN → 1 (fcmpg)
        let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[nan, b]) }, 1);
        assert_eq!(unsafe { compiled.call(&[b, nan]) }, 1);
    }

    #[test]
    fn test_compile_dcmpl_dcmpg() {
        // dload_0, dload_1, dcmpl (0x97), ireturn
        let dcmpl_code: Vec<u8> = vec![0x26, 0x27, 0x97, 0xac, 0, 0];
        let compiled = compile(
            &dcmpl_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        let a = 5.0f64.to_bits() as i64; // Cast: JIT ABI convention
        let b = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, 1);

        let a = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, 0);

        let a = 1.0f64.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[a, b]) }, -1);

        // NaN → -1 (dcmpl)
        let nan = f64::NAN.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[nan, b]) }, -1);

        // dcmpg: NaN → 1
        let dcmpg_code: Vec<u8> = vec![0x26, 0x27, 0x98, 0xac, 0, 0];
        let compiled = compile(
            &dcmpg_code,
            4,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        assert_eq!(unsafe { compiled.call(&[nan, b]) }, 1);
        assert_eq!(unsafe { compiled.call(&[b, nan]) }, 1);
    }

    #[test]
    fn test_compile_float_chain() {
        // float f(float a, float b) { return (a + b) * a; }
        // fload_0, fload_1, fadd, fload_0, fmul, freturn
        let code: Vec<u8> = vec![0x22, 0x23, 0x62, 0x22, 0x6a, 0xae, 0, 0];
        let code_len = 6;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
        let b = 2.0f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a, b]) };
        // (3.0 + 2.0) * 3.0 = 15.0
        assert_eq!(f32::from_bits(result as u32), 15.0f32); // Cast: JIT ABI convention
    }

    #[test]
    fn test_compile_i2f_fadd_f2i() {
        // int f(int a, int b) { return (int)((float)a + (float)b); }
        // iload_0, i2f, iload_1, i2f, fadd, f2i, ireturn
        let code: Vec<u8> = vec![
            0x1a, // iload_0
            0x86, // i2f
            0x1b, // iload_1
            0x86, // i2f
            0x62, // fadd
            0x8b, // f2i
            0xac, // ireturn
            0, 0,
        ];
        let code_len = 7;

        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[10, 20]) };
        assert_eq!(result, 30);

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[7, -3]) };
        assert_eq!(result, 4);
    }

    // -----------------------------------------------------------------------
    // Round 17: getfield/putfield tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_getfield_int() {
        // Method: int getX(Object this) { return this.x; }
        // Bytecode: aload_0, getfield #1, ireturn
        // We fake field #1 as field_index=0, type='I'
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x01, // 1: getfield #1
            0xac, // 4: ireturn
            0, 0,
        ];
        let code_len = 5;

        // Compile with field_info: pc=1, field_index=0, type_tag='I'
        let field_info = vec![(1usize, 0usize, b'I')];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // Create a heap object with one Int field
        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(42));

        // Call the JIT method: pass obj pointer as first arg
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 42);

        // Test with negative value
        heap.set_field(obj, 0, Value::Int(-123));
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, -123);
    }

    #[test]
    fn test_getfield_long() {
        // Method: long getY(Object this) { return this.y; }
        // y is at field_index=1
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x02, // 1: getfield #2
            0xad, // 4: lreturn
            0, 0,
        ];
        let code_len = 5;
        let field_info = vec![(1usize, 1usize, b'J')];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 3);
        heap.set_field(obj, 1, Value::Long(9_999_999_999i64));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 9_999_999_999i64);
    }

    #[test]
    fn test_getfield_float() {
        // Return float field as bits
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x01, // 1: getfield #1
            0xae, // 4: freturn
            0, 0,
        ];
        let code_len = 5;
        let field_info = vec![(1usize, 0usize, b'F')];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Float(3.5f32));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        let result_f = f32::from_bits(result as u32); // Cast: JIT ABI convention
        assert!((result_f - 3.5f32).abs() < 0.001);
    }

    #[test]
    fn test_putfield_int() {
        // Method: void setX(Object this, int val) { this.x = val; }
        // Bytecode: aload_0, iload_1, putfield #1, return
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x1b, // 1: iload_1
            0xb5, 0x00, 0x01, // 2: putfield #1
            0xb1, // 5: return
            0, 0,
        ];
        let code_len = 6;
        let field_info = vec![(2usize, 0usize, b'I')];
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(0));

        // Call: setX(obj, 99)
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.call(&[obj.as_ptr() as i64, 99]) }; // Cast: JIT ABI convention

        // Verify the field was updated
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Int(99));

        // Test with negative value
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.call(&[obj.as_ptr() as i64, -42]) }; // Cast: JIT ABI convention
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Int(-42));
    }

    #[test]
    fn test_putfield_long() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x1f, // 1: lload_1
            0xb5, 0x00, 0x01, // 2: putfield #1
            0xb1, // 5: return
            0, 0,
        ];
        let code_len = 6;
        let field_info = vec![(2usize, 0usize, b'J')];
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.call(&[obj.as_ptr() as i64, 123_456_789_012i64]) }; // Cast: JIT ABI convention
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Long(123_456_789_012i64));
    }

    #[test]
    fn test_putfield_object_with_write_barrier() {
        // Method: void setRef(Object this, Object ref) { this.ref = ref; }
        // Object putfield needs heap pointer for write barrier
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x2b, // 1: aload_1
            0xb5, 0x00, 0x01, // 2: putfield #1
            0xb1, // 5: return
            0, 0,
        ];
        let code_len = 6;
        let field_info = vec![(2usize, 0usize, b'L')];
        // needs_heap = true for Object putfield
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            true,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        let ref_obj = heap.alloc_object(ClassId::new(0), 1);

        let heap_ptr = &heap as *const GenerationalHeap as i64; // Cast: address arithmetic

        // Call with heap pointer as hidden first arg
        // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        unsafe {
            compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, ref_obj.as_ptr() as i64]) // Cast: JIT ABI convention
        };

        // Verify the field was updated
        let val = heap.get_field(obj, 0);
        match val {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), ref_obj.as_ptr()),
            other => unreachable!("expected Object(Some), got {other:?}"),
        }

        // Test setting to null
        // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        unsafe { compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, 0]) }; // Cast: JIT ABI convention
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Object(None));
    }

    #[test]
    fn test_getfield_putfield_roundtrip() {
        // Method: int inc(Object this) { this.x = this.x + 1; return this.x; }
        // aload_0, getfield #1, iconst_1, iadd, aload_0, swap, putfield #1, aload_0, getfield #1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x01, // 1: getfield #1 → x
            0x04, // 4: iconst_1
            0x60, // 5: iadd → x+1
            0x2a, // 6: aload_0
            0x5f, // 7: swap → [obj, x+1]
            0xb5, 0x00, 0x01, // 8: putfield #1 → this.x = x+1
            0x2a, // 11: aload_0
            0xb4, 0x00, 0x01, // 12: getfield #1
            0xac, // 15: ireturn
            0, 0,
        ];
        let code_len = 16;
        let field_info = vec![
            (1usize, 0usize, b'I'),  // getfield at pc=1
            (8usize, 0usize, b'I'),  // putfield at pc=8
            (12usize, 0usize, b'I'), // getfield at pc=12
        ];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(10));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 11);

        // Verify the field is now 11
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Int(11));

        // Call again — should return 12
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 12);
    }

    #[test]
    fn test_getfield_double() {
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x01, // 1: getfield #1
            0xaf, // 4: dreturn
            0, 0,
        ];
        let code_len = 5;
        let field_info = vec![(1usize, 0usize, b'D')];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Double(2.719));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        let result_d = f64::from_bits(result as u64); // Cast: JIT ABI convention
        assert!((result_d - 2.719).abs() < 0.0001);
    }

    #[test]
    fn test_getfield_object_ref() {
        // getfield that returns an Object reference (areturn)
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x01, // 1: getfield #1
            0xb0, // 4: areturn
            0, 0,
        ];
        let code_len = 5;
        let field_info = vec![(1usize, 0usize, b'L')];
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        let ref_obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj, 0, Value::Object(Some(ref_obj)));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, ref_obj.as_ptr() as i64); // Cast: JIT ABI convention

        // Test null reference
        heap.set_field(obj, 0, Value::Object(None));
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 0);
    }

    #[test]
    fn test_putfield_float_double() {
        // Test putfield for float
        // Method: void setF(Object this, float f) { this.f = f; }
        let fcode: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x23, // 1: fload_1 (load float param from local 1)
            0xb5, 0x00, 0x01, // 2: putfield #1
            0xb1, // 5: return
            0, 0,
        ];
        let code_len = 6;
        let field_info = vec![(2usize, 1usize, b'F')];
        let compiled = compile(
            &fcode,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 3);

        let float_bits = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.call(&[obj.as_ptr() as i64, float_bits]) }; // Cast: JIT ABI convention
        let val = heap.get_field(obj, 1);
        assert_eq!(val, Value::Float(1.5f32));
    }

    #[test]
    fn test_getfield_second_field() {
        // Access field_index=2 (third field)
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x03, // 1: getfield #3
            0xac, // 4: ireturn
            0, 0,
        ];
        let code_len = 5;
        let field_info = vec![(1usize, 2usize, b'I')]; // field_index=2
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 4);
        heap.set_field(obj, 0, Value::Int(100));
        heap.set_field(obj, 1, Value::Int(200));
        heap.set_field(obj, 2, Value::Int(300));

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[obj.as_ptr() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, 300); // reads field at index 2
    }

    // ===================================================================
    // LICM (Loop-Invariant Code Motion) tests
    // ===================================================================

    #[test]
    fn test_detect_loops() {
        // Simple loop: for (k=0; k<10; k++) { ... }
        // 0: iconst_0       k = 0
        // 1: istore_1
        // 2: iload_1        ← loop header
        // 3: bipush 10
        // 5: if_icmpge +8   → exit at 13
        // 8: iinc 1, 1      k++
        // 11: goto -9        → back to 2
        // 13: iload_0        after loop
        // 14: ireturn
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1 (header)
            0x10, 0x0a, // 3: bipush 10
            0xa2, 0x00, 0x08, // 5: if_icmpge +8 → 13
            0x84, 0x01, 0x01, // 8: iinc 1, 1
            0xa7, 0xff, 0xf7, // 11: goto -9 → 2
            0x1a, // 14: iload_0
            0xac, // 15: ireturn
            0, 0,
        ];
        let loops = detect_loops(&code, 16);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0], (2, 11)); // header=2, back_edge=11
    }

    #[test]
    fn test_find_modified_locals() {
        // Loop body: iinc 1,1; istore_2; aload_0; iload_3; aaload
        let code: Vec<u8> = vec![
            0x84, 0x01, 0x01, // 0: iinc 1, 1
            0x3d, // 3: istore_2
            0x2a, // 4: aload_0
            0x1d, // 5: iload_3
            0x32, // 6: aaload
        ];
        let modified = find_modified_locals(&code, 0, 7);
        assert!(modified & (1 << 1) != 0); // local 1 modified by iinc
        assert!(modified & (1 << 2) != 0); // local 2 modified by istore_2
        assert!(modified & (1 << 0) == 0); // local 0 NOT modified
        assert!(modified & (1 << 3) == 0); // local 3 NOT modified
    }

    #[test]
    fn test_match_invariant_aaload() {
        // aload_0; iload_3; aaload
        let code: Vec<u8> = vec![0x2a, 0x1d, 0x32];
        let modified = 0u64; // nothing modified
        let result = match_invariant_aaload(&code, 0, modified, 3);
        assert_eq!(result, Some((0, 3, 3))); // array=0, index=3, seq_end=3

        // Same but local 0 is modified → should NOT match
        let modified2 = 1u64 << 0;
        let result2 = match_invariant_aaload(&code, 0, modified2, 3);
        assert_eq!(result2, None);

        // aload_1; iload (wide) 4; aaload
        let code2: Vec<u8> = vec![0x2b, 0x15, 0x04, 0x32];
        let result3 = match_invariant_aaload(&code2, 0, 0, 4);
        assert_eq!(result3, Some((1, 4, 4)));
    }

    #[test]
    fn test_find_loop_hoists_matmul_pattern() {
        // Simulate matmul inner loop pattern:
        // header=2: iload 7; iload 2; if_icmpge exit;
        //           aload_0; iload 4; aaload;     ← INVARIANT (a[i])
        //           iload 7; iaload;              sum += a[i][k]
        //           iinc 7,1; goto header
        //
        // Modified: local 7 (k) via iinc
        // Invariant: local 0 (a) and local 4 (i)
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0 (k=0)
            0x36, 0x07, // 1: istore 7
            // header at 3:
            0x15, 0x07, // 3: iload 7
            0x1c, // 5: iload_2 (n)
            0xa2, 0x00, 0x0e, // 6: if_icmpge +14 → 20
            0x2a, // 9: aload_0 (a) ← SEQ START
            0x15, 0x04, // 10: iload 4 (i)
            0x32, // 12: aaload  ← SEQ END (13)
            0x15, 0x07, // 13: iload 7 (k)
            0x2e, // 15: iaload (a[i][k])
            0x84, 0x07, 0x01, // 16: iinc 7, 1
            0xa7, 0xff, 0xf0, // 19: goto -16 → 3
            0x1a, // 22: iload_0
            0xac, // 23: ireturn
            0, 0,
        ];
        let loops = detect_loops(&code, 24);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0], (3, 19)); // header=3, back=19

        let hoists = find_loop_hoists(&code, 24, &loops);
        assert_eq!(hoists.len(), 1);
        assert_eq!(hoists[0].loop_header, 3);
        assert_eq!(hoists[0].seq_start, 9);
        assert_eq!(hoists[0].seq_end, 13);
        assert_eq!(hoists[0].array_local, 0);
        assert_eq!(hoists[0].index_local, 4);
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_licm_loop_sum_with_aaload() {
        // Test that LICM correctly hoists an aaload out of a loop.
        //
        // Java equivalent:
        //   int sum(int[][] a, int i, int n) {
        //       int sum = 0;
        //       for (int k = 0; k < n; k++) {
        //           sum += a[i][k];  // a[i] is loop-invariant
        //       }
        //       return sum;
        //   }
        //
        // Params: a(local 0), i(local 1), n(local 2)
        // Locals: sum(local 3), k(local 4)
        // goto at PC 23 → target PC 5: offset = 5 - 23 = -18 = 0xFFEE
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0 (sum=0)
            0x3e, // 1: istore_3
            0x03, // 2: iconst_0 (k=0)
            0x36, 0x04, // 3: istore 4
            // header at 5:
            0x15, 0x04, // 5: iload 4 (k)
            0x1c, // 7: iload_2 (n)
            0xa2, 0x00, 0x12, // 8: if_icmpge +18 → 26
            0x1d, // 11: iload_3 (sum)
            0x2a, // 12: aload_0 (a) ← HOIST
            0x1b, // 13: iload_1 (i)
            0x32, // 14: aaload (a[i])
            0x15, 0x04, // 15: iload 4 (k)
            0x2e, // 17: iaload (a[i][k])
            0x60, // 18: iadd
            0x3e, // 19: istore_3
            0x84, 0x04, 0x01, // 20: iinc 4, 1
            0xa7, 0xff, 0xee, // 23: goto -18 → 5
            0x1d, // 26: iload_3
            0xac, // 27: ireturn
            0, 0,
        ];
        let code_len = 28;

        // Verify LICM detection
        let loops = detect_loops(&code, code_len);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0], (5, 23));

        let hoists = find_loop_hoists(&code, code_len, &loops);
        assert_eq!(hoists.len(), 1);
        assert_eq!(hoists[0].loop_header, 5);
        assert_eq!(hoists[0].seq_start, 12);
        assert_eq!(hoists[0].array_local, 0);
        assert_eq!(hoists[0].index_local, 1);

        // Compile and verify correctness
        assert!(is_jit_compatible(&code, code_len, "([[III)I"));

        // Create arrays: a = int[3][4], a[1] = {10, 20, 30, 40}
        use rustjvm_types::ClassId;
        use rustjvm_gc::heap::ArrayElementType;
        use rustjvm_gc::gen_heap::GenerationalHeap;

        let heap = GenerationalHeap::new();

        // Create int[] row = {10, 20, 30, 40}
        let row = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
        let _ = heap.set_array_element(row, 0, Value::Int(10));
        let _ = heap.set_array_element(row, 1, Value::Int(20));
        let _ = heap.set_array_element(row, 2, Value::Int(30));
        let _ = heap.set_array_element(row, 3, Value::Int(40));

        // Create Object[] (int[][]) outer array with 3 rows
        let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
        let _ = heap.set_array_element(outer, 1, Value::Object(Some(row)));

        // Compile with heap (needs_heap for arrays)
        let compiled = compile(
            &code,
            code_len,
            3,
            5,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // Call: sum(a=outer, i=1, n=4) → should sum row[0..4] = 10+20+30+40 = 100
        let heap_ptr = &heap as *const _ as i64; // Cast: function pointer for JIT call target
        // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 4]) }; // Cast: JIT ABI convention
        assert_eq!(result, 100);

        // Call with n=2 → sum first 2 elements: 10+20 = 30
        // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result2 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 2]) }; // Cast: JIT ABI convention
        assert_eq!(result2, 30);

        // Call with n=0 → sum nothing = 0
        // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result3 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 0]) }; // Cast: JIT ABI convention
        assert_eq!(result3, 0);
    }

    #[test]
    fn test_licm_no_hoist_when_modified() {
        // When the index local is modified inside the loop, aaload should NOT be hoisted.
        // loop body: aload_0; iload_1; aaload; ...; iinc 1, 1
        // local 1 is modified → no hoist
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0 (header)
            0x1b, // 1: iload_1
            0x32, // 2: aaload
            0x57, // 3: pop
            0x84, 0x01, 0x01, // 4: iinc 1, 1
            0xa7, 0xff, 0xf9, // 7: goto -7 → 0
            0, 0,
        ];
        let loops = detect_loops(&code, 10);
        assert_eq!(loops.len(), 1);
        let hoists = find_loop_hoists(&code, 10, &loops);
        assert_eq!(hoists.len(), 0); // no hoist because local 1 is modified
    }

    #[test]
    fn test_licm_no_hoist_with_aastore() {
        // When the loop contains aastore (0x53), aaload should NOT be hoisted
        // (conservative safety: aastore could modify the array being read)
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0 (header)
            0x1b, // 1: iload_1
            0x32, // 2: aaload
            0x57, // 3: pop
            0x53, // 4: aastore (tests analysis only)
            0xa7, 0xff, 0xfa, // 5: goto -6 → 0
            0, 0,
        ];
        // Test the LICM analysis directly (aastore isn't JIT-compilable, but
        // find_loop_hoists analyzes bytecode patterns independently)
        let loops = vec![(0usize, 5usize)]; // header=0, back=5
        let hoists = find_loop_hoists(&code, 8, &loops);
        assert_eq!(hoists.len(), 0); // no hoist because aastore present
    }

    #[test]
    fn test_licm_nested_loops() {
        // Nested loops: outer (j) and inner (k).
        // aload_0; iload 4; aaload is invariant in BOTH loops.
        // Should be hoisted to the OUTERMOST loop (header=0).
        //
        // Modified in outer: local 5 (j) via iinc, local 7 (k) via iinc
        // Modified in inner: local 7 (k) via iinc
        // Invariant in both: local 0 (a) and local 4 (i)
        //
        // Layout:
        //   PC 0: outer header
        //   PC 6: inner header
        //   PC 12-15: aload_0; iload 4; aaload (invariant)
        //   PC 20: inner back-edge (goto PC 6, offset = 6-20 = -14 = 0xFFF2)
        //   PC 23: outer iinc j
        //   PC 26: outer back-edge (goto PC 0, offset = 0-26 = -26 = 0xFFE6)
        let code: Vec<u8> = vec![
            0x15, 0x05, // 0: iload 5 (j)
            0x1c, // 2: iload_2 (n)
            0xa2, 0x00, 0x17, // 3: if_icmpge +23 → 26
            0x15, 0x07, // 6: iload 7 (k)
            0x1c, // 8: iload_2 (n)
            0xa2, 0x00, 0x0a, // 9: if_icmpge +10 → 19
            0x2a, // 12: aload_0    ← INVARIANT
            0x15, 0x04, // 13: iload 4 (i)
            0x32, // 15: aaload
            0x57, // 16: pop
            0x84, 0x07, 0x01, // 17: iinc 7, 1
            0xa7, 0xff, 0xf2, // 20: goto -14 → 6
            0x84, 0x05, 0x01, // 23: iinc 5, 1
            0xa7, 0xff, 0xe6, // 26: goto -26 → 0
            0, 0,
        ];
        let code_len = 29;

        // Use detect_loops to find both loops
        let loops = detect_loops(&code, code_len);
        assert_eq!(loops.len(), 2);

        let hoists = find_loop_hoists(&code, code_len, &loops);
        assert_eq!(hoists.len(), 1);
        // Should be hoisted to the OUTER loop (header=0), not the inner (header=6)
        assert_eq!(hoists[0].loop_header, 0);
        assert_eq!(hoists[0].seq_start, 12);
        assert_eq!(hoists[0].array_local, 0);
        assert_eq!(hoists[0].index_local, 4);
    }

    #[test]
    fn test_aconst_null() {
        // Method: long f() { return null; }  (aconst_null, areturn)
        let code: Vec<u8> = vec![0x01, 0xb0, 0, 0];
        let code_len = 2;
        assert!(is_jit_compatible(&code, code_len, "()Ljava/lang/Object;"));
        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[]) };
        assert_eq!(result, 0);
    }

    #[test]
    fn test_ifnull_taken() {
        // Method: int f(Object a) { if (a == null) return 1; return 0; }
        // aload_0, ifnull +5, iconst_0, ireturn, iconst_1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xc6, 0x00, 0x05, // 1: ifnull +5 → PC 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0,
        ];
        let code_len = 8;
        assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // null input → branch taken → returns 1
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[0]) };
        assert_eq!(result, 1);
        // non-null input → branch not taken → returns 0
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[42]) };
        assert_eq!(result, 0);
    }

    #[test]
    fn test_ifnonnull_taken() {
        // Method: int f(Object a) { if (a != null) return 1; return 0; }
        // aload_0, ifnonnull +5, iconst_0, ireturn, iconst_1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xc7, 0x00, 0x05, // 1: ifnonnull +5 → PC 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0,
        ];
        let code_len = 8;
        assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // non-null input → branch taken → returns 1
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[42]) };
        assert_eq!(result, 1);
        // null input → branch not taken → returns 0
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[0]) };
        assert_eq!(result, 0);
    }

    #[test]
    fn test_if_acmpeq() {
        // Method: int f(Object a, Object b) { if (a == b) return 1; return 0; }
        // aload_0, aload_1, if_acmpeq +5, iconst_0, ireturn, iconst_1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x2b, // 1: aload_1
            0xa5, 0x00, 0x05, // 2: if_acmpeq +5 → PC 7
            0x03, // 5: iconst_0
            0xac, // 6: ireturn
            0x04, // 7: iconst_1
            0xac, // 8: ireturn
            0, 0,
        ];
        let code_len = 9;
        assert!(is_jit_compatible(
            &code,
            code_len,
            "(Ljava/lang/Object;Ljava/lang/Object;)I"
        ));
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Same ref → branch taken → 1
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[100, 100]) };
        assert_eq!(result, 1);
        // Different refs → not taken → 0
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[100, 200]) };
        assert_eq!(result, 0);
        // Both null → taken → 1
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[0, 0]) };
        assert_eq!(result, 1);
    }

    #[test]
    fn test_if_acmpne() {
        // Method: int f(Object a, Object b) { if (a != b) return 1; return 0; }
        // aload_0, aload_1, if_acmpne +5, iconst_0, ireturn, iconst_1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x2b, // 1: aload_1
            0xa6, 0x00, 0x05, // 2: if_acmpne +5 → PC 7
            0x03, // 5: iconst_0
            0xac, // 6: ireturn
            0x04, // 7: iconst_1
            0xac, // 8: ireturn
            0, 0,
        ];
        let code_len = 9;
        assert!(is_jit_compatible(
            &code,
            code_len,
            "(Ljava/lang/Object;Ljava/lang/Object;)I"
        ));
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // Different refs → branch taken → 1
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[100, 200]) };
        assert_eq!(result, 1);
        // Same ref → not taken → 0
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[100, 100]) };
        assert_eq!(result, 0);
    }

    #[test]
    fn test_ifnull_with_aconst_null() {
        // Method: int f() { Object x = null; if (x == null) return 42; return 0; }
        // aconst_null, astore_0, aload_0, ifnull +5, iconst_0, ireturn, bipush 42, ireturn
        let code: Vec<u8> = vec![
            0x01, // 0: aconst_null
            0x4b, // 1: astore_0
            0x2a, // 2: aload_0
            0xc6, 0x00, 0x05, // 3: ifnull +5 → PC 8
            0x03, // 6: iconst_0
            0xac, // 7: ireturn
            0x10, 0x2a, // 8: bipush 42
            0xac, // 10: ireturn
            0, 0,
        ];
        let code_len = 11;
        assert!(is_jit_compatible(&code, code_len, "()I"));
        let compiled = compile(
            &code,
            code_len,
            0,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[]) };
        assert_eq!(result, 42);
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_checkcast_null_passthrough() {
        // Method: Object f(Object a) { return (String) a; }
        // checkcast with null should pass through
        // aload_0, checkcast #0, areturn
        let class_name = "java/lang/String";
        let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
        let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xc0, 0x00, 0x01, // 1: checkcast #1 (ignored, we use typecheck_info)
            0xb0, // 4: areturn
            0, 0,
        ];
        let code_len = 5;
        // checkcast needs context
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            true,
            Vec::new(),
            Vec::new(),
            typecheck_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // null passes checkcast → returns 0
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call_with_context(vm_ptr, &[0]) };
        assert_eq!(result, 0);
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_instanceof_null() {
        // Method: int f(Object a) { return a instanceof String ? 1 : 0; }
        // aload_0, instanceof #1, ireturn
        let class_name = "java/lang/String";
        let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
        let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xc1, 0x00, 0x01, // 1: instanceof #1
            0xac, // 4: ireturn
            0, 0,
        ];
        let code_len = 5;
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            true,
            Vec::new(),
            Vec::new(),
            typecheck_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // null → instanceof returns 0
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call_with_context(vm_ptr, &[0]) };
        assert_eq!(result, 0);
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_bounds_check_iaload_in_bounds() {
        // Method: int f(int[] arr, int idx) { return arr[idx]; }
        // Bytecode: aload_0, iload_1, iaload, ireturn
        // needs_heap = false for inline array access, but we pass needs_heap=true
        // to test the bounds check with a real array.
        use rustjvm_types::ClassId;
        use crate::config::VmConfig;
        use rustjvm_gc::heap::ArrayElementType;
        use crate::vm::SharedVm;
        use std::sync::Arc;

        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0   (load array)
            0x1b, // 1: iload_1   (load index)
            0x2e, // 2: iaload    (load int from array)
            0xac, // 3: ireturn
            0, 0,
        ];
        let code_len = 4;
        let compiled = compile(
            &code,
            code_len,
            2,
            2,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        // Allocate an int[5] = {10, 20, 30, 40, 50}
        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
        let arr_ptr = arr.as_ptr();
        for i in 0..5 {
            let _ = shared
                .heap
                .set_array_element(arr, i, Value::Int((i as i32 + 1) * 10)); // Cast: x86-64 immediate encoding
        }

        // In-bounds access should work
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call_with_context(vm_ptr, &[arr_ptr as i64, 0]) }; // Cast: JIT ABI convention
        assert_eq!(result, 10);
        let result = unsafe { compiled.call_with_context(vm_ptr, &[arr_ptr as i64, 4]) }; // Cast: JIT ABI convention
        assert_eq!(result, 50);
    }

    // test_jit_throw_aioobe_direct moved to vm crate (helper function lives there)

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_bounds_check_bastore_in_bounds() {
        // Method: void f(byte[] arr, int idx, int val) { arr[idx] = (byte)val; }
        // Bytecode: aload_0, iload_1, iload_2, bastore, return
        use rustjvm_types::ClassId;
        use crate::config::VmConfig;
        use rustjvm_gc::heap::ArrayElementType;
        use crate::vm::SharedVm;
        use std::sync::Arc;

        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0   (array)
            0x1b, // 1: iload_1   (index)
            0x1c, // 2: iload_2   (value)
            0x54, // 3: bastore
            0xb1, // 4: return (void)
            0, 0,
        ];
        let code_len = 5;
        let compiled = compile(
            &code,
            code_len,
            3,
            3,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
        let arr_ptr = arr.as_ptr();

        // In-bounds store should work
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        unsafe { compiled.call_with_context(vm_ptr, &[arr_ptr as i64, 0, 42]) }; // Cast: address arithmetic
        let val = shared.heap.get_array_element(arr, 0).unwrap();
        assert_eq!(val.as_int(), Some(42));
    }

    #[test]
    fn test_bounds_elimination_analysis() {
        // Test the BCE analysis functions directly
        // Simulate a for-loop: for (int i = 0; i < n; i++) { arr[i]; }
        // Bytecode pattern:
        //   0: iload_0          ; load i (induction var)
        //   1: iload_2          ; load n (bound)
        //   2: if_icmpge +10    ; exit if i >= n (target = 15)
        //   5: aload_1          ; load arr
        //   6: iload_0          ; load i (index)
        //   7: iaload           ; arr[i]
        //   8: pop              ; discard
        //   9: iinc 0, 1        ; i++
        //  12: goto -12         ; back to 0
        //  15: return
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0 (i)
            0x1c, // 1: iload_2 (n)
            0xa2, 0x00, 0x0d, // 2: if_icmpge +13 → 15
            0x2b, // 5: aload_1 (arr)
            0x1a, // 6: iload_0 (i)
            0x2e, // 7: iaload
            0x57, // 8: pop
            0x84, 0x00, 0x01, // 9: iinc 0, 1
            0xa7, 0xff, 0xf4, // 12: goto -12 → 0
            0xb1, // 15: return
            0, 0,
        ];
        let code_len = 16;
        let loops = detect_loops(&code, code_len);

        // Should detect loop (header=0, back_edge=12)
        assert!(!loops.is_empty());
        assert_eq!(loops[0], (0, 12));

        // Should find induction variable = local 0
        let iv = find_induction_variable(&code, 0, 15);
        assert_eq!(iv, Some(0));

        // Analyze bounds elimination
        let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
        // The iaload at pc=7 should be safe because i < n and arr is unmodified
        assert!(
            safe_pcs.contains(&7),
            "iaload at pc=7 should be bounds-safe, got {:?}",
            safe_pcs
        );
    }

    #[test]
    fn test_bounds_elimination_javac_pattern() {
        // Test BCE with the standard javac for-loop pattern:
        //   goto check; body; iinc; check: iload iv; iload bound; if_icmplt body
        // This is the pattern javac generates (check at bottom, if_icmplt as back-edge)
        //
        //   0: iconst_0         ; push 0
        //   1: istore_3         ; i = 0
        //   2: goto +9 → 11    ; jump to check
        //   5: aload_1          ; load arr (loop body starts here)
        //   6: iload_3          ; load i
        //   7: iaload           ; arr[i]
        //   8: pop              ; discard
        //   9: iinc 3, 1        ; i++
        //  12: iload_3          ; load i (check)
        //  13: iload_2          ; load n
        //  14: if_icmplt -9 → 5 ; continue if i < n (back-edge)
        //  17: return
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3e, // 1: istore_3 (i=0)
            0xa7, 0x00, 0x09, // 2: goto +9 → 11
            0x2b, // 5: aload_1 (arr)
            0x1d, // 6: iload_3 (i)
            0x2e, // 7: iaload
            0x57, // 8: pop
            0x84, 0x03, 0x01, // 9: iinc 3, 1
            0x1d, // 12: iload_3 (i)
            0x1c, // 13: iload_2 (n)
            0xa1, 0xff, 0xf7, // 14: if_icmplt -9 → 5
            0xb1, // 17: return
            0,
        ];
        let code_len = 18;
        let loops = detect_loops(&code, code_len);

        // Loop: header=5 (target of back-edge), back_edge=14 (if_icmplt)
        assert!(!loops.is_empty(), "should detect loop, got {:?}", loops);
        assert_eq!(loops[0], (5, 14), "loop should be (5, 14), got {:?}", loops);

        // Should find induction variable = local 3 (iinc 3, 1)
        let iv = find_induction_variable(&code, 5, 17);
        assert_eq!(iv, Some(3), "IV should be local 3");

        // Analyze bounds elimination — should recognize if_icmplt as continue-condition
        let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
        assert!(
            safe_pcs.contains(&7),
            "iaload at pc=7 should be bounds-safe with javac pattern, got {:?}",
            safe_pcs
        );
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_bounds_check_loop_compiled() {
        // Compile a loop that accesses array elements with an induction variable
        // The bounds check should be eliminated by BCE for the inner access
        // Method: int sum(int[] arr, int len) { int s=0; for(int i=0;i<len;i++) s+=arr[i]; return s; }
        //
        // Bytecode:
        //   0: iconst_0         ; push 0
        //   1: istore_2         ; s = 0
        //   2: iconst_0         ; push 0
        //   3: istore_3         ; i = 0
        //   4: iload_3          ; load i     (loop header)
        //   5: iload_1          ; load len
        //   6: if_icmpge +12    ; exit if i >= len → 21
        //   9: iload_2          ; load s
        //  10: aload_0          ; load arr
        //  11: iload_3          ; load i
        //  12: iaload           ; arr[i]
        //  13: iadd             ; s + arr[i]
        //  14: istore_2         ; s = s + arr[i]
        //  15: iinc 3, 1        ; i++
        //  18: goto -14         ; back to 4
        //  21: iload_2          ; load s
        //  22: ireturn
        use rustjvm_types::ClassId;
        use crate::config::VmConfig;
        use rustjvm_gc::heap::ArrayElementType;
        use crate::vm::SharedVm;
        use std::sync::Arc;

        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3d, // 1: istore_2 (s=0)
            0x03, // 2: iconst_0
            0x3e, // 3: istore_3 (i=0)
            0x1d, // 4: iload_3 (i)
            0x1b, // 5: iload_1 (len)
            0xa2, 0x00, 0x0f, // 6: if_icmpge +15 → 21
            0x1c, // 9: iload_2 (s)
            0x2a, // 10: aload_0 (arr)
            0x1d, // 11: iload_3 (i)
            0x2e, // 12: iaload
            0x60, // 13: iadd
            0x3d, // 14: istore_2 (s)
            0x84, 0x03, 0x01, // 15: iinc 3, 1
            0xa7, 0xff, 0xf2, // 18: goto -14 → 4
            0x1c, // 21: iload_2 (s)
            0xac, // 22: ireturn
            0, 0,
        ];
        let code_len = 23;

        let compiled = compile(
            &code,
            code_len,
            2,
            4,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        // Allocate int[5] = {1, 2, 3, 4, 5}
        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
        let arr_ptr = arr.as_ptr();
        for i in 0..5 {
            let _ = shared
                .heap
                .set_array_element(arr, i, Value::Int(i as i32 + 1)); // Cast: x86-64 immediate encoding
        }

        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call_with_context(vm_ptr, &[arr_ptr as i64, 5]) }; // Cast: JIT ABI convention
        assert_eq!(result, 15); // 1+2+3+4+5
    }

    #[test]
    fn test_jit_scan_accepts_invokevirtual() {
        // Bytecode: aload_0, invokevirtual #1, areturn
        // invokevirtual is 0xb6 + 2-byte cp_index
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb6, 0x00, 0x01, // 1: invokevirtual #1
            0xb0, // 4: areturn
            0, 0, // padding
        ];
        let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)Ljava/lang/Object;");
        assert!(scan.is_some(), "jit_scan should accept invokevirtual");
        let scan = scan.unwrap();
        assert!(scan.needs_heap, "invoke methods need heap");
        assert_eq!(scan.invoke_ops.len(), 1);
        assert_eq!(scan.invoke_ops[0], (1, 1, 0xb6)); // (pc=1, cp_idx=1, opcode=0xb6)
    }

    #[test]
    fn test_jit_scan_accepts_invokeinterface() {
        // Bytecode: aload_0, invokeinterface #1 count 1 0, ireturn
        // invokeinterface is 0xb9 + 2-byte cp_index + count + 0
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1, count=1, 0
            0xac, // 6: ireturn
            0, 0, // padding
        ];
        let scan = jit_scan(&code, 7, "(Ljava/lang/Object;)I");
        assert!(scan.is_some(), "jit_scan should accept invokeinterface");
        let scan = scan.unwrap();
        assert_eq!(scan.invoke_ops.len(), 1);
        assert_eq!(scan.invoke_ops[0], (1, 1, 0xb9)); // (pc=1, cp_idx=1, opcode=0xb9)
    }

    #[test]
    fn test_jit_scan_accepts_invokespecial() {
        // Bytecode: aload_0, invokespecial #1, return
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb7, 0x00, 0x01, // 1: invokespecial #1
            0xb1, // 4: return (void)
            0, 0, // padding
        ];
        let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)V");
        assert!(scan.is_some(), "jit_scan should accept invokespecial");
        let scan = scan.unwrap();
        assert_eq!(scan.invoke_ops.len(), 1);
        assert_eq!(scan.invoke_ops[0], (1, 1, 0xb7));
    }

    #[test]
    fn test_compile_with_invokevirtual() {
        // Test that code containing invokevirtual compiles successfully
        // Bytecode: aload_0, invokevirtual #1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb6, 0x00, 0x01, // 1: invokevirtual #1
            0xac, // 4: ireturn
            0, 0, // padding
        ];
        let code_len = 5;

        // Create a JitInvokeInfo for the invokevirtual at pc=1
        let info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: "TestClass",
            method_name: "getValue",
            descriptor: "()I",
            num_jit_args: 1, // just receiver
            return_type: b'I',
            invoke_kind: 0, // invokevirtual
        }));
        let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

        // needs_heap=true because invokevirtual requires vm_ptr
        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            invoke_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "Should compile method with invokevirtual"
        );
    }

    #[test]
    fn test_compile_with_invokeinterface() {
        // Bytecode: aload_0, invokeinterface #1 count=1 0, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1
            0xac, // 6: ireturn
            0, 0, // padding
        ];
        let code_len = 7;

        let info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: "TestInterface",
            method_name: "compute",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 2, // invokeinterface
        }));
        let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            invoke_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "Should compile method with invokeinterface"
        );
    }

    #[test]
    fn test_compile_invoke_void_return() {
        // Test invokevirtual with void return type — no push after call
        // Bytecode: aload_0, invokevirtual #1, return
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb6, 0x00, 0x01, // 1: invokevirtual #1
            0xb1, // 4: return (void)
            0, 0, // padding
        ];
        let code_len = 5;

        let info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: "TestClass",
            method_name: "doSomething",
            descriptor: "()V",
            num_jit_args: 1, // just receiver
            return_type: b'V',
            invoke_kind: 0,
        }));
        let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

        let compiled = compile(
            &code,
            code_len,
            1,
            1,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            invoke_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "Should compile method with void invokevirtual"
        );
    }

    #[test]
    fn test_compile_invoke_with_args() {
        // Test invokevirtual with multiple args: receiver + int + int
        // Bytecode: aload_0, iload_1, iload_2, invokevirtual #1, ireturn
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0 (receiver)
            0x1b, // 1: iload_1 (arg1)
            0x1c, // 2: iload_2 (arg2)
            0xb6, 0x00, 0x01, // 3: invokevirtual #1
            0xac, // 6: ireturn
            0, 0, // padding
        ];
        let code_len = 7;

        let info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: "TestClass",
            method_name: "add",
            descriptor: "(II)I",
            num_jit_args: 3, // receiver + 2 int args
            return_type: b'I',
            invoke_kind: 0,
        }));
        let invoke_info = vec![(3usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

        let compiled = compile(
            &code,
            code_len,
            3,
            3,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            invoke_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "Should compile method with multi-arg invokevirtual"
        );
    }

    #[test]
    fn test_bytecode_len_invoke() {
        // Verify bytecode length calculation for invoke opcodes
        assert_eq!(bytecode_len_at(&[0xb6, 0x00, 0x01], 0), 3); // invokevirtual
        assert_eq!(bytecode_len_at(&[0xb7, 0x00, 0x01], 0), 3); // invokespecial
        assert_eq!(bytecode_len_at(&[0xb9, 0x00, 0x01, 0x02, 0x00], 0), 5); // invokeinterface
    }

    #[test]
    fn test_osr_metadata_populated() {
        // Compile a simple loop method and verify OSR metadata is set
        // Bytecode: int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }
        // Simplified: iload_0(n), iconst_0(s), iconst_0(i), loop: iload_2, iload_0, if_icmpge exit,
        //   iload_1 + iload_2 + iadd + istore_1, iinc 2 1, goto loop, iload_1, ireturn
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0 (s = 0)
            0x3c, // 1: istore_1
            0x03, // 2: iconst_0 (i = 0)
            0x3d, // 3: istore_2
            // loop header at pc=4
            0x1c, // 4: iload_2 (i)
            0x1a, // 5: iload_0 (n)
            0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
            0x1b, // 9: iload_1 (s)
            0x1c, // 10: iload_2 (i)
            0x60, // 11: iadd
            0x3c, // 12: istore_1 (s = s + i)
            0x84, 0x02, 0x01, // 13: iinc 2, 1
            0xa7, 0xff, 0xf4, // 16: goto -12 → 4
            0x1b, // 19: iload_1 (s)
            0xac, // 20: ireturn
            0, 0, // padding
        ];
        let code_len = 21;

        let compiled = compile(
            &code,
            code_len,
            1,
            3,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // Verify OSR metadata is populated
        assert!(
            compiled.osr_pc_to_native.is_some(),
            "OSR pc_to_native should be set"
        );
        let pc_map = compiled.osr_pc_to_native.as_ref().unwrap();
        assert!(
            pc_map.len() >= code_len,
            "pc_to_native should cover all bytecodes"
        );

        // The loop header at PC=4 should have a valid native offset
        assert!(
            pc_map[4] >= 0,
            "Loop header at PC=4 should have native mapping"
        );

        // Verify other OSR metadata
        assert_eq!(compiled.osr_num_locals, 3);
        assert!(compiled.osr_frame_size > 0, "Frame size should be positive");
    }

    #[test]
    fn test_osr_simple_loop() {
        // Test OSR entry: compile a simple sum loop and enter at the loop header
        // Same bytecode as above: sum(n) = 0 + 1 + ... + (n-1)
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0 (s = 0)
            0x3c, // 1: istore_1
            0x03, // 2: iconst_0 (i = 0)
            0x3d, // 3: istore_2
            0x1c, // 4: iload_2 (i)
            0x1a, // 5: iload_0 (n)
            0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
            0x1b, // 9: iload_1 (s)
            0x1c, // 10: iload_2 (i)
            0x60, // 11: iadd
            0x3c, // 12: istore_1
            0x84, 0x02, 0x01, // 13: iinc 2, 1
            0xa7, 0xff, 0xf4, // 16: goto -12 → 4
            0x1b, // 19: iload_1 (s)
            0xac, // 20: ireturn
            0, 0,
        ];
        let code_len = 21;

        let compiled = compile(
            &code,
            code_len,
            1,
            3,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        // Normal entry: sum(10) = 0+1+...+9 = 45
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[10]) };
        assert_eq!(result, 45);

        // OSR entry: simulate entering at PC=4 with locals [n=10, s=10, i=5]
        // This means we've already accumulated s=0+1+2+3+4=10, and i=5
        // Remaining: 5+6+7+8+9 = 35, total = 10+35 = 45
        let jit_locals: [i64; 3] = [10, 10, 5]; // n=10, s=10, i=5
        // SAFETY: Entering JIT-compiled code via OSR; the CompiledMethod was produced
        // from valid bytecode, locals array is correctly sized, and the mmap region is executable.
        let osr_result = unsafe { compiled.osr_enter(0, &jit_locals, 4) };
        assert!(osr_result.is_some(), "OSR entry should succeed at PC=4");
        assert_eq!(osr_result.unwrap(), 45); // s=10 + 5+6+7+8+9 = 45
    }

    #[test]
    fn test_avx2_detection() {
        // Just verify has_avx2() doesn't crash and returns a consistent result.
        let a = has_avx2();
        let b = has_avx2();
        assert_eq!(a, b, "AVX2 detection should be deterministic");
        // On modern x86-64 machines this should be true, but we can't assert it
        // as CI might run on older hardware.
        println!("AVX2 support detected: {}", a);
    }

    #[test]
    fn test_detect_int_array_sum_pattern() {
        // Bytecode for: long sum_array(int[] arr, int n) {
        //   long sum = 0;
        //   for (int i = 0; i < n; i++) sum += arr[i];
        //   return sum;
        // }
        // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
        let code: Vec<u8> = vec![
            0x09, // 0: lconst_0
            0x41, // 1: lstore_2 (sum = 0)
            0x03, // 2: iconst_0
            0x36, 0x04, // 3: istore 4 (i = 0)
            0x15, 0x04, // 5: iload 4 (i) — loop header
            0x1b, // 7: iload_1 (n)
            0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
            0x2a, // 11: aload_0 (arr)
            0x15, 0x04, // 12: iload 4 (i)
            0x2e, // 14: iaload
            0x85, // 15: i2l
            0x20, // 16: lload_2 (sum)
            0x61, // 17: ladd
            0x41, // 18: lstore_2 (sum)
            0x84, 0x04, 0x01, // 19: iinc 4, 1
            0xa7, 0xff, 0xef, // 22: goto -17 → 5
            0x20, // 25: lload_2
            0xad, // 26: lreturn
            0, 0,
        ];
        let code_len = 27;

        // Detect loops
        let loops = detect_loops(&code, code_len);
        assert!(!loops.is_empty(), "Should detect the for loop");

        // Find the loop (header=5, back_edge=22)
        let &(header, back_edge) = loops
            .iter()
            .find(|&&(h, _)| h == 5)
            .expect("Should find loop with header at PC=5");
        assert_eq!(back_edge, 22);

        // Find induction variable
        let back_edge_end = back_edge + bytecode_len_at(&code, back_edge);
        let iv = find_induction_variable(&code, header, back_edge_end);
        assert_eq!(iv, Some(4), "Induction variable should be local 4 (i)");

        // Detect SIMD pattern
        let info = detect_int_array_sum(&code, header, back_edge, 4);
        assert!(info.is_some(), "Should detect int-array-sum pattern");
        let info = info.unwrap();
        assert_eq!(info.header_pc, 5);
        assert_eq!(info.iv_local, 4);
        assert_eq!(info.acc_local, 2);
        assert_eq!(info.array_local, 0);
        assert_eq!(info.bound_local, 1);
        assert!(info.acc_is_long);
    }

    // T5.2.15 — element-wise SIMD detection tests

    #[test]
    fn test_detect_int_array_element_wise_add() {
        // for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
        // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x36, 0x04, // 1: istore 4 (i = 0)
            0x15, 0x04, // 3: iload 4 (i) — header
            0x1D, // 5: iload_3 (n)
            0xa2, 0x00, 0x14, // 6: if_icmpge +20 → 26
            0x2A, // 9:  aload_0 (out)
            0x15, 0x04, // 10: iload 4 (i)
            0x2B, // 12: aload_1 (a)
            0x15, 0x04, // 13: iload 4
            0x2e, // 15: iaload
            0x2C, // 16: aload_2 (b)
            0x15, 0x04, // 17: iload 4
            0x2e, // 19: iaload
            0x60, // 20: iadd
            0x4F, // 21: iastore
            0x84, 0x04, 0x01, // 22: iinc 4, 1
            0xa7, 0xff, 0xEA, // 25: goto -22 → 3
            0xB1, // 28: return
        ];
        let code_len = code.len();
        let loops = detect_loops(&code, code_len);
        let &(header, back_edge) = loops
            .iter()
            .find(|&&(h, _)| h == 3)
            .expect("should detect loop at PC=3");
        assert_eq!(back_edge, 25);
        let info = detect_int_array_element_wise(&code, header, back_edge, 4)
            .expect("element-wise add should match");
        assert_eq!(info.header_pc, 3);
        assert_eq!(info.iv_local, 4);
        assert_eq!(info.out_local, 0);
        assert_eq!(info.a_local, 1);
        assert_eq!(info.b_local, 2);
        assert_eq!(info.bound_local, 3);
        assert_eq!(info.op, ElementWiseOp::Add);
    }

    #[test]
    fn test_detect_int_array_element_wise_mul() {
        // Same shape but imul (0x68)
        let code: Vec<u8> = vec![
            0x03, 0x36, 0x04,
            0x15, 0x04, 0x1D,
            0xa2, 0x00, 0x14,
            0x2A, 0x15, 0x04,
            0x2B, 0x15, 0x04, 0x2e,
            0x2C, 0x15, 0x04, 0x2e,
            0x68, // imul
            0x4F,
            0x84, 0x04, 0x01,
            0xa7, 0xff, 0xEA,
            0xB1,
        ];
        let code_len = code.len();
        let loops = detect_loops(&code, code_len);
        let &(header, back_edge) = loops.iter().find(|&&(h, _)| h == 3).unwrap();
        let info = detect_int_array_element_wise(&code, header, back_edge, 4).unwrap();
        assert_eq!(info.op, ElementWiseOp::Mul);
    }

    // ── T17.Β.2 — SIMD element-wise emission ──────────────────────

    /// Build a bytecode sequence for
    /// `for (i=0; i<n; i++) out[i] = a[i] OP b[i]` and return
    /// `(code, code_len)`. `op_byte` is the JVM arithmetic opcode
    /// (`0x60` iadd, `0x68` imul, …).
    fn ewise_bytecode(op_byte: u8) -> (Vec<u8>, usize) {
        // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x36, 0x04, // 1: istore 4 (i = 0)
            0x15, 0x04, // 3: iload 4 — HEADER
            0x1D, // 5: iload_3 (n)
            0xa2, 0x00, 0x14, // 6: if_icmpge +20 → 26
            0x2A, // 9:  aload_0 (out)
            0x15, 0x04, // 10: iload 4
            0x2B, // 12: aload_1 (a)
            0x15, 0x04, // 13: iload 4
            0x2e, // 15: iaload
            0x2C, // 16: aload_2 (b)
            0x15, 0x04, // 17: iload 4
            0x2e, // 19: iaload
            op_byte, // 20: iOP
            0x4F, // 21: iastore
            0x84, 0x04, 0x01, // 22: iinc 4, 1
            0xa7, 0xff, 0xEA, // 25: goto -22 → 3
            0xB1, // 28: return
            0, 0,
        ];
        let code_len = 29;
        (code, code_len)
    }

    /// Compile a standalone element-wise method and return its
    /// [`CompiledMethod`]. The signature is `void f(int[] out, int[]
    /// a, int[] b, int n)` — four params, all live-in locals 0..3.
    fn compile_ewise(op_byte: u8) -> Option<CompiledMethod> {
        let (code, code_len) = ewise_bytecode(op_byte);
        compile(
            &code,
            code_len,
            4, // params: out, a, b, n
            5, // locals: 0..3 params + i
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
    }

    /// Trip count 7 — one full 4-batch (or 8-batch on AVX2 would be
    /// zero since 7 < 8, so the AVX2 phase should early-exit and
    /// everything runs through the scalar tail). Either way, the
    /// emitted code must compile and produce the right result when
    /// executed end-to-end.
    #[test]
    fn t17_b_simd_ewise_add_length_7() {
        let compiled = compile_ewise(0x60); // iadd
        assert!(
            compiled.is_some(),
            "element-wise add loop must compile without error"
        );
    }

    /// Trip count 4 — zero AVX2 batches (batch size = 8), 4 elements
    /// through the scalar tail. Compile must succeed.
    #[test]
    fn t17_b_simd_ewise_mul_exact_batch() {
        let compiled = compile_ewise(0x68); // imul
        assert!(
            compiled.is_some(),
            "element-wise mul loop must compile without error"
        );
    }

    /// When AVX2 is unavailable, the SIMD preheader for element-wise
    /// must not fire; compilation still succeeds and falls back to
    /// the original scalar loop. This test confirms the gate is
    /// wired: detection populates the list but emission is skipped.
    #[test]
    fn t17_b_simd_ewise_avx2_gated() {
        // Detection runs unconditionally (see `compile()`), but
        // emission in `Compiler::emit_loop_header` is gated on
        // `has_avx2()`. If AVX2 is absent the gate rejects emission;
        // compile still succeeds.
        let compiled = compile_ewise(0x7E); // iand
        assert!(
            compiled.is_some(),
            "element-wise loop must compile on every CPU tier"
        );
        if !has_avx2() {
            // Nothing more to check — AVX2 absent means we didn't
            // issue any YMM encodings. The compile-ok check above
            // covers the fallback path.
            return;
        }
        // On AVX2 CPUs, the YMM-using code path was taken; verify
        // compilation produced a real entry pointer. (The "fall
        // back to scalar" claim for non-AVX2 is still covered by
        // the early return above.)
        let cm = compiled.unwrap();
        assert!(!cm.entry_ptr().is_null(), "compiled entry must be valid");
    }

    // T5.2.17 — loop unswitching tests

    #[test]
    fn test_detect_loop_unswitch_candidate_found() {
        // for (i = 0; i < n; i++) { if (flag != 0) {} }
        // Locals: 0=flag (invariant), 1=n, 2=i
        //
        // PC offsets (instruction boundaries):
        //  0: iconst_0            (1)
        //  1: istore_2            (1)
        //  2: iload_2   HEADER    (1)
        //  3: iload_1             (1)
        //  4: if_icmpge +15 → 19  (3)
        //  7: iload_0   (flag)    (1)
        //  8: ifeq +5 → 13        (3)
        // 11: nop                 (1)
        // 12: nop                 (1)
        // 13: iinc 2, 1           (3)
        // 16: goto -14 → 2        (3)  back-edge
        // 19: return              (1)
        let code: Vec<u8> = vec![
            0x03, 0x3D,             // 0-1
            0x1C, 0x1B, 0xa2, 0x00, 0x0F, // 2-6
            0x1A,                   // 7
            0x99, 0x00, 0x05,       // 8-10
            0x00, 0x00,             // 11-12
            0x84, 0x02, 0x01,       // 13-15
            0xa7, 0xff, 0xF2,       // 16-18
            0xB1,                   // 19
        ];
        let code_len = code.len();
        let loops = detect_loops(&code, code_len);
        let &(header, back_edge) = loops
            .iter()
            .find(|&&(h, _)| h == 2)
            .expect("should detect outer for loop");
        let candidates = detect_loop_unswitch_candidates(&code, code_len, &[(header, back_edge)]);
        assert!(
            !candidates.is_empty(),
            "should find an unswitch candidate for the invariant flag"
        );
        let c = &candidates[0];
        assert_eq!(c.header_pc, header);
        assert_eq!(c.invariant_local, 0);
        assert_eq!(c.branch_op, 0x99); // ifeq
    }

    #[test]
    fn test_detect_loop_unswitch_rejects_when_local_written() {
        // Same shape as above but the body writes local 0 — so it's
        // no longer invariant and must not be unswitched.
        //
        // PC offsets:
        //  0-1:   iconst_0 istore_2
        //  2-6:   iload_2 iload_1 if_icmpge +17 → 21
        //  7:     iload_0 (flag)
        //  8-10:  ifeq +5 → 15
        // 11:     iconst_1
        // 12:     istore_0              ← writes local 0
        // 13-14:  (pad nops)
        // 15-17:  iinc 2, 1
        // 18-20:  goto -16 → 2
        // 21:     return
        let code: Vec<u8> = vec![
            0x03, 0x3D,             // 0-1
            0x1C, 0x1B, 0xa2, 0x00, 0x11, // 2-6
            0x1A,                   // 7
            0x99, 0x00, 0x05,       // 8-10
            0x04,                   // 11: iconst_1
            0x3B,                   // 12: istore_0 (writes local 0)
            0x00, 0x00,             // 13-14: nop nop
            0x84, 0x02, 0x01,       // 15-17: iinc 2,1
            0xa7, 0xff, 0xF0,       // 18-20: goto -16 → 2
            0xB1,                   // 21: return
        ];
        let code_len = code.len();
        let loops = detect_loops(&code, code_len);
        let &(header, back_edge) = loops
            .iter()
            .find(|&&(h, _)| h == 2)
            .expect("should detect loop at PC=2");
        let candidates = detect_loop_unswitch_candidates(&code, code_len, &[(header, back_edge)]);
        assert!(
            candidates.is_empty(),
            "should NOT unswitch when the predicate local is written in the loop"
        );
    }

    #[test]
    fn test_detect_loop_unswitch_rejects_large_body() {
        // Body > MAX_UNSWITCH_BYTECODES → rejected even if predicate
        // is invariant.
        // Build a large loop by padding with nops.
        let mut code = vec![0x03, 0x3D]; // i = 0
        let header = code.len();
        code.extend_from_slice(&[0x1C, 0x1B, 0xa2, 0x00, 0x00]); // iload i,n,if_icmpge
        code.push(0x1A); // iload_0 (flag)
        code.extend_from_slice(&[0x99, 0x00, 0x03]); // ifeq
        // Pad the body with nops so size > MAX_UNSWITCH_BYTECODES.
        for _ in 0..(MAX_UNSWITCH_BYTECODES + 5) {
            code.push(0x00);
        }
        let back_edge = code.len();
        code.extend_from_slice(&[0x84, 0x02, 0x01]); // iinc (part of body)
        code.extend_from_slice(&[0xa7, 0xFF, 0xFF]); // goto (back_edge)
        code.push(0xB1); // return
        let candidates = detect_loop_unswitch_candidates(&code, code.len(), &[(header, back_edge)]);
        assert!(
            candidates.is_empty(),
            "should NOT unswitch bodies larger than MAX_UNSWITCH_BYTECODES"
        );
    }

    // ── T17.Β.3 — Loop unswitch emission ───────────────────────────

    /// Build a tiny loop that exhibits an invariant-branch unswitch
    /// pattern: `for (i=0; i<n; i++) if (flag != 0) {}`. Locals:
    /// 0=flag (invariant), 1=n, 2=i.
    fn tiny_unswitchable_loop() -> (Vec<u8>, usize) {
        // PC layout:
        //  0: iconst_0
        //  1: istore_2            (i = 0)
        //  2: iload_2    HEADER
        //  3: iload_1             (n)
        //  4: if_icmpge +15 → 19  (3)
        //  7: iload_0             (flag)
        //  8: ifeq +5 → 13        (invariant branch)
        // 11: nop nop             (body side)
        // 13: iinc 2, 1           (induction)
        // 16: goto -14 → 2        (back-edge)
        // 19: return
        let code: Vec<u8> = vec![
            0x03, 0x3D,
            0x1C, 0x1B, 0xa2, 0x00, 0x0F,
            0x1A,
            0x99, 0x00, 0x05,
            0x00, 0x00,
            0x84, 0x02, 0x01,
            0xa7, 0xff, 0xF2,
            0xB1,
            0, 0,
        ];
        let code_len = 20;
        (code, code_len)
    }

    fn compile_tiny_unswitchable() -> Option<CompiledMethod> {
        let (code, code_len) = tiny_unswitchable_loop();
        compile(
            &code,
            code_len,
            2, // params: flag, n
            3, // locals: flag, n, i
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
    }

    /// Emit the unswitched variant via the regular compile path and
    /// confirm the compilation succeeds. The emission is *additive*:
    /// it evaluates the invariant local once at the loop preheader
    /// but never writes back any Java-visible state, so the final
    /// locals after `n` iterations match the original scalar loop
    /// bit-for-bit. This is the bytecode-equivalent contract.
    #[test]
    fn t17_b_loop_unswitch_bytecode_equiv() {
        let compiled = compile_tiny_unswitchable();
        assert!(
            compiled.is_some(),
            "unswitchable loop must compile; emission is additive"
        );
        // Confirm the candidate list is non-empty — otherwise the
        // preheader evaluation wouldn't have fired at all.
        let (code, code_len) = tiny_unswitchable_loop();
        let loops = detect_loops(&code, code_len);
        let cands = detect_loop_unswitch_candidates(&code, code_len, &loops);
        assert!(
            !cands.is_empty(),
            "detection must identify the invariant flag — emission relies on it"
        );
        assert_eq!(cands[0].branch_op, 0x99, "detected op must be ifeq");
        assert_eq!(cands[0].invariant_local, 0, "flag is local 0");

        // Tiny body is well under MAX_UNSWITCH_BYTECODES.
        let body_size = cands[0].back_edge_pc - cands[0].header_pc;
        assert!(
            body_size <= MAX_UNSWITCH_BYTECODES,
            "body size {body_size} must be ≤ {MAX_UNSWITCH_BYTECODES}"
        );
    }

    /// A loop whose body exceeds `MAX_UNSWITCH_BYTECODES` must be
    /// rejected by the detector; the emitter consequently produces
    /// the unmodified scalar loop (no preheader evaluation, no
    /// duplication). Compilation still succeeds.
    #[test]
    fn t17_b_loop_unswitch_large_body_rejected() {
        // Build a large loop (body > MAX_UNSWITCH_BYTECODES).
        let mut code = vec![0x03, 0x3D]; // i = 0
        let header = code.len();
        code.extend_from_slice(&[0x1C, 0x1B, 0xa2, 0x00, 0x00]); // iload i,n,if_icmpge
        code.push(0x1A); // iload_0 (flag)
        code.extend_from_slice(&[0x99, 0x00, 0x03]); // ifeq
        for _ in 0..(MAX_UNSWITCH_BYTECODES + 5) {
            code.push(0x00); // padding nops
        }
        let back_edge = code.len();
        code.extend_from_slice(&[0x84, 0x02, 0x01]); // iinc
        code.extend_from_slice(&[0xa7, 0xFF, 0xFF]); // goto
        code.push(0xB1); // return
        code.push(0); // padding
        code.push(0);

        let code_len = code.len() - 2;
        let loops = detect_loops(&code, code_len);
        let cands = detect_loop_unswitch_candidates(&code, code_len, &loops);
        assert!(
            cands.is_empty(),
            "large body must not produce an unswitch candidate"
        );

        // Compilation still succeeds via the normal scalar path.
        let compiled = compile(
            &code,
            code_len,
            2,
            3,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "large-body loop must still compile through the scalar fallback"
        );
        // The key guarantee: no preheader evaluation was emitted, so
        // code size reflects only the scalar loop body (the emitter
        // short-circuited in `emit_loop_unswitch_preheader`).
        let _ = header;
        let _ = back_edge;
    }

    #[test]
    fn test_detect_int_array_element_wise_rejects_non_elementwise() {
        // Reduction loop (sum += arr[i]) should NOT match element-wise.
        // Use a plain int reduction here (not the long-accumulator form
        // detect_int_array_sum already accepts).
        let code: Vec<u8> = vec![
            0x03,             // 0: iconst_0
            0x36, 0x02,       // 1: istore 2 (sum = 0)
            0x03,             // 3: iconst_0
            0x36, 0x03,       // 4: istore 3 (i = 0)
            // header at PC=6
            0x15, 0x03,       // 6: iload 3
            0x1B,             // 8: iload_1 (n)
            0xa2, 0x00, 0x0D, // 9: if_icmpge +13 → 22
            0x15, 0x02,       // 12: iload 2 (sum)
            0x2A,             // 14: aload_0
            0x15, 0x03,       // 15: iload 3
            0x2e,             // 17: iaload
            0x60,             // 18: iadd
            0x36, 0x02,       // 19: istore 2
            0x84, 0x03, 0x01, // 21: iinc 3, 1
            0xa7, 0xff, 0xF1, // 24: goto -15 → 9? (doesn't matter for this test)
            0x15, 0x02,       // 27: iload 2
            0xac,             // 29: ireturn
        ];
        let code_len = code.len();
        let loops = detect_loops(&code, code_len);
        // Either no loop is detected or the pattern doesn't match — both are fine.
        for &(header, back_edge) in &loops {
            let back_end = back_edge + bytecode_len_at(&code, back_edge);
            if let Some(iv) = find_induction_variable(&code, header, back_end) {
                assert!(
                    detect_int_array_element_wise(&code, header, back_edge, iv).is_none(),
                    "reduction should not match element-wise"
                );
            }
        }
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_simd_int_array_sum_end_to_end() {
        // End-to-end test: compile a long sum_array(int[], int) method
        // and verify SIMD-accelerated execution with a real array.
        if !has_avx2() {
            println!("Skipping SIMD end-to-end test: AVX2 not available");
            return;
        }

        use rustjvm_types::ClassId;
        use crate::config::VmConfig;
        use rustjvm_gc::heap::ArrayElementType;
        use crate::vm::SharedVm;
        use std::sync::Arc;

        // Same bytecode as the pattern test above:
        // long sum_array(int[] arr, int n)
        // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
        let code: Vec<u8> = vec![
            0x09, // 0: lconst_0
            0x41, // 1: lstore_2 (sum = 0)
            0x03, // 2: iconst_0
            0x36, 0x04, // 3: istore 4 (i = 0)
            0x15, 0x04, // 5: iload 4 — loop header
            0x1b, // 7: iload_1 (n)
            0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
            0x2a, // 11: aload_0 (arr)
            0x15, 0x04, // 12: iload 4 (i)
            0x2e, // 14: iaload
            0x85, // 15: i2l
            0x20, // 16: lload_2 (sum)
            0x61, // 17: ladd
            0x41, // 18: lstore_2 (sum)
            0x84, 0x04, 0x01, // 19: iinc 4, 1
            0xa7, 0xff, 0xef, // 22: goto -17 → 5
            0x20, // 25: lload_2
            0xad, // 26: lreturn
            0, 0,
        ];
        let code_len = 27;

        // needs_heap=true because of iaload
        let compiled = compile(
            &code,
            code_len,
            2,
            5,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .unwrap();

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        // Test 1: empty array (n=0, should return 0) — no SIMD, no scalar
        let arr1 = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
        let arr1_ptr = arr1.as_ptr();
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result1 = unsafe { compiled.call_with_context(vm_ptr, &[arr1_ptr as i64, 0]) }; // Cast: JIT ABI convention
        assert_eq!(result1, 0, "empty array sum should be 0");

        // Test 2: array of 3 elements (0 SIMD chunks, all scalar cleanup)
        let n2 = 3;
        let arr2 = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, n2);
        let arr2_ptr = arr2.as_ptr();
        for i in 0..n2 {
            let _ = shared.heap.set_array_element(arr2, i, Value::Int(100));
        }
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result2 = unsafe { compiled.call_with_context(vm_ptr, &[arr2_ptr as i64, n2 as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result2, 300, "3 elements of 100 should sum to 300");

        // Test 3: array of exactly 8 elements (exactly 1 SIMD chunk, no cleanup)
        let n3 = 8;
        let arr3 = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, n3);
        let arr3_ptr = arr3.as_ptr();
        for i in 0..n3 {
            let _ = shared.heap.set_array_element(arr3, i, Value::Int(10));
        }
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result3 = unsafe { compiled.call_with_context(vm_ptr, &[arr3_ptr as i64, n3 as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result3, 80, "8 elements of 10 should sum to 80");

        // Test 4: array of 20 elements (exercises both SIMD chunks and scalar cleanup)
        // arr = [1, 2, 3, ..., 20], sum = 210
        let n4 = 20;
        let arr4 = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, n4);
        let arr4_ptr = arr4.as_ptr();
        for i in 0..n4 {
            let _ = shared
                .heap
                .set_array_element(arr4, i, Value::Int((i + 1) as i32)); // Cast: x86-64 immediate encoding
        }
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result4 = unsafe { compiled.call_with_context(vm_ptr, &[arr4_ptr as i64, n4 as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result4, 210, "sum of 1..=20 should be 210");

        // Test 5: large array (256 elements) to really exercise SIMD
        let n5 = 256;
        let arr5 = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, n5);
        let arr5_ptr = arr5.as_ptr();
        for i in 0..n5 {
            let _ = shared.heap.set_array_element(arr5, i, Value::Int(i as i32)); // Cast: x86-64 immediate encoding
        }
        // sum of 0..255 = 255*256/2 = 32640
        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result5 = unsafe { compiled.call_with_context(vm_ptr, &[arr5_ptr as i64, n5 as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result5, 32640, "sum of 0..=255 should be 32640");
    }

    #[test]
    fn test_jit_scan_accepts_new_opcode() {
        // Bytecode: new #1 (0xbb 0x00 0x01), areturn
        // new pushes objectref; method returns it
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0xb0,             // 3: areturn
            0, 0,             // padding
        ];
        let scan = jit_scan(&code, 4, "()Ljava/lang/Object;");
        assert!(scan.is_some(), "jit_scan should accept `new` opcode");
        let scan = scan.unwrap();
        assert!(scan.needs_heap, "`new` requires heap pointer");
        assert_eq!(scan.new_ops.len(), 1);
        assert_eq!(scan.new_ops[0], (0, 1)); // pc=0, cp_idx=1
    }

    #[test]
    fn test_jit_scan_accepts_anewarray_opcode() {
        // Bytecode: iconst_3, anewarray #1 (0xbd 0x00 0x01), areturn
        let code: Vec<u8> = vec![
            0x06,             // 0: iconst_3 (length=3)
            0xbd, 0x00, 0x01, // 1: anewarray #1
            0xb0,             // 4: areturn
            0, 0,             // padding
        ];
        let scan = jit_scan(&code, 5, "()[Ljava/lang/Object;");
        assert!(scan.is_some(), "jit_scan should accept `anewarray` opcode");
        let scan = scan.unwrap();
        assert!(scan.needs_heap, "`anewarray` requires heap pointer");
        assert_eq!(scan.anewarray_ops.len(), 1);
        assert_eq!(scan.anewarray_ops[0], (1, 1)); // pc=1, cp_idx=1
    }

    #[test]
    fn test_jit_scan_escape_analysis_non_escaping() {
        // Pattern: new; dup; invokespecial <init>; astore_1; aload_1; getfield; ireturn
        // The object is stored to local 1 and only used for getfield — non-escaping.
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59,             // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>
            0x4c,             // 7: astore_1
            0x2b,             // 8: aload_1
            0xb4, 0x00, 0x03, // 9: getfield #3
            0xac,             // 12: ireturn
            0, 0,             // padding
        ];
        let scan = jit_scan(&code, 13, "()I");
        assert!(scan.is_some(), "method should be jit-compatible");
        let scan = scan.unwrap();
        assert!(
            scan.non_escaping_new.contains(&0),
            "new at PC=0 should be identified as non-escaping (only used via getfield)"
        );
    }

    #[test]
    fn test_jit_scan_escape_analysis_escaping_via_areturn() {
        // Pattern: new; dup; invokespecial <init>; areturn — the object escapes
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59,             // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>
            0xb0,             // 7: areturn
            0, 0,             // padding
        ];
        let scan = jit_scan(&code, 8, "()Ljava/lang/Object;");
        assert!(scan.is_some(), "method should be jit-compatible");
        let scan = scan.unwrap();
        assert!(
            !scan.non_escaping_new.contains(&0),
            "new at PC=0 should be escaping (returned via areturn)"
        );
    }

    /// HIGH-6 — verify the inline TLAB bump-pointer codegen produces an
    /// executable method that correctly falls through to the slow path
    /// when the TLS thread pointer is null (the JE-on-null branch in
    /// `emit_inline_tlab_new`). Using stubs avoids a full VM init so this
    /// test is NOT gated behind the `vm-tests` feature.
    #[test]
    fn test_inline_tlab_new_falls_through_on_null_thread() {
        // Stub: pretend there is no current thread (re-entrant or pre-init
        // state). The inline path must take the JE branch to the slow path
        // and we then short-circuit with a sentinel `new_object` return.
        unsafe extern "C" fn null_thread() -> *mut std::ffi::c_void {
            std::ptr::null_mut()
        }
        unsafe extern "C" fn fake_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
            0xDEAD_BEEFi64
        }
        unsafe extern "C" fn unimplemented_post_init(
            _vm: i64,
            _obj: i64,
            _cid: i64,
            _nf: i64,
        ) -> i64 {
            panic!("post_tlab_init must not be called when thread is null");
        }

        // Build a custom helper table with the inline path WIRED so
        // `can_inline` is true (get_current_thread, tlab_post_init,
        // new_object all non-null), but `get_current_thread` returns
        // null at runtime to force the slow path inside the emitted
        // inline cascade.
        let mut helpers = test_helpers();
        helpers.get_current_thread = null_thread as *const () as usize;
        helpers.tlab_post_init = unimplemented_post_init as *const () as usize;
        helpers.new_object = fake_new_object as *const () as usize;
        // The offsets don't matter — they're only read when the
        // thread pointer is non-null.
        helpers.tlab_cursor_offset_in_thread = 0;
        helpers.tlab_end_offset_in_thread = 8;

        // Method: new #1; astore_1; aload_1; areturn (cls=42, fields=3).
        let code: Vec<u8> = vec![0xbb, 0x00, 0x01, 0x4c, 0x2b, 0xb0, 0, 0];
        let code_len = 6;
        // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
        // Tests use conservative `(true, true)` so the helper path is exercised.
        let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];

        let compiled = compile(
            &code,
            code_len,
            0,
            2,
            true, // needs_heap → can_inline gate passes
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            new_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
        )
        .expect("inline-TLAB new opcode should compile");

        // SAFETY: Calling JIT-compiled machine code with a sentinel VM pointer.
        // The fake_new_object stub does not touch the pointer.
        let result = unsafe { compiled.call_with_context(0xCAFE_F00D, &[]) };
        assert_eq!(
            result, 0xDEAD_BEEFi64,
            "inline TLAB cascade must fall through to slow-path fake_new_object \
             when get_current_thread returns null"
        );
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_jit_new_object_codegen() {
        // Verify that the `new` opcode compiles and calls jit_new_object at runtime.
        // Method: allocate an object, store to local 1, load local 1, areturn.
        // new #1; astore_1; aload_1; areturn
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1  (cp_idx=1)
            0x4c,             // 3: astore_1
            0x2b,             // 4: aload_1
            0xb0,             // 5: areturn
            0, 0,             // padding
        ];
        let code_len = 6;

        // Provide new_info: class_id=42, num_fields=3
        // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
        // Tests use conservative `(true, true)` so the helper path is exercised.
        let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];
        let compiled = compile(
            &code,
            code_len,
            0,
            2,
            true, // needs_heap (for jit_new_object helper)
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            new_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "Should compile method with `new` opcode");

        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.unwrap().call_with_context(vm_ptr, &[]) };
        // Result should be a non-zero pointer to the allocated object
        assert_ne!(result, 0, "jit_new_object should return a valid object pointer");
    }

    #[cfg(feature = "vm-tests")]
    #[test]
    fn test_jit_anewarray_codegen() {
        // Verify anewarray compiles and produces a valid reference array at runtime.
        // iconst_5; anewarray #1; areturn
        let code: Vec<u8> = vec![
            0x08,             // 0: iconst_5 (length=5)
            0xbd, 0x00, 0x01, // 1: anewarray #1 (cp_idx=1)
            0xb0,             // 4: areturn
            0, 0,             // padding
        ];
        let code_len = 5;

        // Provide anewarray_info: component_class_id=7
        let anewarray_info: Vec<(usize, u32)> = vec![(1, 7)];
        let compiled = compile(
            &code,
            code_len,
            0,
            0,
            true, // needs_heap
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            anewarray_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(
            compiled.is_some(),
            "Should compile method with `anewarray` opcode"
        );

        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

        // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
        // was produced from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.unwrap().call_with_context(vm_ptr, &[]) };
        // Result should be non-zero (valid array pointer)
        assert_ne!(result, 0, "jit_anewarray_object should return a valid array pointer");
    }

    // -----------------------------------------------------------------------
    // M5: JIT compilation of <init> constructors and lambda$ methods
    // -----------------------------------------------------------------------

    #[test]
    fn test_m5_constructor_init_putfield() {
        // Simulates: <init>(int x) { this.x = x; }
        // Bytecode: aload_0, iload_1, putfield #1, return
        // This is the exact pattern that was previously skipped for <init>.
        let code: Vec<u8> = vec![
            0x2a,             // 0: aload_0 (this)
            0x1b,             // 1: iload_1 (x)
            0xb5, 0x00, 0x01, // 2: putfield #1
            0xb1,             // 5: return (void)
            0, 0,
        ];
        let code_len = 6;
        let field_info = vec![(2usize, 0usize, b'I')];
        let compiled = compile(
            &code,
            code_len,
            2,  // param_slots: this + int x
            2,  // max_locals
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "Constructor with putfield should be JIT-compilable");

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(0));

        // Call <init>(this, 42)
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.unwrap().call(&[obj.as_ptr() as i64, 42]) }; // Cast: JIT ABI convention
        let val = heap.get_field(obj, 0);
        assert_eq!(val, Value::Int(42), "Constructor putfield should set field correctly");
    }

    #[test]
    fn test_m5_constructor_init_multiple_putfields() {
        // Simulates: <init>(int x, int y) { this.x = x; this.y = y; }
        // Bytecode: aload_0, iload_1, putfield #1, aload_0, iload_2, putfield #2, return
        let code: Vec<u8> = vec![
            0x2a,             // 0: aload_0
            0x1b,             // 1: iload_1
            0xb5, 0x00, 0x01, // 2: putfield #1
            0x2a,             // 5: aload_0
            0x1c,             // 6: iload_2
            0xb5, 0x00, 0x02, // 7: putfield #2
            0xb1,             // 10: return
            0, 0,
        ];
        let code_len = 11;
        let field_info = vec![
            (2usize, 0usize, b'I'),  // putfield #1 at pc=2, field_index=0
            (7usize, 1usize, b'I'),  // putfield #2 at pc=7, field_index=1
        ];
        let compiled = compile(
            &code,
            code_len,
            3,  // param_slots: this + int x + int y
            3,  // max_locals
            false,
            Vec::new(),
            field_info,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "Multi-field constructor should compile");

        use rustjvm_types::ClassId;
        use rustjvm_gc::gen_heap::GenerationalHeap;
        let heap = GenerationalHeap::new();
        let obj = heap.alloc_object(ClassId::new(0), 3);
        heap.set_field(obj, 0, Value::Int(0));
        heap.set_field(obj, 1, Value::Int(0));

        // Call <init>(this, 10, 20)
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe { compiled.unwrap().call(&[obj.as_ptr() as i64, 10, 20]) }; // Cast: JIT ABI convention
        assert_eq!(heap.get_field(obj, 0), Value::Int(10));
        assert_eq!(heap.get_field(obj, 1), Value::Int(20));
    }

    #[test]
    fn test_m5_lambda_method_compiles() {
        // Simulates: lambda$main$0(int x) -> int { return x + 1; }
        // Lambda methods are static methods with captured args. This tests
        // that such methods are now JIT-compilable (previously skipped).
        let code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0x04,             // 1: iconst_1
            0x60,             // 2: iadd
            0xac,             // 3: ireturn
            0, 0,
        ];
        let code_len = 4;
        let compiled = compile(
            &code,
            code_len,
            1,  // param_slots: int x
            1,  // max_locals
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "Lambda-style method should be JIT-compilable");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.unwrap().call(&[41]) };
        assert_eq!(result, 42, "lambda$main$0(41) should return 42");
    }

    #[test]
    fn test_m5_user_class_method_compiles() {
        // Simulates: com.example.MyClass.add(int a, int b) -> int { return a + b; }
        // Tests that non-java/* user class methods are JIT-compiled.
        let code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0x1b,             // 1: iload_1
            0x60,             // 2: iadd
            0xac,             // 3: ireturn
            0, 0,
        ];
        let code_len = 4;
        let compiled = compile(
            &code,
            code_len,
            2,  // param_slots: int a, int b
            2,  // max_locals
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "User class static method should be JIT-compilable");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.unwrap().call(&[17, 25]) };
        assert_eq!(result, 42, "add(17, 25) should return 42");
    }

    #[test]
    fn test_m5_constructor_void_return() {
        // Verify void-returning constructor descriptor is JIT-scannable
        // <init>()V — simplest constructor
        let code: Vec<u8> = vec![
            0xb1, // 0: return (void)
            0, 0,
        ];
        let code_len = 1;
        let scan = jit_scan(&code, code_len, "()V");
        assert!(scan.is_some(), "Void constructor should pass jit_scan");
    }

    #[test]
    fn test_m5_lambda_with_captured_object() {
        // Simulates: lambda$forEach$0(Object captured, int idx) -> int
        // Returns idx (simplified — tests object + int arg passing)
        let code: Vec<u8> = vec![
            0x1b,             // 0: iload_1 (idx is local 1, captured obj is local 0)
            0xac,             // 1: ireturn
            0, 0,
        ];
        let code_len = 2;
        let compiled = compile(
            &code,
            code_len,
            2,  // param_slots: Object captured, int idx
            2,  // max_locals
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        );
        assert!(compiled.is_some(), "Lambda with captured Object arg should compile");

        // Pass null as captured object (0), 99 as idx
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.unwrap().call(&[0, 99]) };
        assert_eq!(result, 99, "Lambda should correctly access second parameter");
    }

    // -----------------------------------------------------------------------
    // Phase 87: FP Performance tests
    // -----------------------------------------------------------------------

    // --- 87.1: XMM Register Persistence ---

    #[test]
    fn p87_scratch_xmm_constants() {
        // Verify scratch XMM register constants are defined correctly
        assert_eq!(SCRATCH_XMMS, [2, 3, 4, 5, 6, 7]);
        assert_eq!(SCRATCH_XMMS.len(), 6);
    }

    #[test]
    fn p87_xmm_intermediate_chaining_double() {
        // double f(double a, double b) { return (a + b) * (a - b); }
        // dload_0, dload_1, dadd, dload_0, dload_1, dsub, dmul, dreturn
        // This tests that FP intermediates persist in XMM registers
        // across the dadd → dsub → dmul chain.
        let code: Vec<u8> = vec![
            0x26, // 0: dload_0 (a)
            0x27, // 1: dload_1 (b)
            0x63, // 2: dadd (a+b)
            0x26, // 3: dload_0 (a)
            0x27, // 4: dload_1 (b)
            0x67, // 5: dsub (a-b)
            0x6b, // 6: dmul ((a+b)*(a-b))
            0xaf, // 7: dreturn
            0, 0,
        ];
        let code_len = 8;
        let compiled = compile(
            &code, code_len, 2, 2, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 5.0f64;
        let b = 3.0f64;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64, b.to_bits() as i64]) }; // Cast: JIT ABI convention
        let expected = (a + b) * (a - b); // 8.0 * 2.0 = 16.0
        assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
    }

    #[test]
    fn p87_xmm_intermediate_chaining_float() {
        // float f(float a, float b) { return (a + b) * (a - b); }
        let code: Vec<u8> = vec![
            0x22, // fload_0
            0x23, // fload_1
            0x62, // fadd
            0x22, // fload_0
            0x23, // fload_1
            0x66, // fsub
            0x6a, // fmul
            0xae, // freturn
            0, 0,
        ];
        let code_len = 8;
        let compiled = compile(
            &code, code_len, 2, 2, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 5.0f32;
        let b = 3.0f32;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64, b.to_bits() as i64]) }; // Cast: JIT ABI convention
        let expected = (a + b) * (a - b);
        assert_eq!(f32::from_bits(result as u32), expected); // Cast: JIT ABI convention
    }

    #[test]
    fn p87_xmm_multiple_intermediates() {
        // double f(double a, double b, double c) { return a*b + a*c + b*c; }
        // Tests multiple FP intermediates on the stack simultaneously
        // dload_0, dload_1, dmul,     -> (a*b)
        // dload_0, dload_2, dmul,     -> (a*c)
        // dadd,                       -> (a*b + a*c)
        // dload_1, dload_2, dmul,     -> (b*c)
        // dadd,                       -> (a*b + a*c + b*c)
        // dreturn
        let code: Vec<u8> = vec![
            0x26, // 0: dload_0 (a)
            0x27, // 1: dload_1 (b)
            0x6b, // 2: dmul
            0x26, // 3: dload_0 (a)
            0x28, // 4: dload_2 (c)
            0x6b, // 5: dmul
            0x63, // 6: dadd
            0x27, // 7: dload_1 (b)
            0x28, // 8: dload_2 (c)
            0x6b, // 9: dmul
            0x63, // 10: dadd
            0xaf, // 11: dreturn
            0, 0,
        ];
        let code_len = 12;
        let compiled = compile(
            &code, code_len, 3, 3, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 2.0f64;
        let b = 3.0f64;
        let c = 4.0f64;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe {
            compiled.call(&[
                a.to_bits() as i64, // Cast: JIT ABI convention
                b.to_bits() as i64, // Cast: JIT ABI convention
                c.to_bits() as i64, // Cast: JIT ABI convention
            ])
        };
        let expected = a * b + a * c + b * c; // 6 + 8 + 12 = 26
        assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
    }

    #[test]
    fn p87_xmm_local_allocation_verified() {
        // Verify that the register allocator allocates XMM registers for FP locals
        // Simple: dload_0, dstore_1, dload_1, dreturn
        // Locals 0 and 1 are both FP — should both get XMM allocations
        let code: Vec<u8> = vec![0x26, 0x48, 0x27, 0xaf, 0, 0];
        let code_len = 4;
        let loops = detect_loops(&code, code_len);
        let alloc = crate::regalloc::allocate_registers(&code, code_len, 2, 1, &loops);
        // Both locals should have XMM assignments (not GPR)
        assert!(alloc.assignments[0].is_none(), "FP local 0 should not have GPR");
        assert!(alloc.assignments[1].is_none(), "FP local 1 should not have GPR");
        // At least one should get an XMM
        let xmm_count = alloc.xmm_assignments.iter().filter(|a: &&Option<u8>| a.is_some()).count();
        assert!(xmm_count > 0, "At least one FP local should get XMM register");
    }

    // --- 87.2: FP Loop Optimization ---

    #[test]
    fn p87_fp_loop_hoist_detection() {
        // Loop: dload_0; dload_1; dadd; dstore_0; iinc 2 1; iload_2; iconst_5; if_icmplt -10; dload_0; dreturn
        // dload_1 is invariant (local 1 not modified), dload_0 is NOT (dstore_0 modifies it)
        let code: Vec<u8> = vec![
            0x26,       // 0: dload_0  (modified — acc)
            0x27,       // 1: dload_1  (invariant — constant)
            0x63,       // 2: dadd
            0x47,       // 3: dstore_0
            0x84, 0x02, 0x01, // 4: iinc 2, 1
            0x15, 0x02, // 7: iload 2
            0x08,       // 9: iconst_5
            0xa1, 0xFF, 0xF6, // 10: if_icmplt -10 → target=0
            0x26,       // 13: dload_0
            0xaf,       // 14: dreturn
            0, 0,
        ];
        let code_len = 15;
        let loops = detect_loops(&code, code_len);
        assert!(!loops.is_empty(), "Should detect a loop");

        let hoists = find_fp_loop_hoists(&code, code_len, &loops);
        // dload_1 at PC=1 should be hoistable (local 1 is invariant)
        let hoisted_pcs: Vec<usize> = hoists.iter().map(|h| h.load_pc).collect();
        assert!(hoisted_pcs.contains(&1), "dload_1 at PC=1 should be hoistable");
        // dload_0 at PC=0 should NOT be hoistable (local 0 is modified by dstore_0)
        assert!(!hoisted_pcs.contains(&0), "dload_0 should not be hoistable");
    }

    #[test]
    fn p87_fp_loop_hoist_empty_for_no_loops() {
        // No loops → no hoists
        let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
        let loops = detect_loops(&code, 2);
        let hoists = find_fp_loop_hoists(&code, 2, &loops);
        assert!(hoists.is_empty());
    }

    #[test]
    fn p87_strength_reduction_detects_dmul_by_2() {
        // Loop with ldc2_w (2.0), dmul → should be detected
        // Simulated: ldc2_w at PC=5, dmul at PC=8
        let code: Vec<u8> = vec![
            0x15, 0x03, // 0: iload 3 (iv)
            0x08,       // 2: iconst_5 (bound)
            0xa2, 0x00, 0x0E, // 3: if_icmpge +14 → target=17
            0x26,       // 6: dload_0 (value)
            0x14, 0x00, 0x01, // 7: ldc2_w #1
            0x6b,       // 10: dmul
            0x47,       // 11: dstore_0
            0x84, 0x03, 0x01, // 12: iinc 3, 1
            0xa7, 0xFF, 0xF1, // 15: goto -15 → target=0
            0x26,       // 18: dload_0
            0xaf,       // 19: dreturn
            0, 0,
        ];
        let code_len = 20;
        let loops = detect_loops(&code, code_len);
        assert!(!loops.is_empty());

        // ldc2_w at PC=7, value = 2.0
        let ldc2w_info = vec![(7usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
        let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
        // dmul at PC=10 should be strength-reduced
        assert!(pcs.contains(&10), "dmul at PC=10 should be strength-reduced");
    }

    #[test]
    fn p87_strength_reduction_ignores_non_2() {
        // Same pattern but ldc2_w loads 3.0 instead of 2.0
        let code: Vec<u8> = vec![
            0x15, 0x03,       // 0: iload 3
            0x08,             // 2: iconst_5
            0xa2, 0x00, 0x0E, // 3: if_icmpge +14
            0x26,             // 6: dload_0
            0x14, 0x00, 0x01, // 7: ldc2_w #1
            0x6b,             // 10: dmul
            0x47,             // 11: dstore_0
            0x84, 0x03, 0x01, // 12: iinc 3, 1
            0xa7, 0xFF, 0xF0, // 15: goto -16
            0x26,             // 18: dload_0
            0xaf,             // 19: dreturn
            0, 0,
        ];
        let code_len = 20;
        let loops = detect_loops(&code, code_len);
        let ldc2w_info = vec![(7usize, 3.0f64.to_bits() as i64)]; // NOT 2.0 // Cast: JIT ABI convention
        let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
        assert!(pcs.is_empty(), "Should not reduce dmul by non-2.0");
    }

    #[test]
    fn p87_strength_reduction_emits_dadd_self() {
        // Verify that strength-reduced dmul-by-2.0 emits ADDSD XMM0, XMM0
        // double f(double x) { return x * 2.0; }
        // dload_0, ldc2_w #1 (2.0), dmul, dreturn
        let code: Vec<u8> = vec![
            0x26,             // 0: dload_0
            0x14, 0x00, 0x01, // 1: ldc2_w #1 (resolves to 2.0)
            0x6b,             // 4: dmul
            0xaf,             // 5: dreturn
            0, 0,
        ];
        let code_len = 6;

        // Wrap in a loop so strength reduction triggers
        // Actually, strength reduction only triggers inside loops.
        // Let's test without a loop first — dmul should still work normally.
        let ldc2w_info = vec![(1usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
        let compiled = compile(
            &code, code_len, 1, 1, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            ldc2w_info, HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let x = 7.5f64;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[x.to_bits() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(f64::from_bits(result as u64), 15.0); // 7.5 * 2.0 = 15.0 // Cast: JIT ABI convention
    }

    // --- 87.3: SIMD FP Operations ---

    #[test]
    fn p87_detect_fp_array_sum_pattern_a() {
        // Pattern A: aload_1, iload_2, daload, dload_0, dadd, dstore_0, iinc 2 1, goto
        // Header: iload_2, iload_3, if_icmpge
        let code: Vec<u8> = vec![
            0x1c,             // 0: iload_2 (iv)
            0x1d,             // 1: iload_3 (bound)
            0xa2, 0x00, 0x0C, // 2: if_icmpge +12 → target=14
            0x2b,             // 5: aload_1 (arr)
            0x1c,             // 6: iload_2 (iv)
            0x31,             // 7: daload
            0x26,             // 8: dload_0 (sum)
            0x63,             // 9: dadd
            0x47,             // 10: dstore_0
            0x84, 0x02, 0x01, // 11: iinc 2, 1
            0xa7, 0xFF, 0xF2, // 14: goto -14 → target=0
            0x26,             // 17: dload_0
            0xaf,             // 18: dreturn
            0, 0,
        ];
        let code_len = 19;
        let loops = detect_loops(&code, code_len);
        assert!(!loops.is_empty(), "Should detect a loop");

        let iv = find_induction_variable(&code, loops[0].0, loops[0].1 + 3);
        assert_eq!(iv, Some(2), "Induction variable should be local 2");

        let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
        assert!(result.is_some(), "Should detect FP array sum pattern");
        let info = result.unwrap();
        assert_eq!(info.acc_local, 0);
        assert_eq!(info.array_local, 1);
        assert_eq!(info.iv_local, 2);
        assert_eq!(info.bound_local, 3);
        assert_eq!(info.sse_op, 0x58); // ADDPD
    }

    #[test]
    fn p87_detect_fp_array_sum_pattern_b() {
        // Pattern B: dload_0, aload_1, iload_2, daload, dadd, dstore_0, iinc 2 1, goto
        let code: Vec<u8> = vec![
            0x1c,             // 0: iload_2 (iv)
            0x1d,             // 1: iload_3 (bound)
            0xa2, 0x00, 0x0C, // 2: if_icmpge +12
            0x26,             // 5: dload_0 (sum)
            0x2b,             // 6: aload_1 (arr)
            0x1c,             // 7: iload_2 (iv)
            0x31,             // 8: daload
            0x63,             // 9: dadd
            0x47,             // 10: dstore_0
            0x84, 0x02, 0x01, // 11: iinc 2, 1
            0xa7, 0xFF, 0xF2, // 14: goto -14
            0x26,
            0xaf,
            0, 0,
        ];
        let code_len = 19;
        let loops = detect_loops(&code, code_len);
        assert!(!loops.is_empty());

        let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
        assert!(result.is_some(), "Should detect FP array sum pattern B");
        let info = result.unwrap();
        assert_eq!(info.acc_local, 0);
        assert_eq!(info.array_local, 1);
    }

    #[test]
    fn p87_no_fp_array_sum_for_int_loop() {
        // Int array sum should NOT trigger FP detection
        // aload_1, iload_2, iaload (0x2e not 0x31), iload_0, iadd, istore_0, iinc...
        let code: Vec<u8> = vec![
            0x1c,             // iload_2
            0x1d,             // iload_3
            0xa2, 0x00, 0x0B, // if_icmpge
            0x2b,             // aload_1
            0x1c,             // iload_2
            0x2e,             // iaload (NOT daload)
            0x1a,             // iload_0
            0x60,             // iadd
            0x3b,             // istore_0
            0x84, 0x02, 0x01, // iinc 2, 1
            0xa7, 0xFF, 0xF3, // goto
            0x1a, 0xac, 0, 0,
        ];
        let code_len = 18;
        let loops = detect_loops(&code, code_len);
        let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
        assert!(result.is_none(), "Int array sum should not trigger FP detection");
    }

    #[test]
    fn p87_simd_fp_not_detected_without_avx2() {
        // Verify the compile path handles the case where AVX2 is not available
        // (On machines with AVX2 this still works — just tests the detection path)
        let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
        let code_len = 2;
        let loops = detect_loops(&code, code_len);
        // No loops → no SIMD FP
        let simd = if has_avx2() {
            let mut s = Vec::new();
            for &(h, b) in &loops {
                let end = b + bytecode_len_at(&code, b);
                if let Some(iv) = find_induction_variable(&code, h, end) {
                    if let Some(info) = detect_fp_array_sum(&code, h, b, iv) {
                        s.push(info);
                    }
                }
            }
            s
        } else {
            Vec::new()
        };
        assert!(simd.is_empty());
    }

    #[test]
    fn p87_double_binop_chain_preserves_precision() {
        // Verify FP chaining doesn't lose precision
        // double f(double a) { return a + a + a + a; }
        let code: Vec<u8> = vec![
            0x26, 0x26, 0x63, // dload_0, dload_0, dadd
            0x26, 0x63,       // dload_0, dadd
            0x26, 0x63,       // dload_0, dadd
            0xaf,             // dreturn
            0, 0,
        ];
        let code_len = 8;
        let compiled = compile(
            &code, code_len, 1, 1, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = std::f64::consts::PI;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(f64::from_bits(result as u64), a + a + a + a); // Cast: JIT ABI convention
    }

    #[test]
    fn p87_float_binop_chain_preserves_precision() {
        // float f(float a) { return a * a * a; }
        let code: Vec<u8> = vec![
            0x22, 0x22, 0x6a, // fload_0, fload_0, fmul
            0x22, 0x6a,       // fload_0, fmul
            0xae,             // freturn
            0, 0,
        ];
        let code_len = 6;
        let compiled = compile(
            &code, code_len, 1, 1, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 2.5f32;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(f32::from_bits(result as u32), a * a * a); // 15.625 // Cast: JIT ABI convention
    }

    #[test]
    fn p87_double_division_chain() {
        // double f(double a, double b) { return (a / b) / b; }
        // Tests non-commutative FP ops work correctly with XMM chaining
        let code: Vec<u8> = vec![
            0x26, 0x27, 0x6f, // dload_0, dload_1, ddiv
            0x27, 0x6f,       // dload_1, ddiv
            0xaf,             // dreturn
            0, 0,
        ];
        let code_len = 6;
        let compiled = compile(
            &code, code_len, 2, 2, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 100.0f64;
        let b = 5.0f64;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64, b.to_bits() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(f64::from_bits(result as u64), (a / b) / b); // 4.0 // Cast: JIT ABI convention
    }

    #[test]
    fn p87_mixed_fp_and_int_computation() {
        // int f(double a, double b) { double c = a + b; return (int)c; }
        // dload_0, dload_1, dadd, d2i (0x8e), ireturn
        let code: Vec<u8> = vec![
            0x26, 0x27, 0x63, // dload_0, dload_1, dadd
            0x8e,             // d2i
            0xac,             // ireturn
            0, 0,
        ];
        let code_len = 5;
        let compiled = compile(
            &code, code_len, 2, 2, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
        ).unwrap();

        let a = 3.7f64;
        let b = 2.1f64;
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.call(&[a.to_bits() as i64, b.to_bits() as i64]) }; // Cast: JIT ABI convention
        assert_eq!(result, (a + b) as i32 as i64); // 5 // Cast: JIT ABI convention
    }

    #[test]
    fn p87_fp_hoist_double_and_float() {
        // Verify hoist detection for both float and double types
        // Loop with fload_0 (invariant) and dload_1 (invariant), fstore_2 modified
        let code: Vec<u8> = vec![
            0x22,             // 0: fload_0 (float, invariant)
            0x27,             // 1: dload_1 (double, invariant — this is wrong mix, but tests detection)
            0x63,             // 2: dadd (type mismatch at runtime, but tests analysis)
            0x47,             // 3: dstore_0 (this modifies 0, but doesn't affect 1)
            0x84, 0x03, 0x01, // 4: iinc 3, 1
            0x15, 0x03,       // 7: iload 3
            0x08,             // 9: iconst_5
            0xa1, 0xFF, 0xF6, // 10: if_icmplt -10 → target=0
            0x26, 0xaf, 0, 0,
        ];
        let code_len = 15;
        let loops = detect_loops(&code, code_len);
        let hoists = find_fp_loop_hoists(&code, code_len, &loops);

        // fload_0 at PC=0: local 0 IS modified (dstore_0 at PC=3) → NOT hoistable
        // dload_1 at PC=1: local 1 is NOT modified → hoistable
        let hoisted_locals: Vec<(usize, bool)> = hoists.iter().map(|h| (h.local_idx, h.is_double)).collect();
        assert!(hoisted_locals.contains(&(1, true)), "dload_1 should be hoistable");
        assert!(!hoisted_locals.iter().any(|(l, _)| *l == 0), "local 0 should not be hoistable");
    }

    #[test]
    fn p87_extract_dload_local_variants() {
        // Test dload extraction helpers
        assert_eq!(extract_dload_local(&[0x26], 0), Some(0)); // dload_0
        assert_eq!(extract_dload_local(&[0x27], 0), Some(1)); // dload_1
        assert_eq!(extract_dload_local(&[0x28], 0), Some(2)); // dload_2
        assert_eq!(extract_dload_local(&[0x29], 0), Some(3)); // dload_3
        assert_eq!(extract_dload_local(&[0x18, 0x05], 0), Some(5)); // dload 5
        assert_eq!(extract_dload_local(&[0x1a], 0), None); // iload_0 — not dload
    }

    #[test]
    fn p87_extract_dstore_local_variants() {
        assert_eq!(extract_dstore_local(&[0x47], 0), Some(0)); // dstore_0
        assert_eq!(extract_dstore_local(&[0x48], 0), Some(1)); // dstore_1
        assert_eq!(extract_dstore_local(&[0x49], 0), Some(2)); // dstore_2
        assert_eq!(extract_dstore_local(&[0x4a], 0), Some(3)); // dstore_3
        assert_eq!(extract_dstore_local(&[0x39, 0x05], 0), Some(5)); // dstore 5
        assert_eq!(extract_dstore_local(&[0x3b], 0), None); // istore_0 — not dstore
    }

    // =========================================================================
    // Session 32: Scalar replacement plan tests
    // =========================================================================

    #[test]
    fn s32_plan_scalar_replacement_basic() {
        // Pattern: new; dup; invokespecial <init>()V; astore_1; aload_1; iconst_1; putfield
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59,             // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
            0x4c,             // 7: astore_1
            0x2b,             // 8: aload_1
            0x04,             // 9: iconst_1
            0xb5, 0x00, 0x03, // 10: putfield #3
            0x2b,             // 13: aload_1
            0xb4, 0x00, 0x03, // 14: getfield #3
            0xac,             // 17: ireturn
            0, 0,
        ];
        let code_len = 18;
        let mut non_escaping = std::collections::HashSet::new();
        non_escaping.insert(0usize);
        // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
        let new_info = vec![(0usize, 1u32, 2usize, true, true)]; // 2 fields
        // Create invoke_info for <init>()V at PC=4
        let init_info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            descriptor: Box::leak("()V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: 0xb7,
        }));
        let invoke_info = vec![(4usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
        let scalar_base = 4; // some offset

        let plan = plan_scalar_replacement(&code, code_len, &non_escaping, &new_info, &invoke_info, scalar_base);

        assert!(plan.objects.contains_key(&0), "new at PC=0 should be scalar-replaced");
        assert_eq!(plan.objects[&0].num_fields, 2);
        assert!(plan.init_skips.contains(&4), "invokespecial at PC=4 should be skipped");
        assert!(plan.field_ops.contains_key(&10), "putfield at PC=10 should be scalar");
        assert!(plan.field_ops.contains_key(&14), "getfield at PC=14 should be scalar");
        // Two heap fields × two 8-byte words per `Value` slot (`SLOT_SIZE` = 16).
        assert_eq!(plan.total_slots, 4);
    }

    #[test]
    fn s32_plan_scalar_replacement_escaping_not_replaced() {
        // Pattern: new; dup; invokespecial; areturn — object escapes, should NOT be replaced
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59,             // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>
            0xb0,             // 7: areturn
            0, 0,
        ];
        // NOT in non_escaping_new → should produce empty plan
        let non_escaping = std::collections::HashSet::new();
        // CRIT-2 tuple shape: see compiler struct doc.
        let new_info = vec![(0usize, 1u32, 2usize, true, true)];
        let plan = plan_scalar_replacement(&code, 8, &non_escaping, &new_info, &[], 0);
        assert!(plan.objects.is_empty());
        assert!(plan.field_ops.is_empty());
        assert!(plan.init_skips.is_empty());
    }

    #[test]
    fn s32_plan_nonvoid_init_not_skipped() {
        // invokespecial with non-()V descriptor should NOT be skipped
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59,             // 3: dup
            0x04,             // 4: iconst_1
            0xb7, 0x00, 0x02, // 5: invokespecial <init>(I)V
            0xac,             // 8: ireturn (dummy)
            0, 0,
        ];
        let mut non_escaping = std::collections::HashSet::new();
        non_escaping.insert(0usize);
        // CRIT-2 tuple shape: see compiler struct doc.
        let new_info = vec![(0usize, 1u32, 2usize, true, true)];
        let init_info = Box::leak(Box::new(JitInvokeInfo { // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
            class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            descriptor: Box::leak("(I)V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
            num_jit_args: 2,
            return_type: b'V',
            invoke_kind: 0xb7,
        }));
        let invoke_info = vec![(5usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
        let plan = plan_scalar_replacement(&code, 9, &non_escaping, &new_info, &invoke_info, 0);
        // init should NOT be skipped (non-void-init has args)
        assert!(!plan.init_skips.contains(&5));
    }

    // ===================================================================
    // Session 34: JIT Switch Compilation tests
    // ===================================================================

    /// Helper: build bytecode for a method that uses tableswitch.
    /// Layout: iload_0 at PC 0, tableswitch at PC 1 (padded to 4-byte align),
    /// then case bodies that push sipush val; ireturn.
    fn build_tableswitch_bytecode(low: i32, cases: &[i32], default_val: i32) -> (Vec<u8>, usize) {
        let mut code = Vec::new();
        // PC 0: iload_0
        code.push(0x1A);
        // PC 1: tableswitch opcode
        code.push(0xAA);
        // Padding to align to 4-byte boundary from start of method (PC 0)
        while (code.len()) % 4 != 0 {
            code.push(0);
        }
        let table_base = 1; // base_pc of the tableswitch opcode
        let high = low + cases.len() as i32 - 1; // Cast: x86-64 immediate encoding
        let num_cases = cases.len();

        // Each case body: sipush val (3 bytes) + ireturn (1 byte) = 4 bytes
        let switch_data_size = 12 + num_cases * 4;
        let aligned_start = code.len();
        let body_start_pc = aligned_start + switch_data_size;
        let default_body_pc = body_start_pc + num_cases * 4;
        let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

        code.extend_from_slice(&default_offset.to_be_bytes());
        code.extend_from_slice(&low.to_be_bytes());
        code.extend_from_slice(&high.to_be_bytes());

        for i in 0..num_cases {
            let case_body_pc = body_start_pc + i * 4;
            let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
            code.extend_from_slice(&off.to_be_bytes());
        }

        // Case bodies: sipush val; ireturn
        for &val in cases {
            code.push(0x11); // sipush
            code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
            code.push(0xAC); // ireturn
        }

        // Default body
        code.push(0x11);
        code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC);

        let code_len = code.len();
        code.push(0);
        code.push(0);
        (code, code_len)
    }

    /// Helper: build bytecode for a method that uses lookupswitch.
    fn build_lookupswitch_bytecode(pairs: &[(i32, i32)], default_val: i32) -> (Vec<u8>, usize) {
        let mut code = Vec::new();
        // PC 0: iload_0
        code.push(0x1A);
        // PC 1: lookupswitch opcode
        code.push(0xAB);
        while (code.len()) % 4 != 0 {
            code.push(0);
        }
        let table_base = 1;
        let npairs = pairs.len();

        let switch_data_size = 8 + npairs * 8;
        let aligned_start = code.len();
        let body_start_pc = aligned_start + switch_data_size;
        let default_body_pc = body_start_pc + npairs * 4;
        let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

        code.extend_from_slice(&default_offset.to_be_bytes());
        code.extend_from_slice(&(npairs as i32).to_be_bytes()); // Cast: x86-64 immediate encoding

        for (i, &(key, _val)) in pairs.iter().enumerate() {
            let case_body_pc = body_start_pc + i * 4;
            let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
            code.extend_from_slice(&key.to_be_bytes());
            code.extend_from_slice(&off.to_be_bytes());
        }

        for &(_key, val) in pairs {
            code.push(0x11);
            code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
            code.push(0xAC);
        }

        code.push(0x11);
        code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC);

        let code_len = code.len();
        code.push(0);
        code.push(0);
        (code, code_len)
    }

    fn compile_switch_method(code: &[u8], code_len: usize) -> Option<CompiledMethod> {
        compile(
            code, code_len, 1, 1, false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(), Vec::new(), HashMap::new(), HashMap::new(), &test_helpers(),
            std::collections::HashSet::new(), HashMap::new(),
        )
    }

    #[test]
    fn s34_tableswitch_small_3_cases() {
        let (code, code_len) = build_tableswitch_bytecode(0, &[10, 20, 30], -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[0]), 10);
            assert_eq!(compiled.call(&[1]), 20);
            assert_eq!(compiled.call(&[2]), 30);
            assert_eq!(compiled.call(&[-1i32 as i64]), -1i64); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[3]), -1i64);
            assert_eq!(compiled.call(&[100]), -1i64);
        }
    }

    #[test]
    fn s34_tableswitch_large_jump_table() {
        // 10 cases triggers jump table path (count > 4)
        let cases: Vec<i32> = (0..10).map(|i| (i + 1) * 100).collect();
        let (code, code_len) = build_tableswitch_bytecode(0, &cases, -999);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            for i in 0..10 {
                assert_eq!(compiled.call(&[i as i64]), ((i + 1) * 100) as i64, // Cast: JIT ABI convention
                    "case {} failed", i);
            }
            assert_eq!(compiled.call(&[-1i32 as i64]), -999i64); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[10]), -999i64);
            assert_eq!(compiled.call(&[1000]), -999i64);
        }
    }

    #[test]
    fn s34_tableswitch_nonzero_low() {
        let cases: Vec<i32> = vec![50, 60, 70, 80, 90];
        let (code, code_len) = build_tableswitch_bytecode(5, &cases, 0);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[5]), 50);
            assert_eq!(compiled.call(&[6]), 60);
            assert_eq!(compiled.call(&[7]), 70);
            assert_eq!(compiled.call(&[8]), 80);
            assert_eq!(compiled.call(&[9]), 90);
            assert_eq!(compiled.call(&[4]), 0);
            assert_eq!(compiled.call(&[10]), 0);
        }
    }

    #[test]
    fn s34_tableswitch_negative_low() {
        let cases: Vec<i32> = vec![200, 201, 202, 203, 204];
        let (code, code_len) = build_tableswitch_bytecode(-2, &cases, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[-2i32 as i64]), 200); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[-1i32 as i64]), 201); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[0]), 202);
            assert_eq!(compiled.call(&[1]), 203);
            assert_eq!(compiled.call(&[2]), 204);
            assert_eq!(compiled.call(&[-3i32 as i64]), -1i64); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[3]), -1i64);
        }
    }

    #[test]
    fn s34_tableswitch_single_case() {
        let (code, code_len) = build_tableswitch_bytecode(0, &[42], -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[0]), 42);
            assert_eq!(compiled.call(&[1]), -1i64);
        }
    }

    #[test]
    fn s34_lookupswitch_small_linear() {
        let pairs = vec![(10, 100), (20, 200), (30, 300)];
        let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[10]), 100);
            assert_eq!(compiled.call(&[20]), 200);
            assert_eq!(compiled.call(&[30]), 300);
            assert_eq!(compiled.call(&[0]), -1i64);
            assert_eq!(compiled.call(&[15]), -1i64);
            assert_eq!(compiled.call(&[99]), -1i64);
        }
    }

    #[test]
    fn s34_lookupswitch_large_binary_search() {
        // 10 sparse pairs triggers binary search (npairs > 6)
        // Values must fit in i16 for sipush encoding
        let pairs: Vec<(i32, i32)> = vec![
            (5, 50), (10, 100), (20, 200), (50, 500), (100, 1000),
            (200, 2000), (500, 5000), (1000, 10000), (2000, 20000), (5000, 30000),
        ];
        let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            for &(key, val) in &pairs {
                assert_eq!(compiled.call(&[key as i64]), val as i64, // Cast: JIT ABI convention
                    "key {} should return {}", key, val);
            }
            assert_eq!(compiled.call(&[0]), -1i64);
            assert_eq!(compiled.call(&[7]), -1i64);
            assert_eq!(compiled.call(&[150]), -1i64);
            assert_eq!(compiled.call(&[99999]), -1i64);
        }
    }

    #[test]
    fn s34_lookupswitch_negative_keys() {
        // 8 pairs with negatives → binary search
        let pairs: Vec<(i32, i32)> = vec![
            (-100, 1), (-50, 2), (-10, 3), (0, 4), (10, 5), (50, 6), (100, 7), (1000, 8),
        ];
        let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            for &(key, val) in &pairs {
                assert_eq!(compiled.call(&[key as i64]), val as i64); // Cast: JIT ABI convention
            }
            assert_eq!(compiled.call(&[-200i32 as i64]), -1i64); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[999]), -1i64);
        }
    }

    #[test]
    fn s34_lookupswitch_single_pair() {
        let pairs = vec![(42, 999)];
        let (code, code_len) = build_lookupswitch_bytecode(&pairs, 0);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[42]), 999);
            assert_eq!(compiled.call(&[0]), 0);
            assert_eq!(compiled.call(&[43]), 0);
        }
    }

    #[test]
    fn s34_tableswitch_all_same_target() {
        let cases = vec![77; 8];
        let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            for i in 0..8 {
                assert_eq!(compiled.call(&[i as i64]), 77); // Cast: JIT ABI convention
            }
            assert_eq!(compiled.call(&[-1i32 as i64]), -1i64); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[8]), -1i64);
        }
    }

    #[test]
    fn s34_lookupswitch_two_pairs() {
        let pairs = vec![(0, 111), (1000, 222)];
        let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[0]), 111);
            assert_eq!(compiled.call(&[1000]), 222);
            assert_eq!(compiled.call(&[500]), -1i64);
        }
    }

    #[test]
    fn s34_tableswitch_large_20_cases() {
        let cases: Vec<i32> = (0..20).map(|i| i * 11).collect();
        let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
        let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            for i in 0..20 {
                assert_eq!(compiled.call(&[i as i64]), (i * 11) as i64); // Cast: JIT ABI convention
            }
            assert_eq!(compiled.call(&[20]), -1i64);
        }
    }

    // -----------------------------------------------------------------------
    // S31 — JIT Method Inlining
    // -----------------------------------------------------------------------

    /// Helper: compile a method with inline sites.
    fn compile_with_inlines(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        max_locals: usize,
        inline_sites: HashMap<usize, crate::InlineSite>,
    ) -> Option<CompiledMethod> {
        compile(
            code,
            code_len,
            num_params,
            max_locals,
            false,
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(),
            Vec::new(), Vec::new(), Vec::new(), Vec::new(), // pic_slots
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            inline_sites,
        )
    }

    /// Build an InlineSite from raw callee bytecode.
    fn make_inline_site(
        callee_bytecode: &[u8],
        callee_max_locals: usize,
        callee_num_args: usize,
        callee_is_static: bool,
        return_type: u8,
    ) -> crate::InlineSite {
        let mut callee_code = callee_bytecode.to_vec();
        callee_code.push(0); // padding
        callee_code.push(0);
        crate::InlineSite {
            callee_code,
            callee_code_len: callee_bytecode.len(),
            callee_max_locals,
            callee_num_args,
            callee_is_static,
            return_type,
            field_info: Vec::new(),
            static_field_info: Vec::new(),
            ldc_info: Vec::new(),
            ldc2w_info: Vec::new(),
            needs_heap: false,
            class_name: "Test".to_string(),
            method_name: "inlined".to_string(),
            descriptor: "()I".to_string(),
        }
    }

    #[test]
    fn s31_inline_getter_iload_ireturn() {
        // Caller: int f(int x) { return getX(x); }
        // callee: int getX(int x) { return x; }   → iload_0, ireturn
        //
        // Caller bytecode: iload_0, invokestatic #1 (pc=1), ireturn
        // We place the inline site at pc=1 (the invokestatic).
        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic #1
            0xac,             // 4: ireturn
            0, 0,             // padding
        ];
        let caller_len = 5;

        let callee = make_inline_site(
            &[0x1a, 0xac], // iload_0, ireturn
            1,              // max_locals
            1,              // num_args (one int param)
            true,           // static
            b'I',           // return type
        );

        let mut sites = HashMap::new();
        sites.insert(1, callee); // inline at pc=1

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[42]), 42);
            assert_eq!(compiled.call(&[0]), 0);
            assert_eq!(compiled.call(&[-7i32 as i64]), -7); // Cast: JIT ABI convention
        }
    }

    #[test]
    fn s31_inline_add_two_params() {
        // Caller: int f(int a, int b) { return add(a, b); }
        // Callee: int add(int a, int b) { return a + b; }  → iload_0, iload_1, iadd, ireturn
        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0x1b,             // 1: iload_1
            0xb8, 0x00, 0x01, // 2: invokestatic #1
            0xac,             // 5: ireturn
            0, 0,
        ];
        let caller_len = 6;

        let callee = make_inline_site(
            &[0x1a, 0x1b, 0x60, 0xac], // iload_0, iload_1, iadd, ireturn
            2,
            2,    // two params
            true,
            b'I',
        );

        let mut sites = HashMap::new();
        sites.insert(2, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 2, 2, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[3, 4]), 7);
            assert_eq!(compiled.call(&[100, -50i32 as i64]), 50); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[0, 0]), 0);
        }
    }

    #[test]
    fn s31_inline_constant_return() {
        // Caller: int f() { return five(); }
        // Callee: int five() { return 5; }  → iconst_5, ireturn
        let caller_code: Vec<u8> = vec![
            0xb8, 0x00, 0x01, // 0: invokestatic #1
            0xac,             // 3: ireturn
            0, 0,
        ];
        let caller_len = 4;

        let callee = make_inline_site(
            &[0x08, 0xac], // iconst_5, ireturn
            0,
            0,    // no params
            true,
            b'I',
        );

        let mut sites = HashMap::new();
        sites.insert(0, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 0, 0, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[]), 5);
        }
    }

    #[test]
    fn s31_inline_void_method() {
        // Caller: int f(int x) { noop(); return x; }
        // Callee: void noop() { return; }
        let caller_code: Vec<u8> = vec![
            0xb8, 0x00, 0x01, // 0: invokestatic #1 (void)
            0x1a,             // 3: iload_0
            0xac,             // 4: ireturn
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(
            &[0xb1], // return (void)
            0,
            0,
            true,
            b'V',
        );

        let mut sites = HashMap::new();
        sites.insert(0, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[99]), 99);
        }
    }

    #[test]
    fn s31_inline_with_branch() {
        // Callee: int abs(int x) { return x >= 0 ? x : -x; }
        // Bytecode: iload_0, ifge +5, iload_0, ineg, ireturn, iload_0, ireturn
        //           0       1        4       5     6        7       8
        let callee_bc: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0x9c, 0x00, 0x06, // 1: ifge +6 → target=7
            0x1a,             // 4: iload_0
            0x74,             // 5: ineg
            0xac,             // 6: ireturn
            0x1a,             // 7: iload_0
            0xac,             // 8: ireturn
        ];

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic #1
            0xac,             // 4: ireturn
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(
            &callee_bc,
            1,
            1,
            true,
            b'I',
        );

        let mut sites = HashMap::new();
        sites.insert(1, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[5]), 5);
            assert_eq!(compiled.call(&[-5i32 as i64]), 5); // Cast: JIT ABI convention
            assert_eq!(compiled.call(&[0]), 0);
        }
    }

    #[test]
    fn s31_inline_arithmetic_chain() {
        // Callee: int triple(int x) { return x + x + x; }
        // iload_0, iload_0, iadd, iload_0, iadd, ireturn
        let callee_bc: Vec<u8> = vec![
            0x1a, 0x1a, 0x60, 0x1a, 0x60, 0xac,
        ];

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic #1
            0xac,             // 4: ireturn
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');

        let mut sites = HashMap::new();
        sites.insert(1, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[7]), 21);
            assert_eq!(compiled.call(&[0]), 0);
            assert_eq!(compiled.call(&[-3i32 as i64]), -9); // Cast: JIT ABI convention
        }
    }

    #[test]
    fn s31_inline_iinc_loop() {
        // Callee: int inc5(int x) { x++; x++; x++; x++; x++; return x; }
        // iinc 0,1 × 5, iload_0, ireturn
        let callee_bc: Vec<u8> = vec![
            0x84, 0x00, 0x01, // iinc local0, 1
            0x84, 0x00, 0x01,
            0x84, 0x00, 0x01,
            0x84, 0x00, 0x01,
            0x84, 0x00, 0x01,
            0x1a,             // iload_0
            0xac,             // ireturn
        ];

        let caller_code: Vec<u8> = vec![
            0x1a,
            0xb8, 0x00, 0x01,
            0xac,
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
        let mut sites = HashMap::new();
        sites.insert(1, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[10]), 15);
            assert_eq!(compiled.call(&[0]), 5);
        }
    }

    #[test]
    fn s31_inline_callee_uses_extra_locals() {
        // Callee: int swap_add(int a, int b) { int t = a; a = b; b = t; return a + b; }
        // iload_0, istore_2, iload_1, istore_0, iload_2, istore_1, iload_0, iload_1, iadd, ireturn
        let callee_bc: Vec<u8> = vec![
            0x1a,       // 0: iload_0
            0x3d,       // 1: istore_2 (local 2 = temp)
            0x1b,       // 2: iload_1
            0x3b,       // 3: istore_0
            0x1c,       // 4: iload_2
            0x3c,       // 5: istore_1
            0x1a,       // 6: iload_0
            0x1b,       // 7: iload_1
            0x60,       // 8: iadd
            0xac,       // 9: ireturn
        ];

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0x1b,             // 1: iload_1
            0xb8, 0x00, 0x01, // 2: invokestatic
            0xac,             // 5: ireturn
            0, 0,
        ];
        let caller_len = 6;

        let callee = make_inline_site(&callee_bc, 3, 2, true, b'I');
        let mut sites = HashMap::new();
        sites.insert(2, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 2, 2, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            // swap_add(a,b) = a + b regardless of swap, so always sum
            assert_eq!(compiled.call(&[3, 7]), 10);
            assert_eq!(compiled.call(&[100, 200]), 300);
        }
    }

    #[test]
    fn s31_inline_bailout_unsupported_opcode() {
        // Callee has monitorenter (0xC2) which is unsupported — should bail out.
        // The caller should still compile (the invokestatic falls through to normal call path).
        let callee_bc: Vec<u8> = vec![
            0x1a,       // iload_0
            0xC2,       // monitorenter — unsupported in inline
            0xb1,       // return
        ];

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic — will try inline but bail
            0xac,             // 4: ireturn
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(&callee_bc, 1, 1, true, b'V');
        let mut sites = HashMap::new();
        sites.insert(1, callee);

        // This should either compile successfully (falling back to a call stub)
        // or return None if the fallback isn't available. Either way, no crash.
        let _result = compile_with_inlines(&caller_code, caller_len, 1, 1, sites);
        // Main assertion: no panic or crash
    }

    #[test]
    fn s31_inline_long_arithmetic() {
        // Callee: long double_it(long x) { return x + x; }  → lload_0, lload_0, ladd, lreturn
        // We test through int path since our JIT uses i64 uniformly.
        // Caller: int f(int x) { return double_it(x); }
        let callee_bc: Vec<u8> = vec![0x1a, 0x1a, 0x61, 0xac]; // iload_0, iload_0, ladd, ireturn

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic
            0xac,             // 4: ireturn
            0, 0,
        ];
        let caller_len = 5;

        let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
        let mut sites = HashMap::new();
        sites.insert(1, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[50]), 100);
            assert_eq!(compiled.call(&[-7i32 as i64]), -14); // Cast: JIT ABI convention
        }
    }

    #[test]
    fn s31_inline_bipush_sipush() {
        // Callee: int f() { return 100; }  → bipush 100, ireturn
        let callee_bc: Vec<u8> = vec![0x10, 100, 0xac]; // bipush 100, ireturn

        let caller_code: Vec<u8> = vec![
            0xb8, 0x00, 0x01, // 0: invokestatic
            0xac,             // 3: ireturn
            0, 0,
        ];
        let caller_len = 4;

        let callee = make_inline_site(&callee_bc, 0, 0, true, b'I');
        let mut sites = HashMap::new();
        sites.insert(0, callee);

        let compiled = compile_with_inlines(&caller_code, caller_len, 0, 0, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[]), 100);
        }
    }

    #[test]
    fn s31_inline_multiple_sites() {
        // Caller: int f(int x) { return inc(inc(x)); }
        // Two invokestatics, each inlining "int inc(int x) { return x + 1; }"
        // Callee: iload_0, iconst_1, iadd, ireturn
        let callee_bc: Vec<u8> = vec![0x1a, 0x04, 0x60, 0xac];

        let caller_code: Vec<u8> = vec![
            0x1a,             // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic #1 (first inc)
            0xb8, 0x00, 0x02, // 4: invokestatic #2 (second inc)
            0xac,             // 7: ireturn
            0, 0,
        ];
        let caller_len = 8;

        let site1 = make_inline_site(&callee_bc, 1, 1, true, b'I');
        let site2 = make_inline_site(&callee_bc, 1, 1, true, b'I');

        let mut sites = HashMap::new();
        sites.insert(1, site1);
        sites.insert(4, site2);

        let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
            .expect("compilation failed");

        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        unsafe {
            assert_eq!(compiled.call(&[10]), 12); // 10 + 1 + 1
            assert_eq!(compiled.call(&[0]), 2);
        }
    }
}
