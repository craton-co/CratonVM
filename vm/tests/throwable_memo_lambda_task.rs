// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A throw loop under a lambda body called through an interface reuses the
//! thread's capture memo.
//!
//! The memo was refused for every such capture: the `Thread.run` middle-frame
//! screen lets any lambda-body callee through, and a screen that engaged kept
//! the memo from being made even though the caller (`task.run()`, an
//! `invokeinterface`) is not the `run()V` call the rule needs. Every JUnit or
//! `ForkJoinPool` task frame is such a pair, so each `new Throwable()` of
//! Netty's paranoid leak tracking walked and laid out the whole stack, and
//! `AdaptiveByteBufAllocatorTest.purgeScanShouldEvictIdleChunks` took 51 s
//! against HotSpot's 19 s. See
//! `docs/feature-designs/jit-ir-optimizations.md`.
//!
//! The probe prints HotSpot's output in both modes, and the capture-memo census
//! (`CRATONVM_DBG_CAPTURE_MEMO=1`) must report the captures as hits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const FRAMES: &str = "ThrowableMemoLambdaTaskProbe.lambda$main$0:31\n\
ThrowableMemoLambdaTaskProbe.work:11\n\
ThrowableMemoLambdaTaskProbe.descend:16\n";

/// `hits=<n>` of the last capture-memo census line.
fn memo_hits(stderr: &str) -> Option<u64> {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("[capture-memo] hits="))?;
    line["[capture-memo] hits=".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

