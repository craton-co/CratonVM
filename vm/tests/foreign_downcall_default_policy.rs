// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Netty's Java 24 cleaner can bind malloc/free after a restricted-method
//! warning. A second gate at invocation must not reject that accepted handle.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

