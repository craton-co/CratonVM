# `TestManagerWebapp` — read-timeout residual behind the fixed `seek0` bug

> ## ✅ 2026-08-06 FIXED — retired from `docs/known-issues/tomcat/`
>
> `org.apache.catalina.manager.TestManagerWebapp` is **green, 3/3**. Two real
> defects and one structural cost were found in the deploy path and removed;
> `testBug57700`'s deploy went from **74.3 s to 16.3 s** against the client's
> fixed 30 s read timeout, and the class no longer fails.
>
> ### What this doc got wrong, first
>
> Its header says the `seek0` retirement commit "opens the deploy-scan wall
> behind it" but that "no doc for that residual was found in either
> `docs/known-issues` or `docs/internal`". **There is one, in the same
> directory**: `!webapp-deploy-annotation-scan-interpreted-226x.md`. This doc
> was a rediscovery of that doc's residual, not a new defect, which is why it
> retires here rather than being fixed on its own terms. That doc stays OPEN —
> see § What is still open.
>
> Its two "Not yet done" items are now done, and both answers are negative for
> the hypotheses they were checking:
>
> * **HotSpot control on this exact build/fixture.** `OK (3 tests)` in
>   **8.892 s** (whole class) on the Linux fixture at
>   `/data/data/tomcat-dohead-fixture-20260717`. `testBug57700` alone: 2.085 s
>   wall, deploy **487 ms**. The class does pass on HotSpot.
> * **Standalone isolation / host-timing sensitivity.** Not timing sensitivity.
>   Driven as a single method through a JUnit `Request.method` runner, the
>   failure is **deterministic**: 4 runs of pristine `origin/dev`, 4 failures,
>   deploy 72.5–75.5 s against a 30 s timeout. It never came close to passing.
>
> ### The three findings
>
> All three were found by profiling the **real deploy**, not the probe — which
> matters, see § Why three sessions on the probe missed all of this.
>
> #### 1. `jmx_locked_monitors` leaked, and the leak was quadratic (14.9%)
>
> `ThreadRegistry::complete_jmx_monitor_enter` was the **hottest symbol in the
> entire run at 14.87%**, and `perf annotate` put **96% of that inside its
> `owned.iter().any(...)` dedupe loop**, stalled on cache misses — a shape only
> reachable with hundreds of stale entries in a per-thread set that should hold
> a handful.
>
> Every `monitorenter` publishes into that set. Two of the four publishers never
> retracted: `SynchronizedMethodGuard::drop` (the `ACC_SYNCHRONIZED` path in
> `invoke_on_class_shared_inner` — every synchronized native and every
> synchronized method whose bytecode this VM shadows) and
> `NativeContextImpl::monitor_exit` (whose partner `monitor_enter_gc_safe` does
> publish). The interpreted frame-pop path, the `monitorexit` opcode and
> `JitSynchronizedMonitorGuard::drop` all already did the `holds`-gated retract.
>
> The diagnostic that pointed at it: `complete_jmx_monitor_enter` at 14.87%
> while its counterpart `remove_jmx_locked_monitor` did not reach 0.4%. Enters
> and exits must balance; that asymmetry is the leak.
>
> This is a **correctness** bug too, not only a performance one:
> `getLockedMonitors()` reported monitors the thread does not hold — the exact
> failure mode the write side was added to prevent — and the entries are GC
> roots, so every object ever locked through those two paths stayed reachable
> for the life of the thread. Commit `62d28988e`.
>
> #### 2. `Multi-Release` was re-parsed once per jar ENTRY (8.3%)
>
> `p59_jar_lookup_versioned_entry` asks `p59_jar_is_multi_release(path)` on
> every entry lookup, and answering it re-read and re-parsed the whole
> `../../../../apps/META-INF/MANIFEST.MF` every time. `<Utf8Chunks as Iterator>::next` under that
> function was the **second hottest symbol at 8.3%**. Memoized on
> `JarContents::multi_release`, a `OnceLock<bool>` keyed on the same
> (path, mtime) pair as the rest of the jar cache. Commit `3517a77fe`.
>
> #### 3. Typed `DataInputStream` reads re-entered the interpreter per two bytes
>
> This is the big one, and it is the actual mechanism behind the whole
> "annotation scan is 226x slower" wall.
>
> `dis_read_exact` (native-io) is the shared helper behind `readByte`,
> `readShort`, `readUnsignedShort`, `readChar`, `readInt`, `readLong`,
> `readFloat`, `readDouble`, `readBoolean` and `readUTF`. For a **two byte**
> `readUnsignedShort` it allocated a Java `byte[2]` on the heap and re-entered
> the VM through `invoke_virtual` to run the wrapped stream's **interpreted**
> `read(byte[],int,int)` bytecode, then copied the bytes back out.
>
> On a real-JDK `BufferedInputStream` that bytecode is `read` → `read1` →
> `getBufIfOpen` ×2 → `ensureOpen` → `System.arraycopy`. Six interpreted
> invocations plus a heap allocation, per two bytes of class file. Tomcat's BCEL
> `ClassParser` reads entire class files that way, and `ContextConfig`'s
> annotation scan runs it over every `.class` in every jar on the container
> classpath.
>
> `--stack-sample-ms 20` over the failing run, aggregated by leaf frame:
>
> | frame | share of interpreted time |
> |---|---|
> | `java/io/BufferedInputStream.read` | 30.6% |
> | `java/io/BufferedInputStream.read1` | 23.0% |
> | `java/io/BufferedInputStream.getBufIfOpen` | 12.9% |
> | `java/io/BufferedInputStream.ensureOpen` | 6.2% |
> | `java/io/BufferedInputStream.fill` | 5.6% |
> | **BufferedInputStream total** | **78.3%** |
> | `tomcat/util/bcel/classfile/ConstantPool.getConstant` | 5.6% |
>
> **None of those five can ever JIT-compile.** `read` and
> `read(byte[],int,int)` are `ACC_SYNCHRONIZED`, which
> `try_jit_upgrade_with_gate` refuses. `read1`, `getBufIfOpen`, `ensureOpen` and
> `fill` are private and — this is the part that was not previously understood —
> are reached only through this **native re-entry**, so they have no
> inline-cache site at all and the tiering manager never counts them. Confirmed
> with `CRATONVM_DBG=jit-method-stats`: 267 distinct methods tracked, 129 k
> counted invocations, against `CRATONVM_DBG=invokestats`' 22.4 M inline-cache
> hits. Not one `BufferedInputStream` body appears in the compiled census.
>
> `dis_fast_pull` copies the bytes straight out of the stream's own `buf` at
> `pos` and advances `pos`, in Rust. `dis_fast_skip` does the same discard-only
> for `skipBytes` — how BCEL steps over every attribute it does not parse
> (`Utility.skipFully`), previously an 8 KiB scratch allocation per call.
> `DataInputStream.read()` takes the same shortcut. The generic path still runs
> when the buffer is exhausted, which is what refills it, so the cost becomes
> one VM re-entry per 8 KiB instead of one per two bytes. Commit `e899910c2`.
>
> ### Evidence
>
> Azure host, load 22–27 throughout, arms interleaved ABBA within every pass.
> Pristine arm is `origin/dev` `c3919f7d3` built from a detached worktree, i.e.
> the exact tip this branch merges.
>
> **The test itself** — `testBug57700` driven alone, 4 runs per arm:
>
> | arm | result | deploy of `/bug57700` |
> |---|---|---|
> | pristine `c3919f7d3` | **4/4 FAIL**, `SocketTimeoutException: Read timed out` | 72 494 / 74 184 / 75 078 / 75 537 ms — mean **74 323** |
> | this branch | **4/4 PASS** | 15 536 / 16 260 / 16 365 / 17 081 ms — mean **16 311** |
> | HotSpot 25.0.3 | PASS | **956 ms** |
>
> **4.56x**, no overlap, and the client's read timeout is a fixed 30 000 ms — so
> the fix clears it with roughly 2x margin even on a host at load 25.
>
> **Whole class**, `org.junit.runner.JUnitCore`:
>
> | arm | result | time |
> |---|---|---|
> | pristine | `Tests run: 3, Failures: 1` | 84 s |
> | this branch | **`OK (3 tests)`** | 50–58 s |
> | HotSpot | `OK (3 tests)` | 8.9 s |
>
> **`probes/AnnotationScanCostProbe.java`** — the owning doc's own metric, three
> interleaved passes with the arm order reversed inside each, `taglibs-standard-impl`
> parse:
>
> | arm | readings (µs/class) | mean |
> |---|---|---|
> | pristine | 3283.6 3482.9 3253.1 3191.0 3264.8 3231.3 | **3284.5** |
> | this branch | 870.1 879.5 797.4 805.2 802.3 726.2 | **813.5** |
>
> **4.04x**, total separation in both orders. (HotSpot read 4.6–19.3 µs/class
> across the same passes; the whole parse is ~2 ms there, so treat that column
> as noise and read the CratonVM-vs-CratonVM ratio, per that doc's own rule.)
>
> ### Correctness
>
> * **Regression suite: 31/31 green** on the merged tree (29/29 before `dev`
>   added `RBlockingQueue`, plus the new vector below).
> * **New vector `regression-suite/src/RDataInputFastPull.java`.** Every way the
>   fast path can be wrong is silent — a wrong `pos` advance shifts later reads,
>   no advance re-delivers bytes, bypassing an override yields raw bytes — so it
>   is built so each mistake changes a *printed* value, and the suite diffs it
>   against HotSpot. It covers ten buffer sizes chosen to straddle field
>   boundaries, a `ByteArrayInputStream` with no buffering at all, position
>   coherence when the same stream is read both through the `DataInputStream`
>   and directly, `mark`/`reset` across typed reads, a `BufferedInputStream`
>   subclass whose overridden `read` complements every byte, a `readUTF` payload
>   far larger than the buffer, `skipBytes`, and EOF.
> * **Positive control.** A build with the `pos` advance off by one fails it at
>   buffer size 2; a build with the exact-class guard removed fails its
>   overridden-read vector; the unbroken control build is identical to HotSpot.
>   So the vector is demonstrably sensitive to both failure modes rather than
>   merely green.
> * **15 further Tomcat classes**, pristine vs this branch, covering jar
>   resources, class loaders, serialization, JSP parsing and the connector
>   streams: **identical outcomes on all 15** — same passes, and the same three
>   pre-existing failures with the same failure counts
>   (`TestWebappClassLoader` 1, `TestEncodingDetector` 5, `TestIOTools` 1;
>   all three fail on pristine too, so none is a regression). Several got much
>   faster: `TestEncodingDetector` 132 s → 63 s, `TestWebappClassLoader`
>   28 s → 13 s.
>
> ### Why three sessions on the probe missed all of this
>
> `probes/AnnotationScanCostProbe.java` is the load-bearing measurement in
> `!webapp-deploy-annotation-scan-interpreted-226x.md`, and it is **not
> representative of the deploy it stands in for**. That doc dismissed the
> monitor cost on the strength of the probe's profile — "the only monitor symbol
> that appears at all is `complete_jmx_monitor_enter`, at 1.47%". On the **real
> deploy** the same symbol is **14.87%**, ten times larger and the single
> hottest thing in the process. The jar-manifest re-parse does not appear in the
> probe at all, because the probe opens one jar and the deploy opens hundreds.
>
> The general lesson is one this repo keeps relearning in new costumes: a probe
> written to isolate a hypothesis measures the hypothesis, not the workload. The
> profile that found all three defects was `perf record` on the **actual failing
> test method**, run alone, on a quiet host.
>
> ### A lever that was built, measured, and dropped
>
> `CRATONVM_JIT=special-tierup` was implemented on the theory that
> `execute_invokevirtual_cached`'s tier-up block opening with `!is_special`
> gates *counting* as well as promotion, so a private method called through warm
> inline-cache sites can never reach the threshold — the "untaken lever" the
> owning doc records. It was built, and it **measured nothing**: counted
> invocations went 129 391 → 129 455, the compiled-method census went 672 → 674
> (two extra constructors), no `BufferedInputStream` body compiled, and
> combined with `sync-methods` the deploy got *worse* (38 s → 47 s, twice).
>
> The reason is finding 3: those methods are not reached from a bytecode call
> site at all, so there is no counter for the gate to have been suppressing. The
> lever's premise was falsified by the thing that actually fixed the problem, so
> it was reverted rather than shipped default-off with a rationale now known to
> be wrong. Recorded here so the next person does not rebuild it.
>
> ### What is still open
>
> `docs/known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md`
> stays OPEN. Its exit criterion is "`AnnotationScanCostProbe` within ~5x of
> HotSpot"; this work moved the probe 4.04x closer but not to 5x, and the
> remaining distance really is the flat interpreter profile that doc describes
> (after these fixes the hottest Rust symbol on the deploy is
> `is_object_address` at 6.0%, and nothing else exceeds it). What has changed is
> that the doc's stated conclusion — "no hotspot to remove", "not reachable by
> tiering work", "a genuine interpreter rewrite" — was **premature**: 23% of the
> run was two removable defects, and the single largest cost was a native
> re-entry pattern, not interpreter dispatch. Its § What this is NOT — measured
> has been corrected accordingly.
>
> Both test methods this doc and that one name (`testBug57700`, `testDeploy`)
> now pass, so that doc's remaining scope is throughput, not a failing test.

---

## Original report (as the doc stood in `known-issues`)

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | medium |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun |
| **Prior history** | [`testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md`](testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md) — the original `ExpandWar` `seek0: bad fd for rw_seek` defect is confirmed fixed in this build (no longer appears); its retirement commit (`d77025185`) says it "opens the deploy-scan wall behind it" but no doc for that residual was found in either `docs/known-issues` or `docs/internal` |

## Symptom

With the `seek0` bug fixed, `TestManagerWebapp` still fails 2 of 3 methods,
now with a plain client-side read timeout instead of the old `ExpandWar`
error:

```
1) testBug57700(org.apache.catalina.manager.TestManagerWebapp)
java.net.SocketTimeoutException: Read timed out
	at sun.nio.ch.NioSocketImpl.timedRead(NioSocketImpl.java:277)
	at org.apache.catalina.startup.SimpleHttpClient.readResponse(SimpleHttpClient.java:273)
	at org.apache.catalina.manager.TestManagerWebapp.testBug57700(TestManagerWebapp.java:571)
```

Total class time is 177s (vs. HotSpot's low-teens-of-seconds for this class
historically) — consistent with a slow deploy/scan cycle that occasionally
exceeds the client's read timeout rather than a hard hang, similar in shape
to the general deploy-throughput-wall family
(see `gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`),
though not yet confirmed to share that exact cause.

## Not yet done

- HotSpot control run on this exact build/fixture (the class historically
  passes on HotSpot, but not re-verified this round).
- Standalone isolation to rule out shared-host timing sensitivity, given the
  `SimpleHttpClient` read timeout is a fixed wall-clock value that a slow
  deploy could plausibly exceed even without a real defect.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.manager.TestManagerWebapp
```

Linux equivalent used for the fix (single method, ~40 s instead of ~60 s for
the class):

```bash
cd /data/data/tomcat-dohead-fixture-20260717
<cratonvm> --java-home /home/victor/jdk25 -Xmx2g \
  -cp "<TMWOne dir>:$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)" \
  TMWOne org.apache.catalina.manager.TestManagerWebapp testBug57700
```

where `TMWOne` is a three-line `JUnitCore().run(Request.method(cls, name))`
driver — `org.junit.runner.JUnitCore` cannot select a single method, and the
other two methods cost ~35 s of the class's runtime.
