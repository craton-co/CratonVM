# 32 — the four per-test performance residuals from known-issue 04

**Status:** 🔴 **OPEN** (32.2 closed). Carved out of known-issue 04 on
2026-07-27 when that group was retired; **re-derived item by item on
2026-07-30**, which changed the disposition of every one of them. These are the
residual items that are *not* webapp-deploy throughput (formerly
[31](../../internal/fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md)) — each is one test whose
assertion is about speed, with its own separate reason.

| item | disposition after the 2026-07-30 re-derivation |
|---|---|
| 32.1 | OPEN, **fully root-caused** — a dev regression, not a mapper problem. Closes when that is fixed; no Tomcat work left. |
| 32.2 | ✅ **CLOSED** — passes; a property of the test's shape, not a defect. |
| 32.3 | OPEN, improved ~12–16 %. Root cause identified as per-completion processing, not I/O; no further AIO work will close it. |
| 32.4 | OPEN, and **harder than recorded** — belongs to doc [30](30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md)'s family, not here. |

Nothing here is a Tomcat defect any more. Two items are consumers of VM-wide
throughput problems and one is a test-shape artefact; only 32.1 has a
single, named, fixable cause.

HotSpot reference for all four, same host, same day: all PASS.

**Every number the 2026-07-27 revision of this document carried is stale.**
Re-measured on dev `9b86a9ac1`, quiet host:

| item | test | 07-27 doc | 07-30 measured |
|---|---|---|---|
| 32.1 | `TestMapperPerformance` | easiest host 2.8–3.7 s | easiest host **20.4 s** |
| 32.2 | `TestELParserPerformance` | "flips on a loaded host" | **PASS**, 321 s |
| 32.3 | `TestAsyncMessagesPerformance` | gaps 1–9 ms | gaps **0.6–1.1 ms** |
| 32.4 | `TestOneLineFormatterPerformance` | `SimpleDateFormat` 834× | **635×**, and the bar moved |

---

## 32.1 `catalina.mapper.TestMapperPerformance.testPerformance` — a dev regression, not a mapper problem

The test maps 10⁶ URIs per hostname and asserts each hostname finishes in
under **5 000 ms** (an absolute wall-clock budget, no baseline).

**This item was misattributed.** The previous revision blamed the Rust `Mapper`
shadow and proposed instrumenting `mapper_internal_fast_cache`'s hit rate. Both
are wrong, and both are now settled by measurement rather than by reading code.

### The memo was never ineffective

`CRATONVM_DBG_MAPPER=1` (added 2026-07-30) reports `internalMap`'s memo hit
rate directly. It is **1.0000 — exactly one miss per hostname**, i.e. the first
call. The suspected validity condition,

```rust
let paused_now = if let Some(v) = entry.selected_version { …read "paused"… }
                 else { !entry.no_context };          // ← suspected always-true
```

cannot misbehave: every insert site sets `selected_version: Some(..)` exactly
when it sets `no_context: false`, so `selected_version.is_none()` holds if and
only if `no_context` is true, and the `else` arm therefore always evaluates
`!true == false` — "not paused", memo usable. The arm is dead, not wrong.

### The cost is in `MappingData.recycle()`, which is bytecode

`apps/tomcat-suite-runner/probes/MapperPerfProbe.java` runs the test's own setup
and loop standalone, and splits the loop body. Easiest hostname, 300k
iterations, quiet host:

| loop body | dev `b695d468f` (07-27) | dev `9b86a9ac1` (07-30) |
|---|---|---|
| `recycle()` + `map()` (what the test times) | 2.03 µs | 27.1 µs |
| **`MappingData.recycle()` alone** | **0.73 µs** | **24–30 µs** |
| `MessageBytes.toChars()` ×2 (the shadow) | 0.75 µs | 1.04 µs |

The native shadow barely moved. `MappingData.recycle()` — six field stores and
four nested `recycle()` calls, ordinary Tomcat bytecode with no native
registration anywhere in it — went **41× slower**. That is the whole regression,
and it is why the `CRATONVM_TOMCAT_MAPPER_NATIVES=0` A/B looks so different from
the one the previous revision recorded: both sides of that A/B pay `recycle()`.

