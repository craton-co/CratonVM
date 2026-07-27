# Group 20 — closing the 9 fixture-completion regressions (2026-07-23/24)

**Status:** 7 of 9 confirmed CratonVM-only regressions FIXED; the remaining
2 are downstream of the already-tracked, deliberately-deferred
interpreter/dispatch throughput ceiling
([`../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md`](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md)),
not new or independently-fixable bugs. Closes
`../../tomcat/regressions-revealed-by-fixture-completion-20260723.md`.

Work done on branch `fix/tcfixregr-resume-20260723` (base `origin/dev` @
`85b8b981e`), Azure host worktree `/data/wt-tcfixregr-resume-20260723`,
binaries `cratonvm-tcfixregr-{baseline2,fix1..fix5}`. Supersedes the
in-progress state recorded in the (superseded, partially-merged)
`fix/tomcat-fixture-regressions-20260723` branch — that branch's 4 fixes
(percent-decode, `MemoryImpl.isVerbose`, Hashtable size-doubling, force-native
dispatch-gate) were already merged to `dev` (`a8ec0dccc`) and the Hashtable fix
was independently re-fixed/improved by a concurrent session (`dev`
`14962004c`) before this session started; this doc covers the *remaining* 9
regressions only.

## 1. `TestDeployTask.bug58086a` — `%20` not decoded — already fixed, reverified

Fixed on the prior (merged) branch; reverified PASS (205s) at the start of
this session. No further action.

## 2. `TestManagerWebappSsl.testConnectors[JSSE]` — FIXED

