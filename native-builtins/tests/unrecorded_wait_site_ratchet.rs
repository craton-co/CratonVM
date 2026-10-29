// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! UNRECORDED-WAIT-SITE RATCHET -- the kind-less blocking-region doors, frozen
//! per file (interpreter round i2 wave 67, lane L1;
//! `docs/feature-designs/native-dispatch-architecture.md`,
//! item 1).
//!
//! A blocking region that says what Java code it stands for
//! (`NativeContext::begin_blocking_region_as` with `ObjectWait`, `Parked`,
//! `Sleeping`, `InNative`, ...) publishes that kind, and JVMTI / JDWP / the
//! thread dump read it. A region opened through a *kind-less door* publishes
//! nothing and reads `PARKED` to JVMTI whatever the JDK body does. The doors
//! are the ones the VM's `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` census counts:
//!
//! * `.begin_blocking_region()`
//! * `.begin_timed_blocking_region()`
//! * `begin_blocking_region_as(...)` whose argument text names
//!   `BlockKind::Waiting` or `BlockKind::TimedWaiting` (a state, not a kind).
//!
//! `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` can only default on once these sites are
//! classified or on an agreed allow-list, and the census only sees what a run
//! reaches. This gate is the static half: it counts the doors in the source of
//! every workspace member (`<crate>/src/**/*.rs`, `vendor/` skipped) and
//! compares the per-file counts with [`BASELINE`] **exactly**, no slack:
//!
//! * a new kind-less site fails: give it the `BlockKind` of the JDK code it
//!   stands for (or `begin_vm_wait_region` for a wait HotSpot performs inside
//!   the VM), or, if no kind is right yet, raise its file's row here with the
//!   reason in the commit;
//! * classifying a site fails too, until its row is lowered in the same
//!   commit -- so the number can only fall.
//!
//! ## What it counts, and what it cannot see
//!
//! Text, not calls. Each line is cut at its first `//` (comments and doc
//! comments, and -- harmlessly -- a `//` inside a string), then the file is
//! searched. So:
//!
//! * a door reached through a wrapper that takes the kind as a value
//!   (`begin_blocking_region_as(kind)`) is not counted; the dynamic census is
//!   what sees those. That includes the kill-switch fallbacks of
//!   `CRATONVM_CLASSIFIED_STAND_IN_WAITS` (wave 67), which name
//!   `BlockKind::Waiting` in a helper so the default path's site counts as
//!   classified;
//! * `native-api/src/registry.rs`' 5 are the trait's default forwarders (the
//!   defaults of `begin_timed_blocking_region`, `begin_blocking_region_as`,
//!   `begin_native_region`, `begin_vm_wait_region`), not sites; they stay in
//!   the baseline so the scan has no exceptions;
//! * `#[cfg(test)]` code is counted like the rest.
//!
//! ## How to update
//!
//! The failure prints the whole table as it is now; paste it over
//! [`BASELINE`]. Do not hand-edit counts you did not just change.
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test unrecorded_wait_site_ratchet -- --nocapture
//! ```

use std::path::{Path, PathBuf};

