# native-io review

## Summary

- **HIGH — Path validation bypass in security-critical entry points.** `RandomAccessFile.open0` (`random_access_file.rs:302`), `sun/nio/fs/*WatchService.register0` (`watch.rs:580`), `ProcessBuilder.start` working directory (`process.rs:173`), and `UNIXProcess.forkAndExec` working directory (`process.rs:560`) all open paths supplied by guest bytecode WITHOUT routing through the crate's central `validate_path` (`lib.rs:207`). This silently defeats the documented null-byte and `..`-segment guards on those surfaces — even with `set_path_confine_to_cwd(true)` the watch service and RAF can still escape.
- **HIGH — Unbounded synchronous TCP connect.** `socket_channel.rs:705` (blocking branch of `sc_connect_inner`) and `net.rs:474` (`sun/nio/ch/Net.connect0`) call `TcpStream::connect` with NO timeout, which can hang for the OS connect timeout (typically ~2 minutes) holding the VM thread. Guest bytecode can dial any RFC 1918/link-local/`169.254.169.254`/metadata-service host (SSRF). The async path (`async_socket.rs:297`) has a 30 s timeout; the blocking NIO path has none. Embedders running untrusted code have no programmatic outbound-host allowlist.
- **MED — `Unsafe`/native-pointer reads/writes are trust-only.** `net.rs::validate_native_range` (`net.rs:243`) only checks `addr > 0` and `len > 0/<1 GiB` — it does NOT verify the address points inside a live `direct_buffer`/`unsafe_allocs` registry entry (the comment at `net.rs:235-242` acknowledges this). `read0`/`write0` then `ptr::copy_nonoverlapping` into that raw address. Same trust hole in `nio_native.rs:113,140,158,180`, `socket_channel.rs:529,561`, and `async_socket.rs:402,1014`. A bug in JDK Java code passing a stale `MappedByteBuffer.address()` will corrupt arbitrary process memory.
- **MED — `lock0`/`release0` are silent no-ops despite returning success.** `nio_native.rs:277,282` returns `0` (success) for `FileChannel.tryLock`/`lock` without taking any OS lock. Java code relying on advisory file locking for cross-process coordination (Lucene index lock, Tomcat single-instance check) silently has no protection.
- **LOW — Test coverage is strong but uneven; zip-bomb guards are best-in-class.** ~235 `#[test]` blocks across the crate (lib.rs 133, nio_selector 17, file_channel 17, net 15, watch 14, direct_buffer 7, stream_decoder 6, datagram 5, process 5, zip_real_jar 4, pipe 4, raf 3, socket_channel 3, async_socket 2). `zip_real_jar.rs` has exemplary decompression-bomb protection (`guard_zip_entry_size`, ratio cap, env-overrides). Estimated coverage ≥ 85 % for unit paths, but error-path / EINTR / partial-write coverage is thin and no `tests/` integration directory exists.

## 1. Code review

### Bugs

- `lib.rs:1052` and `lib.rs:1404` — bounds check uses `off + len > arr_len` after `checked_add`, but the error reports `off.saturating_add(len)` which is fine; however the alias `let off = off as usize` at `lib.rs:1059`/`1411` can still later index by `usize::MAX`-class values on 32-bit hosts because `i32 as usize` on `i32 < 0` is rejected earlier — correct, but a comment block explaining the invariant would prevent regression.
- `lib.rs:2113` — `OutputStreamWriter.write(String, off, len)` clamps `end = (off + len).min(text.len())` but does NOT validate that `off` is itself within `text.len()`, so a negative-sized slice is impossible only because `off`/`len` came in as `usize`; the JDK contract requires `IndexOutOfBoundsException` here, and we currently silently truncate.
- `process.rs:269` — POSIX `signal()` returns `Option<i32>`. `128 + status.signal().unwrap_or(0)` returns `128` when the signal is unknown — but on macOS `signal()` returns `None` for stop signals, which then masquerade as "exited with code 128". Minor JDK-fidelity gap.
- `net.rs:127` — `as_int().unwrap_or(-1) as u32` after `if fd_id >= 0` is safe, but the same pattern is duplicated inline rather than going through `FdId`/`net_fd_from_descriptor`.
- `direct_buffer.rs:679` — `unsafe_allocs().lock().ok()?` swallows poisoned-lock errors, returning `None` (= "size unknown") which causes `unsafe_free_memory` (line 599) to skip the `Bits.unreserve` accounting. A poisoned lock means a prior panic — the bookkeeping drift will leak the OS allocation accounting forever.
- `random_access_file.rs:302` — `opts.open(&path_str).map_err(|_| fnf(&path_str))?` discards the actual `io::Error`. EACCES, EMFILE, ENOSPC, and "not found" all surface as `FileNotFoundException`, which violates JDK contract (real JDK throws `FileNotFoundException` only for ENOENT and `SecurityException` for EACCES etc.).
- `async_socket.rs:298` — `TcpStream::connect(&addr)` (no timeout) is the fallback branch when `addr.parse::<SocketAddr>()` fails, i.e. exactly for hostnames. Hostname-based connects therefore lose the 30 s cap that line 297 enforces for IPs — DNS resolution + connect can hang for the OS default.