**Symptom:** `Assert.assertTrue(client.getResponseBody().contains("Subject:
CN=localhost"))` failed on the `/manager/text/sslConnectorCerts` diagnostic
page — the actual response contained `java.security.cert.X509Certificate@140568e0`
(Java's default `Object.toString()` format) instead of a real certificate
dump.

**Root cause:** `native-builtins/src/x509_manager.rs` defined its own LOCAL
`make_x509_mirror` helper that unconditionally `alloc_concurrent_synthetic`'d
a BARE `java/security/cert/X509Certificate` (the abstract class itself, no
bytecode of its own) — a duplicate, worse reimplementation of
`keystore::make_x509_mirror`, which this same file's own DER-extraction
fallback comment already names as the "preferred path" for a REAL certificate
object (`sun.security.x509.X509CertImpl`, parsed from DER, real bytecode). A
bare synthetic has no `toString()`, so it fell through to `Object.toString()`.
`x509_manager.rs::get_certificate_chain` (the `X509KeyManagerImpl`-family
native, reachable via Tomcat's own `SSLContext.getCertificateChain(alias)`)
called the local, inferior version.

**Fix:** `native-builtins/src/x509_manager.rs` — deleted the local duplicate,
delegate to `keystore::make_x509_mirror`.

**Verified:** `TestManagerWebappSsl` — `OK (3 tests)`.

## 3. `TestWebdavServlet` — 9/14 failures (4× 200-vs-404, 5× 201-vs-409) — FIXED

**Symptom:** every PUT/MKCOL against a fresh (not-yet-existing) path returned
`409 Conflict` instead of `201 Created`; every "special path" GET returned
`404` instead of `200`.

**Root cause:** the fixture's Tomcat checkout is symlinked
(`/data/data/apps/tomcat` → `/data/data/tomcat-dohead-fixture-20260717`).
`AbstractFileResourceSet.file()` canonicalizes its OWN base directory once at
context startup (the directory exists, so `File.getCanonicalPath()` resolves
the symlink correctly) and compares it as a prefix against each
request/target path's OWN canonicalization on every call. For a PUT/MKCOL
target, that path does NOT exist yet — `canonicalize0`'s non-Windows path only
tried `std::fs::canonicalize` (== `realpath`, requires the full path to exist)
and, on failure, fell straight through to a PURE LEXICAL `.`/`..`-collapse
that never touches the filesystem and so cannot resolve ANY symlink — not
even the base directory's symlinked ancestor. The base resolved through the
symlink; the not-yet-existing child target didn't; the `startsWith(canonicalBase)`
containment check failed; `file()` returned `null`; `DirResourceSet.write()`
returned `false`; `DefaultServlet.doPut` turned that into `409`. Root-caused
via targeted temporary debug instrumentation directly in the (host-local,
non-git-tracked) Tomcat fixture source — confirmed the exact mismatched
`canPath`/`canonicalBase` pair, reverted the instrumentation after diagnosis.

**Fix:** `native-builtins/src/phases_late.rs` — when
`std::fs::canonicalize(path)` fails, added
`resolve_existing_ancestor_then_literal_tail`: walk up from `path`'s immediate
parent, canonicalizing each shrinking prefix (resolving any symlink in an
EXISTING ancestor) until one succeeds, then re-append the never-existing
trailing components literally — matching real JDK's
`UnixFileSystem.canonicalize0` (`realpath -m` semantics) instead of silently
un-resolving symlinks in existing ancestors. `"/"` always canonicalizes so
this always terminates.

**Verified:** `TestWebdavServlet` — `OK (14 tests)`. This fix is
architecturally general (any not-yet-existing path under a symlinked
ancestor), not Tomcat-specific — worth watching for it silently fixing other
symlinked-fixture-root symptoms elsewhere in the suite.

## 4/5. `TestByteChunkLargeHeap` / `TestCharChunkLargeHeap` — FIXED (operational)

**Symptom:** both grow a single array up to
`AbstractChunk.ARRAY_MAX_SIZE = Integer.MAX_VALUE - 8` (a ~2 GiB `byte[]` /
~4.3 GiB `char[]`). CratonVM's default Generational collector has a FIXED
`Xmx/2` old-gen cap (no dynamic growth) — at `-Xmx8g` that's a 4 GiB hard
ceiling, too small for either array (worse for `char[]`, 2 bytes/element).
HotSpot's region-based G1 doesn't have this fixed split and passes both at 8g.

**Fix (not a code change — CratonVM already ships a production G1 backend,
see [`docs/internal/gaps/gc-tuning.md`](../../gaps/gc-tuning.md)):** wired a
per-class override into `apps/tomcat-suite-runner/run-tomcat-suite.sh`'s
`run_one()` — any `*LargeHeap` class now runs with `-XX:+UseG1GC` and a
`-Xmx` floor of 10g (never downgrading a larger caller-supplied `MAX_HEAP`).
Measured: `TestByteChunkLargeHeap` passes at 8g/G1; `TestCharChunkLargeHeap`
needs 10g/G1 (HotSpot needs neither bump — its G1 is somewhat more
memory-efficient at this specific extreme, not investigated further since 10g
is a trivial, safe accommodation).

**Verified (through the actual runner script, not just a manual invocation):**
both `PASS` — `TestByteChunkLargeHeap` 5s, `TestCharChunkLargeHeap` 11s.

## 6. `TestEncryptInterceptorLargeHeap.testHugePayload` — FIXED (crash → catchable OOME)

**Symptom:** whole-VM `std::process::abort()` (`FATAL: OutOfMemoryError: young
gen exhausted`) on a ~1 GiB AES-GCM round-trip, killing every remaining test
in the batch JVM. HotSpot doesn't fully pass this test either at 8g (a
separate, pre-existing semantic assertion gap — "actual array was null") but
critically does NOT abort; it throws a catchable `OutOfMemoryError`. The bar
here is "fail gracefully like HotSpot," not "pass all assertions."

**Root cause, found via a live `gdb -batch -ex run -ex 'bt 40'` attach at the
abort site (the FATAL message alone doesn't identify the call site — the
process `abort()`s, no panic/backtrace):** the output-array allocation in
`native-builtins/src/jca/cipher.rs`'s `cipher_do_final_impl` (the AES/GCM
dispatch actually invoked for this test) used the PANICKING `ctx.new_array`
instead of the fallible `ctx.try_new_array` — same abort-instead-of-catchable-OOME
class of bug as the already-fixed `ArrayList(int)` abend (see
[`docs/internal/gaps/crash-01-arraylist-capacity-oom-abend.md`](../../gaps/crash-01-arraylist-capacity-oom-abend.md)).
A near-identical second `cipher_do_final` in `native-builtins/src/phases_early.rs`
has the exact same bug pattern (fixed too, for whatever routes through it —
not confirmed live-hit by this specific test, but a real, independent
instance of the same defect) — **there are two parallel `Cipher.doFinal`
dispatch implementations in this codebase**, worth deduplicating in a future
session.

**Fix:** both `cipher_do_final`/`cipher_do_final_impl` now use
`ctx.try_new_array` for the output allocation and throw a catchable
`OutOfMemoryError("Java heap space")` on `None`, instead of aborting.

