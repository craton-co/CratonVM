# TestAccessLogValve / TestRewriteValve — connection-level `-1` response failures

**Status:** OPEN — re-verified in isolation, both are CONFIRMED GENUINE bugs
(not contention). One root cause found along the way has been **FIXED**; two
newly-discovered, deeper root causes remain **OPEN** with full diagnosis but
no landed fix (see linked docs). **HotSpot:** PASS on both.

## 2026-07-09 re-verification (Azure host, dev @ `7e382917`, `-Parallel 1`)

Per the 2026-07-09 recommendation, both classes were re-run **individually**
(`-Parallel 1` equivalent — single process, no other load), real JDK, JIT on,
generous timeout (300s), on an otherwise-idle host (`uptime` load average
~1.7 on 16 cores). Worktree `/data/data/wt-valveconn-reverify` (branch
`investigate/valveconn-reverify-20260709`, off `dev`).

**Both classes reproduce the failure 100% of the time in complete
isolation.** This is not contention/noise — it's a deterministic bug.

### Layer 1 (found and FIXED): `URL.openConnection()` ClassCastException

Before touching anything, an isolated `TestAccessLogValve` run showed **141
`ClassCastException: java.net.URLConnection cannot be cast to
java.net.HttpURLConnection`** failures (at
`TomcatBaseTest.methodUrl`) — not the originally-reported `-1` symptom at
all. This is a severe regression, unrelated to logging or rewrite rules: **any**
code doing `(HttpURLConnection) url.openConnection()` on a `java.net.URL`
built via real bytecode (`new URL(String)` or `URI.toURL()`) was broken.

**Root cause:** `native-builtins/src/net_phase_e.rs`'s `URL.openConnection()`
native picks its return-carrier class by sniffing the URL's external form:
```rust
let s5 = read_field_string_or(ctx, this, 5, "");
let s = if s5.contains(':') { s5 } else { /* fall back to toExternalForm() */ };
```
Field 5 of a **real-JDK** `java.net.URL` is the `authority` (`host:port`),
which — trivially — always contains a `:`. That short-circuits the
`toExternalForm()` fallback, so `ext` becomes the bare authority
(`"127.0.0.1:8080"`), which matches neither the `jar:` nor `http(s)://`
prefix check, so the method falls through to the generic `URLConnection`
carrier instead of `HttpURLConnection`. The file already has the correct
discriminator for exactly this synthetic-vs-real ambiguity
(`field5_is_full_url`, used by the neighboring `openStream()`/
`toExternalForm()` paths) — `openConnection()` just wasn't using it.

