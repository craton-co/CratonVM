// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! In-memory `SSLEngine` handshakes against a custom `X509TrustManager` on both
//! sides, while peer threads allocate, print HotSpot's output on G1 in both
//! modes: no `NoSuchMethodError: java.lang.Object.check*Trusted`.
//!
//! Under `--compatible` the handshake reaches `engine_consult_trust_managers`
//! (`native-builtins/src/t27_tls.rs`), which clones the context's TrustManagers
//! out of the rooted table as raw addresses. It used to pin them only after it
//! had allocated the certificate chain, the X509 mirrors and the authType
//! string, so on G1 a young collection in that window left the pins naming
//! vacated addresses: the first `invoke_virtual` read `ClassId(0)`
//! (`java.lang.Object`) and raised `NoSuchMethodError`, a `LinkageError` the
//! engine never turns into an alert. That is the stall netty's
//! `ParameterizedSslHandlerTest` kept hitting. Before the fix every launch of
//! this probe hit it within its first 25-80 handshakes; the probe runs 300.
//! See `docs/feature-designs/real-jdk-mode.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str =
    include_str!("tls_trust_manager_pin_churn_fixtures/TlsTrustManagerChurnProbe.expected");

