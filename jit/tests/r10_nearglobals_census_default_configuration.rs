// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `code_near_globals_stats()` reads `(0, 0)` in the DEFAULT configuration, and
//! this is the caller the re-export was created for.
//!
//! # Why this file exists
//!
//! `cratonvm_jit::platform::code_near_globals_stats` is a `pub use` of a private
//! module's `stats` accessor. An accessor re-exported out of a private module
//! exists for exactly one reason — so that something outside the module can read
//! the census — and nothing did. Filed as item 1 of
//! `docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`,
//! which names writing this test as "the only item here with a clear right
//! answer", and the `stats` doc records that a previous version of its own
//! sentence claimed such a test already existed when it did not. A reader who
//! believes the test exists does not go looking for it, which is how the claim
//! survived.
//!
//! # What it actually asserts, and what it deliberately does not
//!
//! The census is `(placed in reach, fell back, retired)`. `near_globals::place`
//! documents, in a comment whose whole purpose is this property, that the
//! `!enabled()` early return happens **before any counter**:
//!
//! > The flag being off is NOT a fallback and must not be counted [...] counting
//! > here would also put one relaxed atomic on every JIT buffer allocation in
//! > the default configuration.
//!
//! So with `CRATONVM_JIT_CODE_NEAR_GLOBALS` unset, allocating executable buffers
//! through the crate's single door to the OS mapping primitive must leave BOTH
//! counters at zero. That is a real invariant with a real failure mode in both
//! directions: a counter that moved here would mean the default configuration
//! had started paying for an instrument nothing asked for, and a `fell_back`
//! that moved would make the census's documented reading ("`in_reach=0` alone
//! cannot separate 'the flag is off' from 'the flag is on and this host has no
//! room'") false, because the flag being off would then look like the flag being
//! on and failing.
//!
//! It does **not** assert "the flag works". That needs a run with the flag set
//! on a host with room near the anchor, and on a host without room the honest
//! census reads `in_reach=0 fell_back=N` — indistinguishable from a bug by
//! design, because `mmap`'s first argument is a hint the kernel may ignore. The
//! retirement DECISION, which is the part that was defective (see
//! `docs/internal/fixed-bugs/r10-readers-near-globals-place-retires-on-a-stale-cursor-FIXED-20260922.md`),
//! is pinned by the `walk` tests in `platform.rs`'s `near_globals_tests`, which
//! need neither the flag nor this census.
//!
//! # Why an integration binary and not a unit test
//!
//! `IN_REACH`, `FELL_BACK` and `RETIRED` are process-global and monotone. A unit
//! test in `jit/src/platform.rs` shares its process with every other test in the
//! crate, any of which may allocate executable memory, so it could only ever
//! observe the counters at or above whatever the rest of the binary did. An
//! integration test is its own process, which is the only place a must-be-zero
//! statement can be written — the same argument `jit/tests/code_free_audit.rs`
//! makes for itself in its own header.

use cratonvm_jit::ExecutableBuffer;

/// The flag `near_globals::enabled()` latches, read through the same accessor it
/// uses so that a value set through the flags-override mechanism rather than the
/// environment is seen here too.
fn near_globals_flag_is_on() -> bool {
    matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_CODE_NEAR_GLOBALS").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

#[test]
fn the_near_globals_census_is_all_zero_when_the_flag_is_unset() {
    if near_globals_flag_is_on() {
        // Honest skip rather than a false pass. With the flag set, non-zero
        // counters are the CORRECT outcome and asserting zero would be
        // asserting that the feature does not work.
        eprintln!(
            "skipped: CRATONVM_JIT_CODE_NEAR_GLOBALS is set, so a non-zero \
             census is the expected reading"
        );
        return;
    }

    // Before anything is allocated. On Unix this is the untouched statics; on
    // Windows and macOS/ARM64 `code_near_globals_stats` is a `cfg` stub that
    // returns the constant `(0, 0, true)` because the strategy is not built
    // there at all.
    let (in_reach_before, fell_back_before, _) = cratonvm_jit::platform::code_near_globals_stats();
    assert_eq!(
        (in_reach_before, fell_back_before),
        (0, 0),
        "the near-globals census must be untouched before this test allocates \
         anything; a non-zero reading here means some other part of this binary \
         engaged the strategy, which with the flag unset it cannot do"
    );

    // Drive the door. `ExecutableBuffer::new` calls `platform::alloc_executable`,
    // which is the crate's single entry to the OS mapping primitive for code and
    // the only caller of `near_globals::place`. Sizes are varied because the
    // filed defect is size-dependent: `in_reach` measures `p + size`, so a
    // larger request is strictly harder to satisfy at the same address, and a
    // walk that ran at all would show up in the counters.
    let mut buffers = Vec::new();
    for size in [64usize, 4096, 64 * 1024, 1 << 20] {
        buffers.push(
            ExecutableBuffer::new(size)
                .unwrap_or_else(|| panic!("the OS refused a {size}-byte code mapping")),
        );
    }
    assert_eq!(buffers.len(), 4, "four buffers were asked for and taken");

    let (in_reach, fell_back, retired) = cratonvm_jit::platform::code_near_globals_stats();
    assert_eq!(
        (in_reach, fell_back),
        (0, 0),
        "with CRATONVM_JIT_CODE_NEAR_GLOBALS unset, `place` returns before \
         either counter, so four hinted-allocation opportunities must have \
         moved neither. `in_reach` moving would mean the flag engaged without \
         being set; `fell_back` moving would mean the default configuration \
         pays a relaxed atomic per JIT buffer AND that `in_reach=0 fell_back=0` \
         no longer distinguishes 'flag off' from 'flag on, no room here'"
    );

    // `retired` is the one element whose value is platform-dependent, so it is
    // asserted per platform rather than left unchecked. Leaving it unchecked
    // would be the easy thing and would lose the only assertion that says the
    // `cfg` stub and the real accessor are both wired to this name.
    #[cfg(all(
        not(target_os = "windows"),
        not(all(target_os = "macos", target_arch = "aarch64"))
    ))]
    assert!(
        !retired,
        "on a target that HAS the strategy, an unset flag means `place` returns \
         at `!enabled()` and never reaches the walk, so nothing can have \
         retired it"
    );
    #[cfg(any(
        target_os = "windows",
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    assert!(
        retired,
        "on Windows and macOS/ARM64 `code_near_globals_stats` is the `cfg` stub \
         returning the constant (0, 0, true): there is no near-globals module to \
         retire, and `true` is how the census says placement is not this \
         target's business"
    );

    // Held to here on purpose: dropping a buffer runs `free_executable`, and the
    // assertions above are about the state after ALLOCATION. Nothing in the free
    // path touches this census, but the census is exactly the kind of thing that
    // acquires a decrement later, and a test that had already dropped its
    // buffers would not notice.
    drop(buffers);
}
