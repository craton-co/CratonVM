// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 9 (lane osrframe;
//! `docs/feature-designs/deopt-osr.md`):
//! the single-pass sibling tail call no longer JMPs into a helper that takes
//! the VM context (`CRATONVM_JIT_SP_TAILCALL_KEEPS_VM_HELPER_FRAME`, default
//! ON). Under `--compatible`, a compiled `Objects.checkIndex` tail-called the
//! thin `Preconditions.checkIndex` helper, whose throwing arm captured the
//! trace while the frame-chain mirror still named the torn-down frame: its
//! line came out `-1` (HotSpot `Objects.java:365`), and where the dead frame's
//! saved RBP was overwritten, every frame below it -- an OSR body's inlined
//! callee and its bci -- was lost too. Every arm of both modes must print
//! HotSpot's output; the `=0` arm is run as the control and reported, not
//! asserted (whether the dead frame still reads right is up to the helper's
//! own code generation).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;


