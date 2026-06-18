# native-api crate review — 2026-06-10 (Fable)

Scope: `native-api/src` (9 files, ~8.2k LOC) and `native-api/tests` (4 integration files).
Static review only — no builds/tests run.

## Summary

`cratonvm-native-api` is the ABI boundary crate for all `native-*` crates: it defines the
`NativeContext` trait (~150 methods), the `NativeMethodRegistry` (hashed native dispatch table),
the `FileDescriptorTable` (files / sockets / pipes / TLS / child pipes), the `NativeMemoryTable`
+ `UpcallTable` (Panama FFI off-heap memory and upcalls), charset transcoding, the process-global
init-level state machine, the native-call diagnostic ring buffer, and an interpreter-intrinsic
enum.

Overall code quality is **high**. The crate shows extensive, dated audit history (`AUDIT 2026-05-*`
markers), generation-tagged handles to defeat use-after-free / confused-deputy in the FFI tables,
explicit "the trait does NOT validate this, the impl MUST" contracts (M4a) for index-taking
methods, and consistent fail-closed behavior. Most findings from the prior internal review
(`docs/internal/reviews/native-api-review.md`) have since been **fixed**: the `allocate` ID-overflow
leak now deallocs (ffi.rs:149), `set_init_level` now uses `fetch_max` (init_level.rs:74),
`tcp_available` no longer toggles the blocking flag (fd_table.rs:1328). A handful of medium/low
issues remain, plus one newly-spotted contract gap (`bulk_array_copy` default missing bounds check).

There are **no production-path `unwrap()`/`expect()`/`panic!`/`todo!`/`unimplemented!`** — all such
calls are inside `#[cfg(test)]`. The `unsafe` surface (raw native-memory alloc/free, OS `poll`/`WSAPoll`
FFI) is small, well-documented, and bounds/overflow-checked.

## Bugs

