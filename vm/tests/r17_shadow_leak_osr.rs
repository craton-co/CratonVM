// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! An OSR-compiled loop whose virtual call is served by an inline PIC/MIC hit
//! arm must retract that call's shadow-stack publication on every iteration.
//! Since the cold-call service started publishing a fresh safepoint of its own
//! (`1f27937f66`), the hit arm's service consumed the site's pending shadow
//! reload at compile time; the shared `.done` reload then emitted nothing and
//! every call leaked its live homes until the 2 MiB shadow stack was full
//! (`[JIT] shadow-stack overflow: N push(es) bailed on the end guard`). See
//! `docs/feature-designs/deopt-osr.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

