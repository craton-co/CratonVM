// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 wave 1 (lane osrpoly19; proposals OSRPOLY18-1 / OSRFOLLOW18-3
//! of `docs/feature-designs/deopt-osr.md` and
//! `docs/feature-designs/deopt-osr.md`): the optimizing tier's polymorphic
//! guarded splice with its class tests sunk to their branches
//! (`CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH`), fused
//! (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`), and scoped to OSR-door compiles
//! (`CRATONVM_JIT_OSR_DOOR_IR_POLYMORPHIC_SPLICE`) or process-wide
//! (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`). Every arm, in both modes,
//! must print HotSpot's output, the null receiver's NPE included.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;



