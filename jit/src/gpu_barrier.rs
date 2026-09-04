// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The compiled-tier GPU input-residency barrier.
//!
//! # The hole this closes
//!
//! `offload::input_cache` mirrors a Java primitive array in device memory
//! across kernel submissions. A host write to such an array makes the
//! device copy stale, so every path that writes one must evict the entry.
//! The interpreter's `*astore` arms do, the quickened `field_fast` arm
//! does, and `jit::helpers::jit_iastore` / `jit_bastore` do.
//!
//! **Neither compiled tier does, because neither calls those helpers.**
//! The single-pass backend lowers `iastore`/`lastore`/`fastore`/
//! `dastore`/`bastore`/`castore`/`sastore` inline
//! (`x64/bytecode_walk.rs`, one arm each, straight to
//! `emit_int_astore_regs` and friends), and the IR backend lowers
//! `Op::ArrayStore` to a raw `MOV`/`MOVSS`/`MOVSD` (`ir_lower.rs`). The
//! complete-opcode helpers that DO invalidate are simply not reached from
//! either — the same shape as the `aastore` covariance check that
//! `jit_aastore` kept enforcing after the inline path stopped calling it
//! (`W7-38-jit-aastore-never-called-its-own-check.md`).
//!
//! What stood in for a barrier was `runtime::offload_jit_gate`: while
//! `--gpu` is active it refused JIT admission to *any* method containing
//! one of those seven opcodes. That is sound, and enormously broad. On
//! kfusion it denied compilation to 21 methods — `Float2/3/4/8.set`,
//! `Int2/Int3.set`, `Short2.set`, `Byte3/Byte4.set`, `ImageFloat.get`
//! and `.set` — i.e. the whole TornadoVM vector and image accessor
//! family, the data-structure layer every host-side loop in that program
//! runs through, whether or not a single kernel ever offloads.
//!
//! # Why this is a dirty-mark and not a call
//!
//! The obvious barrier is "call `input_cache::invalidate` on a filter
//! hit". Both backends make that expensive in the same way: values live
//! in caller-saved registers across a store (the single-pass operand
//! stack parks in `SCRATCH_REGS`/`Xmm` slots, and only
//! `flush_scratch_registers` — emitted on the HOT path — makes a call
//! safe there). Paying a flush on every array store to fund a call taken
//! essentially never is the wrong trade.
//!
//! So the compiled store does not evict. It records which *bucket* was
//! written into a 64-byte side table, and the eviction happens in
//! `runtime::offload::input_cache::drain_compiled_writes` — from Rust, on
//! the marshalling path, before any cached buffer can be read. The window
//! between the store and the drain is invisible: nothing consults the
//! cache inside it.
//!
//! The bucket is `(addr >> 3) & 63`, the same index `input_cache`'s own
//! `addr_bit` uses, so a dirty bucket names at most the entries that
//! filter bit already names. Collisions cost a re-upload and nothing
//! else.
//!
//! # The emitted sequence
//!
//! Assumes **RAX holds the array pointer** — true at every store site in
//! both backends, since that is where the null and bounds checks put it.
//! Clobbers **R10, R11 and the flags**, all three of which are dead at a
//! store boundary in both backends (R10 is the bounds check's own
//! scratch; R11 is in neither `ARG_REGS` nor `SCRATCH_REGS`, so the
//! single-pass operand-stack cache never parks a value there; the IR
//! backend's register residency uses only callee-saved GPRs).
//!
//! ```text
//!   MOV  R11, &ADDR_FILTER      ; 10
//!   CMP  QWORD [R11], 0         ;  4   nothing cached anywhere ->
//!   JZ   .skip                  ;  2   16 bytes, 3 insns, always
//!   MOV  R11, [R11]             ;  3   the filter word
//!   MOV  R10, RAX               ;  3
//!   SHR  R10, 3                 ;  4   heap objects are 8-aligned
//!   AND  R10, 63                ;  4
//!   BT   R11, R10               ;  4   is THIS array cached?
//!   JNC  .skip                  ;  2
//!   MOV  R11, &DIRTY            ; 10
//!   MOV  BYTE [R11 + R10], 1    ;  5
//! .skip:
//! ```
//!
//! The hot path is what a process that never submits a kernel runs: one
//! `MOV`-immediate, one compare against a word that is zero for the life
//! of such a run, one not-taken branch.
//!
//! # Arming, and why it is also the kill switch
//!
//! [`arm`] is called once at VM startup, and only when GPU offload is
//! enabled for the process. Unarmed, [`barrier_bytes`] returns `None` and
//! **not one byte is emitted** — a CPU-only run, or a `gpu-offload` build
//! started without `--gpu`, gets byte-identical codegen to before this
//! module existed. `CRATONVM_JIT_GPU_ARRAY_BARRIER=0` refuses to arm,
//! which restores the old refuse-to-compile behaviour in
//! `offload_jit_gate` along with it.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Address of `input_cache::ADDR_FILTER`, or 0 when not armed.
static FILTER_ADDR: AtomicUsize = AtomicUsize::new(0);
/// Address of `input_cache::DIRTY[0]`, or 0 when not armed.
static DIRTY_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Total length of the emitted sequence. Both `rel8` displacements are
/// derived from it, so the encoder and the jump targets cannot drift
/// apart.
pub const BARRIER_LEN: usize = 51;

