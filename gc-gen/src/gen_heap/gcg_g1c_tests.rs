// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gcg g1/c (2026-10-01) — unit tests of lane c's wave-1 readings of the
//! young trigger (`gcf-f5c-young-trigger-is-half-of-hotspot-serial-eden-so-medium-lived-objects-tenure-20261001`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcg_g1c_`.

use super::*;

// On a JIT-warm run every young cycle is the in-place sweep, which fires at
// `young_gc_trigger_bytes(.., non_moving_young = true, ..)`: the smaller of
// the moving threshold and the 90 % non-moving trigger while a pause goal is
// armed, the 90 % trigger without one. Under
// `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` the moving threshold is 95 % of
// the semi-space, so the SWEEP fires at 90 % -- the same level as the default
// arm with `CRATONVM_GC_YOUNG_PAUSE_MS=0`, or the start of
// `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL`'s loop. That is what the switch
// changes on the f7 perf rows (hashmap-pair), and why the decomposition rows
// carry those two arms.
