# TestAccessLogValve / TestRewriteValve — connection-level `-1` response failures

**Status:** OPEN. Six layered root causes found across this investigation.
Five are FIXED and landed on `dev`: Layer 1 (`URL.openConnection()` CCE),
Layer 2 (`ByteBuffer.address`), Layer 3 (`StringReader.read()`), a
cross-cutting fourth (`SocketWrapperBase.lock`, tracked in the
swallow-uploads doc), and a **fifth, found and fixed 2026-07-10**: a JIT
miscompile of `ConcurrentLinkedQueue`'s allocate-then-CAS hot methods
(`offer`/`tryCasSuccessor`), which crashed `TestAccessLogValve` with a
message-less `NullPointerException` **during JUnit test discovery, before
any HTTP request ever happened** — a regression that appeared on `dev`
sometime between this doc's 2026-07-10 morning re-run and the afternoon
follow-up, and which fully explained the "no server-side exception is ever
logged" mystery from the earlier fifth-cause hunt below (the crash was never
server-side at all — it was client-side JUnit machinery blowing up before a
server ever started). See the "2026-07-10 (afternoon): fifth cause found —
JIT ConcurrentLinkedQueue miscompile" section. **`TestRewriteValve` improved
dramatically** (was: 0/121 complete hang → 80/121 → now **110/121 pass**,
remaining 11 are UTF-8/percent-encoding query-string residuals, a narrower
and different issue than the `302`-vs-`200`/`400` rewrite-rule bug
previously blamed for the bulk of the 41). **`TestAccessLogValve` now gets
past test discovery and runs real HTTP-based test cases for the first time**
(was: 0/94, immediate crash) but hits a **sixth cause — a SIGSEGV around test
#8** on an `http-nio` worker thread, inside JIT-compiled Tomcat NIO code.
This is confirmed (register-signature match) to be the same already-tracked,
currently-OPEN "register-invisible JIT root" bug family documented in
`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`
and `swallowabortedupploads-unexpected-socketexception.md` — deep JIT/GC
root-precision infrastructure work (precise oop maps / shadow stack), not a
skip-list-sized fix, and deliberately NOT attempted here. Keep this doc in
`known-issues/` until the sixth cause is fixed (or until whoever owns the
precise-JIT-maps/shadow-stack roadmap item lands a fix and this can be
re-verified). **HotSpot:** PASS on both.

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
2. **Layer 2 fix is landed**, but did NOT resolve `TestAccessLogValve` —
   see the 2026-07-10 re-run section below for a fifth, still-open cause.
3. **Layer 3 fix is landed and verified** — `TestRewriteValve` goes from a
   complete hang to 80/121 passing.
4. **`SocketWrapperBase.lock` fix (commit `9cbbc82c`) is landed and
   verified** — zero `lock is null` NPEs in either class now.
5. Do NOT move this doc to `docs/internal/` yet — `TestAccessLogValve`
   still fails 94/94 for an undiagnosed fifth reason. Follow the candidate
   next steps in the 2026-07-10 section below.

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

## 2026-07-10: `TestAccessLogValve` re-run against ALL four fixes together — still 94/94 fail at `-1`. A fourth, distinct, unexplained cause remains.

