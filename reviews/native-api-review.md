# native-api review

Reviewed: `C:\Projects\CratonVM\native-api` (workspace member `cratonvm-native-api`).
Files: `Cargo.toml`, `README.md`, `src/{lib,charset,fd_table,ffi,init_level,intrinsic,native_ring,registry,test_mock}.rs`,
`tests/{atomic_fetch_add_err_path,phase_b_bufferedwriter_roundtrip,phase_b_charset_integration,registry_build_once}.rs`.

## Summary

- **MED — `NativeMemoryTable::allocate` leaks on ID overflow** (`src/ffi.rs:65-68`): when `next_id.checked_add(1)` returns `None`, `alloc_zeroed` has already succeeded but the returned `None` never inserts the entry — the allocation is irrecoverable.
- **MED — `set_init_level` is racy** (`src/init_level.rs:64-76`): load-then-store on the AtomicI32 with no CAS allows two concurrent calls to drop the level below `cur`, defeating the "no downward transition" contract the comment promises.
- **MED — `tcp_available` clobbers persistent non-blocking flag** (`src/fd_table.rs:1218-1235`): the function unconditionally toggles `set_nonblocking(true)` then `false`, overwriting any non-blocking mode the caller deliberately set; the same hazard `poll_ready` was rewritten to avoid is back here.
- **MED — TLS error kind discarded** (`src/fd_table.rs:1405,1417`): `tls_read`/`tls_write` wrap every error in `io::Error::new(ErrorKind::Other, …)`, erasing `WouldBlock`/`TimedOut`/`Interrupted` so callers cannot drive a non-blocking TLS event loop.
- **MED — `find_by_method_descriptor` keeps stale callback on legitimate re-register** (`src/registry.rs:1953-1954`): `entry().or_insert(callback)` deliberately preserves the first registration, but the documented "first match" guarantee also makes the secondary index stuck on a stale callback when a triple is *re-registered* with a different callback (the primary `methods` map updates; this one does not).

## 1. Code review

### Bugs

- **HIGH — handle/memory leak on ID-space exhaustion in `NativeMemoryTable::allocate`**, `src/ffi.rs:60-68`. `alloc::alloc_zeroed` succeeds, then `next_id.checked_add(1)` returns `None` and we `return None` *without* `dealloc`-ing the freshly minted block. Practically unreachable (`i64` IDs), but listed because the comment claims correctness and the function is the FFI memory primitive — leaks in an FFI surface are exactly what audits flag. Easy fix: dealloc on the early-return path.
- **MED — `set_init_level` race**, `src/init_level.rs:64-76`. Two concurrent calls can both observe `cur < their level`, then both `store`, the second one going backwards. This violates the documented monotonic-progression contract that the test `monotonic_advance` claims to verify (the test is single-threaded). Replace with `atomic.fetch_max(level, Release)` (or a CAS loop) and only `notify_all` if the value actually advanced.
- **MED — `tcp_available` toggles persistent socket state**, `src/fd_table.rs:1218-1235`. Comment on `poll_ready` (line 1001-1006) explains *exactly* why this dance is unsafe under concurrency; the same dance lives on here. A concurrent `tcp_read` on the same fd is serialised by the `Mutex<TcpStream>`, but the function also resets `set_nonblocking(false)` unconditionally — clobbering any caller that *deliberately* configured the stream non-blocking via `tcp_set_nonblocking`.
- **MED — TLS error type erased**, `src/fd_table.rs:1400-1421`. The `.map_err(|e| io::Error::new(ErrorKind::Other, e.to_string()))` flattens every `native_tls::Error` to `ErrorKind::Other`. Java-side `SocketTimeoutException`/`SocketException` natives downstream cannot reliably distinguish handshake failure from a slow remote.
- **MED — `find_by_method_descriptor` ignores re-registration**, `src/registry.rs:1953-1954`. Combined with `register()` overwriting the primary table on duplicate triples (line 1918) but `or_insert`-ing the secondary index, the two indices diverge for the (admittedly unusual) re-register path. Either reject duplicates or update both indices consistently.
- **MED — `live_bytes` underflow possible if `free` is ever called after `Drop`-style table teardown**, `src/ffi.rs:81`. The unchecked `live_bytes -= alloc.layout.size()` would underflow `usize` if the table state ever desyncs (e.g., a future refactor that frees without removing). Use `saturating_sub`, or assert that the entry was present (which it must be for `remove` to have returned `Some`). LOW today, would catch a future regression.
- **LOW — `set_init_level` `eprintln!` on downward transition**, `src/init_level.rs:70-73`: silent + warning is fine, but writing to stderr from a library is generally avoided — at minimum, the message should be testable via the `tracing` crate (already a workspace dep through `native-builtins`).
- **LOW — `native_ring::record_exit` writes to a slot that may have been recycled**, `src/native_ring.rs:140-147`. After 64 entries are recorded between enter and exit, the recorded slot for THIS call has been overwritten by an unrelated entry; `record_exit` writes its `exit_ms` into that unrelated entry. Diagnostic-only, but the dump becomes misleading. Stamp the slot with a per-enter sequence and verify before writing.
- **LOW — inconsistent fd-counter rollback policy in `fd_table`**: `open_read`/`open_write` (lines 271, 289) explicitly DO NOT roll back on overflow (with a documented rationale); `open_read_write`, `open_udp`, `open_tcp_connect`, `open_tls_connect`, `open_tcp_listener` (lines 585, 783, 845, 899, 1383) all DO roll back. Pick one. The rollback is racy enough that the no-rollback policy is preferable.

