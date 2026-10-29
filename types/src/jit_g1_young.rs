// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The table G1's compiled young-holder filter reads (JIT round 23 wave 3,
//! lane L4; `CRATONVM_JIT_G1_INLINE_YOUNG_FILTER`).
//!
//! It lives here, not in `cratonvm-gc`, for the reason [`crate::narrow_oop`]
//! does: the JIT reaches `cratonvm-gc` only as a dev-dependency, so a table
//! the emitter reads at compile time must sit below both crates. `cratonvm-gc`
//! owns the POLICY (`gen_heap::publish_jit_g1_young` /
//! `clear_jit_g1_young_owned_by`, the claim and poison protocol, and the
//! numbers it publishes); this module owns only the words and the reader.
//!
//! The region geometry the sequence needs -- the stride of G1's region array,
//! the offset of the type byte in a region, the largest young type byte -- is
//! a property of `cratonvm-gc`'s `G1Region` layout. It is carried IN the
//! table (words 5-7, published with the rest) rather than as constants here,
//! so no constant in this crate can drift from the struct it describes; the
//! emitter bakes what the table says at compile time and validates it.
//!
//! # Layout (`words`)
//!
//! * `[0]` the address of G1's region array (address-stable for the
//!   collector's life);
//! * `[1]` the owning collector's arena base;
//! * `[2]` the OWNER claim: `0` free, the owner's arena base, or
//!   [`JIT_G1_YOUNG_CONTENDED`] (sticky) once two G1 collectors coexisted;
//! * `[3]` the region shift -- the LIVENESS word: `0` means "do not filter";
//!   the emitted sequence compares it at run time against the shift it was
//!   compiled with;
//! * `[4]` the region count (the index bound);
//! * `[5]` the region array stride in bytes, `[6]` the type byte's offset in a
//!   region, `[7]` the largest young type byte (Eden 0, Survivor 1).
//!
//! The emitted sequence reads only `[0]`, `[3]` and `[4]` at run time, at
//! byte offsets 0, 24 and 32.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Word indices of [`JitG1YoungTable::words`].
pub const W_REGIONS: usize = 0;
/// See [`W_REGIONS`].
pub const W_BASE: usize = 1;
/// See [`W_REGIONS`].
pub const W_CLAIM: usize = 2;
/// See [`W_REGIONS`].
pub const W_SHIFT: usize = 3;
/// See [`W_REGIONS`].
pub const W_COUNT: usize = 4;
/// See [`W_REGIONS`].
pub const W_STRIDE: usize = 5;
/// See [`W_REGIONS`].
pub const W_TYPE_OFFSET: usize = 6;
/// See [`W_REGIONS`].
pub const W_YOUNG_MAX: usize = 7;

/// [`JIT_G1_YOUNG`] `[2]` once two G1 collectors have claimed the table in one
/// process: the inline young filter stays off for the rest of the process.
pub const JIT_G1_YOUNG_CONTENDED: usize = usize::MAX;

/// See the module doc.
#[repr(C)]
pub struct JitG1YoungTable {
    pub words: [AtomicUsize; 8],
}

/// The process's young-holder table (see the module doc).
pub static JIT_G1_YOUNG: JitG1YoungTable = JitG1YoungTable {
    words: [const { AtomicUsize::new(0) }; 8],
};

/// Address of [`JIT_G1_YOUNG`], baked into the emitted sequence.
pub fn jit_g1_young_addr() -> usize {
    &JIT_G1_YOUNG as *const _ as usize
}

/// The region shift [`JIT_G1_YOUNG`] currently publishes (`0` = not
/// published or poisoned).
pub fn jit_g1_young_shift() -> u32 {
    // Cast: a shift below 64 by construction; a wider value is refused by
    // `jit_g1_young_geometry`.
    JIT_G1_YOUNG.words[W_SHIFT].load(Ordering::Acquire) as u32
}