/// `(path from the workspace root, exact number of kind-less doors)`, sorted by
/// path; files with none are absent.
///
/// Frozen 2026-10-31 (wave 67, lane L1) at 91 doors in 31 files, after the
/// wave classified the four sites the wave-66 census measured on
/// `L1W66UnrecordedWaitKinds --compatible` (`reference.rs` `ReferenceQueue`
/// poll, the two `native-collections` CHM stripe waits, and the timed
/// `Process.waitFor` arms of `native-io/src/process.rs`, whose two left are
/// the `onExit` fallback and the exit-waiter thread).
///
/// Interpreter round i3 wave 80, lane L6 (page
/// `i59-L6-blocking-regions-not-yet-classified`): 87 -> 79 doors, three rows gone --
/// `native-io/src/pipe.rs` 4 -> 0 (the direct / gather pipe read and write
/// regions: `InNative`, as the file's other pipe regions), `native-io/src/lib.rs`
/// 2 -> 0 (the synthetic `WatchService.poll(long, TimeUnit)` / `take()`:
/// `TimedParked` / `Parked`, the JDK's `LinkedBlockingDeque` waits) and
/// `native-io/src/net_uds.rs` 2 -> 0 (`UnixDomainSockets.connect0` /
/// `accept0`, both `ACC_NATIVE`: `InNative`).
///
/// Interpreter round i3 wave 81, lane L6 (same page): 79 -> 66 doors, seven rows
/// gone, one lowered -- `vm/src/runtime/interpreter/gc_and_alloc.rs` 2 -> 0
/// (`wait_for_delivery`, a `System.gc()` caller waiting on the delivery
/// thread, and `finalizer_thread_main`'s idle: `begin_vm_wait_region`),
/// `vm/src/runtime/interpreter/gen_conc_service.rs` 2 -> 0 (teardown's wait
/// and the service's idle: `begin_vm_wait_region`),
/// `native-builtins/src/phases_late/concurrent.rs` 2 -> 0 and
/// `native-builtins/src/util_concurrent_ext.rs` 1 -> 0 (the stand-ins' own
/// delays HotSpot does not take, its caller RUNNABLE: `begin_vm_wait_region`),
/// `native-builtins/src/lib.rs` 2 -> 1 (`ForkJoinPool.awaitQuiescence`'s
/// back-off: `TimedParked`, JDK 25 `externalHelpQuiesce` parks with
/// `LockSupport.parkNanos`; the `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE=0`
/// fallback stays), `native-builtins/src/phases_early.rs` 1 -> 0 (the synthetic
/// `ServerSocket.accept`: `InNative`, `Net.accept`) and
/// `native-builtins/src/t27_tls.rs` 1 -> 0 (`gc_blocked_syscall`, a TLS
/// socket's read / write syscall: `InNative`),
/// `native-builtins/src/phases_late/ssl_security.rs` 3 -> 0 (the JSSE
/// stand-ins' TCP connect, connect-and-handshake and layered handshake: socket
/// I/O, `InNative`).
///
/// Interpreter round i3 wave 82, lane L6 (same page): 66 -> 24 doors, six rows
/// gone, two lowered -- every socket / file syscall region a HotSpot thread
/// would sit in a native for (`NioSocketImpl` / `Net.connect` / `Net.poll`,
/// `EPoll.wait` under `Selector.select`, `FileDispatcherImpl`,
/// `FileOutputStream.writeBytes`): `InNative` --
/// `native-builtins/src/http_url_connection.rs` 14 -> 0 (the HUC carriers'
/// connect, head / body writes, flush, shutdown, pooled and TLS exchanges,
/// response reads), `native-builtins/src/servlet.rs` 5 -> 0 (the selector polls,
/// `s2_blocking_with_pinned`, the acceptor), `native-builtins/src/phases_late/net_channels.rs`
/// 13 -> 1 (channel read / write / accept, the select slice, `AsynchronousFileChannel.force`,
/// datagram receive / read, `p72_impl_accept`; the `AsynchronousChannelGroup.awaitTermination`
/// slice is `TimedParked`, `ThreadPoolExecutor.awaitTermination`'s `awaitNanos`; the
/// WebSocket handshake stays: an async API, measure first),
/// `native-builtins/src/net_phase_e.rs` 8 -> 2 (`huc_perform`, `URL.openStream`, the
/// TLS connects and reconnects, the `HttpServer` loop's idle = `Selector.select`; the
/// publisher-body wait and `re5_do_request`'s send / sendAsync door stay),
/// `native-builtins/src/phases_late/zip_streams.rs` 2 -> 0 (file copy / write) and
/// `native-builtins/src/classloader.rs` 1 -> 0 (the loader's HTTP resource fetch);
/// and the two `HttpClient` doors now take the kind from their caller:
/// `native-builtins/src/http_client.rs` 1 -> 0 and `native-builtins/src/http2.rs`
/// 1 -> 0 (`send` = `Parked`, JDK 25 `sendAsync(..).get()`; `sendAsync` =
/// `begin_vm_wait_region`, RUNNABLE, the JDK returns at once).
///
/// Interpreter round i3 wave 83, lane L6 (same page): 24 -> 11 doors, seven rows
/// gone -- `native-awt/src/natives.rs` 2 -> 0 (`EventQueue.getNextEvent` =
/// `Parked`, JDK 25 `pushPopCond.await()`; `invokeAndWait` = `ObjectWait`, the
/// JDK's `lock.wait()` loop), `native-builtins/src/jfr_vm_more.rs` 1 -> 0 (the
/// VM's own JFR sampler idle: `begin_vm_wait_region`, as HotSpot's idle
/// VM-service threads read RUNNABLE), `native-builtins/src/xnio_async.rs` 3 -> 0
/// (xnio-api 3.8 `AbstractIoFuture.await` / `get` = `Parked`, `await(long,
/// TimeUnit)` = `TimedParked`: `LockSupport.park` / `parkNanos`),
/// `native-builtins/src/xnio_conduits.rs` 2 -> 0 and `native-builtins/src/xnio_worker.rs`
/// 2 -> 0 (the conduits' `awaitReadable` / `awaitWritable` and the I/O thread's
/// idle: `Selector.select`, `InNative`), `native-builtins/src/net_phase_e.rs` 2 -> 0
/// (`re5_do_request` and its publisher wait take the kind from the JDK door,
/// `Re5Caller`: `send` = `Parked`, `sendAsync` inline / `contentLength` =
/// `begin_vm_wait_region`, the async task's exchange = `InNative`) and
/// `native-builtins/src/phases_late/net_channels.rs` 1 -> 0 (the WebSocket
/// `buildAsync` handshake: `begin_vm_wait_region`, the JDK returns at once).
/// What is left is by design: the trait's 5 default forwarders, four kill-switch
/// fallbacks of default-on switches (`lib.rs`, `panama.rs`, `panama_upcall.rs`,
/// `vm/src/jvmti/native_env.rs`) and two regions HotSpot would show BLOCKED,
/// which no `BlockKind` expresses (`lang_system.rs`, `quarkus_staticinit.rs`).
const BASELINE: &[(&str, usize)] = &[
    ("native-api/src/registry.rs", 5),
    ("native-builtins/src/lang_system.rs", 1),
    ("native-builtins/src/lib.rs", 1),
    ("native-builtins/src/panama.rs", 1),
    ("native-builtins/src/panama_upcall.rs", 1),
    ("native-builtins/src/quarkus_staticinit.rs", 1),
    ("vm/src/jvmti/native_env.rs", 1),
    // gci round (2026-10-13): the OOME dump's GC-safe slice (i9/hj, i10/al)
    // and the synchronizer census's declined-pause slice (i8/hd). Both are
    // VM-internal waits that open `begin_vm_wait_region` under
    // `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` and fall back to the kind-less
    // door otherwise, exactly as `jvmti::native_env::gc_safe` above; no
    // `BlockKind` names a wait HotSpot performs inside the VM.
    ("vm/src/runtime/interpreter/gc_and_alloc.rs", 1),
    // `vm_exec.rs`: 1 + 8 = 9 at the merge of dev into the gci round
    // (2026-10-16) -- the gci census slice above (1, production) and
    // interpreter round i4/i5 (w99/w100): the `w99_l1_*` unit tests in its
    // `mod tests` call `begin_blocking_region()` to test that door itself (8).
    ("vm/src/vm/vm_exec.rs", 9),
];

