# CratonVM Module Review — `nb-rest` (native-builtins/src, residual files)

Reviewer: Fable (Opus 4.8). Date: 2026-06-10. Static review only — no builds run.

## Scope

All `native-builtins/src/**/*.rs` NOT owned by the four other native-builtins
agents and NOT in the explicit exclusion list (lib.rs, phases_*, lang_*,
util_*, classloader.rs, serialization.rs, unsafe_natives.rs, atomic_updater.rs,
panama.rs, vector_api.rs, jdk25_concurrency.rs, properties_sidetable.rs,
shared_secrets_bridge.rs, deprecated_*, tests_extracted.rs, crypto*.rs, jca/*,
tls*.rs, t27_tls.rs, t3_impl.rs, x509_manager.rs, security_manager*, net_phase_e.rs,
http2.rs, http_client.rs, servlet.rs, jboss_module_loader.rs, jboss_msc.rs,
wildfly_core.rs, spring_startup_bootstrap.rs, quarkus_staticinit.rs,
vertx_eventloop.rs, xnio_*.rs, logmanager.rs, jmx*.rs, graalvm_compat.rs, aot.rs).

This left ~120 files. Triaged by size/risk: deep-read the parsers, FFI, crypto,
networking, keystore, reflection; sampled the app `*_extras.rs` stub clusters.

## Summary

The high-risk infrastructure (BigInteger limbs, BigInteger intrinsics, JKS/PKCS12
keystore parsing, OS-CSPRNG SecureRandom, libffi Panama bridge, zlib/CRC, real
sockets, DNS) is generally well-engineered: bounds-checked readers, constant-time
MAC comparison, documented `unsafe`, and good differential/unit test coverage.

The chief problems are (a) one outright functional defect in the KeyStore native
surface (`read_string_arg` is a permanent `None` placeholder, so every alias
lookup fails), (b) a cluster of policy-forbidden synthetic stubs that fake app
behavior — `classfile_api.rs` (entire JEP 484 API fabricated), `log4j_extras.rs`
(silent no-op logging pipeline, registered unconditionally and NOT tagged
SyntheticStub), `apps_h2.rs` (`TableFilter.prepare` reimplemented in Rust to mask
a VM optimizer bug), and the WildFly `jboss_extras.rs` module-graph fakery, and
(c) one process-wide lock-contention hazard in the blocking socket accept path.

---

## Bugs

### B1 (HIGH) — KeyStore alias lookups always fail: `read_string_arg` is a no-op placeholder
`native-builtins/src/keystore.rs:926-933`. `read_string_arg(v: &Value) -> Option<&'static str>`
unconditionally returns `None` (its body is `let _ = v; None`). Every alias-taking
`engine*` callback reads the alias via `args.get(1).and_then(read_string_arg).map(...).unwrap_or_default()`
(lines 775, 809, 828, 884, 892, 902, 912), so the alias is **always the empty
string**. Effect: `engineGetKey`, `engineGetCertificate`, `engineGetCertificateChain`,
`engineContainsAlias`, `engineIsKeyEntry`, `engineIsCertificateEntry`, and
`engineGetCreationDate` all resolve against `""` and return null/false unless an
entry happens to be keyed by the empty alias. `engineLoad`/`engineAliases`/`engineSize`
(no alias arg) work, so a keystore loads and enumerates but you can never retrieve a
key or cert by name. The unit tests exercise `load_jks`/`load_pkcs12` directly and
never drive these native callbacks through a ctx, so the defect is uncaught. Fix:
read the alias through `ctx.read_string(obj)` (the comment claims "real reads go
through the longer form below" but no such form exists).

### B2 (MEDIUM) — Charset decoder can spin forever when output buffer is empty
`native-builtins/src/charset.rs:443-453` (`native_decoder_decode`) and 376-388
(encoder). `to_write = decoded.len().min(avail)`; when `avail == 0` (output
ByteBuffer/CharBuffer has no remaining space) `to_write == 0`, `written == 0`,
`proportional_input_consumed_bytes` returns `to_write * bytes.len() / decoded.len()`
= 0, and `set_pos(bb, bpos + 0)` leaves the input position unchanged. The result is
`OVERFLOW` with zero progress on both buffers — a JDK caller that does not drain the
output (or whose output is genuinely full) loops calling `decode` forever. The JDK
contract is that OVERFLOW means "make room and retry"; a correct impl guarantees ≥1
unit of forward progress once room exists, but the zero-room case here is a live-lock
trap for any caller that mis-sizes the output buffer.

### B3 (MEDIUM) — `Charset.decode(ByteBuffer)` silently truncates real-JDK buffers > 256 bytes
`native-builtins/src/charset.rs:526-540`. When `buf_state` yields an empty range
(real-JDK HeapByteBuffer whose `hb`/pos/limit don't match the probed slots), the
fallback scans field slots 0..=8 for the first `byte[]` and reads at most
`len.min(256)` bytes ("Heuristic: cap at 256"). A real-JDK ByteBuffer carrying > 256
bytes routed through `Charset.decode(ByteBuffer)` is silently truncated, producing a
short/wrong String with no error. The Tomcat 1-byte-probe path is fine; any bulk
decode that hits this fallback is corrupted.

### B4 (MEDIUM) — `defl_deflate_stub` reports no-progress for direct-buffer deflate (potential hang)
`native-builtins/src/zip_real.rs:445-447,600-602`. The three direct-ByteBuffer
`deflateBytesBuffer`/`deflateBufferBytes`/`deflateBufferBuffer` natives return
packed `Long(0)` (zero input consumed, zero output, not finished). The symmetric
*inflate* direct-buffer natives were deliberately changed to raise
`NotImplemented` precisely because returning Long(0) makes the JDK spin forever
(see the round-6 comment at lines 271-294). The deflate side was not given the same
treatment, so a direct-buffer Deflater user hangs instead of failing loudly. Mirror
the inflate fix.

### B5 (LOW) — `biginteger_intrinsics` self-aliasing check via `as_ptr()` may be unreliable
`native-builtins/src/biginteger_intrinsics.rs:531,573`. The read-after-write hazard
guard uses `std::ptr::eq(new_arr.as_ptr(), old_arr.as_ptr())` to detect when the
newArr and oldArr ObjectRefs are the same heap array. If `ObjectRef::as_ptr()` is a
handle/index rather than the array's data pointer (common in handle-based VMs), two
distinct refs to the same logical array (or a relocated array) could compare unequal,
defeating the snapshot. The fallback (re-read newArr) is still memory-safe, so worst
case is a subtly wrong shift result for an aliased in-place shift, not UB. Low because
the `shiftLeftImplWorker(a,a,0,n,len)` call site is the only aliasing case and it is
covered by the snapshot when `ptr::eq` *does* fire.

---

## Vulnerabilities

### V1 (MEDIUM) — Panama FFI performs unvalidated raw read from a Java-controlled address
`native-builtins/src/panama_libffi.rs:459-473` (`marshal_arg`, struct/union/sequence
by-value). `addr = segment_address(ctx, seg)` reads a `Long` out of the
MemorySegment's field 0 + field 5 (both Java-writable), then does
`copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), total)` with `total` up
to `MAX_STRUCT_BYTES` (4096). There is no check that `addr` points into a live arena
the VM owns. A classfile that reaches a Panama downcall with a struct-by-value arg
can therefore read up to 4 KiB from any process address (arbitrary-read primitive).
Panama FFI is privileged by design (`--enable-native-access`), so this matches the
JDK's trust model, but the VM does no `enableNativeAccess`/arena-bounds gating that
HotSpot does. Hardening: validate the segment's base against the registered off-heap
arena store before the raw copy.

### V2 (LOW) — `prep_cif_var` re-preps an already-prepped libffi CIF using its internals
`native-builtins/src/panama_libffi.rs:586-613`. The variadic path builds a normal
`Cif::new`, then drops to `libffi::low::prep_cif_var` re-prepping the same `ffi_cif`
using its own `arg_types`/`rtype` pointers. This relies on libffi-internal invariants
(that re-prep is idempotent and the backing storage stays owned by `cif_obj`) not
guaranteed by the safe `middle` API; a libffi version bump could turn this into UB.
Low because libffi's ABI here is stable in practice.

### V3 (LOW) — Boxed `Cif` cache leaks (finalizer not wired)
`native-builtins/src/panama_libffi.rs:633-643,703-709`. `box_cif_to_u64` /
`free_cached_cif` provide a manual lifecycle but the comment states "the Panama
finalizer path isn't yet wired" — so every cached `Cif` leaks until process exit.
Bounded (one per native function per run) but it is an unfinished cleanup path.

### V4 (LOW) — Classpath JAR zip read trusts entry size header (zip-bomb DoS)
`native-builtins/src/jdbc.rs:157-164`. `read_url_bytes` does
`Vec::with_capacity(f.size() as usize)` then `read_to_end` on a ZIP entry from a
classpath JAR. A maliciously crafted JAR with a huge declared size triggers a large
up-front allocation. Memory-safe but a DoS vector. Classpath JARs are semi-trusted,
so severity is low.

### V5 (LOW) — Blocking DNS/`to_socket_addrs` inside native code with no timeout
`native-builtins/src/plain_socket.rs:250` (`read_inet_addr` hostname fallback),
`native-builtins/src/inet_address.rs:89` (`resolve_addrs`). These call the platform
`getaddrinfo` synchronously with no timeout while holding the calling Java thread.
A slow/hostile DNS server stalls the thread indefinitely. Matches JDK behavior but
worth noting for daemon workloads.

---

## Stubs and Unimplemented (policy: synthetic stubs that fake app behavior are forbidden)

### S1 (HIGH) — `log4j_extras.rs`: silent no-op logging pipeline, unconditionally registered, NOT tagged SyntheticStub
`native-builtins/src/log4j_extras.rs`. Intercepts `LogManager.getLogger`/`getContext`/
`getFactory` (86 registrations) to hand back synthetic `SimpleLogger`/`SimpleLoggerContext`/
`Log4jContextFactory` objects whose logging methods are no-ops — "any LogManager caller
observes a complete, non-null logging pipeline that silently discards every record"
(header, lines 42-45). This fakes app behavior (logging silently dropped) and is wired
**unconditionally** from `register_essential_natives` with **no `NativeKind::SyntheticStub`
tag** (`register_log4j_stubs`, line 365, sets no category), so the strict
`drop_synthetic_stubs` build cannot remove it. Root cause is that `LogManager.<clinit>`
is no-op'd elsewhere (leaving `factory` null); the real fix is the LambdaMetafactory/
invokedynamic clinit path, not a per-framework shim.

### S2 (HIGH) — `apps_h2.rs`: `org.h2.table.TableFilter.prepare()V` reimplemented in Rust to mask a VM optimizer bug
`native-builtins/src/apps_h2.rs:108-297` (`register_h2_table_filter_prepare` /
`table_filter_prepare_on`). This natively reimplements H2 application bytecode
(walking indexConditions, pruning, recursing nestedJoin/join, optimizing conditions)
because "our Optimizer.optimize pipeline leaves index unset... which trips
TableFilter.prepare pc=52 with NullPointerException." Per the no-stubs policy the
real fix belongs in the VM optimizer, not a per-app native shim. Tagged SyntheticStub
(at least disclosed). The thread-state overrides in the same file are dead code
(`#[allow(dead_code)]`, disabled).

### S3 (HIGH) — `classfile_api.rs`: entire JEP 484 Class-File API is fabricated
`native-builtins/src/classfile_api.rs`. `ClassFile.parse([B)` ignores the bytes and
returns a fixed ClassModel (major=69, public, this_class=1, super=3, 0 fields, lines
79-100); `build`/`buildTo`/`buildModule`/`transformClass` return empty `byte[]`;
every Model/Builder accessor returns canned values. No real class-file parsing or
generation happens. Correctly self-tagged `NativeKind::SyntheticStub` with a FLAGGED
comment (line 774-782), and tests only assert registration, not behavior. Any app
using JEP 484 reflection or bytecode generation gets wrong answers.

### S4 (MEDIUM) — `jboss_extras.rs`: WildFly module-graph fakery (live, gated)
`native-builtins/src/jboss_extras.rs:403-655`. Live registrations override real
jboss-modules bytecode: `Module.getBootModuleLoader`/`getCallerModuleLoader` return a
synthetic LocalModuleLoader; `getDependencies`/`getResourceLoaders`/`ModuleSpec.getDependencies`
return empty arrays; `getPaths`/`getExportedPaths`/`forClass`/`preloadModule`/
`findLoadedModuleLocal`/`loadClassFromCallerModuleLoader` return null;
`installMBeanServer*`/`setModuleLogger`/`ModuleLogger.<init>` are no-ops. The empty-array
returns substitute fabricated empty module data for the real module graph (faking app
state). Gated by `CRATONVM_SKIP_JBOSS_PLUMBING=1`; not tagged SyntheticStub. Disclosed
but still app-faking.

### S5 (LOW) — `zip_real.rs` direct-buffer inflate `NotImplemented`; CRC32 direct-buffer returns CRC unchanged
`native-builtins/src/zip_real.rs:295-321` (inflate direct-buffer → NotImplemented, by
design, fail-loud), and `crc32_update_byte_buffer_0` (line 550-562) returns the input
CRC unchanged for direct buffers — "a stable (but technically wrong) checksum." The
latter silently produces a wrong CRC for any `CRC32.update(ByteBuffer)` direct-buffer
caller; only the array path is correct.

### S6 (LOW) — Misc documented no-op natives
- `zip_real.rs`: `infl_set_dictionary`/`defl_set_dictionary*` are no-ops (preset
  dictionaries unsupported); `getAdler` returns 1 (placeholder).
- `plain_socket.rs:875-876,904-905`: `initProto`/`init` no-ops (UnsatisfiedLinkError
  avoidance, legitimate).
- `securerandom.rs`: `SecureRandom.getInstance(algorithm)` ignores the requested
  algorithm and always returns an OS-CSPRNG-backed instance — a deterministic SHA1PRNG
  requested for reproducible test vectors will be non-reproducible (documented choice).
- The bulk of `*_extras.rs` (hbase, jetty, eclipse, felix, glassfish, nexus, jenkins,
  cassandra, solr, etc.) have been correctly **gutted to disabled no-ops** per the
  no-stubs policy — good cleanup, not flagged.

---

## Performance

### P1 (HIGH) — Blocking `socket.accept()` holds a process-wide read lock for its entire duration
`native-builtins/src/plain_socket.rs:438-443`. In the no-SO_TIMEOUT branch,
`socket_accept` takes `registry().read()` and calls the blocking `s.socket.accept()`
while still holding the guard. Every other socket operation needs `registry().write()`
(`with_socket`), so any single blocking accept on a daemon server serializes ALL
socket I/O process-wide until a connection arrives. The SO_TIMEOUT branch correctly
drops the guard around the poll/sleep; the indefinite-blocking branch does not. This
is a denial-of-service-grade contention bug for any multi-socket server. Fix: clone/take
the socket handle out under a short lock, or release before the blocking accept.

### P2 (MEDIUM) — Per-element array marshalling on hot zlib/CRC/charset paths
`zip_real.rs` `read_byte_array`/`write_byte_array`, `charset.rs` `read_byte_array`/
`read_char_array`/`write_*`, `biginteger_intrinsics.rs` `read_int_array`/`write_int_array`.
Each calls `ctx.get_array_element`/`set_array_element` once per element through the
dyn-trait boundary. For inflate/deflate/CRC of large JAR entries and BigInteger ops on
kilobit operands, this is O(n) virtual dispatches where a bulk `copy_to_native`/
`copy_from_native` slice API would be one call. CRC32 of multi-MB ZIP entries is the
worst case.

### P3 (LOW) — `socket_accept` SO_TIMEOUT busy-poll sleeps 5 ms per iteration
`native-builtins/src/plain_socket.rs:399-420`. Busy-poll with `set_nonblocking(true)`
+ `thread::sleep(5ms)` adds up to 5 ms latency per accept and burns a wakeup every
5 ms. Acceptable but a real edge-trigger (or shorter adaptive sleep) would be better.

### P4 (LOW) — `read_password` / `hex_lower` per-char `format!` allocation
`keystore.rs:534-540` (`hex_lower` does `format!("{:02x}")` per byte) and the
per-element password projection. Cold path (keystore load), negligible.

### P5 (LOW) — `find_subseq` is O(hay·needle) naive substring scan
`keystore.rs:1012-1022` (`detect_algo_idx`). Fine for short DER, but a naive scan.

---

## Tests

Coverage is bimodal. The pure-algorithm cores are *excellently* tested; the
heap-facing native callbacks and the app-stub modules are barely tested.

Strong coverage (differential or thorough unit tests):
- `bigint.rs` — 7 differential tests vs the decimal reference across add/sub/mul/
  div/mod/modpow/primality/bitops/shifts with randomized + edge operands. ~95%.
- `biginteger_intrinsics.rs` — 20 tests including naive-mul ground-truth up to 2048-bit
  and heap-dispatch + bounds-rejection cases. ~90%.
- `securerandom.rs` — LCG constants, scramble, OS entropy, distribution, seed table. ~80%.
- `keystore.rs` — JKS/PKCS12 round-trip, MAC mismatch, wrong password, truncation,
  format detection, algo detection. BUT zero coverage of the `engine_*` native
  callbacks (which is exactly why B1 slipped through). ~70% of the parser, ~5% of the
  native surface.
- `plain_socket.rs` — registry round-trip, options, end-to-end loopback accept,
  close/error. No test for the lock-contention path (P1). ~65%.
- `zip_real.rs` — inflate round-trip, CRC32 vectors + JDK contract, pack layout. No
  direct-buffer hang test. ~70%.
- `panama_libffi.rs` — only primitive-type translation + a trivial slot round-trip.
  No marshalling, no struct, no variadic CIF, no actual downcall, no upcall context.
  The riskiest unsafe code is essentially untested. ~25%.

Weak/absent coverage:
- `charset.rs` — 4 tests, all on the standalone helper functions; the
  encoder/decoder *native callbacks* (where B2/B3 live) and the buffer-state
  dual-layout logic are untested. ~30%.
- `classfile_api.rs`, `log4j_extras.rs`, `jboss_extras.rs`, `apps_h2.rs` — only
  "registration doesn't panic" smoke tests; no behavioral assertions.
- `lang_reflect.rs` — one smoke test (registry size); the canAccess/getParameters/
  toString/depth-guard logic is untested here (some coverage in
  tests/wp2_1_reflect.rs, outside scope).

Overall best estimate: ~52%. Does NOT plausibly reach 85%. The biggest gaps that
would move the needle: (1) keystore `engine_*` callback tests through a mock ctx
(would catch B1); (2) charset encoder/decoder OVERFLOW/zero-room and real-JDK
buffer-layout tests (B2/B3); (3) Panama marshal_arg/struct/variadic and a smoke
downcall; (4) zip direct-buffer deflate behavior (B4); (5) the socket
blocking-accept contention scenario (P1).

---

## Feature Suggestions

1. Add a bulk array-copy API to `NativeContext` (`copy_array_to_slice` /
   `copy_slice_to_array`) and route zip/CRC/charset/BigInteger marshalling through
   it — fixes P2 and simplifies a lot of per-element loops.
2. Implement direct-ByteBuffer support for Inflater/Deflater/CRC32 by exposing the
   off-heap arena store (the same store panama uses) so `infl_*_buffer*` and
   `crc32_update_byte_buffer_0` can read/write real direct-buffer memory instead of
   NotImplemented / wrong-CRC stubs (S5).
3. Gate Panama downcalls behind an arena-bounds check and an `enableNativeAccess`
   allowlist so V1 can't be reached from an un-blessed module — closer to HotSpot's
   FFM access model.
4. Replace the `apps_h2.rs` / `log4j_extras.rs` / `jboss_extras.rs` shims with the
   underlying VM fixes (H2: optimizer leaving `TableFilter.index` null; log4j/wildfly:
   the LambdaMetafactory/invokedynamic clinit path that forces the `<clinit>` no-ops),
   then delete the per-app stubs.
5. Make the legacy blocking `socket.accept()` release the registry lock for its
   duration (P1) and add a real `SocketImpl` getInputStream/getOutputStream read/write
   surface so the plain-socket path is self-contained rather than depending on net_phase_e.
6. Real JKS/PKCS12 key decryption hookup: `engine_get_key` currently hands back a
   synthetic PrivateKey mirror keyed by a composite id; wire it to a real
   `java.security.PrivateKey` via the JCA KeyFactory so consumers can use the key
   object directly rather than only via the TLS side-channel.

---

## Files sampled vs fully read

Fully read (line by line):
- zip_real.rs, bigint.rs, biginteger_intrinsics.rs, charset.rs, classfile_api.rs,
  lang_reflect.rs, plain_socket.rs, keystore.rs, securerandom.rs, jdbc.rs,
  apps_h2.rs, panama_libffi.rs, hbase_extras.rs, jetty_extras.rs, jboss_extras.rs,
  log4j_extras.rs (header + registration), service_loader.rs (first ~160 lines),
  generics.rs (first ~100 lines), inet_address.rs (first ~120 lines).

Sampled (structure grep + targeted reads, not exhaustive):
- The remaining `*_extras.rs` (concurrent_extras, eclipse_extras, and the ~30 tiny
  disabled stubs) via register-count and disable-pattern greps.
- File-wide greps for unimplemented!/todo!/panic!/NotImplemented/unsafe across the
  whole scope (panics confirmed test-only; NotImplemented mostly in excluded files).

Not opened (lower risk / out of remaining budget): infinispan_local.rs,
quarkus_arc.rs, agroal_pool.rs, ironjacamar_pool.rs, wildfly_security.rs,
wildfly_undertow.rs, wildfly_naming.rs, wildfly_datasources_tx.rs,
jboss_jdkspecific.rs, jboss_logmanager.rs, jboss_module_xml.rs,
jboss_resource_loader.rs, cds.rs, aot_pipeline.rs, craton_gpu.rs, stamped_lock.rs,
stack_walker.rs, lang_stackwalker.rs, reference.rs, streams.rs, charset engine
(in native-api crate), locale_resources/bootstrap, proxy_selector.rs,
http_url_connection.rs (beyond header), classloader_real.rs, lookup_define.rs,
boot_loader.rs, system_bootstrap.rs, scheduled_pump.rs, uncaught_handlers.rs,
zip_crc32c.rs, bc_aes/bc_chacha/bc_newhope*, sunec_*.rs, unsafe_jdk25.rs,
classloader_value_sidetable.rs, jdk25_language/patterns.rs, intrinsics/* (mostly
owned/covered by lang_* agents), cglib_enhancer/extras, bytebuddy_extras,
letsgo_compat, demo/arduino/bluej/mindustry/freemind/netbeans/jedit/jdownloader_extras.
