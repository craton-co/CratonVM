# Group 04 — Embedded-server deployment throughput wall  (CLOSED 2026-07-27)

Retired from `docs/known-issues/tomcat/` on 2026-07-27. Every defect this
group documented is fixed; what was left of its "residual wall" was re-derived
from scratch and turned out to have a single, different, and now separately
tracked owner:

* **`docs/internal/fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md`** —
  the synchronized-method JIT admission defect, fixed on 2026-07-28.
* **`docs/known-issues/tomcat/32-doc04-residual-perf-assertions.md`** — the
  four per-test residuals that are not deploy throughput.

The rest of this file is the closing evidence, then the group's own history.

---

## Closing evidence (2026-07-27)

### 1. Every "fix lever" the doc asked for has landed and is DEFAULT-ON

The 2026-06-15 re-verification ranked three levers against
`update_root_snapshot`. All three are done, and the doc's ranking of them is
now obsolete:

| lever | state today |
|---|---|
| #1 cut publish FREQUENCY | **DONE** — `bcd336339` (2026-07-21). `safe_native_call` and `native_return_pushed_to_stack` no longer publish at all; the snapshot is built only at safepoints and at blocking deposits. `vm_exec.rs`'s own tests assert `root_snapshot.lock().is_empty()` after a native return. |
| #2 drop the SECOND publish | subsumed by #1 (there is no second publish left). |
| #3 make the cache survive collections | **DEFAULT-ON** — `env_cache::rootsnap_cache_survive_gc`, opt-out `=0`. |
| (the cache itself) | **DEFAULT-ON** — `env_cache::rootsnap_cache`, opt-out `=0`. |

### 2. The "latent, NOT bug 04" `new URL(String)` slot-5 note is fixed

`net_phase_e.rs::field5_is_full_url` now discriminates a synthetic full-URL
cache from a real-JDK `authority` slot (rejecting `host:port` and
`user:pass@host:port` shapes), and is applied at `toExternalForm`,
`openStream` and the `methodUrl` path alike. Covered by
`re1_field5_full_url_discriminator`.

### 3. The residual is NOT a JIT/hot-path problem — measured

A webapp deploy runs at the same speed with the JIT switched off:

| | JIT on | JIT off |
|---|---|---|
| `catalina.core.TestApplicationFilterConfig` | 11.1 s | 10.8 s |

(HotSpot: 0.86 s.) So every hot-loop lever this doc chased — root-snapshot
publishing, the `CRATONVM_ROOTSNAP_CACHE` family, the C2 exception-table
exclusion, direct calls — cannot move the deploy classes. Bare VM start-up on
the same classpath is 0.36 s vs HotSpot 0.08 s, so start-up is 0.3 s of it,
not 10.

### 4. What the deploy time actually is

One `TestHostConfigAutomaticDeploymentAddition` test method, run alone through
`RunMethods` (one `@Test`, one WAR + one DIR deploy):

* **CratonVM 245.2 s / HotSpot 1.92 s = 128×.**

`--stack-dump-on-timeout=90` puts the main thread in Tomcat's annotation scan,
inside its BCEL class parser, reading class bytes **one at a time**:

```
ContextConfig.processAnnotationsJar
  -> tomcat.util.bcel.classfile.ConstantPool.<init>
     -> Constant.readConstant
        -> java.io.BufferedInputStream.read      <-- per byte
           -> read1
```

`ByteReadProbe` prices that operation: `DataInputStream.readUnsignedByte` over
a `BufferedInputStream` costs **13–16 µs per byte** on CratonVM vs 21–39 ns on
HotSpot (~500×), and every layer of that chain is a `synchronized` method or a
`lock/try/finally/unlock` body that this VM never JIT-compiles. That is
known-issue 31; the numbers and the two independent refusals are written up
there.

### 5. Group-04 residual class list, re-run 2026-07-27

Same list the 2026-07-27 note carried, on a binary with the direct-call fix
(`e3cb2ab17`), `-Xmx2g`, 1500 s cap, shared host with peer VMs running:

| class | CratonVM | HotSpot | verdict |
|---|---|---|---|
| `catalina.startup.TestHostConfigAutomaticDeploymentAddition` | HANG @1500 s | PASS 32.2 s | deploy wall → **31** |
| `catalina.startup.TestHostConfigAutomaticDeploymentModification` | HANG @1500 s | PASS 35.0 s | deploy wall → **31** |
| `catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` | HANG @1500 s | PASS 20.2 s | deploy wall → **31** |
| `coyote.http2.TestHttp2Section_8_2` | **FAIL 332.9 s** (was HANG) | PASS 219.6 s | **no longer a throughput item** — 252 tests run at 1.5× HotSpot, then a VM `internal error: current class not found`. Reproduces identically on the pre-fix binary (387 s), so it is a pre-existing, separate defect → **33** |
| `catalina.mapper.TestMapperPerformance` | FAIL 54.8 s | PASS 1.8 s | absolute 5 s budget → **32.1** |
| `el.parser.TestELParserPerformance` | **PASS** 809.1 s | PASS 4.1 s | now passes → **32.2** keeps the warm-up-order note only |
| `websocket.server.TestAsyncMessagesPerformance` | FAIL 39.1 s | PASS 32.2 s | client drain rate → **32.3** |
| `juli.TestOneLineFormatterPerformance` | FAIL 350.9 s | PASS 2.0 s | `SimpleDateFormat` 834× → **32.4**, likely a consumer of **31** |