Shadow ON vs OFF today is 24 vs 40 µs (1.7×), against the 3.34 vs 35.4 µs (10×)
recorded on 07-27. The bytecode path is essentially unchanged; only the total
regressed, because `recycle()` is common to both.

### Mechanism

`CRATONVM_DBG_JIT_METHOD_STATS=1` on the recycle-only loop:

```
old (07-27): 5 methods tracked, 4 ever invoked, 2192 invocations, c1=4 c2=4, osr=1
new (07-30): 1 method  tracked, 0 ever invoked,    0 invocations, c1=0 c2=0, osr=1
```

The driving loop still OSR-compiles in **both**. On current dev its callees are
never counted, never enqueued, and never compiled — they interpret forever.

This is **not a Tomcat bug**. `probes/CalleeTierUpProbe.java` reproduces it with
nothing on the classpath (ns per call, quiet host):

```
HotSpot                  17.6
CratonVM dev b695d468f   511 / 490      (07-27)
CratonVM dev 9b86a9ac1   18270 / 18930  (07-30)
```

### Cause 1 — `c28bdd687` turned instance-method tier-up off by default

`fix: stabilize spring boot JIT request dispatch` (2026-07-30 02:00) flipped
`jit_virtual_tierup` (vm/src/runtime/env_cache.rs) from default-ON to
default-OFF:

```diff
-        Err(_) => true,
+        Err(_) => false,
```

`recycle()` and everything it calls are **instance** methods reached by
`invokevirtual`, so with that gate off they are never counted, never enqueued
and never compiled — exactly the `0 invocations` above. Confirmed directly on
today's binary rather than by inference (`CalleeTierUpProbe`, ns per call):

| `CRATONVM_JIT_VIRTUAL_TIERUP` | round 0 | round 1 |
|---|---|---|
| `0` (current default) | 18 047 | 19 130 |
| `1` (pre-`c28bdd687` default) | 2 432 | 2 187 |

**8.4×.** The flip was deliberate — the commit message records that the
pre-decoded instance-call route "can strand a live embedded server request
(Spring Boot `MultipartAutoConfigurationTests`) after promotion" — so this is a
load-bearing gate in the same family as doc
[30](30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md)'s RBC.6/RBC.7,
not an accident. **Do not simply flip it back.** The real fix is to repair the
pre-decoded instance-call route so tier-up can be re-enabled; what this document
adds is the measured cost of leaving it off.

Note `git log -S jit_virtual_tierup` does **not** find this commit: `-S` counts
occurrences of the string and the identifier count is unchanged, only
`true` → `false`. Use `git log -G` or diff the window.

### Cause 2 — a second, smaller regression in the same window

Flipping the gate back is **necessary but not sufficient**. With
`CRATONVM_JIT_VIRTUAL_TIERUP=1` on today's binary the test's own loop runs
4.87 s (`xxxxxxxxxxx`) and 5.07 s (`iowejoiejfoiew`) per 10⁶ calls against the
5 000 ms budget — the harder hostname still fails, where dev `b695d468f` ran it
in 2.19 s. `CalleeTierUpProbe` with the gate pinned on shows the same residual:
520 ns (07-27) vs 2 314 ns (07-30), **4.45×**.

This residual is **diffuse, not one commit**, so do not spend a bisect on it:
bisecting the same window with the gate pinned on and a 1 200 ns threshold put
`1a7e6e6dc` — early in the window — at 1 194 ns, already 2.3× over the 520 ns
baseline. The cost is accumulated across several of the 07-29/07-30 JIT changes
(the CratonBench Matrix/Fibonacci work and the dispatch fixes all land here)
rather than concentrated in one.

Eliminated along the way, so nobody re-tests them: the ctor putfield-init ban
(`CRATONVM_JIT_PUTFIELD_INIT=0` changes nothing — 24.2 vs 24.6 µs — and it is
benign-by-default since 07-28), and the mapper shadow itself.

**32.1 closes when both causes are fixed**; there is no mapper work to do.

---

