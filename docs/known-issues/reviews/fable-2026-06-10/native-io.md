# Code Review — `native-io` crate

Reviewer: Fable (Opus 4.8) — static review only, no build/test executed.
Date: 2026-06-10
Scope: `native-io/src` (18 files, ~34k LOC). No `tests/` directory; all tests are inline `#[cfg(test)]`.

## Summary

`native-io` exposes the host filesystem, subprocess spawn, and network stack to running
Java bytecode. It is a large, mature, and — overall — unusually careful crate: the raw-memory
paths route untrusted addresses through `ctx.copy_to/from_native_memory` (avoiding raw derefs of
synthetic arena handles), the byte-array read/write natives use a shared `check_array_bounds`
that rejects negative `off`/`len` before allocating, the zip natives have explicit
decompression-bomb guards, `direct_buffer.rs` carries a thorough ABA double-free guard, and the
SSRF/outbound-policy hook is genuinely well designed and well tested. Many comments document
prior security fixes (V12 confinement profile, C27 GC-stable selector keys, the
`dbb_allocate_direct0` slot-clobber fix), indicating active hardening.

The findings below are therefore mostly **medium/low correctness and spec-compliance issues**
rather than memory-safety holes — the GC layer's `set_array_element`/`get_array_element` are
bounds-checked and silently no-op / return 0 on OOB, so the missing native-level bounds checks
manifest as wrong results (and, in debug builds, panics from integer overflow), not memory
corruption. The most important items are: (1) a **subprocess-spawn sandbox bypass** — under
`set_path_confine_to_cwd(true)` the spawned program path is not validated; (2) a class of
**ByteBuffer / ByteArrayInputStream bulk natives that skip the JDK-mandated index/overflow
checks**; and (3) a **default-on synthetic DatagramChannel stub** (`t16_dc_connect`) that
fabricates "connected" state and a fake `127.0.0.1:9` target — a "no synthetic stubs" policy
violation.

---

## Bugs

### B1 (medium) — `ByteBuffer` bulk get/put skip destination-bounds + negative/overflow checks
File: `native-io/src/lib.rs:5327` (`native_bb_get_bulk`), `:5399` (`native_bb_put_bulk`)