### B1 (MEDIUM) — `bulk_array_copy` trait default has no bounds check — contract violation, reachable OOB in non-validating impls
`registry.rs:624-637`. The default impl loops `get_array_element(src, src_off+i)` /
`set_array_element(dst, dst_off+i, ...)` for `i in 0..len` with **no validation** of `src_off`,
`dst_off`, or `len` against the array lengths, then unconditionally `return true`. This contradicts:
(a) the method's own doc ("Returns `false` on bounds / element-type mismatch"), and (b) the
sibling methods `write_byte_array_from` / `read_byte_array_into` / `read_char_array_into` /
`write_char_array_from` (registry.rs:533-616) that were *explicitly* hardened with "CRIT fix"
bounds guards. The element accessors are documented as NOT validating (`get_array_element` /
`set_array_element`, registry.rs:498-512: "The trait does NOT validate it ... MUST be in
`0..array_length`"). So an impl whose accessors don't self-validate gets driven out of bounds by
this default, reachable from `System.arraycopy`-class natives with untrusted offsets/lengths. The
production VM override (`vm/src/vm/vm_exec.rs:2050`) does its own checks and the VM's element
accessors swallow OOB (`.unwrap_or`/`let _ =`), so the *current* VM is safe — but the trait-level
contract is broken and any future/out-of-tree impl inherits an unsound default. Fix: add the same
`checked_add` + length guard as the sibling methods and return `false` on violation.

### B2 (LOW) — `find_by_method_descriptor` secondary index diverges from primary on re-register
`registry.rs:2370` vs `2411`. `register()` does `self.methods.insert(key, callback)` (overwrites)
for the primary index but `self.by_method_desc.entry(md_key).or_insert(callback)` (first-wins) for
the class-agnostic fallback index. Re-registering the *same* triple with a *different* callback
updates the primary map but leaves the fallback index pointing at the stale first callback. The
"first match" contract is documented, but the divergence between the two indices for an identical
triple is surprising. Low severity (re-registering a triple with a new callback is rare; the
fallback is only consulted on the NSME slow-recovery path). Fix: update both indices consistently
on re-register, or document the asymmetry at the call site.

### B3 (LOW) — `native_ring::record_exit` can stamp `exit_ms` on a recycled, unrelated slot
`native_ring.rs:137-152`. `record_enter` returns a ring slot index; `record_exit` writes `exit_ms`
into `ring.entries[idx]`. The ring is 64 entries and wraps. If ≥64 native calls are recorded between
a given enter and its exit (deeply nested / long-lived native frame), the slot has been overwritten
by an unrelated entry and `record_exit` corrupts that unrelated entry's duration. Diagnostic-only
(the ring is a hang-dump aid, default-off and only armed under a watchdog), but it makes the dump
misleading exactly when nesting is deep. Fix: stamp each enter with a monotonic sequence number and
have `record_exit` verify the slot still belongs to it before writing.

### B4 (LOW) — `pread_at`/`pwrite_at` leave the file cursor moved if the inner read/write errors
`fd_table.rs:666-732`. Both save the position, seek to `position`, do the I/O via `?`, then restore.
If the `read`/`write_all` errors, the `?` returns early and the restore-seek is skipped, leaving the
cursor at `position` rather than the saved offset — a subsequent sequential `rw_read`/`rw_write` then
runs from the wrong offset. Low impact (positional I/O errors are rare and file-position-on-error is
loosely defined), but a true atomic `pread64`/`pwrite64` (or a restore in a guard/`finally`-style
wrapper) would be more robust.

## Vulnerabilities

### V1 (LOW) — TLS read/write flatten all errors to `ErrorKind::Other`, losing `WouldBlock`/`TimedOut`
`fd_table.rs:1532-1553` (also the handshake errors at 1521-1524). `tls_read`/`tls_write` do
`.map_err(|e| io::Error::new(ErrorKind::Other, e.to_string()))`, erasing the original `ErrorKind`.
A Java-side non-blocking TLS event loop cannot distinguish "would block / retry" from a fatal
handshake/connection error, and downstream natives can't map to `SocketTimeoutException` vs
`SSLException`. Not memory-unsafe; a correctness/robustness gap on the TLS path. Note the positive:
`open_tls_connect` uses `native_tls::TlsConnector::new()` with **default cert + hostname validation**
(no `danger_accept_invalid_certs`), so TLS trust is enforced — good.

### V2 (LOW) — charset lossy fallbacks silently substitute on unsupported charset name
`charset.rs:149` (`decode_bytes_lossy`) returns an all-`U+FFFD` buffer for an unsupported charset
name; `charset.rs:167` (`encode_chars_lossy`) silently encodes as UTF-8 for an unsupported name.
A caller using these as "best effort" gets plausible-looking but wrong output (every char replaced,
or bytes in the wrong encoding) instead of an unsupported-charset signal. Defensible for malformed
*input*, but masks an unsupported-*charset* misconfiguration. Consider a latin1 passthrough or
surfacing the error for the unknown-name case.

## Stubs and Unimplemented

The crate is a trait/registry definition layer, so most "empty" bodies are **legitimate trait
defaults** that the real VM overrides and mock contexts rely on — these are NOT the forbidden
"synthetic stub that fakes app behavior" kind. Enumerated for completeness:

- `registry.rs:1845` `redefine_class` default → `Err("redefine_class not implemented")`. Conservative
  default; VM overrides for instrumentation/redefinition. Fine as a default, but note it is a real
  capability gap if any path relies on the default.
- `registry.rs:1855/1862` `list_loaded_class_ids` / `list_initiated_class_ids` defaults → empty `Vec`.
- `registry.rs:1222` `static_field_index_by_name` default → `None`.
- `registry.rs:1663` `class_bytes`, `1671` `find_all_resource_urls`, `1683` `find_all_resource_bytes`,
  `1693` `find_class_source_path`, `1704` `class_code_base`, `1713`/`1724` cert digests/certs,
  `2048-2088` `class_file_version`/`inner_classes`/`enclosing_method`/`declaring_class`/`raw_annotations`/
  `nest_host_name`/`nest_member_names` — all return empty/None/default. Reflection/CDS/agent surface;
  VM overrides. Acceptable trait defaults.
- GPU-offload hooks `gpu_*` (registry.rs:188-281) default to `None`/no-op — gated behind the
  `gpu-offload` VM feature; default-off is intended.
- `native_ring.rs:42` carries a stale `TODO: re-arm via native_ring::enable(true) when watchdog
  wires up` — but it **is** actually wired up (`vm-cli/src/main.rs:1627` calls
  `native_ring::enable(true)` when a watchdog is enabled). The TODO is stale and should be deleted to
  avoid implying the diagnostic is dead.

No `todo!()` / `unimplemented!()` / `NotImplemented` / fake-value-returning natives in this crate.

## Performance

### P1 (MEDIUM) — `read_line` reads one byte at a time through the locked `BufRead`
`fd_table.rs:394-457`. `read_line_inner` calls `read_one` (a 1-byte `r.read()`) per character. For a
multi-MB single-line file this is millions of `read` trait dispatches and largely defeats `BufReader`.
Use `fill_buf` + memchr-scan for the terminator + `consume`, copying spans. (The `\r`/`\r\n`/`\n`
handling already peeks via `fill_buf` for the `\r\n` case, so the buffered API is available.)

### P2 (LOW) — every `register()` formats a `String` for the ring-buffer name map
`registry.rs:2424-2425`. ~3,100 boot registrations each `format!("{class}.{method}{desc}")` + a
`Mutex` lock into the name map, unconditionally (the gating was deliberately removed — WF32-fix).
~3,100 small allocs + lock pairs at boot. Comment acknowledges the cost; acceptable but could be
deferred/lazy-resolved (store the three `Arc<str>` parts already in `registrations` and format only
when the dump actually runs).

### P3 (LOW) — native-call ring uses a single process-global `parking_lot::Mutex`
`native_ring.rs:77` + call sites in the interpreter/JIT. When enabled, `record_enter`/`record_exit`
both lock a global mutex on every native call — global serialization across all carrier threads under
heavy native traffic (e.g. an `Unsafe` hot loop). Default-off mitigates it, but when a watchdog is
armed it is on for the whole run. A per-thread ring or a lock-free atomic ring would remove the
contention. (Also tracked in `docs/internal/roadmap.md`.)

### P4 (LOW) — `decode_utf16_fixed` validates surrogate pairing in a second pass
`charset.rs:296-321`. A separate post-decode loop re-walks the output to validate surrogate pairs;
this could be folded into the single decode loop. Benign.

## Tests

Inventory (read, not run):
- `charset.rs` — ~13 inline tests (UTF-8/16/16BE/16LE/32 round-trips, BOM endianness, lossy
  substitution, cp1252/koi8-r, latin1 full-256). Integration `phase_b_charset_integration.rs` adds
  ~18 more (round-trips, metadata, sub-byte code points).
- `fd_table.rs` — ~40 inline tests (open/read/write, read_line CR/LF/CRLF, byte/bulk read+write,
  close semantics, stdin/stdout/stderr, fd-overflow guard, round-trips). Integration
  `phase_b_bufferedwriter_roundtrip.rs` adds ~4.
- `ffi.rs` — ~35 inline tests (allocate/free/zeroed/min-size/overflow-returns-none, exhaustion-dealloc
  hook, get_ptr_checked window validation, AllocHandle stale-after-reuse, UpcallTable register/remove/
  slot-reuse/handle-staleness, layout sizes/alignment, align_up edge cases, FFM sanity).
- `registry.rs` — ~15 inline tests (register/find, distinguish by class/descriptor, hash determinism/
  independence/swap-sensitivity, alias_class copy). Integration `registry_build_once.rs` adds 3
  (freeze/Arc-uniqueness/Send+Sync).
- `init_level.rs` — 6 inline tests incl. a concurrent `fetch_max` regression test.
- `atomic_fetch_add_err_path.rs` — 6 integration tests on the int/long atomic type-mismatch Err path.
- `intrinsic.rs` (pure enum) and `native_ring.rs` — **0 tests**.

Estimated coverage: **~70%** blended. Well-covered: `charset` (~80%), `ffi` memory/upcall tables
(~80%), `fd_table` file I/O (~70%), `registry` core hash/register/find (~75%), `init_level` (~70%
incl. the concurrency regression test). Poorly/un-covered: the socket and TLS paths of `fd_table`
(open_tcp/udp/tls, poll_ready, pipe EOF heuristic, tcp_available) have essentially no integration
tests; `native_ring.rs` is 0%; the ~60 non-atomic `NativeContext` trait default bodies
(`bulk_array_copy`, `write_byte_array_from`/`read_byte_array_into` bounds paths,
`set_static_field_by_name`, `find_with_descriptor_quirks` rewrite variants) are mostly untested.

Does it plausibly reach 85%? **No** — blended ~70%. Biggest gaps to close to approach 85%:
1. `bulk_array_copy` + the byte/char-array bounds guards (would have caught B1) — pure-logic, mockable.
2. `find_with_descriptor_quirks` (whitespace/NUL/CRLF/return-type-`;` rewrite variants) — untested cold path.
3. `native_ring` enable/record_enter/record_exit/dump/register_name/name_of, incl. the 64-wrap B3 case.
4. Socket/TLS smoke: local TCP listener + accept/read/write; self-signed-cert TLS connect; pipe
   writer-open-vs-EOF (`strong_count` heuristic, fd_table.rs:1456-1481).
5. A charset fuzz/proptest target (arbitrary bytes through every `decode_bytes`/`*_lossy` — assert
   no panic/UB) and a round-trip proptest.
6. `find_by_method_descriptor` re-register behavior (pins B2 either way).

## Feature Suggestions

1. Add a `bulk_array_copy` bounds-validated default (closes B1) and, more broadly, a single internal
   `fn array_window_ok(len, off, n)` helper reused by all five bulk array trait defaults so the
   guard cannot drift between them again.
2. Map `native_tls::Error` discriminants to precise `io::ErrorKind` (`WouldBlock`/`TimedOut`/
   `Interrupted`/`Other`) in `tls_read`/`tls_write` so Java non-blocking TLS event loops work and
   exception mapping is faithful (closes V1).
3. Replace the global-mutex native ring with a per-thread ring (or atomic ring with a generation
   stamp) — removes P3 contention and fixes B3's recycled-slot corruption in one change.
4. Switch `pread_at`/`pwrite_at` to true positional syscalls (`pread64`/`pwrite64` on Unix,
   `ReadFile`/`WriteFile` with `OVERLAPPED` on Windows) — atomic, no cursor save/restore, fixes B4
   and halves syscall count.
5. Add a `charset` proptest + `cargo-fuzz` target for the decoders (the canonical fuzz surface for a
   JVM transcoder) and a module-level "supported charsets" doc list.
6. Make the registry publishable-first: it has the cleanest external API. The `register`-time
   `format!` name population could move behind a lazy resolver, and the crate-level rustdoc should
   summarize the four submodules (registry/fd_table/ffi/charset) and the `NativeContext` ABI-stability
   stance.

## Files sampled vs fully read

Fully read:
- `lib.rs`, `init_level.rs`, `intrinsic.rs`, `native_ring.rs` (small, read in full).
- `ffi.rs` — implementation read in full (NativeMemoryTable, UpcallTable, alloc/free/get_ptr_checked,
  align_up, layout helpers); test module skimmed.
- `charset.rs` — `decode_bytes`/`encode_chars` dispatch, lossy fallbacks, UTF-16/UTF-32 fixed
  decoders (strict + lossy), single-byte helpers structure read fully; test module + static tables
  skimmed.
- `registry.rs` (2880 lines) — read structure (grep), then deep-read: hash functions
  (`native_method_hash`/`hash_pass`/`fmix64`), `register`/`find`/`find_with_descriptor_quirks`/
  `alias_class`/`find_by_method_descriptor`, the array-bulk trait defaults, the atomic-fetch-add
  defaults, the volatile/CAS contract docs, and the resource/class-loading/redefine defaults. GPU
  trait-default block read. Inline test module skimmed.
- `fd_table.rs` (2224 lines) — read structure (grep), then deep-read: FileEntry enum, the OS
  poll/WSAPoll FFI module, open_read/open_write/open_read_write, read_bytes/read_line/write_bytes,
  pread_at/pwrite_at/rw_*, open_udp/udp_send/udp_recv, tcp_available, poll_ready, open_pipe/pipe_read/
  pipe_write, open_tls_connect/tls_read/tls_write. Many of the ~40 small per-option socket setters
  (tcp_set_*/udp_set_*) sampled, not each read in full. Inline test module skimmed.

Sampled (not fully read): `test_mock.rs` (test-only mock, `cfg(test, feature=test-mock)`; read the
header + field-store design — `UnsafeCell` single-threaded-test pattern, not production). Test files
`atomic_fetch_add_err_path.rs`, `phase_b_*` read at the grep/inventory level; `registry_build_once.rs`
read in full.
