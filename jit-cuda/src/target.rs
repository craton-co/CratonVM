// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! PTX ISA version / `.target` compatibility.
//!
//! # Why this module exists
//!
//! A PTX module's first two directives are not independent:
//!
//! ```text
//! .version <PTX ISA version>
//! .target  sm_<compute capability>
//! ```
//!
//! Each ISA version admits a fixed, closed set of targets — a target
//! introduced after an ISA version was published cannot appear in a
//! module declaring that version, and `ptxas` (or the driver's JIT)
//! rejects the module outright rather than degrading.
//!
//! Until 2026-09-02 [`crate::emitter::PtxModule::render`] wrote a
//! literal `.version 7.5` and filled `.target` from the compute
//! capability the VM probed at run time (`vm/src/runtime/offload.rs`,
//! which clamps the probe *upward* only). PTX ISA 7.5 tops out at
//! `sm_87`, so on every device newer than Ampere — Ada (`sm_89`),
//! Hopper (`sm_90`), Blackwell (`sm_100`/`sm_120`) — the emitted
//! header named a target its own ISA version does not know, the module
//! failed to load, `OffloadCache::lookup_or_compile` blacklisted the
//! method, and the VM silently ran every kernel on the CPU. Correct
//! answers, an `info`-level log line, and a `--gpu` flag that bought a
//! CUDA context and nothing else.
//!
//! Nothing caught it because the only real-hardware gate
//! (`.github/workflows/gpu-selfhosted.yml`) runs on an RTX 2060 —
//! `sm_75`, the one architecture where the hardcoded literal happens
//! to be correct. Every measurement in `docs/gpu/` and
//! `bench-gpu/results/` was taken on that same card, and the
//! `ptxas_round_trip_*` tests, which could have caught it without any
//! GPU at all, assembled every kernel for `sm_75` too.
//!
//! # Measured, not reasoned
//!
//! The break was reproduced with NVIDIA's own assembler (`ptxas` from
//! CUDA 13.3) on a representative lowered kernel, one architecture per
//! row, holding `.version 7.5` fixed:
//!
//! ```text
//!   sm_75  ACCEPTED      sm_89  ptxas: PTX .version 7.5 does not support .target sm_89
//!   sm_80  ACCEPTED      sm_90  ptxas: PTX .version 7.5 does not support .target sm_90
//!   sm_86  ACCEPTED      sm_100 ptxas: PTX .version 7.5 does not support .target sm_100
//!                        sm_120 ptxas: PTX .version 7.5 does not support .target sm_120
//! ```
//!
//! One caveat worth knowing before reproducing it: an EMPTY module
//! (`.version` / `.target` / `.address_size` and no `.entry`) is
//! accepted at every combination above, because `ptxas` never reaches
//! the version check when there is no code to compile. A probe has to
//! carry a real kernel body or it reports a clean bill of health for a
//! header that cannot load. `ptxas_knows_arch` in `lowering.rs`'s tests
//! relies on exactly that leniency, deliberately, to ask a different
//! question — whether the toolkit knows an architecture at all.
//!
//! # The two directions
//!
//! Both have to be handled, and they are different questions:
//!
//! 1. **The floor.** [`min_isa_for_target`] answers "what is the
//!    lowest `.version` that admits this `.target`". This is what
//!    `render` needs, and it makes the header self-consistent by
//!    construction.
//! 2. **The ceiling.** An ISA version newer than the installed driver
//!    understands is rejected just as hard as a target newer than the
//!    ISA. [`max_isa_for_cuda_version`] maps `cuDriverGetVersion`'s
//!    answer to the newest ISA that driver can parse, and
//!    [`clamp_target_to_isa`] lowers the *target* until its floor fits
//!    under that ceiling. Lowering the target is always safe: an
//!    `sm_80` module runs on an `sm_90` device through the driver JIT,
//!    it simply cannot use instructions introduced after Ampere — and
//!    this crate emits none.
//!
//! # Conservative on the unknown
//!
//! A CUDA version newer than the last row of [`max_isa_for_cuda_version`]
//! resolves to that last row rather than to something extrapolated. The
//! cost of guessing low is a target one generation behind the device;
//! the cost of guessing high is the module-load failure this module
//! exists to prevent. Every table below therefore saturates downward.

/// A PTX ISA version, as it appears in the `.version` directive.
pub type IsaVersion = (u32, u32);

/// A compute capability, as it appears in the `.target sm_XY` directive.
pub type SmTarget = (u32, u32);

