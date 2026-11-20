// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A callee that calls `AtomicInteger.getAndIncrement()` keeps its own compile
//! instead of being spliced into its caller.
//!
//! The atomic ladders turn the call into one `LOCK XADD` at the compiled
//! method's own sites; a spliced body's calls are ordinary calls, so splicing
//! the wrapper copied `Unsafe.getAndAddInt`'s CAS retry loop into the loop. Once
//! `CRATONVM_JIT_IR_INLINE_COLD_ATHROW` let a wrapper with a cold `athrow` arm be
//! spliced, Netty's `new FastThreadLocal<Boolean>()` in a loop went from 8-9 ns to
//! 135-142 ns per iteration and `FastThreadLocalTest.testConstructionWithIndex`
//! (two billion of them, 120 s deadline) timed out. See
//! `docs/feature-designs/real-jdk-mode.md`.
//!
//! The probe prints HotSpot's output in both modes; under `CRATONVM_DBG=jitc` the
//! optimizing tier must also say it refused the wrapper.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str = "index=5 next=6\nsum=6000000\ncaught too many indexed variables next=2147483639\nATOMIC-WRAPPER-OK\n";