`offset` and `length` are read as `*v as usize` directly from Java `Int` args. The only check is
`if length > remaining` (against the *buffer's* remaining). There is **no check that
`offset + length <= dst.array_length()`** for `get_bulk` (or `src.array_length()` for `put_bulk`),
and **no `offset >= 0` check**: a negative Java `offset` becomes a huge `usize`, and the inner
loop `ctx.set_array_element(dst, offset + i, v)` then writes out of range. Because the GC-level
`set_array_element` is bounds-checked and discards OOB writes (`let _ = ... ` in
`vm_exec.rs:1843`), the result is **silent data loss instead of the `IndexOutOfBoundsException`
the JDK contract requires**, not memory corruption. In a debug build (`overflow-checks=true` by
default), `offset + i` overflows and **panics**. The release profile (`Cargo.toml:168`) has no
`overflow-checks`, so it wraps silently.

### B2 (medium) — `ByteArrayInputStream.read([BII)` skips off/len bounds check
File: `native-io/src/lib.rs:2428` (`native_bais_read_bytes`)

`off`/`len` are taken as `*v as usize` (lines 2438–2443) with no validation against `buf`'s length
and no negative check, unlike the sibling `native_fis_read_bytes`/`native_fos_write_bytes` which
correctly call the bounds guard first. Both the subclass fallback loop (`set_array_element(buf,
off + i, ...)`, line 2480) and the BAIS fast path (line 2510) can index out of range (silently
dropped at the GC layer) or overflow `off + i` (debug panic). JDK requires
`IndexOutOfBoundsException`.

### B3 (low) — Typed-buffer absolute accessors check upper bound but not lower bound / overflow
File: `native-io/src/lib.rs:5485` (`native_bb_get_int_abs`), `:5535` (`native_bb_put_int_abs`),
and the long/short/etc. variants nearby.

The guard is `if index + 4 > cap`. A negative `index` (e.g. `-1`, where `-1 + 4 = 3`) passes when
`cap >= 3`, then `(index + i) as usize` yields a huge index (GC no-op / reads zeros). An `index`
near `i32::MAX` makes `index + 4` overflow to negative, passing `> cap`, and in a debug build the
`index + 4` itself panics. Should be `index < 0 || index > cap - 4` (or a checked add). Same
no-memory-unsafety / wrong-result-or-panic profile as B1/B2.

### B4 (low) — `host_part` mis-splits a bare IPv6 literal without brackets
File: `native-io/src/outbound_policy.rs:158`

`host_part("fe80::1")` (no brackets, no port) falls to `rsplit_once(':')` and returns `"fe80:"` —
not a parseable `IpAddr`, so `default_policy` returns `Allow`. A guest connecting to an
unbracketed link-local IPv6 literal therefore bypasses the default metadata block at the
literal-string layer. The per-resolved-address recheck in `policy_connect` re-brackets
(`SocketAddr::V6 => "[{}]:{}"`) and *does* re-vet, so the end-to-end `policy_connect` path is
still safe; the gap is only if `check_outbound` is called directly with an unbracketed v6 host.
Low impact, but the helper is incorrect for that input shape.

### B5 (low) — `native_process_builder_start` reads working dir from a fixed File slot 0
File: `native-io/src/process.rs:1131`

The directory is read as `ctx.get_field(file_obj, 0)` (synthetic File layout) rather than by name.
A real-JDK `File` may not place the path string at slot 0, so `ProcessBuilder.directory(dir)`
could be silently ignored (spawn in CWD instead of `dir`). Cosmetic vs. the command path itself,
but a behavioral divergence. (The command-list extraction immediately above *was* fixed to read
`size`/`elementData` by name — the directory read was not given the same treatment.)

---

## Vulnerabilities

### V1 (high) — Subprocess spawn is not sandbox-confined; bypasses CWD confinement
File: `native-io/src/process.rs:155` (`spawn_and_wrap`), reachable from `ProcessImpl.create`,
`UNIXProcess.forkAndExec`, `ProcessBuilder.start`.

`spawn_and_wrap` validates only `work_dir` against `validate_path` (lines 184–209). The `program`
argument is passed straight to `Command::new(program)`. Under the certified/untrusted profile
(`CRATONVM_CONFINE_IO` / `set_path_confine_to_cwd(true)`), file *reads/writes* are confined to the
CWD sandbox, but untrusted bytecode can still `new ProcessBuilder("/bin/sh", ...).start()` or
`Runtime.exec("C:\\Windows\\System32\\cmd.exe ...")` and spawn an arbitrary host program that
inherits the JVM's full ambient authority — a far larger escape than the file-path reads the
confinement profile is designed to block. At minimum, when confinement is on, the program path
(and ideally the whole spawn) should be gated by a policy check; the module doc claims the
certified profile "fails closed", but process spawn is left wide open. Note also the work-dir
validation maps `SecurityException → IOException`, but no equivalent gate exists for the
executable itself.

### V2 (low) — Default outbound policy blocks only link-local metadata, not loopback/RFC1918
File: `native-io/src/outbound_policy.rs:144` (`default_policy`)

By design the default policy only denies `169.254.0.0/16` + IPv6 link-local/`fd00:ec2::254`. SSRF
to `127.0.0.1`, `10/8`, `192.168/16`, `::1`, etc. is allowed by default. This is a documented,
deliberate choice (metadata-only) and embedders can install a stricter `set_policy`, so it is a
hardening gap rather than a defect — flagged so the open-source default is understood. The
function-pointer `transmute` round-trip (`store_policy`/`load_policy`) is sound on supported
64-bit targets.

### V3 (low) — `validate_path` accepts arbitrary absolute paths when confinement is OFF (default)
File: `native-io/src/lib.rs:302`

Documented behavior: with confinement off (the default, JDK-faithful), only `..` *segments* and
NUL bytes are rejected; an absolute path or a symlink that escapes is accepted. This is correct
for single-tenant `java -jar` and is loudly documented, but is the single most important thing an
embedder must understand before exposing this crate to untrusted bytecode. Listed for completeness;
not a code defect.

---

## Stubs and Unimplemented

### S1 (policy violation) — Default-on synthetic `DatagramChannel` shims fabricate state
File: `native-io/src/nio_native.rs:1133` (`t16_dc_connect`), `:1073`–`1216` (`t16_dc_*` family),
registered via `register_t16_channel_overrides` from `register_io_natives` (lib.rs:4254) — i.e.
**default build, registered LAST to win over real registrations**.

`t16_dc_connect` invents a `"127.0.0.1:9"` target when the `SocketAddress` can't be decoded,
ignores the result of the underlying `udp.connect()` ("Ignore connect errors … we still mark the
channel as connected"), and unconditionally sets the connected flag (slot 2 = 1). This is exactly
the "fake app behavior" the project's no-synthetic-stubs policy forbids: a Java caller observes a
*connected* DatagramChannel even when no connect succeeded, and a default loopback target it never
asked for. The real DatagramChannel implementation lives in `datagram.rs`; these overrides shadow
it. Recommend gating to synthetic-jdk only or removing in favor of `datagram.rs`.

### S2 (info) — `ProcessHandleImpl` introspection natives return hardcoded "unknown" values
File: `native-io/src/process.rs:952` (`parent0` → -1), `:960` (`getProcessPids0` → 0),
`:971`/`:977` (`Info.initIDs`/`info0` → no-op leaving null/-1 fields).

These return the JDK's documented "unknown" sentinels rather than fabricating data, so they are
acceptable degraded behavior (the JDK code paths handle partial `Info`). Flagged so the open-source
reader knows `ProcessHandle.Info` (command/user/arguments/startTime) is never populated and child-
process enumeration always reports none.

### S3 (info) — `native_jarfile_init_string_verify` accepts but ignores the `verify` flag
File: `native-io/src/zip_real_jar.rs:258`

JAR signature verification is not performed (`JarFile(file, true)` does not validate signatures).
Documented in-code. Acceptable for the current targets (JBoss Modules bootstrap jars are unsigned)
but means signed-jar tamper detection is a no-op — relevant if any consumer relies on it for trust.

### S4 (info) — `Unsafe.freeMemory(addr)` with no size leaks accounting for non-tracked addrs
File: `native-io/src/direct_buffer.rs:636` (`unsafe_free_memory`)

If `addr` is in neither `unsafe_allocs` nor the Cleaner registry, the free is a no-op (correct: a
wrong-layout `dealloc` would be UB). Documented; only leaks when an address was not minted by this
allocator, in which case there is no reservation to refund. Not a defect, noted for completeness.

---

## Performance

### P1 — `generations` map in `direct_buffer.rs` is never pruned of dead addresses
File: `native-io/src/direct_buffer.rs:774` (`generations`), `:784` (`bump_generation`)

`bump_generation` does `g.entry(addr).or_insert(0)` and never removes addr keys. Pool-recycled
addresses reuse their entry, but an address that is `dealloc`'d back to the OS (pool full /
unbucketable) and never recurs leaves a permanent `(addr → gen)` row. Over a long-running process
with high churn of large (>2 MiB, unbucketed) direct buffers this is a slow unbounded growth.
The companion `freed_addrs` set *is* pruned per-address; `generations` should be too (or entries
dropped when an address leaves the pool to the OS).

### P2 — `pollfds.is_empty()` Windows select path busy-polls in 5 ms sleeps
File: `native-io/src/nio_selector.rs:930`

When a Windows selector has no interest fds, `select(timeout)` loops `std::thread::sleep(5ms)`
re-acquiring the selectors RwLock + per-selector Mutex each iteration to check `woken`. For a
long blocking select this is wasted wakeups and lock traffic; a condvar on the wakeup pipe would
be cleaner. Minor.

### P3 — `bb_get_bulk`/`bb_put_bulk` copy element-by-element via virtual dispatch
File: `native-io/src/lib.rs:5352`, `:5424`

These loop `get_array_element`/`set_array_element` per byte (Value boxing + dispatch each), while
the FIS/FOS/zip paths were migrated to the `write_byte_array_from`/`read_byte_array_into` bulk
intrinsics. For large `ByteBuffer.get(byte[])` / `put(byte[])` transfers this is the same hotspot
those audits already fixed elsewhere. Could use the bulk intrinsic when both arrays are byte[].

### P4 — `jar_table`/`process_table`/`net_sockets` global locks serialize unrelated ops
File: `native-io/src/zip_real_jar.rs:50`, `process.rs:90`, `net.rs:176`

The net path already mitigates by cloning the per-socket `Arc` under a brief read-lock then
dropping the map lock (good). The jar table holds a single `Mutex<HashMap>` across `read_to_end`
of an entry, so a large `getInputStream` inflate serializes all other jar operations process-wide.
For concurrent class-loading from multiple jars this can contend. Consider per-handle locking or
cloning the archive handle out first.

---

## Tests

Inventory: **260 inline `#[test]` functions** across all 16 source files; every module has a
`#[cfg(test)]` block. No `tests/` integration directory (the crate's integration tests, e.g.
`wp1_12_process.rs`, live under the consuming `vm` crate). Breakdown:
lib.rs 139, file_channel 17, nio_selector 17, net 15, watch 15, datagram 8, outbound_policy 8,
direct_buffer 7, stream_decoder 6, process 5, socket_channel 5, async_socket 2, nio_native 4,
pipe 4, random_access_file 4, zip_real_jar 4.

Strengths: the pure-logic helpers are very well covered — `decode_modified_utf8`/`encode` has a
full positive+negative suite (truncated sequences, lone/paired surrogates, illegal leading bytes,
C0 80 NUL), the Scanner tokenizer is thoroughly tested, `direct_buffer` covers reserve/unreserve
balance, OOM-at-max, pool round-trip, idempotent cleaner, and bucket classification, and
`outbound_policy` covers IMDS v4/v6, the /16 neighbour, custom-policy override, timeout, and
bracket/port parsing. `random_access_file` even has a path-traversal-under-confinement test.

Gaps (highest value first):
- **No negative/overflow/OOB input tests for the bulk-array natives** (`bb_get_bulk`,
  `bb_put_bulk`, `bais_read_bytes`, `bb_*_int_abs`) — exactly the B1–B3 findings. A test passing
  `offset = -1` or `offset = array.len()` and asserting an exception is missing.
- **No test that subprocess spawn is sandbox-confined** (V1) — `spawn_and_wrap` with a program
  outside the sandbox under confinement is untested.
- `validate_path` confinement: symlink-escape and absolute-path-acceptance behavior is exercised
  only indirectly (one RAF traversal test); a focused `validate_path` unit suite (NUL byte, `..`
  segment vs `foo..bar`, confinement on/off, registered sandbox root) would lock the security
  contract.
- The `t16_dc_*` synthetic shims (S1) have tests that assert the *fabricated* connected flag —
  these tests encode the policy-violating behavior rather than catch it.
- Windows-specific FFI paths (`WSAPoll`, `GetFileInformationByHandle`) and Linux-only
  `copy_file_range`/`sendfile`/`flock` are only reachable on their platform; cross-platform CI
  coverage is structurally limited.

Estimated coverage: **~62%**. Basis: pure helpers and the direct-buffer/outbound-policy/zip
modules are well covered; the giant `lib.rs` has 139 tests but they concentrate on codec/scanner/
path helpers and end-to-end async/watch/udp/selector flows, leaving the ~200 ByteBuffer / stream
bulk natives and their error/bounds paths largely untested. **Does not plausibly reach 85%** —
the untested surface is the bulk-array and error-path code where the B1–B3 bugs live. Most
important missing tests: negative/OOB args on every `*_bulk` and `*_abs` native; confinement on
`spawn_and_wrap`; a dedicated `validate_path` matrix.

---

## Feature Suggestions

1. **Confinement-aware process policy.** Add an outbound-style policy hook for subprocess spawn
   (allow/deny by program path), and under `set_path_confine_to_cwd(true)` validate the executable
   path the same way `work_dir` is validated — closing V1 and making the certified profile truly
   fail-closed for process creation.
2. **Centralize array-range validation.** Route every bulk native (`bb_*_bulk`, `bais_read_bytes`,
   `*_abs`) through the existing `check_array_bounds` so JDK-correct `IndexOutOfBoundsException`
   is thrown uniformly and debug-build overflow panics are eliminated.
3. **Replace the `t16_dc_*` synthetic DatagramChannel shims** with the real `datagram.rs` path (or
   feature-gate them to synthetic-jdk), removing the fabricated connect state per the no-stubs
   policy.
4. **Broaden the default SSRF policy** to optionally include loopback/RFC1918 (opt-in env flag,
   e.g. `CRATONVM_BLOCK_PRIVATE_NETS`) so untrusted-code deployments get a stronger default without
   each embedder reimplementing `set_policy`.
5. **Bound `generations` growth** by dropping per-address rows when a block is returned to the OS
   (pool eviction / unbucketed free), mirroring the `freed_addrs` pruning.
6. **JAR signature verification** behind the existing `verify` flag (or an explicit env gate), so
   `JarFile(f, true)` actually validates — useful for any trust-on-jar consumers.

---

## Files sampled vs fully read

Fully read:
- `lib.rs` — structure mapped (function index), then deep-read of: path validation (302–483),
  `check_array_bounds` + File natives (626–745), FIS/FOS read/write bulk (1124–1201, 1476–1542),
  BB bulk + abs accessors (5290–5560), `bb_state` (4625–4649), modified-UTF8 codec +
  `native_dis_read_utf` (6755–6913), BAIS read_bytes (2428–2515), registration wiring
  (3648–3702, 4240–4256).
- `zip_real_jar.rs` — full.
- `process.rs` — full.
- `direct_buffer.rs` — full.
- `outbound_policy.rs` — full.

Sampled (structure + targeted deep reads of risk regions):
- `net.rs` — function index + `validate_native_range`, `read0`/`write0`, `connect0` policy use.
- `file_channel.rs` — function index + `map0`/`unmap0`, `copy_file_range` unsafe loop.
- `nio_native.rs` — header doc, `fd_from_descriptor`, `read0`/`pread0` raw-memory routing,
  `t16_dc_*` family + registration.
- `nio_selector.rs` — unsafe inventory, `KeyState`/`SelectorState`, Windows `WSAPoll` path,
  confirmed the C27 GC-stable-key fix.
- `random_access_file.rs` — function index + `native_readBytes0` bounds ordering + open0 validation.

Lightly sampled (grep-level, not deep-read): `socket_channel.rs`, `async_socket.rs`,
`datagram.rs`, `watch.rs`, `pipe.rs`, `stream_decoder.rs`, `stream_encoder.rs`, `test_support.rs`.
Risk concentrated in the fully/targeted-read files; these remaining files share the same idioms
(per-handle global tables, `validate_path` on file inputs, GC-checked array access) and showed no
new red flags in grep for unsafe/unwrap/overflow patterns.