### Vulnerabilities

- **MED — `decode_bytes_lossy` for unsupported charset returns all-`U+FFFD`**, `src/charset.rs:149`. A caller using `decode_bytes_lossy` as a "best effort" fallback for an unknown name produces a buffer where every position is `0xFFFD`. That's defensible for malformed input but is misleading when the input was *valid bytes for some encoding* the library doesn't support — the application sees a string of replacement characters instead of an unsupported-charset error. Consider returning `bytes.iter().map(|&b| b as u16).collect()` (latin1 fallback) or surfacing the error.
- **MED — `encode_chars_lossy` for unsupported charset silently encodes as UTF-8**, `src/charset.rs:167`. The downstream byte sink receives UTF-8 when it asked for, say, `windows-1257`. Logging or returning the underlying error would be safer.
- **LOW — `set_field_volatile`/`get_field_volatile` default impls absent on the trait** — every implementor must provide a body. Verified: trait declares them without defaults (`src/registry.rs:1030,1033`). Good.
- **LOW — `is_package_exported_unqualified` and friends have NO defaults** (`src/registry.rs:1627-1645`), with an explicit `AUDIT 2026-05-19` comment explaining why fail-open defaults were removed. This is exemplary; flagged here as a *good* design that downstream implementors must respect.

### Stubs

- **`native_ring.rs:38`**: `TODO: re-arm via native_ring::enable(true) when watchdog wires up`. Stubbed but called from the workspace — flagged because the comment hints it's wired one-way.
- **`registry.rs:1547-1554` (`redefine_class`)**: trait default returns `Err("redefine_class not implemented")`. This is fine for mocks; just call it out as a stub that downstream implementors must override for instrumentation support.
- No `todo!()`, `unimplemented!()`, `panic!()` or `FIXME` calls in production code paths. (Tests use `.unwrap()`/`.expect()` liberally, which is normal.)

### Performance

