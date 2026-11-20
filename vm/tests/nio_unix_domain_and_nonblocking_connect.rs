// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The real JDK's NIO channels under `--real-jdk`: a non-blocking connect is
//! in progress and the selector reports `OP_CONNECT` for it, Unix-domain
//! channels (`sun.nio.ch.UnixDomainSockets`) open, bind, connect and echo,
//! and the extended socket options answer on a channel with no connection yet.
//! The expected output is HotSpot 25's, line for line (Linux only).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const EXPECTED: &str = "nb connect immediate=false
pending=true connected=false
select=1 ready=8
finishConnect=true connected=true
refused connect immediate=false
refused: java.net.ConnectException: Connection refused (connect failed)
uds bound=true name=s.sock
uds client echo=hello-uds n=9
uds server read=9 peerNamed=false
missing path: java.net.SocketException: No such file or directory
sc supports TCP_NODELAY=true
sc TCP_QUICKACK default=true
sc SO_INCOMING_NAPI_ID default=0
ssc TCP_QUICKACK default=true
ssc SO_INCOMING_NAPI_ID default=0
NIO-CONNECT-PROBE-DONE
";

