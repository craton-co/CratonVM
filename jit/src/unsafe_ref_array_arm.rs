// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 6 (lane T1DISP20, DISP20-8 of
//! `docs/feature-designs/native-dispatch-architecture.md`; design
//! `docs/feature-designs/jit-ir-optimizations.md`,
//! phase 2): the REFERENCE-ARRAY-element arm of the `Unsafe`
//! `compareAndSetReference` prefix (AT-4) and of the reference volatile GET
//! prefix (UPDATERS21-1), in both tiers. Real users:
//! `ConcurrentHashMap.casTabAt` / `tabAt` (`putVal`, `get`), `ForkJoinPool`
//! queue slots, JCTools-style ring buffers.
//!
//! # Why it is in the natives' atomicity domain
//!
//! The design's phase 1 was built by nio21 wave 6 L3: every `Unsafe` CAS on a
//! reference-array element is ONE hardware `LOCK CMPXCHG` on the element slot
//! (`vm_exec::hw_cas_reference_array_element`, reached from
//! `compare_and_swap_field_shared` by the native and by the thin helper
//! `jit_unsafe_cas_reference_direct`). The tier-1 book's "blocking fact"
//! (`hw_atomic_addr_checked` answering `HwAtomicRefusal::Array`) is the FIELD
//! cell publisher's refusal only, not the element CAS. So a compiled
//! `LOCK CMPXCHG` on the same 8 bytes compares the same raw pointer words the
//! native does, and a plain `aastore` / release store racing either is the
//! JMM's racy write on both sides, as on HotSpot.
//!
//! The VM sets this arm's admission bits ([`UNSAFE_REF_ARRAY_CAS_GATE_BIT`],
//! [`UNSAFE_REF_ARRAY_GET_GATE_BIT`]) only where that holds for every element
//! the screen admits: the Generational or G1 heap (never ZGC), no compressed
//! oops (a slot is then the 8-byte bare pointer at `base + offset`, the
//! `Unsafe` scale being 8 in every layout), `CRATONVM_UNSAFE_REF_ARRAY_HW_CAS`
//! on, and the thin helper's own gate not refused (as bit 18 for AT-4).
//!
//! # The screen ([`unsafe_ref_array_screen_bytes`])
//!
//! Exactly the elements `fast_native::unsafe_reference_array_slot` maps to an
//! index (the thin helper's array arm): the base's kind/element tag byte is
//! the reference array's (`ObjectKind::Array`, element `Reference`: one byte,
//! bits 6..7 reserved zero, every dimension), `16 <= offset < 0x2000_0000`,
//! `(offset - 16) % 8 == 0` and `(offset - 16) / 8 < length`. Anything else
//! declines to the call, whose native keeps its answer (a band offset, a
//! misaligned or out-of-range offset).
//!
//! # Barriers
//!
//! A get needs none on Generational / G1 (the compiled `aaload` is the same
//! plain load). The CAS takes the gated reference-store screen BEFORE the
//! `CMPXCHG` (`ir_lower::vh_ref_store_gate_screen_bytes`, as AT-4): the SATB
//! pre-gate clear, and a null new value, a young holder or no old objects --
//! all holder-level facts, so they rule out the element card the native's
//! `ref_cas_post_barrier` would dirty (`write_barrier_ref_array_element`) as
//! much as a field card. Anything else declines; a collector with no gate plan
//! (G1) keeps every CAS site on its call.
//!
//! No safepoint lies between the screen's loads and the access, so the base
//! cannot move under the computed address (AT-4's argument); the decline edge
//! is the unchanged call with its own map.

use crate::direct_helpers::DirectHelperTable;

/// `CRATONVM_JIT_UNSAFE_REF_ARRAY_INLINE` (j27 wave 6, lane T1DISP20,
/// DISP20-8; default ON since 2026-10-28 (j27 WARNING flip; =0 restores)):
/// the reference-array arm of the
/// `compareAndSetReference` and `getReference{Volatile,Acquire,Opaque}`
/// prefixes in both tiers. Off, the VM sets neither admission bit, the get
/// rows are recognised only under `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD`
/// as before, and every emitted byte is unchanged. Read per site at
/// lowering and per table build, so no cache.
pub fn unsafe_ref_array_inline_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true}

/// The [`DirectHelperTable::unsafe_inline_admitted`] bit of the CAS arm: the
/// VM sets it when the heap shape admits the arm (module doc) and the
/// `compareAndSetReference` thin helper's gate would not refuse. After every
/// older row (the sub-word rows end at bit 32).
pub const UNSAFE_REF_ARRAY_CAS_GATE_BIT: usize = 34;
/// ... and of the volatile get arm (the `getReferenceVolatile` thin helper's
/// gate).
pub const UNSAFE_REF_ARRAY_GET_GATE_BIT: usize = 35;
const _: () = assert!(UNSAFE_REF_ARRAY_GET_GATE_BIT < usize::BITS as usize);