/// What the emitter bakes into one young-holder filter sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitG1YoungGeometry {
    /// [`jit_g1_young_addr`].
    pub table: usize,
    /// The published region shift (re-checked at run time).
    pub shift: u8,
    /// The region array stride.
    pub stride: usize,
    /// The type byte's offset in a region.
    pub type_offset: usize,
    /// The largest young type byte.
    pub young_max: u8,
}

/// The geometry to compile a young-holder filter against, or `None` when the
/// table is unpublished, poisoned, or names numbers the encoder cannot use (a
/// zero or over-wide shift, a zero stride, a type offset outside the stride,
/// a young bound past a byte). A table torn by a concurrent teardown can only
/// fail these checks or the run-time shift compare, never pass them wrongly:
/// the stride, offset and bound are build constants of `cratonvm-gc`.
pub fn jit_g1_young_geometry() -> Option<JitG1YoungGeometry> {
    let words = &JIT_G1_YOUNG.words;
    let shift = words[W_SHIFT].load(Ordering::Acquire);
    if shift == 0 || shift >= 64 {
        return None;
    }
    let stride = words[W_STRIDE].load(Ordering::Acquire);
    let type_offset = words[W_TYPE_OFFSET].load(Ordering::Acquire);
    let young_max = words[W_YOUNG_MAX].load(Ordering::Acquire);
    if stride == 0 || type_offset >= stride || young_max > usize::from(u8::MAX) {
        return None;
    }
    Some(JitG1YoungGeometry {
        table: jit_g1_young_addr(),
        // Cast: `shift < 64` and `young_max <= 255`, checked above.
        shift: shift as u8,
        stride,
        type_offset,
        young_max: young_max as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The only test in this crate that touches [`JIT_G1_YOUNG`]: the reader
    /// answers only a live, plausible table, and the run-time words sit at the
    /// byte offsets the emitted sequence hard-codes (0, 24, 32).
    #[test]
    fn the_geometry_reader_refuses_unpublished_poisoned_and_implausible_tables() {
        let w = &JIT_G1_YOUNG.words;
        assert_eq!(std::mem::size_of::<JitG1YoungTable>(), 8 * 8);
        let base = jit_g1_young_addr();
        assert_eq!(&w[W_REGIONS] as *const AtomicUsize as usize - base, 0);
        assert_eq!(&w[W_SHIFT] as *const AtomicUsize as usize - base, 24);
        assert_eq!(&w[W_COUNT] as *const AtomicUsize as usize - base, 32);
        assert_eq!(jit_g1_young_geometry(), None, "unpublished");
        w[W_STRIDE].store(96, Ordering::SeqCst);
        w[W_TYPE_OFFSET].store(40, Ordering::SeqCst);
        w[W_YOUNG_MAX].store(1, Ordering::SeqCst);
        w[W_SHIFT].store(20, Ordering::SeqCst);
        assert_eq!(
            jit_g1_young_geometry(),
            Some(JitG1YoungGeometry {
                table: base,
                shift: 20,
                stride: 96,
                type_offset: 40,
                young_max: 1,
            })
        );
        assert_eq!(jit_g1_young_shift(), 20);
        w[W_TYPE_OFFSET].store(96, Ordering::SeqCst);
        assert_eq!(jit_g1_young_geometry(), None, "type offset outside the stride");
        w[W_TYPE_OFFSET].store(40, Ordering::SeqCst);
        w[W_STRIDE].store(0, Ordering::SeqCst);
        assert_eq!(jit_g1_young_geometry(), None, "zero stride");
        w[W_STRIDE].store(96, Ordering::SeqCst);
        w[W_SHIFT].store(64, Ordering::SeqCst);
        assert_eq!(jit_g1_young_geometry(), None, "over-wide shift");
        w[W_SHIFT].store(0, Ordering::SeqCst);
        assert_eq!(jit_g1_young_geometry(), None, "poisoned");
        for word in w.iter() {
            word.store(0, Ordering::SeqCst);
        }
    }
}