### Vulnerabilities

- **HIGH — Path traversal bypass at multiple entry points.** Files that DO NOT call `validate_path`:
  - `random_access_file.rs:302` `RandomAccessFile.open0` — accepts any path, null-byte or `..`.
  - `watch.rs:580` `WatchService.register0` — registers `inotify`/`ReadDirectoryChangesW` on a guest-supplied directory. The `canonicalize_or_passthrough` call at `watch.rs:200` only normalizes the path; it does NOT enforce containment.
  - `process.rs:173` (`ProcessBuilder.start`) and `process.rs:560` (UNIX `forkAndExec`) accept arbitrary working directories AND arbitrary program paths.
  - `file_channel.rs:174` (`map0`) operates on an already-opened fd, so it inherits whatever path validation happened at open time. If that path came in via RAF (above), there was none.
- **HIGH — Synchronous TCP connect lacks per-call timeout / outbound allowlist.** `socket_channel.rs:705` and `net.rs:474` (`connect0`). Guest code can use `SocketChannel.open(addr)` to dial `169.254.169.254:80` (AWS/Azure/GCP metadata service) and read cloud credentials. The crate-level docs (`lib.rs:9-23`) warn about file confinement but say nothing about network egress — a confined deployment that thinks `set_path_confine_to_cwd(true)` is enough still has SSRF exposure.
- **MED — DNS rebinding window.** `net.rs:474` `TcpStream::connect(&conn_addr)` re-resolves the hostname inside `std::net`. An embedder that has already done its own DNS allowlist check on a higher layer will not catch a name that resolves to a public IP on the first lookup and the metadata service on the second.
- **MED — Native-buffer trust boundary (`validate_native_range`).** As above (`net.rs:243-254`): the crate cannot prove a `(addr, len)` pair is inside a live allocation. A buggy or malicious caller in Java code that obtains a `MappedByteBuffer`/`DirectByteBuffer` address can pass it to `read0`/`write0` after `unmap0` and the kernel will happily overwrite freed pages.
- **MED — `SO_KEEPALIVE` silently no-op.** `net.rs:651-676` documents that the kernel keepalive is not wired (no `socket2` dependency). Network protocols that rely on the keepalive to detect dead peers will run forever waiting on a dead socket. Not a memory-safety issue but a contract violation.
- **MED — Watch service path expansion.** `watch.rs:200` canonicalizes via `fs::canonicalize` (which follows symlinks). Combined with the missing `validate_path`, a guest can register a watch on `/proc/<pid>/root/etc` and learn timing information about host file activity even in confined deployments.
- **LOW — `lock0` / `release0` stubs return success without taking a lock** (`nio_native.rs:277,282`). Cross-process coordination via `FileChannel.tryLock` is silently broken.
- **LOW — `process.rs:177` `command.spawn()`** does not sanitize PATH lookups: a relative `program` is resolved against the host PATH. Combined with the missing `validated_path` call this is a sandbox escape on confined deployments.
- **LOW — `async_socket.rs:402` `std::ptr::copy_nonoverlapping(buf.as_ptr(), bb_addr as *mut u8, n)`** on a worker thread, hours after the user-thread enqueued the job. If the underlying `DirectByteBuffer` was already freed via `Cleaner` between enqueue and completion, the worker writes into reclaimed memory. The completion side-table holds `bb_arr: Option<ObjectRef>` but not a strong ref to the buffer's native allocation.