/// Publish the two addresses the emitted sequence needs.
///
/// Called once, from VM startup, before any method can be compiled — a
/// method compiled before this ran would carry no barrier and could
/// stale a cache entry created later. Idempotent, and safe to call from
/// several VMs in one process: `ADDR_FILTER` and the dirty table are
/// process-global (the drain evicts by bucket across every VM's table),
/// so every caller publishes the same two values.
pub fn arm(filter_addr: usize, dirty_addr: usize) {
    FILTER_ADDR.store(filter_addr, Ordering::Release);
    DIRTY_ADDR.store(dirty_addr, Ordering::Release);
}

/// Is the barrier armed? `false` means every array store compiles exactly
/// as it did before this module existed.
#[inline]
pub fn is_armed() -> bool {
    FILTER_ADDR.load(Ordering::Acquire) != 0 && DIRTY_ADDR.load(Ordering::Acquire) != 0
}

/// The bytes to emit after an inline primitive array store, or `None`
/// when the barrier is not armed.
///
/// See the module docs for the sequence, its register contract (RAX in,
/// R10/R11/flags clobbered) and the two relative jumps' arithmetic.
pub fn barrier_bytes() -> Option<Vec<u8>> {
    let filter = FILTER_ADDR.load(Ordering::Acquire);
    let dirty = DIRTY_ADDR.load(Ordering::Acquire);
    if filter == 0 || dirty == 0 {
        return None;
    }
    let mut b = Vec::with_capacity(BARRIER_LEN);

    // MOV R11, imm64 — REX.WB(0x49) + B8+rd (rd = R11 & 7 = 3)
    b.extend_from_slice(&[0x49, 0xBB]);
    b.extend_from_slice(&(filter as u64).to_le_bytes());
    // CMP QWORD [R11], 0 — REX.WB + 83 /7 ib, ModRM mod=00 reg=111 rm=011
    b.extend_from_slice(&[0x49, 0x83, 0x3B, 0x00]);
    // JZ .skip
    b.extend_from_slice(&[0x74, (BARRIER_LEN - 16) as u8]);
    // MOV R11, [R11] — REX.WRB(0x4D) + 8B /r, ModRM mod=00 reg=011 rm=011
    b.extend_from_slice(&[0x4D, 0x8B, 0x1B]);
    // MOV R10, RAX — REX.WB + 89 /r, ModRM mod=11 reg=000(RAX) rm=010(R10)
    b.extend_from_slice(&[0x49, 0x89, 0xC2]);
    // SHR R10, 3 — REX.WB + C1 /5 ib, ModRM mod=11 reg=101 rm=010
    b.extend_from_slice(&[0x49, 0xC1, 0xEA, 0x03]);
    // AND R10, 63 — REX.WB + 83 /4 ib, ModRM mod=11 reg=100 rm=010
    b.extend_from_slice(&[0x49, 0x83, 0xE2, 0x3F]);
    // BT R11, R10 — REX.WRB + 0F A3 /r, ModRM mod=11 reg=010(R10) rm=011(R11).
    // Register destination, so the bit offset is taken mod 64 — already
    // masked above, so the two agree by construction rather than by luck.
    b.extend_from_slice(&[0x4D, 0x0F, 0xA3, 0xD3]);
    // JNC .skip
    b.extend_from_slice(&[0x73, (BARRIER_LEN - 36) as u8]);
    // MOV R11, imm64 (&DIRTY)
    b.extend_from_slice(&[0x49, 0xBB]);
    b.extend_from_slice(&(dirty as u64).to_le_bytes());
    // MOV BYTE [R11 + R10*1], 1 — REX.XB(0x43) + C6 /0 ib,
    // ModRM mod=00 reg=000 rm=100(SIB), SIB scale=0 index=010(R10) base=011(R11)
    b.extend_from_slice(&[0x43, 0xC6, 0x04, 0x13, 0x01]);

    debug_assert_eq!(b.len(), BARRIER_LEN);
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arming is process-global, so the encoder is exercised through a
    /// saved-and-restored pair rather than by calling `arm` and leaving
    /// the rest of this test binary's codegen changed.
    fn encode_with(filter: usize, dirty: usize) -> Vec<u8> {
        let saved = (
            FILTER_ADDR.load(Ordering::Acquire),
            DIRTY_ADDR.load(Ordering::Acquire),
        );
        arm(filter, dirty);
        let out = barrier_bytes().expect("armed");
        arm(saved.0, saved.1);
        out
    }

    #[test]
    fn the_two_jumps_land_exactly_on_the_end() {
        let b = encode_with(0x1122_3344_5566_7788, 0x0088_7766_5544_3322);
        assert_eq!(b.len(), BARRIER_LEN);
        // Each rel8 is measured from the first byte AFTER the branch.
        // Landing one short would re-enter the sequence at
        // `MOV R11,[R11]` with the filter address already consumed;
        // landing one long would resume mid-instruction in whatever the
        // caller emitted next.
        assert_eq!(b[14], 0x74, "JZ opcode");
        assert_eq!(16 + b[15] as usize, BARRIER_LEN, "JZ must land on .skip");
        assert_eq!(b[34], 0x73, "JNC opcode");
        assert_eq!(36 + b[35] as usize, BARRIER_LEN, "JNC must land on .skip");
    }

    #[test]
    fn both_addresses_are_embedded_little_endian() {
        let filter = 0x0000_7FFF_AABB_CCDDusize;
        let dirty = 0x0000_7FFF_AABB_CD1Dusize;
        let b = encode_with(filter, dirty);
        assert_eq!(&b[2..10], &(filter as u64).to_le_bytes());
        assert_eq!(&b[38..46], &(dirty as u64).to_le_bytes());
    }

    /// The fast path is the whole cost argument: a run that never submits
    /// a kernel executes exactly these three instructions per array
    /// store, against a word that stays zero for its lifetime.
    #[test]
    fn the_fast_path_is_three_instructions() {
        let b = encode_with(0x1000, 0x2000);
        assert_eq!(&b[0..2], &[0x49, 0xBB], "MOV R11, imm64");
        assert_eq!(&b[10..14], &[0x49, 0x83, 0x3B, 0x00], "CMP QWORD [R11], 0");
        assert_eq!(b[14], 0x74, "JZ");
    }

    /// A partially-published pair must emit nothing rather than a
    /// sequence that stores through a null pointer.
    #[test]
    fn a_half_armed_pair_emits_nothing() {
        let saved = (
            FILTER_ADDR.load(Ordering::Acquire),
            DIRTY_ADDR.load(Ordering::Acquire),
        );
        arm(0x1000, 0);
        assert!(barrier_bytes().is_none());
        assert!(!is_armed());
        arm(0, 0x2000);
        assert!(barrier_bytes().is_none());
        assert!(!is_armed());
        arm(saved.0, saved.1);
    }
}