/// The `.version` this crate emitted unconditionally before 2026-09-02,
/// kept as a floor rather than deleted.
///
/// Nothing in the emitter's instruction repertoire needs 7.5 — every
/// mnemonic it produces (`mad.lo`, `mad.wide`, `setp`/`selp`, `ld`/`st.global`,
/// `cvt`, `div`/`rem`, `sqrt.rn`, `fma.rn`, `abs`, `atom.global.add`,
/// `ex2.approx.f32`, `cvt.f32.f16`) predates it — so [`min_isa_for_target`]
/// alone would be correct and would emit `.version 6.3` for the Turing
/// card the whole subsystem was measured on.
///
/// It is a floor anyway, for one reason: 7.5 is the only version this
/// crate has ever shipped, every real-hardware result in `docs/gpu/` and
/// `bench-gpu/results/` was taken under it, and every PTX-text assertion
/// in the test suite was written against it. Raising the header where a
/// modern target *requires* it is the fix; lowering it where nothing
/// requires that is an unmeasured change to the driver JIT's input on
/// the one configuration that is known to work. Fix the break, hold
/// everything else still.
pub const EMITTER_BASELINE_ISA: IsaVersion = (7, 5);

/// The lowest PTX ISA version whose `.target` set contains
/// `sm_<major><minor>`.
///
/// Resolved against [`TARGET_TABLE`] by rounding **up**: the first row
/// at or above the requested capability. See that constant for why the
/// direction matters — rounding down produces a `.version` that cannot
/// name the target it is written beside, which is the exact bug this
/// module exists to prevent, one generation removed.
///
/// A capability *below* every row (`sm_6x` and older) resolves to the
/// first row's `6.0`, and one *above* every row saturates to the newest
/// ISA the table knows. Neither case arises in production —
/// `OffloadCache::new` floors the probe at `sm_70`, and a capability
/// past [`NEWEST_KNOWN_TARGET`] is an architecture that did not exist
/// when this was written — but both keep the function total.
pub fn min_isa_for_target(sm_major: u32, sm_minor: u32) -> IsaVersion {
    let want = (sm_major, sm_minor);
    for &(target, isa) in TARGET_TABLE {
        if target >= want {
            return isa;
        }
    }
    // Past every row we know. Saturate to the newest ISA in the table
    // rather than extrapolating a version number that may not exist —
    // see the module doc. A target this new will be refused by the
    // driver for being an unknown architecture, which is a clear error,
    // rather than accepted against a version we invented.
    TARGET_TABLE.last().map(|&(_, isa)| isa).unwrap_or((6, 0))
}

/// `(target, first PTX ISA version that admits it)`, **ascending**.
///
/// Ascending, and looked up with "the first row at or above the
/// request", because the interesting failure is an architecture we have
/// no row for. Resolving such a capability DOWNWARD — to the newest row
/// below it — produces a `.version` that provably cannot name it: the
/// exhaustive `every_rendered_header_is_self_consistent` test found
/// exactly that for a hypothetical `sm_88`, which fell to the `sm_87`
/// row and rendered `.version 7.5`, a version whose target set stops at
/// `sm_87`. Rounding UP is the safe direction: it can only ever declare
/// a newer ISA than strictly needed, and a newer ISA admits every older
/// target.
const TARGET_TABLE: &[(SmTarget, IsaVersion)] = &[
    ((7, 0), (6, 0)),  // Volta, CUDA 9.0
    ((7, 2), (6, 1)),  // Xavier, CUDA 9.1
    ((7, 5), (6, 3)),  // Turing, CUDA 10.0
    ((8, 0), (7, 0)),  // Ampere GA100, CUDA 11.0
    ((8, 6), (7, 1)),  // Ampere GA10x, CUDA 11.1
    ((8, 7), (7, 4)),  // Orin, CUDA 11.4
    ((8, 9), (7, 8)),  // Ada, CUDA 11.8
    ((9, 0), (7, 8)),  // Hopper, CUDA 11.8
    ((10, 0), (8, 7)), // Blackwell datacenter (sm_100), CUDA 12.8
    ((10, 1), (8, 7)), // Blackwell datacenter (sm_101), CUDA 12.8
    ((10, 3), (8, 8)), // Blackwell datacenter (sm_103), CUDA 12.9
    ((12, 0), (8, 7)), // Blackwell RTX (sm_120), CUDA 12.8
    ((12, 1), (8, 8)), // Blackwell RTX (sm_121), CUDA 12.9
];

/// The newest `.target` any row of [`TARGET_TABLE`] names. Past this,
/// [`min_isa_for_target`] saturates and no self-consistency claim is
/// made — there is no ISA we know that admits an architecture we do not.
pub const NEWEST_KNOWN_TARGET: SmTarget = (12, 1);