## 32.2 `el.parser.TestELParserPerformance.testParserInstanceReuse` — CLOSED, not a defect

Asserts `ReInit` is faster than `new ELParser()`. The test runs its `ReInit`
loop **first**, so that loop absorbs JIT warm-up and the second loop measures
warmed code.

**Re-measured 2026-07-30: PASS in 320.9 s.** Across 12 rounds each, `ReInit`
averaged 3.87 s against `new ELParser()`'s 3.99 s — consistently the right way
round. This is a property of the test's shape on any VM with a warm-up curve,
not a defect. Kept here only so it is not re-triaged as one.

---

## 32.3 `websocket.server.TestAsyncMessagesPerformance.testAsyncTiming` — client-side drain rate

Every `message.capacity()` assertion passes, so the framing is correct; the
timing assertions are what fail.

The previous revision recorded inter-chunk gaps of 1–9 ms and the server's
deliberate 50 ms pause observed as only 2–13 ms. Neither still holds. Measured
2026-07-30 on dev `9b86a9ac1`: gaps are **0.6–1.1 ms** against a 0.5 ms budget,
and only **2** of the 50 ms-pause assertions fail across the whole run.

### The pass criteria, which the previous revision did not record

`AsyncTimingClientHandler.hasFailed()` tolerates, over 1500 iterations:

| counter | what it measures | tolerance | dev today |
|---|---|---|---|
| SEQ0 | 50 ms server pause seen as < 40 ms | > 1 fails | **0 — passes** |
| SEQ1 | gap between the two 8k chunks of one 16k message | > 10 fails | **494** |
| SEQ2 | gap between the 16k message and the 4k message | > 100 fails | 162–248 |

So SEQ0 is already clean and SEQ2 is within ~2× of its tolerance; **SEQ1 is the
item**, and it is not marginal — essentially every 16k message has its two
chunks arrive more than 0.5 ms apart.

### What was done

A handler-form `AsynchronousSocketChannel.read` cost **two thread handoffs**:
the calling thread queued a `Job::ReadFd`, a pool worker woke and blocked in
`recv`, then the dispatcher woke to run the Java `CompletionHandler`. On
loopback the bytes are usually already in the kernel receive buffer by the time
the handler arms its next read, so both wakes are pure latency.

`try_deliver_ready_read` (native-io/src/async_socket.rs) completes the read on
the initiating thread when `FIONREAD` proves it cannot block. Delivering a
completion on the initiating thread is explicitly permitted by
`AsynchronousChannelGroup`, and Tomcat's `Nio2Endpoint` already guards for it.
Bounded to two nested inline completions per thread so a continuously readable
socket returns to the worker model rather than growing the Java stack. The same
treatment was extended to the Future form (`aio_asc_read_future`), which
completes its `CompletableFuture` inline with no re-entrancy bound needed.

Interleaved A/B, two passes, same host load:

| | SEQ1 | SEQ2 | median gap |
|---|---|---|---|
| base | 500 / 500 | 231 / 171 | 842 400 / 819 900 ns |
| with the fast path | 494 / 494 | 162 / 248 | **679 000 / 717 000 ns** |

A real ~16 % latency reduction, reproducible across passes — but SEQ1 barely
moves. **Beware SEQ1 as a metric: it saturates at 500**, so its count cannot
show improvement once essentially every check fails. Use the median gap.

### Why it still fails — measured, not guessed

`CRATONVM_DBG_AIO_INLINE=1` (added 2026-07-30) reports the fast path's take rate
and why it declines. On this test:

```
reads=1500 inline=982 not_ready=27 depth_capped=491 inline_rate=0.655
```

Two separate things:

* **Only 1.8 % of reads decline because the socket was not ready.** The data
  really is already buffered, so the premise of the fast path is right.
* **32.7 % were declined by the fast path's own recursion cap**, which was 2.
  That is the 3-message cycle (8k, 8k, 4k): reads 1 and 2 go inline, read 3
  always hits the cap and pays a full worker + dispatcher round trip for data
  sitting in the kernel buffer. Raised to 16.

