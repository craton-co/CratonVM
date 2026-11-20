// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 10 (lane throws;
//! `docs/feature-designs/real-jdk-mode.md`):
//! a `fillInStackTrace()` refresh of a throwable built with suppression
//! disabled (`Throwable(String, Throwable, false, true)`) left
//! `suppressedExceptions` holding `SUPPRESSED_SENTINEL`, so every later
//! `addSuppressed` was recorded where HotSpot drops it
//! (`CRATONVM_THROWABLE_REFILL_KEEPS_SUPPRESSION`, default ON). Both modes must
//! print HotSpot's output; the `=0` arm is the control, reported, not
//! asserted (expected to differ on the `quiet-after-refill` lines).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;


