// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! An exception thrown out of callees the optimizing tier spliced into an
//! OSR-compiled loop must reach the `catch` that covers the call. The OSR
//! exception route judged the stashed inlined chain by its innermost scope (the
//! spliced callee), took it for a foreign frame and let the exception escape the
//! method: Netty's `NioEventLoopTest.testTaskRemovalOnShutdown...` saw `null`
//! where it expected a `RejectedExecutionException`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