### Stubs

- No `todo!` / `unimplemented!` / `FIXME` / `XXX` / `HACK` markers anywhere (`Grep` confirmed). All `unwrap()` calls outside tests are at `lib.rs:396` (`regex::Regex::new(r"\s+").unwrap()`) — safe constant.
- `nio_native.rs:277,282` — `lock0`/`release0` return `Ok(0)` (advertised as success) but take no OS lock. Documented in the function comment as "let the JDK proceed" — effectively a stub with misleading return value.
- `nio_native.rs:294,298` — `readv0`/`writev0` return `io_error("readv0: unsupported")`. JDK falls back, OK.
- `net.rs:651-676` — `SO_KEEPALIVE` documented contract drift (silent no-op).
- `net.rs:863` — `canIPv6SocketJoinIPv4Group0` always returns 0 (false).
- `async_socket.rs:1199` — `iocp_close` is `Ok(None)` no-op.
- `process.rs:830-839` — `parent0` returns `-1`, `getProcessPids0` returns `0`. Documented "unknown".
- `lib.rs:14461` ends at line 14461 — file is dense, and a large amount of synthetic-jdk behaviour is gated behind `feature = "synthetic-jdk"`. Worth confirming no production path hits a `panic!` under that feature.

### Performance

- `lib.rs:1065,1417` — `let mut buf = vec![0u8; len];` allocates a fresh `Vec` per `read_bytes`/`write_bytes` call. A `BufferPool` keyed by power-of-two would eliminate the malloc churn for hot read-loops (BufferedInputStream is already mitigated via the side-table, but `FIS.readBytes` is not).
- `net.rs:548,593` — same `vec![0u8; len_usize]` per `read0`/`write0` syscall on the hot NIO path.
- `lib.rs:1605` — `ISR_PENDING` is a `Mutex<HashMap<ObjectRef, IsrState>>`; an `ObjectRef`-keyed map taking the lock once per `read_chars` call serializes all InputStreamReader reads in the process. `parking_lot::Mutex` helps but a `DashMap`/`FxHashMap` shard would scale better.
- `lib.rs:1963` — `br_buf_table` is the same global `Mutex<FxHashMap>` pattern; identical bottleneck.
- `async_socket.rs:266` — pool size capped at `parallelism.min(256)`. There's no backpressure on the job queue (unbounded `VecDeque`), so a guest that calls `read` 1M times accumulates 1M `Job::Read` entries in memory. DoS-able.
- `zip_real_jar.rs:332-336` — building the `name_index` for every `getEntry` requires materializing every name. Reasonable trade-off given `file_names()` walks the central directory once.
- `lib.rs:14461` total — the 14k-LOC monolith means rebuilds are slow; consider extracting `BufferedReader`, `BAIS/BAOS`, `Scanner` into sibling modules.

## 2. Tests

### Coverage

- **Count:** ~235 `#[test]` functions across 14 files. Estimated unit-path coverage ≥ 85 % for happy paths.
- **Strongest:** `zip_real_jar` (open + manifest + bomb-cap), `net` (15 tests including bind/accept/read/write/option round-trips, IPv6 capability), `nio_selector` (17 tests including epoll wakeup, key cancellation), `file_channel` (17 tests including `sendfile`, `copy_file_range`, `transferTo` user-space loop).
- **Async path:** `async_socket.rs:1354` `worker_pool_runs_real_connect` is the only end-to-end worker test. Read/write/accept completion paths are NOT exercised end-to-end (no test that drains `completion_queue` via a real handler).
- **No `tests/`** integration directory in the crate. Multi-file scenarios live in higher crates.

### Gaps