Did the follow-up re-run this doc's "Recommendation" section asked for.
Rebuilt worktree `/data/data/wt-valveconn-reverify` fully current with
`origin/dev` (includes Layer 1/2/3 fixes above AND commit `9cbbc82c`'s
`SocketWrapperBase.lock` fix). Restored the harness's missing
`output/build/conf/logging.properties` first (a separate, non-VM fixture
gap on this Azure copy — was causing a `FileNotFoundException` on every
test's teardown and inflating the failure count; unrelated to any of this
doc's bugs, just noise on top of them).

**Result: `Tests run: 94, Failures: 94`, every single failure still
`AssertionError: expected:<200> but was:<-1>`.** `grep -c 'lock is null'`
on the full run: **0** — so this is NOT the `SocketWrapperBase.lock` NPE
either. No exception of any kind appears anywhere in the server-side log
for any of the 94 tests (checked with `org.apache.tomcat.util.net`,
`org.apache.coyote`, and `org.apache.tomcat.util.http.parser.Cookie` bumped
to `FINEST`). The first test does get far enough to log a real, expected
`INFO [org.apache.tomcat.util.http.parser.Cookie] A cookie header was
received ... that contained an invalid cookie` — meaning the request headers
(including the Cookie/custom headers `TestAccessLogValve.test()` sets) DO
reach and get parsed by the server — yet the client still sees `-1`. Total
wall time (~1m53s for 94 tests) rules out each test blocking for the full
300s client read timeout; something fails within roughly a second per test,
just not in a way any of this doc's four already-fixed bugs, or any logged
exception, explains.

**Ruled out as the same-shaped bug** (each independently confirmed against
this exact binary):
- `ByteBuffer.address`/AIOOBE (Layer 2) — a direct minimal repro
  (`SocketChannel.read` into an `allocate()`d buffer, `get(byte[])` on it)
  against both a trusted external Python client/server AND a pure-CratonVM
  client+server round trip both return `200`/correct bytes now.
- `StringReader.read()` (Layer 3) — direct repro terminates correctly
  (`h,e,l,l,o,-1`), no longer relevant to this class anyway (`AccessLogValve`
  doesn't use `StringReader`).
- `SocketWrapperBase.lock` — `grep -c 'lock is null'` = 0.
- Client request-header construction — a minimal repro sending the exact
  same `Cookie`/custom-header set (`HeaderProbe.java`) against a trusted
  Python server gets a clean `200` with correctly-formed headers.
- `ByteBuffer.allocateDirect()` — also verified correct (`DirectByteBuffer`,
  real address, working `put`/`get` round trip) in case Tomcat's
  `NioEndpoint` uses direct buffers on this code path; not conclusively
  ruled in or out as relevant since the exact buffer-allocation call site
  actually used by `Http11InputBuffer`/`SocketBufferHandler` during a live
  request wasn't traced.
- General regression: `TestConnector` (a different, previously-fixed Tomcat
  NIO class, see `reference_tomcat_triage_20260629`) still passes 11/12 on
  this exact binary — so this is not a blanket Tomcat-NIO regression, it is
  specific to something `TestAccessLogValve` (or `AccessLogValve` itself)
  does that the other classes don't.

**Not root-caused.** Candidate next steps for whoever picks this up:
1. Trace the exact `ByteBuffer`/`CharBuffer` allocation and encoding call
   chain `AccessLogValve.log()` and/or Tomcat's response
   `OutputBuffer`/`CoyoteOutputStream` actually use for this specific test
   (`resp.getWriter().print(...)` with `setCharacterEncoding("UTF-8")`) —
   distinct from the plain `SocketChannel.read()`-side path already fixed
   in Layer 2. `native-builtins/src/charset.rs`'s `CharsetEncoder`-result
   buffer allocator already has the equivalent `address=16` fix as
   precedent; worth confirming whether a *different*, still-unfixed
   allocation site is what `AccessLogValve`'s response-writing path hits.
2. Add server-side instrumentation directly in
   `org.apache.tomcat.util.net.NioEndpoint`/`SocketWrapperBase`'s write
   path (same recompile-and-classpath-override technique used for the
   `SocketWrapperBase.lock` investigation) to see whether the response is
   ever actually handed to the socket for writing, or something swallows it
   silently before that point — the total absence of ANY server-side log
   line (even at FINEST) for the failure itself is the most suspicious
   single fact here and suggests either a genuinely silent failure path or
   a logging configuration gap distinct from the `conf/logging.properties`
   fixture issue already fixed.
3. Since this is deterministic (94/94, not a subset), a bisection isolating
   which specific behavior of `TestAccessLogValve` vs `TestConnector`
   triggers it (e.g. try `TestConnector` with an `AccessLogValve` added to
   its pipeline, or `TestAccessLogValve` with the valve removed) would
   likely narrow this down faster than further blind tracing.

Given the host was under heavy concurrent load during this specific
re-verification (`uptime` load average 10-14 on 16 cores, 45 other
`cratonvm` processes from parallel sessions — unlike the original
idle-host re-verification at the top of this doc), also worth a **repeat
confirmation on a quiet host** before fully trusting the 94/94 determinism,
though the identical, exception-free `-1` signature every time (not varying
counts run to run, unlike typical contention noise) argues against pure
load-induced flakiness.

## 2026-07-10 (afternoon): fifth cause found and FIXED — JIT `ConcurrentLinkedQueue` miscompile; sixth cause found (NOT fixed) — known "register-invisible JIT root" SIGSEGV

Picked this doc back up per its own recommendation above. First step (per
[[check-already-fixed-before-setup]]): re-ran `TestAccessLogValve` against a
fresh `origin/dev` tip to see if the undiagnosed fifth cause was already
fixed by other landed work. It was not — but the symptom had CHANGED
entirely from what this doc documents above. Instead of 94/94 `-1`
failures, the class now crashes immediately with:

```
Exception in thread "main" java/lang/NullPointerException
	at org/junit/runner/JUnitCore.main(JUnitCore.java:36)
	...
	at org/junit/runners/ParentRunner.getDescription(ParentRunner.java:401)
	at org/junit/runners/Suite.describeChild(Suite.java:27)
	at org/junit/runners/Suite.describeChild(Suite.java:123)
	at org/junit/runners/ParentRunner.getDescription(ParentRunner.java:401)
	at org/junit/runner/Description.addChild(Description.java:193)
```

This happens during JUnit's `@Parameterized` test-discovery phase, building
`Description` objects for all 94 parameter sets — **before any HTTP server
starts**. This is a NEW, more severe regression than anything this doc
previously tracked, and it fully explains the earlier "no server-side
exception is ever logged" mystery from the 2026-07-10 morning section below:
the failure was never server-side. It also meant the documented fifth cause
(94/94 `-1`s) could no longer be reproduced or investigated until this new
blocker was cleared.

### Root cause: JIT miscompile of `ConcurrentLinkedQueue`'s allocate-then-CAS hot methods

`org.junit.runner.Description.fChildren` is a
`java.util.concurrent.ConcurrentLinkedQueue` (JUnit 4.13+, confirmed via
`javap`). A `@Parameterized` test with enough cases (~40+, e.g.
`TestAccessLogValve`'s 94) crosses the instance-method JIT tier-up threshold
(`CRATONVM_JIT_VIRTUAL_TIERUP`, default on — see
`vm/src/runtime/interpreter.rs`'s comment: "Instance-method invocation
tier-up... short-loop instance hot methods... never JIT-compile" without
it) partway through the suite and JIT-compiles CLQ's `offer()`/
`tryCasSuccessor()`. A subsequent call into the JIT-compiled code raises a
spurious message-less `NullPointerException`.

Bisection (a minimal, content-independent 46-entry `@Parameterized` repro,
`/data/data/alv5th-repro/BisectLongUnrelated.java` on the Azure host)
proved this is **not** content-specific (an earlier theory — that it needed
a specific string reused as both a raw value and a JSON-wrapped substring —
was a red herring caused by testing at a fixed 2 GB heap) and **not** a
GC/root-scanning bug: `CRATONVM_DBG_DESCTRACE` tracing added to
`gen_heap.rs::forward_object` confirmed `gc_quiescence::is_active()` was
**false** at every single object relocation preceding the crash — i.e. no
relocation ever happens while a JIT frame is active, so the GC-quiescence
invariant (`docs` in `vm/src/jit/conservative_roots.rs`) holds and this is
not a stale-pointer-across-GC bug. Decisive isolation:
`CRATONVM_JIT_VIRTUAL_TIERUP=0` alone fixes the repro at any heap size;
`CRATONVM_BG_COMPILE=0` alone does **not** — narrowing the defect to this
specific JIT tier-up path's codegen for the allocate-then-CAS idiom
(`new Node<E>(e)` then CAS/relaxed-append it onto the tail), the exact same
miscompile archetype already tracked for the `AbstractQueuedSynchronizer`
family in `vm/src/jit/skip_list.rs::is_known_miscompile_aqs_family` (see
that function's own doc comment on "allocate-then-CAS hazard").

**FIX** (commit `36bbe9fc` on branch `fix/gengc-description-npe-20260710`,
merged to `dev`): added a new `is_known_miscompile_clq_family` skip-list
function (sibling of the AQS one, same unconditional call-site treatment)
covering `ConcurrentLinkedQueue`'s `add`/`offer`/`tryCasSuccessor`/
`updateHead`/`succ`/`poll`/`skipDeadNodes`/`<init>` and its `Node` inner
class's `<init>`/`appendRelaxed`/`casItem`.

**Verified:**
- Minimal 46-entry repro: passes cleanly at `--Xmx 8m` and `4m` (was: NPE at
  both).
- `TestAccessLogValve`: now gets **past test discovery** and runs real
  HTTP-based test cases for the first time (was: 0/94, immediate crash
  before test 1) — see the sixth cause below for where it now stops.
- `TestRewriteValve`: **110/121 pass** (was: 80/121). The 11 residual
  failures are all UTF-8/percent-encoding query-string tests
  (`testUtf8*`, `testRewriteEmptyHeader`) — a narrower, different issue than
  the `302`-vs-`200`/`400` rewrite-rule logic bug previously blamed for the
  bulk of the 41 failures; not investigated further this session.

### Sixth cause found (NOT fixed): SIGSEGV around `TestAccessLogValve` test #8 — the already-tracked "register-invisible JIT root" bug family

With the fifth cause fixed, `TestAccessLogValve` runs real tests for the
first time but crashes with `SIGSEGV` (`exit 139`) around test #8
(`test[7: Name[pct-A], Type[json]]`), on an `http-nio` worker thread, inside
JIT-compiled code (`rip` falls in an anonymous executable region with no
symbol table — a JIT code buffer). Caught live under `gdb` (`ulimit -c` is 0
on this host, no core dumps, so a live-attach `handle SIGSEGV stop nopass` /
`run` batch script was used instead of the project's usual
`core_pattern`+`ulimit -c unlimited` recipe).

Register dump at the crash:
```
rax=0x5  rdi=0x200106b6010  rsi=0x1894ed00  r10=0x200417c27c0
r12=0x20042260010  r13=0x1  r14=0x200106b6010  r15=0x20042260000
```
Every genuinely live pointer in the register file (`rdi`, `r10`, `r12`,
`r14`, `r15`) shares the same `0x2000xxxxxxxx`-prefixed tagged-pointer
shape. `rsi` alone breaks that pattern (`0x1894ed00` — a bare, truncated
value, not a real 64-bit tagged pointer). This is a **byte-for-byte
signature match** with the "register-invisible JIT root" bug family
documented as still-OPEN in
`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`
("Layer 1 (register-invisible roots...) is UNCHANGED — the real fix remains
precise oop maps / shadow stack") and independently confirmed a third and
now — with this session — a fourth time in
`swallowabortedupploads-unexpected-socketexception.md`'s 2026-07-10 SIGSEGV
section (Tomcat `AbortedPOSTClient` tests) and
`docs/known-issues/hib-global-temptable-nondeterministic-sigsegv-20260710.md`
(Hibernate global-temp-table DDL). Mechanism: JIT-compiled code can keep a
live object reference in a register/stack slot across a GC-capable
safepoint without it being visible to the conservative root scanner; if a GC
cycle runs while that register is the *only* reference to the object, the
object is reclaimed even though a live-but-invisible reference to it still
exists, and the register is left holding a stale/garbage value.

Consistent with that doc's evidence: `CRATONVM_JIT_VIRTUAL_TIERUP=0` (which
disables the whole instance-method tier-up feature, not just the CLQ family)
also avoids this SIGSEGV — the run got **5x further** (41 test cases started
vs. 8) before hitting an unrelated 300s timeout stuck on `STW cross-thread
JIT takeover is still waiting for cooperative mutators` (a different,
already-documented JIT/threading issue, not investigated further here).
This is exactly the shape of evidence the swallow-uploads doc's "Not fixed
this session" section describes: previously-attempted mitigations
(`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`, `CRATONVM_SHADOW_STACK=1`,
`CRATONVM_NO_SELECTIVE_PROMOTE=1`, `CRATONVM_PRECISE_JIT_MAPS=1`) were all
found insufficient there, and the real fix needs precise JIT oop maps /
shadow stack — deep infrastructure work, deliberately **not** attempted in
this session either, per the same reasoning: "a blind patch here would be
exactly the kind of half-fix the project's workflow asks not to merge."

**Practical upshot:** `TestAccessLogValve`'s original 94/94 `-1` question
(the fifth-cause hunt from the section below) is now moot — that specific
symptom no longer reproduces, superseded first by the fifth cause (now
fixed) and now blocked by the sixth. Whoever next picks up the
precise-JIT-maps/shadow-stack roadmap item should treat
`org.apache.catalina.valves.TestAccessLogValve` (full 94-case run, default
JIT, 2 GB heap) as a fifth independent reproduction case for that bug
family, alongside the DoHead, Hibernate, and `TestSwallowAbortedUploads`
ones already tracked.

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
