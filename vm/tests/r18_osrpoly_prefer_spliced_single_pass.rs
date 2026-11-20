// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 9 (lane osrpoly;
//! `docs/feature-designs/deopt-osr.md`):
//! the optimizing OSR door declines its IR attempt where the single-pass OSR
//! body splices a polymorphic in-loop site (`CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS`,
//! default ON). Both arms must print HotSpot's output; the default arm names
//! the decline, the `=0` arm never does.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

// The decline line. Since JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-4 /
// OSRFOLLOW18-4) the door asks it before any compile ("not attempted -- ...",
// `osr_optimizing_build_declined`); the in-compile "IR attempt declined -- ..."
// line it replaced is gone. Matched on the part both spellings shared.
const DECLINE: &str = "the single-pass OSR body splices the polymorphic site";