/// The newest PTX ISA version a driver reporting `cuda_version` can
/// parse.
///
/// `cuda_version` is `cuDriverGetVersion`'s raw answer: `1000 * major +
/// 10 * minor`, e.g. `12080` for CUDA 12.8. Rows are the PTX ISA
/// version each toolkit release published, newest first; anything newer
/// than the last row we know saturates to it (see the module doc's
/// "Conservative on the unknown").
///
/// A driver older than every row resolves to `(6, 0)` — the floor
/// [`min_isa_for_target`] also uses, so the pair always composes.
pub fn max_isa_for_cuda_version(cuda_version: u32) -> IsaVersion {
    // (cuDriverGetVersion value, PTX ISA that toolkit published), newest first.
    const TABLE: &[(u32, IsaVersion)] = &[
        (13030, (9, 3)), // CUDA 13.3
        (13020, (9, 2)),
        (13010, (9, 1)),
        (13000, (9, 0)), // CUDA 13.0
        (12090, (8, 8)), // CUDA 12.9
        (12080, (8, 7)), // CUDA 12.8
        (12060, (8, 5)), // CUDA 12.6 republished 8.5; there is no 8.6 toolkit
        (12050, (8, 5)),
        (12040, (8, 4)),
        (12030, (8, 3)),
        (12020, (8, 2)),
        (12010, (8, 1)),
        (12000, (8, 0)), // CUDA 12.0
        (11080, (7, 8)), // CUDA 11.8
        (11070, (7, 7)),
        (11060, (7, 6)),
        (11050, (7, 5)),
        (11040, (7, 4)),
        (11030, (7, 3)),
        (11020, (7, 2)),
        (11010, (7, 1)),
        (11000, (7, 0)), // CUDA 11.0
        (10020, (6, 5)),
        (10010, (6, 4)),
        (10000, (6, 3)),
        (9010, (6, 1)),
        (9000, (6, 0)),
    ];
    for &(v, isa) in TABLE {
        if cuda_version >= v {
            return isa;
        }
    }
    (6, 0)
}

/// The `.version` a module targeting `sm_<major><minor>` must declare:
/// [`min_isa_for_target`] raised to [`EMITTER_BASELINE_ISA`].
///
/// This is what [`crate::emitter::PtxModule::render`] writes. For every
/// target up to `sm_87` the baseline dominates and the header is
/// byte-identical to what this crate has always emitted; from `sm_89`
/// up the target's own floor takes over and the module becomes loadable
/// for the first time.
pub fn isa_for_target(sm_major: u32, sm_minor: u32) -> IsaVersion {
    let required = min_isa_for_target(sm_major, sm_minor);
    if required > EMITTER_BASELINE_ISA {
        required
    } else {
        EMITTER_BASELINE_ISA
    }
}

/// The newest `.target` an ISA version admits — the inverse of
/// [`min_isa_for_target`], and the value [`clamp_target_to_isa`] falls
/// back to.
fn max_target_for_isa(isa: IsaVersion) -> SmTarget {
    const TABLE: &[(IsaVersion, SmTarget)] = &[
        ((8, 8), (12, 1)),
        ((8, 7), (12, 0)),
        ((7, 8), (9, 0)),
        ((7, 4), (8, 7)),
        ((7, 1), (8, 6)),
        ((7, 0), (8, 0)),
        ((6, 3), (7, 5)),
        ((6, 1), (7, 2)),
        ((6, 0), (7, 0)),
    ];
    for &(want, target) in TABLE {
        if isa >= want {
            return target;
        }
    }
    (7, 0)
}

/// Lower `target` until the ISA version it requires fits under
/// `driver_isa`, so the rendered header is one the installed driver can
/// actually parse.
///
/// Returns `target` unchanged in the ordinary case — a driver at least
/// as new as the card it is driving, which is every correctly-installed
/// system. It only bites on a machine whose driver predates its GPU (a
/// container pinned to an old CUDA base image, a datacenter node whose
/// driver upgrade lags a card swap), where the honest answer is "run
/// this device as the newest architecture your driver knows", not "emit
/// a module nothing can load".
///
/// The result is never raised above `target`: a module must not name a
/// target the device cannot run.
pub fn clamp_target_to_isa(target: SmTarget, driver_isa: IsaVersion) -> SmTarget {
    if min_isa_for_target(target.0, target.1) <= driver_isa {
        return target;
    }
    let capped = max_target_for_isa(driver_isa);
    if capped < target {
        capped
    } else {
        target
    }
}

#[cfg(test)]
mod tests;