Traced to commit `b0dd2e72` (2026-07-07, "Fix ResourceTests HTTP URL
failures") which started populating field 5 with the real authority for
user-info support; that fix broke this unrelated reuse of the same field.

**FIX** (commit on branch `investigate/valveconn-reverify-20260709`):
`openConnection()` now uses `field5_is_full_url(&s5)` instead of the naive
`s5.contains(':')`. Verified byte-identical to HotSpot afterward — CratonVM
now returns a genuine `sun.net.www.protocol.http.HttpURLConnection` for
real http(s) URLs (via the pre-existing `url_custom_handler_connection`
real-handler delegation), matching HotSpot's own `getClass().getName()`
exactly.

### Layer 2 (found, root-caused, NOT fixed): `TestAccessLogValve` — `ByteBuffer.address` never initialized → AIOOBE

With Layer 1 fixed, the ClassCastException is gone, but **every** connection
in `TestAccessLogValve` still fails, now surfacing the **original** `-1`
symptom (94 tests run, 141 failures, all `expected:<200> but was:<-1>`) —
this confirms the original known-issue report was real, just partially
masked by the (now-fixed) more-severe Layer 1 bug.

Root cause fully diagnosed with a minimal, Tomcat-independent repro — see
[`bytebuffer-address-unset-aioobe.md`](../../internal/tomcat-08-07/bytebuffer-address-unset-aioobe.md).
Short version: any `ByteBuffer` returned by CratonVM's synthetic
`ByteBuffer.allocate()` carrier did not have its `java.nio.Buffer.address`
field populated. Real bulk-transfer bytecode
(`ByteBuffer.get(byte[])` → `getArray` → `ScopedMemoryAccess.copyMemory`)
computes a source offset of `address + position`, which comes out `0` instead
of the correct `16` (`ARRAY_BYTE_BASE_OFFSET`) — below the destination
array's real bounds check — throwing `ArrayIndexOutOfBoundsException` on the
**very first** bulk read. This kills the connection outright: the socket
accepts and even receives bytes, but the buffer-fill/parse path throws before
a response is ever written, so the client sees a dead connection (`-1`).

**Not yet fixed**: two natural fix locations
(`servlet.rs::bb_write_hb`, `native-io/src/lib.rs::alloc_byte_buffer`) were
patched with the (correct, already-proven-elsewhere-in `charset.rs`) missing
`address = 16` field write, but **neither is actually on the live dispatch
path** for `ByteBuffer.allocate()` in a real-JDK-mode, default-feature CLI
build — confirmed via an *unconditional* `eprintln!` inside each registered
closure that never fired. See the linked doc for the "phantom native"
investigation and hand-off notes.

### Layer 3 (found, root-caused, NOT fixed, unrelated to sockets): `TestRewriteValve` — `StringReader.read()` never advances → infinite loop

`TestRewriteValve` does **not** reproduce the originally-reported `-1`/`400`
symptom at all anymore. In isolation with a 300s timeout it now **hangs
completely** — zero test progress, not even the first parameterized case
completes. A `--stack-dump-on-timeout 30` capture caught the main thread
spinning tens of millions of times inside
`RewriteValve.parse(Ljava/io/BufferedReader;)V` → `StringReader.read()`.

Root cause fully diagnosed — see
[`stringreader-read-never-advances-infinite-loop.md`](stringreader-read-never-advances-infinite-loop.md).
Short version: `StringReader.read()` unconditionally returns the buffer's
**first** character forever; the position never advances, so any
`BufferedReader`/`StringReader`-based text parsing loop (config parsing,
`RewriteValve`'s rule-file parser here) spins forever. Same "phantom native"
pattern as Layer 2 — the two candidate registered implementations
(`phases_early.rs`, correctly-shaped 3-field synthetic version in
`native-io/src/lib.rs`) are both provably not on the live dispatch path
(confirmed via the same unconditional-`eprintln!` technique), and simple,
targeted probes rule out the VM's general field/post-increment/synchronized
mechanics as the culprit (they all work correctly in isolation) — the bug is
specific to however `java.io.StringReader` actually gets resolved.

## Recommendation for the next session

1. **Layer 1 fix is landed and verified** — no further action needed there.
2. Layers 2 and 3 share a **systemic, unresolved mystery**: a method that
   `classloading/src/class_manager.rs` marks `NATIVE` via its synthetic
   classfile-patching mechanism (`java/nio/ByteBuffer.allocate`,
   `java/io/StringReader.read`) resolves to *something* at runtime, but not
   to any of the plausibly-matching registrations found by exhaustive `grep`
   across `native-builtins`/`native-io` (each confirmed dead via an
   unconditional debug print that never fires). Before attempting another
   fix at the Rust-native-registration layer, first find the actual runtime
   dispatch site — likely requires instrumenting
   `vm/src/runtime/interpreter.rs`'s native-dispatch path itself (around the
   `shared.native_methods.find(...)` call sites, ~15 of them) or checking
   for a second, independent `NativeMethodRegistry` instance / resolution
   cache that isn't `shared.native_methods`.
3. Once the live dispatch site is found, both fixes are simple one-line
   field-initialization / index-arithmetic corrections — the hard part is
   locating where to apply them, not the fix itself.

## Reproduction

```powershell
# Azure host — worktree already exists, left in place for follow-up:
# /data/data/wt-valveconn-reverify (branch investigate/valveconn-reverify-20260709)
cd /data/data/apps/tomcat
EXE=/data/data/wt-valveconn-reverify/target/release/cratonvm  # rebuild after pulling
CP=$(cat .suite/cp.txt)
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 timeout 300 $EXE \
  --java-home /home/victor/jdk25 --Xmx 2g -Dfile.encoding=UTF-8 \
  -Djava.net.preferIPv4Stack=true -Dtomcat.test.basedir=output/build \
  -Dtomcat.test.temp=output/test-tmp -Dtomcat.test.tomcatbuild=output/build \
  -Dtomcat.test.relaxTiming=true --add-opens java.base/java.lang=ALL-UNNAMED \
  --add-opens java.base/java.io=ALL-UNNAMED --add-opens java.base/java.util=ALL-UNNAMED \
  --add-opens java.base/java.util.concurrent=ALL-UNNAMED -c "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.valves.TestAccessLogValve
  # or org.apache.catalina.valves.rewrite.TestRewriteValve
```

Minimal, Tomcat-independent repros for Layers 2 and 3 are in the linked docs.
