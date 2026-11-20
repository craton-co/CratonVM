// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation-driven G1 young pauses clear a `WeakReference` whose referent is
//! otherwise unreachable, with the JIT on and a compiled frame holding the
//! references, in both modes, as HotSpot G1 does (`PROBE-OK`, ~177k
//! allocations at `-Xmx256m`).
//!
//! A blocked peer's stale native-stack word pins the OBJECT it names (g1pin),
//! and the pause roots that object by assumption. The reference round then
//! read the pause's identity map entry as "survived" and never offered the
//! referent to the pinned-referent verdict, so both probes printed
//! `inline=LIVE viaMethod=LIVE allocations=2500000 PROBE-FAIL`. `-Xmx256m` is
//! required: on a big heap neither CratonVM nor HotSpot ZGC collects at all in
//! 2.5 M allocations. See `docs/feature-designs/generative-gc-architecture.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