/// Belt and braces against a scan that walks the right files but stops
/// matching (a mangled needle, a wrong root): a zero would equal an empty
/// baseline only after someone emptied it on purpose.
const MIN_DOORS_FOUND: usize = 1;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-builtins sits in the workspace root")
        .to_path_buf()
}

/// The `members = [...]` list of the workspace `Cargo.toml` (one line there).
fn workspace_members(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("members = ["))
        .expect("a one-line `members = [...]` in the workspace Cargo.toml");
    line.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The file with every line cut at its first `//` (CRLF-safe).
fn code_of(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The kind-less doors in `code` (see the module doc).
fn kindless_doors(code: &str) -> usize {
    let mut n = code.matches(".begin_blocking_region()").count()
        + code.matches(".begin_timed_blocking_region()").count();
    let needle = "begin_blocking_region_as(";
    let bytes = code.as_bytes();
    let mut start = 0usize;
    while let Some(off) = code[start..].find(needle) {
        let args_start = start + off + needle.len();
        let mut depth = 1i32;
        let mut k = args_start;
        while k < bytes.len() && depth > 0 {
            match bytes[k] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            k += 1;
        }
        let args = &code[args_start..k];
        if args.contains("BlockKind::Waiting") || args.contains("BlockKind::TimedWaiting") {
            n += 1;
        }
        start = args_start;
    }
    n
}

/// `(path from the root with '/' separators, doors)` for every member source
/// file with at least one door, sorted by path.
fn census(root: &Path) -> Vec<(String, usize)> {
    let mut rows = Vec::new();
    for member in workspace_members(root) {
        let src = root.join(&member).join("src");
        assert!(
            src.is_dir(),
            "workspace member `{member}` has no src/ at {}",
            src.display()
        );
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let n = kindless_doors(&code_of(&text));
            if n == 0 {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            rows.push((rel, n));
        }
    }
    rows.sort();
    rows
}

#[test]
fn the_kindless_wait_doors_match_the_baseline_exactly() {
    let root = workspace_root();
    let found = census(&root);
    let total: usize = found.iter().map(|(_, n)| n).sum();
    let table: String = found
        .iter()
        .map(|(p, n)| format!("    (\"{p}\", {n}),\n"))
        .collect();
    println!(
        "unrecorded-wait-site-ratchet: {total} kind-less doors in {} files\n{table}",
        found.len()
    );
    assert!(
        total >= MIN_DOORS_FOUND,
        "the scan found no kind-less doors at all; suspect the scan (root {}), not the code",
        root.display()
    );

    let mut diffs = Vec::new();
    for (path, n) in &found {
        let want = BASELINE
            .iter()
            .find(|(p, _)| *p == path.as_str())
            .map_or(0, |(_, w)| *w);
        if *n != want {
            diffs.push(format!("  {path}: baseline {want}, found {n}"));
        }
    }
    for (path, want) in BASELINE {
        if !found.iter().any(|(p, _)| p.as_str() == *path) {
            diffs.push(format!("  {path}: baseline {want}, found 0"));
        }
    }
    assert!(
        diffs.is_empty(),
        "kind-less blocking-region doors moved:\n{}\n\n\
         A RISE is a new region that publishes no wait kind (JVMTI reads PARKED \
         whatever the JDK code does): give it the `BlockKind` of the JDK body it \
         stands for, or `begin_vm_wait_region` for a wait HotSpot performs inside \
         the VM. A FALL is a classification: lower the row. Either way paste the \
         table above over BASELINE in the same commit.",
        diffs.join("\n")
    );
}

#[test]
fn the_baseline_is_sorted_and_has_no_zero_rows() {
    for pair in BASELINE.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "BASELINE out of order at {}",
            pair[1].0
        );
    }
    for (path, n) in BASELINE {
        assert!(*n > 0, "{path}: a zero row belongs out of BASELINE");
    }
}

#[test]
fn the_scan_reads_the_doors_it_claims_to() {
    let src = "\
        ctx.begin_blocking_region();\n\
        ctx.begin_timed_blocking_region(); // ctx.begin_blocking_region()\n\
        // ctx.begin_blocking_region();\n\
        /// `ctx.begin_blocking_region()`\n\
        ctx.begin_blocking_region_as(BlockKind::Waiting);\n\
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::TimedWaiting);\n\
        ctx.begin_blocking_region_as(if cfg!(windows) {\n\
            BlockKind::InNative\n\
        } else {\n\
            BlockKind::Waiting\n\
        });\n\
        ctx.begin_blocking_region_as(BlockKind::Parked);\n\
        ctx.begin_blocking_region_as(kind(BlockKind::TimedParked));\n\
        ctx.begin_blocking_region_as(fallback_kind());\n\
        ctx.begin_vm_wait_region();\n\
        fn begin_blocking_region(&mut self) {}\n\
        fn begin_blocking_region_as(&mut self, kind: BlockKind) {}\r\n";
    // 2 plain doors + 3 `_as` naming a state; comments, kinds, values and
    // declarations do not count.
    assert_eq!(kindless_doors(&code_of(src)), 5);
}
