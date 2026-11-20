// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A masked exception-construction arm is a compile-time view, not the
//! interpreter's continuation. Its omitted local reads must not make the
//! inlined deoptimization frame discard the receiver or exception arguments.
//! The unfixed release binary throws an NPE in this probe's checkReadableBytes0.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;


