// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 11 (lane osrfollow): the optimizing OSR door under the
//! narrowed opaque policy (`CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR`, OSRPOLY18-2),
//! with and without the round-13 handler-header arms, and the fused class-test
//! branch with the IR polymorphic splice (`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`,
//! `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`, OSRPOLY18-1). Every arm, in both
//! modes, must print HotSpot's output: loops with exact guards, a pruned arm
//! that fires after the OSR entry, catches that continue the loop (one
//! rewriting a local the loop reads), and polymorphic interface sites.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;