- **MED — `read_line` reads one byte at a time from the underlying `BufRead`** (`src/fd_table.rs:382-409`). `read_one` calls `r.read(&mut buf)` per byte; for a 1 MB single-line file this is 1M lock acquisitions on `parking_lot::Mutex<BufReader<File>>` (lock is held outside the inner helper, but each `r.read` still goes through the trait). A byte-at-a-time loop also defeats the whole point of `BufReader`. Use `r.fill_buf` + `r.consume` scanning for the terminator.
- **LOW — `native_method_hash` allocates a `format!` for every `register()` ring-buffer name** (`src/registry.rs:1967`). 3,100 calls at boot = ~3,100 small `String` allocations + 3,100 `Mutex` lock pairs. Comment acknowledges this; defer until profiled.
- **LOW — `find_with_descriptor_quirks`** still does up to 3 `format!`/`replace`/`split_at` operations per cold-miss with a quirky descriptor (`src/registry.rs:2027-2079`). Cold path, but the descriptor variants array can be a `[&str; 3]` with `Cow` to skip the second alloc on the no-newline variant.
- **LOW — `pread_at` saves+restores the cursor via two seeks** (`src/fd_table.rs:601-630`). `pread`/`pwrite` syscalls (`platform_pread64` etc.) are atomic and don't move the cursor; replacing the two seeks with platform `pread64`/`pwrite64` would halve syscall count.
- **LOW — `decode_utf16_fixed` validates surrogate pairing in a second pass** (`src/charset.rs:296-321`). The single-pass loop above could validate inline; benign overhead.

## 2. Tests

### Coverage estimate

- **Total tests:** ~140 (inline unit + 4 integration files). Modules:
  - `lib.rs` — re-exports only, 0 tests.
  - `charset.rs` — 13 inline tests (round-trips, BOM detection, lossy, sub-byte CPs).
  - `fd_table.rs` — ~40 inline tests (open/read/write/line, CRLF/CR/LF, FD overflow, stdin/stdout, FileWrite/Read/Read+Write, round-trip).
  - `ffi.rs` — ~35 inline tests (allocate/free/zeroed/min-size/overflow, layout sizes/alignment, align_up, UpcallTable, arena constants, FFM ValueLayout sanity, MemorySegment slice).
  - `init_level.rs` — 4 inline tests.
  - `intrinsic.rs` — 0 (pure data enum, fine).
  - `native_ring.rs` — 0 tests.
  - `registry.rs` — ~15 inline tests (register/find/distinct-by-{class,descriptor}, hash invariants, alias_class).
  - `test_mock.rs` — 4 self-tests on the in-crate mock.
- **Integration tests:**
  - `atomic_fetch_add_err_path.rs` — 6 tests covering the type-mismatch Err path on int/long atomic accessors.
  - `phase_b_bufferedwriter_roundtrip.rs` — 4 tests on fd-table round-trips.
  - `phase_b_charset_integration.rs` — 18 tests on the charset surface (UTF-8/16/32 round-trips, lossy, sub-byte CPs, metadata).
  - `registry_build_once.rs` — 3 tests pinning the "build once, freeze" Arc contract.
- **Coverage estimate (line/branch):** ~70-75% in `charset`, `ffi`, `fd_table`, `registry`; ~30% in `init_level` (concurrency unverified) and `native_ring` (untested at all). Trait `NativeContext` default-impl bodies are exercised by `atomic_fetch_add_err_path.rs` and `test_mock.rs`, but the ~60 other defaults (write_byte_array_from bounds, bulk_array_copy, define_class_full delegation, set_static_field_by_name, etc.) are untested. **Workspace target ≥85% is NOT met overall** — estimate is ~60-70% blended.

### Gaps