- **No tests for path-validation bypass surfaces.** Add tests that confirm `validate_path` IS called on every `<init>`/`open0`/`register0`/`spawn` entry. Right now nothing fails if a reviewer accidentally removes the `validated_path` call from one of the FIS/FOS natives.
- **No EINTR or partial-write tests.** `write_bytes` is treated as a single atomic call; the JDK contract loops on partial writes. `async_socket.rs:466-484` does loop, but `net.rs:613` and `nio_native.rs:161` do not.
- **No socket-shutdown-ordering tests.** Half-close (SHUT_RD then SHUT_WR) is untested.
- **No fuzz / proptest harness.** The workspace has a `fuzz/` crate; `validate_path`, `decode_utf8_into_chars`, `tokenize_command_line`, and `encode_modified_utf8` are excellent fuzz targets but none are wired up.
- **No decompression-bomb regression test.** `zip_real_jar.rs::guard_zip_entry_size` deserves a unit test that builds an archive with an absurd declared size and asserts the guard fires.
- **`async_socket.rs` registration test (`registers_without_panic`) is the only registration test.** No coverage for the `aio_asc_close`-during-pending-read race, or for the `pending_field_resets` drain order.
- **Direct-buffer `OutOfMemoryError`-on-cap-exceeded** is not exercised end-to-end (only the accounting math is).
- **No tests of the `set_path_confine_to_cwd(true) + add_sandbox_root(...)` happy path** — the central security feature has no acceptance test.

### Concrete additions

1. `lib.rs::validate_path` table-driven tests: `assert_eq!(validate_path("foo\0bar").is_err(), true)`, `validate_path("../etc/passwd").is_err()`, `set_path_confine_to_cwd(true); validate_path("/etc/passwd").is_err()`, plus symlink-escape via `tempfile::tempdir`.
2. RAF-open-rejects-traversal test: open `"../escape.txt"` with confinement on, assert `SecurityException`.
3. Watch-service-rejects-traversal test.
4. Zip-bomb test: craft an entry with `uncompressed_size = 1 GiB` and `compressed_size = 1`, expect `guard_zip_entry_size` Err.
5. `async_socket::aio_asc_close_during_pending_read` race: open + connect + read + close, assert handler gets `failed`.
6. `proptest`: `decode_utf8_into_chars(arbitrary_bytes, eof, cap)` never panics and produces a buffer that re-encodes within tolerance.
7. `proptest`: `tokenize_command_line(arbitrary_string)` round-trips through `Command::new(parts[0]).args(parts[1..])` without altering arg count for ASCII inputs.
8. Connect-timeout regression: assert `SocketChannel.connect` does not hang on `192.0.2.0:1` (TEST-NET-1) longer than the configured timeout.
9. Native-range overflow: pass `addr = i64::MAX, len = 1` to `net_read0` — should fail validation, not segfault.
10. Direct-buffer free-then-reuse: confirm `Bits.unreserve` is called exactly once even when `Unsafe.freeMemory` races with the cleaner.

## 3. Documentation

### Existing

- `README.md` (45 lines): scope, non-goals, usage, status, license. Calls out sandbox opt-in explicitly. Good.
- `lib.rs:1-32` crate-level rustdoc: scope, SECURITY section with `validate_path`/`set_path_confine_to_cwd`/`add_sandbox_root` linkage and the `CRATONVM_ZIP_MAX_ENTRY_BYTES` env var. Links to `docs/PLATFORMS.md` for the platform matrix.
- Per-module headers: `async_socket.rs:4-35`, `zip_real_jar.rs:4-25`, `direct_buffer.rs:4-47`, `process.rs:4-59`, `net.rs:4-15`, `watch.rs:4-74`, `pipe.rs`, `file_channel.rs`, `datagram.rs`, `nio_selector.rs`, `random_access_file.rs`, `socket_channel.rs`, `stream_decoder.rs`, `stream_encoder.rs` — every module has a multi-paragraph header explaining intent and tradeoffs. Above average for the workspace.
- Public-API rustdoc: `pub fn` items in `lib.rs` (`set_path_confine_to_cwd`, `add_sandbox_root`, `is_within_sandbox`) are documented. `register_*` registration entry points have one-liners. `validate_path` has the most thorough rustdoc in the crate.
- "AUDIT 2026-05-17" / "Round-9 HIGH" / "C29 fix" markers throughout the code provide audit trail.

### Missing