The decisive result is what this says about SEQ1: **SEQ1's read is one of the
inline ones, and its gap is still ~0.7 ms.** So SEQ1's cost is not the two
thread handoffs and cannot be recovered by more I/O work — it is per-completion
processing (the Rust→Java upcall plus WebSocket frame decode between `onMessage`
callbacks). Raising the cap helps SEQ2 (tolerance 100), not SEQ1 (tolerance 10,
actual ~490), and SEQ1 is the binding constraint.

Ruled out: `write_into_buffer_and_advance` uses bulk copies
(`copy_to_native_memory` / `write_byte_array_from`), not per-byte writes; and
JIT tier-up is not the cause — `CRATONVM_JIT_VIRTUAL_TIERUP=1` moves the median
gap 893 700 → 951 900 and 916 700 → 1 040 400 ns, i.e. no help.

**Status: improved but OPEN.** SEQ0 now passes (0 failures, was 2). Closing this
needs ~30 % off the per-message completion path, which is general upcall and
frame-decode cost, not an AIO defect.

---

## 32.4 `juli.TestOneLineFormatterPerformance.testDateFormat` — OPEN, and the bar moved away

Asserts `DateFormatCache` beats `String.format`. Re-measured with
`probes/DateFmtProbe.java` on dev `9b86a9ac1`:

| operation | HotSpot | CratonVM 07-27 | CratonVM 07-30 | ratio now |
|---|---|---|---|---|
| `String.format` (intrinsic) | 2.26 µs | 16.1 µs | **4.2 µs** | 1.9× |
| `SimpleDateFormat.format` | 0.34 µs | 281.6 µs | 216 µs | **635×** |
| `Calendar.get` | 0.042 µs | 9.13 µs | **51.9 µs** | 1236× |
| `StringBuilder.append(long)` | 0.050 µs | 5.22 µs | 2.3 µs | 46× |

The test passes `System.nanoTime()` to a millisecond-resolution formatter, so
`DateFormatCache`'s one-entry-per-second cache misses on essentially every call
and the miss path — a bare `SimpleDateFormat.format(Date)` — *is* what is being
measured.

**The assertion got harder, not easier.** `String.format` is the side being
raced against, and it improved 4× (16.1 → 4.2 µs) while `SimpleDateFormat`
improved only 1.3×. End to end the class is 224.4 s against `String.format`'s
5.11 s: **44× short**, where the 07-27 numbers implied ~17×. Optimising
`String.format` further would make this test *harder* to pass.

`Calendar.get` regressed 5.7× (9.13 → 51.9 µs) over the same window as 32.1 and
has the same shape (hot callees of a loop), so it is probably the same
regression.

One concrete, independent defect the VM's own diagnostic names —
`CRATONVM_DBG_JIT_METHOD_STATS=1`:

```
17972 queued=false tier_fail_count=3  compile-failed
      java/text/DateFormatSymbols.getProviderInstance(Ljava/util/Locale;)Ljava/text/DateFormatSymbols;
```

classified by the VM as "not policy — these are bugs". Two problems in one: it
fails to compile, and it is reached ~2× per `String.format` call at all (the
JDK caches `DateFormatSymbols` per locale). Note that fixing it speeds up the
*fast* side of this assertion.

**This item is a throughput residual in the family of
[30](30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md), not a Tomcat
defect.** Closing it needs `SimpleDateFormat.format` under ~4 µs — a 54×
improvement in real `java.text` code — which is the standing JIT/interpreter
gap, not something this document can carry.

---

## Reproduction

```
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Exe <exe> -Tag <tag>
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Vm hotspot -Tag hotspot

REM 32.1, standalone and split by loop body (full | recycle | tochars)
run-one.ps1 -Main MapperPerfProbe -Args2 @('xxxxxxxxxxx','1000000','2','recycle') ^
            -ExtraCp <probes-out> -Exe <exe>
REM 32.1 mechanism, no Tomcat on the classpath
cratonvm.exe -cp <probes-out> CalleeTierUpProbe 200000 2
REM 32.1 memo hit rate
set CRATONVM_DBG_MAPPER=1
REM 32.4
cratonvm.exe -Xmx2g -cp <probes-out> DateFmtProbe
```
