# TestAccessLogValve / TestRewriteValve — connection-level `-1` response failures

**Status:** All three root causes found in this investigation now have fixes
**landed on `dev`** (2026-07-09/10) — Layer 1 (`URL.openConnection()` CCE),
Layer 2 (`ByteBuffer.address`, fixed by a separate session — see
`docs/internal/tomcat-08-07/bytebuffer-address-unset-aioobe.md`), and Layer 3
(`StringReader.read()`, fixed in this session — see
`docs/internal/fixed-suite-bugs/stringreader-read-never-advances-infinite-loop-FIXED.md`).
`TestRewriteValve` (Layer 3's reproducer) was re-run against the Layer 3 fix
and now completes all 121 tests instead of hanging. **`TestAccessLogValve`
(Layer 2's reproducer) has not been independently re-run against the Layer 2
fix in this session** — recommend a follow-up full re-run of both classes
together before moving this doc out of `known-issues/`. **HotSpot:** PASS on
both.

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

### Layer 2 (found, root-caused, FIXED by a separate session): `TestAccessLogValve` — `ByteBuffer.address` never initialized → AIOOBE

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

**FIXED** (2026-07-09, separate session): the live default-release allocator
(`native-builtins/src/lib.rs::alloc_heap_bytebuffer` — neither of the two
locations named above) was found and now seeds `address = 16`. See the
linked doc for the "phantom native" investigation write-up, now resolved.

### Layer 3 (found, root-caused, FIXED in this session): `TestRewriteValve` — `StringReader.read()` never advances → infinite loop

`TestRewriteValve` does **not** reproduce the originally-reported `-1`/`400`
symptom at all anymore. In isolation with a 300s timeout it now **hangs
completely** — zero test progress, not even the first parameterized case
completes. A `--stack-dump-on-timeout 30` capture caught the main thread
spinning tens of millions of times inside
`RewriteValve.parse(Ljava/io/BufferedReader;)V` → `StringReader.read()`.

**FIXED** (2026-07-09/10, this session): root-caused and fixed — see
[`stringreader-read-never-advances-infinite-loop-FIXED.md`](../../internal/fixed-suite-bugs/stringreader-read-never-advances-infinite-loop-FIXED.md).
Short version: the live native (`native-io`'s `register_string_rw_natives`,
`NativeKind::SyntheticStub`) wins dispatch over real bytecode by default
(the `CRATONVM_REAL` differential switch defaults to off), but stored
position/length in flat object field slots that don't exist on real JDK
25's `StringReader` (rewritten to a single `Reader` delegate) — the writes
silently no-op'd, so `read()` always saw `pos == 0`. Fixed with a
GC-stable side table instead of object fields. `TestRewriteValve` now
completes all 121 tests within its timeout (previously: complete hang, zero
progress).

## Recommendation for the next session

1. **Layer 1 fix is landed and verified** — no further action needed there.
2. **Layer 2 fix is landed** (separate session) — recommend an isolated
   `TestAccessLogValve` re-run to confirm the `-1`/`400` symptom is actually
   gone now (not independently re-verified in this session).
3. **Layer 3 fix is landed and verified in this session** —
   `TestRewriteValve` completes all 121 tests.
4. Once both Layer 2 and Layer 3 re-runs are independently confirmed clean
   (or at least free of these three specific symptoms), this doc can move
   to `docs/internal/` per the known-issues convention.

## 2026-07-10: the `SocketWrapperBase.lock` NPE noted above is a known,
## cross-cutting bug — now empirically resolved on current `dev`

The `NullPointerException: ... "this.lock" is null` failure flagged as "a
distinct, previously-undocumented issue" in an earlier version of this
section is neither distinct nor previously undocumented — it's the exact
`SocketWrapperBase`/`SocketProcessorBase` NPE root-caused across two prior
sessions in
`docs/known-issues/tomcat-08-07/swallowabortedupploads-unexpected-socketexception.md`
(root-caused as a stale/lost local variable, `TestSwallowAbortedUploads`;
also hit `TestNonBlockingAPI`/`TestHttp11Processor` per
`nonblockingapi-http11processor-http2limits-bare-assertions.md`).

Re-ran `TestRewriteValve` against a fresh `origin/dev` tip (worktree
`/data/data/wt-rewritevalve-lockcheck`) that includes commit `9cbbc82c`
("Fix Tomcat WebSocket close-delay blockers", landed after this doc's Layer
3 fix above): **zero** `lock is null` occurrences across all 121 tests
(previously: every single connection). `Tests run: 121, Failures: 156`
(down from 229 before `9cbbc82c`) — the 109 tests that previously died with
a connection-level `-1` mostly now get a real HTTP response (a new,
narrower `200`/`400`-vs-`302` behavioral mismatch, not investigated).
Full comparison table and the open question of exactly which of
`9cbbc82c`'s three bundled changes is responsible are in the swallow-upload
doc's own "`TestRewriteValve` independently confirms" section — read that
before doing further work here.

**Practical upshot:** don't write a new known-issue doc for this NPE — it
already has one, and it's evidently no longer reproducing (at least for
these two test classes) on current `dev`. The remaining `TestRewriteValve`
failures (200/400-vs-302, UTF-8 percent-encoding round-trip — see the
swallow-upload doc's comparison table for exact counts) are the actual
next-actionable item for this class, not the old NPE.

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
