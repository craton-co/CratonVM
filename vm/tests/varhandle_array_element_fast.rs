// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Every access mode of a real `int[]` / `long[]` element `VarHandle`, hot
//! enough to be compiled, prints HotSpot's output in both modes -- with the
//! in-funnel fast path on and with it switched off -- and the fast path really
//! serves the accesses.
//!
//! `AtomicIntegerArray.get` cost 1.2-1.6 us through the generic VarHandle
//! native (0.7 ns on HotSpot), and netty's `MpscIntQueue` -- the free list of
//! every `AdaptivePoolingAllocator` size class -- is made of them, so an
//! alloc / grow / release cycle cost 17 us against 0.1 us and
//! `AdaptiveByteBufAllocatorGrowthTest` ran 170-300 s against a 180 s cap. See
//! `docs/feature-designs/real-jdk-mode.md`.
//!
//! The probe also drives the shapes the fast path must decline (a widening
//! site, an `Object` site, a null array, an index out of range) and two
//! threads racing on the same elements, so a served access that lost an update
//! or swallowed an exception would show in the output.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str =
    include_str!("varhandle_array_element_fast_fixtures/VarHandleArrayElementProbe.expected");

fn served(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|l| {
        l.split("VarHandle int[]/long[] element accesses served directly: ")
            .nth(1)
            .and_then(|n| n.trim().parse().ok())
    })
}