/// The first `Unsafe` offset past the arm: the synthetic-offset band the thin
/// helper's array arm also leaves to the native.
pub const UNSAFE_REF_ARRAY_OFFSET_LIMIT: i32 = 0x2000_0000;

/// The whole kind/element tag byte of a reference array of any dimension.
pub const REF_ARRAY_KIND_TAGS_BYTE: u8 = cratonvm_types::ObjectKind::Array as u8
    | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);

/// May this compile emit the reference-array arm whose admission bit is
/// `bit`? The bit, the switch, the accessor family, and no inline-blocking
/// reference representation (compressed oops, an armed ZGC read barrier).
pub fn unsafe_ref_array_arm_admitted(table: &DirectHelperTable, bit: usize) -> bool {
    table.unsafe_inline_admitted & (1usize << bit) != 0
        && unsafe_ref_array_inline_enabled()
        && crate::direct_helpers::unsafe_accessor_inline_enabled()
        && !crate::x64::narrow_oops_block_inline_fields()
}

/// The VM's half of the admission (pure, so it is tested here): the heap is
/// Generational or G1, references are not compressed, the VM's
/// reference-array hardware CAS is on, and the switch is on. The VM adds each
/// row's thin-helper gate.
pub fn unsafe_ref_array_heap_admits(
    generational_or_g1: bool,
    narrow_oops: bool,
    ref_array_hw_cas: bool,
) -> bool {
    generational_or_g1 && !narrow_oops && ref_array_hw_cas && unsafe_ref_array_inline_enabled()
}

/// The element screen both tiers emit. On entry RAX = a non-null base already
/// known to be an ARRAY (the not-array test's fall-through), R11 = the
/// `Unsafe` offset (a `long`). Falls through with RAX and R11 unchanged
/// (`[RAX + R11]` is the element slot) or jumps to a decline edge. Writes
/// RCX, RDX, R10 and the flags. Returns the bytes and the rel32 decline
/// fields.
pub fn unsafe_ref_array_screen_bytes() -> (Vec<u8>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: header byte, disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    const LEN: u8 = cratonvm_types::ARRAY_LENGTH_OFFSET as u8; // Cast: header byte, disp8
    const _: () = assert!(cratonvm_types::ARRAY_LENGTH_OFFSET <= 127);
    const ABASE: u8 = cratonvm_types::ARRAY_DATA_OFFSET as u8; // Cast: 16
    const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET == 16);
    let mut b: Vec<u8> = Vec::with_capacity(64);
    let mut decline: Vec<usize> = Vec::new();
    let mut jcc = |b: &mut Vec<u8>, cc: u8| {
        b.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
        decline.push(b.len() - 4);
    };
    // The whole tag byte: exactly a reference array.
    b.extend_from_slice(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX+KIND_TAGS]
    b.extend_from_slice(&[0x83, 0xF9, REF_ARRAY_KIND_TAGS_BYTE]); // CMP ECX, imm8
    jcc(&mut b, 0x85); // JNE: a primitive array
    b.extend_from_slice(&[0x49, 0x83, 0xFB, ABASE]); // CMP R11, 16
    jcc(&mut b, 0x8C); // JL (signed): below the payload, or negative
    b.extend_from_slice(&[0x49, 0x81, 0xFB]); // CMP R11, imm32
    b.extend_from_slice(&UNSAFE_REF_ARRAY_OFFSET_LIMIT.to_le_bytes());
    jcc(&mut b, 0x8D); // JGE (signed): the synthetic band
    // R10 = offset - 16, the element's byte offset in the payload.
    b.extend_from_slice(&[0x4D, 0x8D, 0x53, ABASE.wrapping_neg()]); // LEA R10, [R11 - 16]
    b.extend_from_slice(&[0x41, 0xF6, 0xC2, 0x07]); // TEST R10B, 7
    jcc(&mut b, 0x85); // JNZ: not an element boundary
    // RDX = payload bytes = length * 8 (a u32 shifted by 3: no wrap).
    b.extend_from_slice(&[0x8B, 0x50, LEN]); // MOV EDX, [RAX+LEN] (zero-extends)
    b.extend_from_slice(&[0x48, 0xC1, 0xE2, 0x03]); // SHL RDX, 3
    b.extend_from_slice(&[0x49, 0x39, 0xD2]); // CMP R10, RDX
    jcc(&mut b, 0x83); // JAE (unsigned): past the last element
    (b, decline)
}

/// `[CRATONVM_DBG_JITC]` census line of one emitted arm (`what` names the
/// tier and row), so a host run can count engaged sites.
pub fn note_ref_array_arm(what: &str, method_label: &str) {
    if crate::x64::dbg_jitc_enabled() {
        eprintln!("[cratonvm-jitc] unsafe-ref-array-inline ({what}) {method_label}");
    }
}

#[cfg(test)]
mod j27_t1disp20_ref_array_arm_tests;