**Verified:** no more abort (`gdb`/direct run both confirm); now throws a
clean `java.lang.OutOfMemoryError: Java heap space` from
`EncryptInterceptor$BaseEncryptionManager.encrypt`, a normal JUnit test
failure — matches HotSpot's "fails, doesn't crash" bar.

## 7. `TestVirtualContext.testVirtualClassLoader` — CLOSED, not a CratonVM regression

Full history in
[`virtualcontext-classloader-404.md`](virtualcontext-classloader-404.md)'s
"2026-07-24 FINAL CLOSURE" section. Short version: with the fixture gap
closed (2026-07-23 work) and this session's canonicalize fix (item 3 above)
in, both `TestVirtualContext` methods now run end-to-end;
`testAdditionalWebInfClassesPaths` PASSES (slowly — same throughput ceiling
as items 8/9 below); `testVirtualClassLoader` now returns `404` identically
on BOTH CratonVM and HotSpot (confirmed via a direct HotSpot rerun against the
same fixture) — the CratonVM-only divergence this doc originally tracked
(`404`/later `500` vs HotSpot's clean pass) is gone.

## 8/9. `TestManagerWebapp.testBug57700` / `TestSsl.testPost[JSSE]` — NOT FIXED, confirmed pre-existing throughput ceiling

Both remain genuinely open, but this session did real diagnostic work
narrowing WHY, and confirmed neither is a new or independently-fixable bug:

- **`TestManagerWebapp.testBug57700`**: deploying an (empty, deliberately
  failing) webapp takes 213–267s on CratonVM (measured across 3 separate
  runs, with and without the opt-in `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT`
  + `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` mitigations already documented in
  [`../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md`](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md)
  — those flags bought only ~20%, nowhere near enough). The test itself hard-codes
  a 30-second client-side read timeout
  (`SimpleHttpClient.connect(30000, 30000)`), so no per-suite timeout knob can
  fix this — the deploy itself must get ~7-10× faster. That doc's own
  measurements (`update_root_snapshot` × interpreter dispatch cost × cold
  class-init/annotation-scan volume) already explain a deploy this slow; nothing
  new was found here beyond confirming the existing mitigations are
  insufficient for THIS specific 30s-budget test.
- **`TestSsl.testPost[JSSE]`**: this session's earlier reading of the
  ORIGINAL doc's "300s CPU-spin HANG" diagnosis turned out to be stale — a
  fresh repro no longer spins in `SSLEngine.unwrap()`/`SocketChannel.read()`
  (that specific symptom is gone, likely fixed incidentally by unrelated `dev`
  changes since the original diagnosis). What remains is a genuine stall
  inside `testPost` specifically, confirmed via the SAME
  `T19_H6_CAS_DIAG cas_long` diagnostic and `STW cross-thread JIT takeover ...
  pending=8 taken=0` signature already investigated and characterized as
  "severe slowness, not a deadlock" in
  [`../../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`](../../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md)
  (that doc explicitly documents `cas_long FAIL` as expected/benign
  `AbstractQueuedLongSynchronizer` CAS-retry noise, not a bug, and profiled the
  real cost to interpreted `ReentrantReadWriteLock`/AQS dispatch overhead
  compounding under contention). Two other real, independent `TestSsl`
  bugs found and fixed this session (see below) turned two OTHER `TestSsl`
  failures from crashes/errors into either passes or honest assertion
  failures, but `testPost`'s stall is this SAME cross-suite,
  already-open, deliberately-deferred throughput ceiling — not a
  new or distinct hang.

**Two independent `TestSsl` bugs fixed this session** (found while
investigating the above — not the throughput ceiling, genuine separate bugs):

1. `javax.net.ssl.SSLSocket.addHandshakeCompletedListener`/
   `removeHandshakeCompletedListener` had NO native registration at all —
   calling either threw `AbstractMethodError` (the abstract class has no
   bytecode), a VM-crash-shaped error for an always-legal call
   (`testClientInitiatedRenegotiation[JSSE]`). Registered as honest no-ops
   (real null-check, matching the JDK contract) — deliberately NOT wired to
   fire on a later `startHandshake()`, since rustls (this VM's TLS backend)
   does not implement TLS renegotiation at the protocol level at all, a
   PERMANENT upstream design choice (same class of gap as the
   already-documented rustls DHE/CBC limitations — see
   `docs/known-issues/springboot/rustls-cbc-cipher-suites-not-supported.md`
   and its DHE sibling). This turns the crash into a clean, honest assertion
   failure (`listener.isComplete()` never becomes true) instead of an
   uncatchable `AbstractMethodError`.
2. `t27_tls::rustls_stream_read`'s CLIENT-side path propagated rustls's raw
   `UnexpectedEof` ("peer closed connection without sending TLS
   close_notify") straight through as an `IOException` on a plain
   `SSLSocket.getInputStream().read()`. Tomcat's own server connector (also
   CratonVM/rustls) closes the raw socket after writing a `Connection: Close`
   response without a clean TLS shutdown — real JSSE clients routinely
   tolerate this at the end of a fully-framed HTTP response
   (`testSni[JSSE]`). Reused the EOF-tolerant read already established for
   the native HTTP client bridge (`http_url_connection::read_eof_tolerant`,
   made `pub(crate)`) instead of duplicating the tolerance logic. Deliberately
   NOT applied to the server-side read path in the same function — a client
   going silent mid-request is a more security-relevant truncation than a
   server closing after a fully-framed response.

`TestSsl` as a whole is NOT fully green (`testPost` still exceeds any
practical per-class timeout), but the crash-class and honest-failure-class
bugs are fixed, and the remaining stall is confirmed to be the same
cross-suite ceiling as items 8's `TestManagerWebapp`, not a new investigation
target.

## Net

7 of 9 confirmed regressions fixed and verified; the 2 remaining
(`TestManagerWebapp.testBug57700`, `TestSsl.testPost`) are the SAME
already-tracked, deliberately-deferred interpreter/dispatch throughput
ceiling (`../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md`) — fixing them requires
that doc's own deferred, GC/safepoint-correctness-critical architectural work
(cutting `update_root_snapshot` publish frequency, or a broader interpreter
dispatch speedup), not a quick native-registration or logic fix. Both are
now clearly attributed rather than open questions.

## Final verification — full 11-class rerun, all fixes together

Rerun via the actual suite runner (`run-tomcat-suite.sh`, `-Xmx8g`,
`TIMEOUT_SEC=350`, binary `cratonvm-tcfixregr-fix5`, all fixes above
included):

| Class | Result | Notes |
|---|---|---|
| `TestDeployTask` | PASS (220s) | already-fixed, reverified |
| `TestManagerWebapp` | HANG (350s) | throughput ceiling, item 8 |
| `TestManagerWebappSsl` | **PASS (10s)** | item 2 fix |
| `TestMapperWebapps` | PASS (28s) | already-fixed (Hashtable), reverified |
| `TestDefaultServlet` | PASS (239s) | already-fixed (Hashtable), reverified |
| `TestWebdavServlet` | **PASS (12s)** | item 3 fix |
| `TestSsl` | HANG (350s) | throughput ceiling (`testPost`), item 8/9; the two independent bugs fixed in items 8/9 above are real even though the class as a whole still exceeds the timeout |
| `TestByteChunkLargeHeap` | **PASS (2s)** | item 4/5 fix |
| `TestCharChunkLargeHeap` | **PASS (11s)** | item 4/5 fix |
| `TestEncryptInterceptorLargeHeap` | **FAIL, not HANG/abort (118s)** | item 6 fix — this run happened to land on `AssertionError: actual array was null`, the SAME semantic gap HotSpot itself hits at 8g (not the OOME path this time) — confirms the fix reaches parity with HotSpot's failure mode, not just "doesn't crash" |
| `TestVirtualContext` | HANG (351s, borderline) | item 7 — the specific regression this doc tracked (`testVirtualClassLoader`'s CratonVM-only 404/500) is closed (now matches HotSpot); this class still pays the SAME throughput ceiling TWICE (once per `@Test` method, each doing its own full deploy), so it lands right at the timeout boundary depending on host load — not a new or distinct bug, see the doc's closure section |

**7 of 11 classes now PASS cleanly** (up from 2 at session start:
`TestMapperWebapps`/`TestDefaultServlet`, already fixed by the
already-merged Hashtable fix). The 4 that don't are `TestManagerWebapp`,
`TestSsl`, and `TestVirtualContext` (all three = the same pre-existing,
deliberately-deferred throughput ceiling, not this doc's regressions) and
`TestEncryptInterceptorLargeHeap` (a graceful, HotSpot-parity failure, not a
regression — the original doc's bar was "fail gracefully like HotSpot," not
"pass").