- No explicit documentation of the network egress trust model — only filesystem confinement is described. Should mirror the `SECURITY` section with a `NETWORK` section: "by default the host process can dial any reachable address; embedders running untrusted code SHOULD wrap `connect0` via a host-allowlist hook" — and either add such a hook or be explicit that the threat model excludes SSRF.
- No documentation of `lock0`/`release0` being a no-op (`nio_native.rs:277`). Callers will assume it actually locks.
- `SO_KEEPALIVE` is documented at the source-comment level (`net.rs:651-676`) but not in any rustdoc or README — embedders relying on keepalive timer behavior have no signal.
- No `docs/` markdown inside the crate. The README points at workspace-level `docs/PLATFORMS.md` for the platform matrix; that's fine for the matrix but a `docs/SECURITY.md` enumerating which entry points DO and DO NOT route through `validate_path` would catch the bypass bugs above.
- Inconsistent: some natives say `validated_path(&path)?` early, others (`random_access_file.rs::native_open0`) don't. A grep table in the crate-level doc would surface this.

## 4. OSS readiness

### Cargo.toml

- `package.name = "cratonvm-native-io"` — consistent with sibling crates.
- `version.workspace`, `edition.workspace`, `rust-version.workspace`, `license.workspace`, `repository.workspace`, `keywords.workspace`, `categories.workspace` — properly inherited.
- `description = "Java I/O native methods for CratonVM"` — present.
- `readme = "README.md"` — present.
- `publish` is inherited from the workspace (`publish = false`).
- Features: `synthetic-jdk` documented inline.
- Dependencies: `parking_lot.workspace`, `zip.workspace`, `regex = "1"`, `memmap2 = "0.9"`, `notify = "6"` (default-features=false + macos_kqueue), `rustc-hash.workspace`. Target-conditional `libc = "0.2"` on Unix. `dev-dependencies`: `tempfile = "3"`. Clean.
- `[lints] workspace = true` — present.

### SPDX / NOTICE

- Every `.rs` file inspected has `// SPDX-License-Identifier: Apache-2.0` and `// Copyright 2024-2026 Craton Software Company` on lines 1-2. Consistent.
- No `LICENSE` or `NOTICE` files inside the crate — the README points at workspace-root copies. Acceptable since `package.license = "Apache-2.0"` is set; many OSS projects ship LICENSE only at the workspace root.

### Blockers

- `publish = false` (workspace-inherited) — by design; this crate is internal to the VM workspace.
- The path-traversal bypasses (HIGH) and SSRF (HIGH) above are not OSS-publication blockers — the crate documents itself as "not a sandbox by default" — but they ARE blockers for any multi-tenant production use even with `set_path_confine_to_cwd(true)` enabled, because that flag does not reach RAF/WatchService/ProcessBuilder.
- Repository URL points to `github.com/craton-co/cratonvm`. Cargo.toml: `repository = "https://github.com/craton-co/cratonvm"` matches.

## Top 5 fix priorities

1. **Route `RandomAccessFile.open0`, `WatchService.register0`, `ProcessBuilder` working directory, and `UNIXProcess.forkAndExec` working directory through `validated_path`.** Five lines of code each; closes the silent traversal-guard bypass on every confined deployment.
2. **Add a per-call timeout (and an embedder-supplied outbound host hook) to `socket_channel.rs::sc_connect_inner` blocking branch and `net.rs::net_connect0`.** Even a 30 s default cap (matching the async path) prevents the worst SSRF/DoS shapes; a configurable allowlist closes the cloud-metadata-service exfiltration class.
3. **Replace the `lock0`/`release0` stubs with a real `fs2`/`flock`/`LockFileEx` call, or change the return value to indicate "unsupported".** Returning success without locking is a silent correctness bug for Lucene/Tomcat-class consumers.
4. **Wire `SO_KEEPALIVE` via `socket2` (already a transitive dep on most platforms) instead of the documented silent no-op.** Drift between Java contract and host behavior is exactly the kind of bug that bites in production months later.
5. **Add the path-validation regression tests (item 1-3 in the test "Concrete additions" list) plus a `fuzz/` target for `validate_path` and `decode_utf8_into_chars`.** A regression suite for the central security feature is the most cost-effective defence against item #1 reappearing after a refactor.