Shared host with peer VMs running, so the absolute seconds are upper bounds;
the statuses are not affected (every HANG is 1500 s of real work short of
finishing, and every HotSpot column is from the same window).

---

## History (as the doc stood in `known-issues`)

> ## 2026-07-27 — this group owned the residual throughput evidence from the 1500 s rerun
>
> `29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md` collected a batch of
> Tomcat classes that looked like new bugs but were really this group's wall,
> plus a few that turned out to be genuine, separate defects. After
> root-causing every item in that doc, only the classes in the table above
> remained as group-04 evidence — measurements of this wall, not open bugs of
> their own.
>
> Two of them deserved a footnote because they read as "CratonVM's optimiser is
> backwards" when they are not:
>
> * `juli.TestOneLineFormatterPerformance.testDateFormat` asserts
>   `DateFormatCache` beats `String.format`. It fails for a reason specific to
>   this VM's *shape*, not merely its speed: `java.util.Formatter.format` —
>   what `String.format` delegates to — is registered as a Rust
>   `NativeKind::Intrinsic` (`native-builtins/src/lib.rs`,
>   `register_formatter_natives`), so it runs near HotSpot speed while
>   everything it is raced against is ordinary interpreted bytecode.
>   Re-measured 2026-07-27 with `probes/DateFmtProbe.java`:
>
>   | operation | HotSpot | CratonVM | ratio |
>   |---|---|---|---|
>   | `String.format` (intrinsic) | 2.26 µs | 16.1 µs | 7.1× |
>   | `SimpleDateFormat.format` (the cache's miss path) | 0.34 µs | 281.6 µs | **834×** |
>   | `Calendar.get` | 0.042 µs | 9.13 µs | 218× |
>   | `StringBuilder.append(long).toString()` | 0.050 µs | 5.22 µs | 104× |
>
>   The test passes `System.nanoTime()` to a millisecond-resolution formatter,
>   so `DateFormatCache`'s per-second cache misses on essentially every call
>   and the miss path *is* the measurement. `SimpleDateFormat.format` is ~8×
>   worse than this VM's general bytecode gap (834× vs ~100×), and the JIT
>   only buys it 2.1× (593 µs → 282 µs) where `Calendar.get` gets 11.8× — the
>   signature of code that never compiles. See known-issue 31.
> * `TestELParserPerformance` runs its `ReInit` loop first and its `new
>   ELParser()` loop second, so the first loop absorbs JIT warm-up. That is why
>   it fails on a loaded host and passes on a quiet one.
>
> Everything else doc 29 listed is closed as a real, separate, *fixed* defect —
> `File.setLastModified` on directories, the path-keyed jar/war byte cache, the
> discarded truncated HTTP response body, and the `file:`-URL leading-slash +
> percent-decode ordering bug.

> ## 2026-07-21 CROSS-CONFIRMATION — same mechanism rediscovered from Spring Boot
>
> A separate investigation (Spring Boot "severe slowdown, not a hang" triage)
> converged on the same `update_root_snapshot` mechanism via JUnit5's
> `InterceptingExecutableInvoker` reflective machinery, confirmed with
> `ReflectiveInvokeProbe.java` (cost scales ~1× → 2.9× as reflective nesting
> goes 1 → 10 layers). That raised the priority of lever #1, which has since
> landed (see Closing evidence §1).

> ## ✅ 2026-06-15 RE-VERIFICATION — both FUNCTIONAL sub-problems fixed
>
> 1. **Connector serving (setOption)** — FIXED in `dev`.
> 2. **In-process HTTP client `getUrl`** — FIXED in `dev` (`48183599`,
>    `b628d8d7`); `TestTomcatClassLoader` = `OK (2 tests)`, still passing
>    2026-07-27.
> 3. **Deploy throughput** — the only thing left, and re-attributed above.
>
> The 2026-06-15 root-cause pin on `update_root_snapshot` (~1.8 M calls and
> ~68 % of a 29 s deploy, cdb-sampled) was correct *for that binary*. Lever #1
> removed those calls; `TestApplicationFilterConfig` is 29 s → ~11 s, and what
> remains does not respond to the JIT at all.

> ## ⚠ 2026-06-14 CORRECTION — a FUNCTIONAL connector bug masquerading as throughput
>
> `NioEndpoint.setSocketOptions` calls `SocketChannel.setOption(SocketOption,
> Object)` on every accepted connection. The `sc_set_option` native was
> registered only with the `NetworkChannel` return-type descriptor, but
> `SocketChannel.setOption` **covariantly** returns `SocketChannel`, so the
> native was missed, dispatch hit the abstract method, and the accepted socket
> was aborted before the request was read. Fixed by also registering the
> covariant `SocketChannel` / `ServerSocketChannel` return descriptors
> (`native-io/src/socket_channel.rs`). A significant fraction of the "server
> tests HANG" population was this functional reset, not throughput.

**Original symptom (for the record):** embedded-server classes deploy a webapp
(`tomcat.start()` → `ContextConfig` → TLD/annotation/jar scanning → servlet
init) then serve requests; under CratonVM one deploy took ~150 s where HotSpot
took <1 s, so a class with many `@Test` methods could not finish within any
practical per-class timeout. Not an infinite loop, not a TLS bug — it
reproduced with the JIT off, and the server started, served and tore down
correctly.
