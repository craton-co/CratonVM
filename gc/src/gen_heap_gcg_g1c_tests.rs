// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcg g1/c (2026-10-01) — unit tests of lane c's wave-1 readings of the
//! young trigger (`gcf-f5c-young-trigger-is-half-of-hotspot-serial-eden-so-medium-lived-objects-tenure-20261001`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcg_g1c_`.

use super::*;

/// On a JIT-warm run every young cycle is the in-place sweep, which fires at
/// `young_gc_trigger_bytes(.., non_moving_young = true, ..)`: the smaller of
/// the moving threshold and the 90 % non-moving trigger while a pause goal is
/// armed, the 90 % trigger without one. Under
/// `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` the moving threshold is 95 % of
/// the semi-space, so the SWEEP fires at 90 % -- the same level as the default
/// arm with `CRATONVM_GC_YOUNG_PAUSE_MS=0`, or the start of
/// `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`'s loop. That is what the switch
/// changes on the f7 perf rows (hashmap-pair), and why the decomposition rows
/// carry those two arms.
#[test]
fn gcg_g1c_serial_eden_moves_the_sweep_trigger_to_ninety_percent() {
    const M: usize = 1 << 20;
    // `-Xmx512m`: semi 128 MiB, HotSpot's eden 136.5 MiB (capped at the semi).
    let semi = 512 * M / 4;
    let serial = serial_eden_young_trigger(semi, serial_eden_bytes((512 * M) / 3));
    assert_eq!(serial, semi * 95 / 100, "the Serial-eden moving trigger");
    let ninety = semi * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
    // Switch on, goal armed: the sweep fires at 90 %.
    assert_eq!(young_gc_trigger_bytes(semi, serial, true, true), ninety);
    // Switch off, goal armed: the sweep fires at the moving 50 % (or lower,
    // after the loop halves it).
    let default_moving = semi * 50 / 100;
    assert_eq!(
        young_gc_trigger_bytes(semi, default_moving, true, true),
        default_moving
    );
    assert_eq!(
        young_gc_trigger_bytes(semi, default_moving / 4, true, true),
        default_moving / 4
    );
    // No goal (`CRATONVM_GC_YOUNG_PAUSE_MS=0`): 90 % on both arms.
    assert_eq!(
        young_gc_trigger_bytes(semi, default_moving, true, false),
        ninety
    );
    assert_eq!(young_gc_trigger_bytes(semi, serial, true, false), ninety);
    // The moving cycle (interpreted / no compiled frame) is the threshold itself.
    assert_eq!(young_gc_trigger_bytes(semi, serial, false, true), serial);
    // (gcg g6/yg: the own-goal sweep loop that also started at 90 % is deleted.)
}