1. **`native_ring.rs` has zero tests.** `enable`/`is_enabled`/`record_enter`/`record_exit`/`dump_to_stderr`/`register_name`/`name_of` are all untested.
2. **`init_level.rs` concurrency invariants untested.** The race in `set_init_level` (#bug above) would have been caught by a `crossbeam` test spawning two setters.
3. **TLS path is untested** (`src/fd_table.rs:1380-1421`). No `open_tls_connect` / `tls_read` / `tls_write` integration test.
4. **`pipe_read` writer-still-open vs EOF distinction untested** (`src/fd_table.rs:1335-1360`). The `Arc::strong_count > 1` heuristic is subtle and worth a regression test.
5. **`tcp_available` clobber bug** has no regression test.
6. **`UpcallTable::register`** slot-reuse-after-remove is tested; **out-of-order removes + register interleaved** are not.
7. **`find_by_method_descriptor`** (the O(1) fallback) is not directly tested — only via `find_distinguishes_by_class`.
8. **`alias_class`** is tested once but not the *no-op* idempotent case (calling twice).
9. **No fuzz / proptest harness for `charset` decoders** despite `fuzz/` existing in the workspace. UTF-8/16/32 boundary input is the canonical fuzz target.
10. **No proptest for `native_method_hash`** distribution (the comment claims "birthday-collision probability of 1e-32" but never tests across the 3,100-entry working set).

### Concrete additions

- Add `tests/init_level_concurrency.rs`: spawn N threads each calling `set_init_level(rand 0..=4)`, assert post-condition `get_init_level() == 4` (monotonic) — would currently fail.
- Add `tests/tcp_available_preserves_nonblocking.rs`: `set_nonblocking(true)` → `tcp_available` → assert socket is still non-blocking. Would catch the clobber.
- Add a `proptest` to `charset.rs` (already a workspace dev-dep in `types`): for every `String s`, `decode_bytes("UTF-8", &encode_chars("UTF-8", &s.encode_utf16().collect())) == s.encode_utf16().collect()`.
- Add `fuzz/fuzz_targets/charset_decode.rs` — input arbitrary bytes, exercise every supported name's `decode_bytes` / `decode_bytes_lossy` and assert no panics, no UB.
- Add `tests/native_ring.rs`: enable, register a callback, record enter/exit, dump, parse output, assert format.
- Add `tests/tls_smoke.rs`: spin up a local TCP listener with a self-signed cert and exercise `open_tls_connect`/`tls_read`/`tls_write`.
- Add `tests/registry_re_register.rs`: register `(A,m,d)` with cb1, then re-register with cb2, then call `find_by_method_descriptor("m","d")` and assert it returns cb2 (or document that it returns cb1 — current behaviour).

## 3. Documentation

### What exists

- Crate-level rustdoc in `lib.rs:1-7` — short but accurate.
- README.md covers scope, non-goals, usage, status, license — 47 lines, well-pitched.
- Module-level docs on every file (`charset`, `fd_table`, `ffi`, `init_level`, `intrinsic`, `native_ring`, `registry`, `test_mock`) — all present, all helpful.
- `NativeContext` trait individual methods are heavily commented; ~150 methods, almost every one has a doc-comment explaining VM-side semantics, default-impl rationale, and audit history (the `AUDIT 2026-05-XX` markers are an excellent practice).
- Public structs (`AnnotationData`, `AnnotationElementValue`, `StackTraceEntry`, `DefineClassFull`, `FieldMetadata`, `MethodMetadata`, `NativeCallback`, `NativeMethodRegistry`, `FileDescriptorTable`, `NativeMemoryTable`, `UpcallEntry`, `UpcallTable`) all have rustdoc.
- Layout constants (`LAYOUT_BYTE`, `ARENA_GLOBAL`, …) lack individual doc-comments but the section header explains them.

### Missing

- **Crate-level rustdoc is too terse.** Three lines doesn't convey the full surface. Should at least summarise the four submodules (`registry`, `fd_table`, `ffi`, `charset`) with a sentence each and a link to `NativeContext`.
- **No `Native API stability promise.`** README says "Pre-1.0. API stability is best-effort" but the crate-level rustdoc doesn't echo this. Important because this is the ABI boundary for every `native-*` crate.
- **`InterpIntrinsic` enum variants lack per-variant docs** (`src/intrinsic.rs:28-62`). The module doc covers it, but `pub enum` variants should each carry one line on their semantics for rustdoc.
- **`UpcallEntry`'s `param_kinds`/`return_kind` fields** (`src/ffi.rs:198-208`) carry one-line docs but don't say *which* enum the i32 maps to (the `LAYOUT_*` constants). Cross-link.
- **No `# Errors` / `# Panics` / `# Safety` sections** on public functions that have non-obvious failure or `unsafe` semantics — e.g., `NativeMemoryTable::allocate` returns `None` on at least four distinct failure modes; the rustdoc says only "Returns None if allocation fails."
- **`FileDescriptorTable` rustdoc** doesn't list the supported FD kinds. The module doc is minimal ("File descriptor table for I/O operations."). A short table of what fds are supported (stdin/out/err, files, sockets, pipes, TLS, child pipes) would help.
- **`charset.rs`** is missing a "supported charsets" canonical list at module-level — the names are implicit in the match arms.
- **No CHANGELOG entry** for this crate. Workspace `CHANGELOG.md` exists; should reference major surface changes (e.g., the 2026-05-16 `B3/B4` Arc refactor in fd_table).
- **No examples directory.** Workspace convention isn't visible; the README sample in lines 25-35 is a good minimal example but could be moved to `examples/`.

## 4. OSS readiness

### Cargo.toml

Present: `name`, `version` (workspace), `edition`, `rust-version`, `license`, `description`, `readme`, `repository`, `keywords`, `categories`. Workspace lints inherited. Feature `test-mock` documented. Dependencies are minimal and pinned via workspace.

Missing/problematic:
- **`publish = false` is inherited via `workspace.publish = false`** (`Cargo.toml:6`). This blocks `cargo publish` outright. Removing the workspace-level flag and per-crate setting `publish = true` only for crates ready to ship is the cleaner path — `native-api` is a strong candidate to publish *first* (it has the cleanest external API).
- **No `documentation = "https://docs.rs/cratonvm-native-api"` field.** crates.io will fall back to the README; explicit is nicer.
- **No `[badges]`** section.
- **`socket2`, `native-tls` not pinned via workspace** (`Cargo.toml:22-23`). Inconsistent with the workspace-hoist policy (`parking_lot`, `rustc-hash` ARE workspaced). If two crates pull `socket2` they may diverge versions.

### SPDX / NOTICE / headers

- Every `.rs` file in `src/` and `tests/` (except `atomic_fetch_add_err_path.rs` and `registry_build_once.rs` and `test_mock.rs`) opens with `// SPDX-License-Identifier: Apache-2.0` + copyright. **Three production-path source files are missing the SPDX header:**
  - `src/test_mock.rs` (no header at all)
  - `tests/atomic_fetch_add_err_path.rs` (no header)
  - `tests/registry_build_once.rs` (no header)
- Workspace `LICENSE` and `NOTICE` are present at `C:\Projects\CratonVM\LICENSE`/`NOTICE`; crate-level inclusion (via `include = [...]`) is not declared — `cargo publish` will pick up the workspace LICENSE only if relative paths resolve, which they will not. Either symlink or add `license-file = "../LICENSE"` (deprecated; prefer SPDX `license = "Apache-2.0"`, which IS already set).
- README references `LICENSE` and `NOTICE` "at the workspace root" — fine in-tree, but `cargo publish` ships only the crate dir. Add a per-crate `LICENSE` (copy or symlink).

### Blockers for publishing this crate to crates.io

1. `workspace.publish = false` blocks the whole workspace.
2. Three missing SPDX headers (above).
3. Per-crate `LICENSE`/`NOTICE` not present.
4. Dependency `cratonvm-types = { path = "../types" }` is path-only — must be `version = "..."` to publish.

None are architectural; all are mechanical fixes.

## Top 5 fix priorities

1. **HIGH — fix the `NativeMemoryTable::allocate` leak on ID overflow** (`src/ffi.rs:60-68`). One `unsafe { alloc::dealloc(ptr, layout) }` on the early-return path. ~3 lines.
2. **MED — make `set_init_level` race-free** (`src/init_level.rs:64-76`). Replace load+store with `atomic.fetch_max(level, Release)`; also add a multi-thread regression test.
3. **MED — `tcp_available` should not toggle persistent socket state**. Either use `socket2::peek` against a copy, or compute "available" via `WSAIoctl(FIONREAD)` / `ioctl(FIONREAD)` — never flip the blocking flag the application set. (`src/fd_table.rs:1218-1235`)
4. **MED — preserve TLS error kinds** in `tls_read`/`tls_write` (`src/fd_table.rs:1400-1421`). Map `native_tls::Error` discriminants to `io::ErrorKind::WouldBlock` / `TimedOut` / `Interrupted` / `Other`.
5. **MED — `find_by_method_descriptor` re-register consistency** (`src/registry.rs:1953-1954`). Document the "first wins" guarantee explicitly *or* update the secondary index on re-register; add a regression test either way. Also: add the missing SPDX headers in three files (5 minutes) — gate-condition for publish.
