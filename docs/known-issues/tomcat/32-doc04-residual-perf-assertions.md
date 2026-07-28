# 32 — the four per-test performance residuals from known-issue 04  (OPEN)

**Status:** OPEN, low priority. Carved out of known-issue 04 on 2026-07-27
when that group was retired. These are the residual items that are **not**
webapp-deploy throughput (that is
[31](31-synchronized-code-never-jit-compiled.md)) — each is one test whose
assertion is about speed, with its own separate reason.

HotSpot reference for all four, same host, same day: all PASS.

---

## 32.1 `catalina.mapper.TestMapperPerformance.testPerformance` — absolute budget

The test maps 10⁶ URIs per hostname and asserts each hostname finishes in
**under 5 000 ms** (an absolute wall-clock budget, no baseline).

| hostname | HotSpot | CratonVM |
|---|---|---|
| `xxxxxxxxxxx` (easiest) | 0.10 s | 2.8–3.7 s — **passes** |
| `iowejoiejfoiew` (hardest; also HotSpot's slowest) | 0.37 s | 7.6–8.9 s — **fails** |

`Mapper.map` / `internalMap` / `find*` / `exactFind*` are served by Rust
shadows (`native-builtins/src/lib.rs`), so this is not interpreter speed. Two
things are known about the shadow:

* **The shadow is the right call, not the problem.** With
  `CRATONVM_TOMCAT_MAPPER_NATIVES=0` (added 2026-07-27 so the boundary can be
  A/B'd) Tomcat's own `Mapper` bytecode runs instead, and the *easiest*
  hostname alone takes **35.4 s** — ~10× slower than the shadow. Deleting the
  shadows would make this much worse.
* **The shadow re-reads the heap far more than it needs to.** Every binary-search
  probe used to re-read the searched `CharChunk` — three `get_field_by_name`
  plus a `get_array_element` *per character*. Snapshotting the range once per
  search (`char_chunk_snapshot_range`, 2026-07-27) took the easy hostname from
  3.34 s to 2.82 s but barely moved the hard one, so the remaining cost is
  elsewhere in `native_mapper_internal_map`.

**Next step (unstarted):** instrument `mapper_internal_fast_cache`'s hit rate.
The memo has a validity condition that reads suspicious for exactly the
"selected a context but no version" case —

```rust
let paused_now = if let Some(v) = entry.selected_version { …read "paused"… }
                 else { !entry.no_context };          // ← true ⇒ memo unusable
```

— which would make the memo permanently ineffective for a host whose mapping
selects a context without a version, and the two hostnames differ by exactly
how much context/wrapper structure they carry. Confirm before optimising
anything else here.

---

## 32.2 `el.parser.TestELParserPerformance.testParserInstanceReuse` — relative, order-sensitive

Asserts `ReInit` is faster than `new ELParser()`. The test runs its `ReInit`
loop **first**, so that loop absorbs JIT warm-up and the second loop measures
warmed code. On a quiet host it passes; on a loaded one it flips. This is a
property of the test's shape on any VM with a warm-up curve, not a defect —
keep it here only so it is not re-triaged as one.

---

## 32.3 `websocket.server.TestAsyncMessagesPerformance.testAsyncTiming` — client-side drain rate

Every `message.capacity()` assertion passes, so the framing is correct. The
timing assertions fail in **both** directions: inter-chunk gaps of 1–9 ms
where <0.5 ms is expected, and the server's deliberate 50 ms pause observed as
only 2–13 ms. Both point the same way — the client cannot drain in real time,
so frames queue server-side and are then read back to back (gap too small)
while the chunks of one message arrive far apart (gap too large). Fixing the
client's read throughput is the actual work; see 31 for the JDK-I/O-stack part
of it.

---

## 32.4 `juli.TestOneLineFormatterPerformance.testDateFormat` — a genuine 834× outlier

Asserts `DateFormatCache` beats `String.format`. It cannot pass while
`java.util.Formatter.format` (what `String.format` delegates to) is a Rust
`NativeKind::Intrinsic` running at near-HotSpot speed and everything it is
raced against is interpreted. But the losing side is *also* pathological in
its own right, which is the part worth fixing. `probes/DateFmtProbe.java`,
2026-07-27:

| operation | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `String.format` (intrinsic) | 2.26 µs | 16.1 µs | 7.1× |
| `SimpleDateFormat.format` | 0.34 µs | 281.6 µs | **834×** |
| `Calendar.get` | 0.042 µs | 9.13 µs | 218× |
| `StringBuilder.append(long).toString()` | 0.050 µs | 5.22 µs | 104× |

Note the test passes `System.nanoTime()` to a millisecond-resolution
formatter, so `DateFormatCache`'s one-entry-per-second cache misses on
essentially every call and the miss path *is* what is being measured.

**Why 834× and not the ~100× general gap:** with the JIT off,
`SimpleDateFormat.format` costs 593 µs and `Calendar.get` 107 µs; with it on,
282 µs and 9.1 µs. The JIT buys `Calendar.get` 11.8× and
`SimpleDateFormat.format` only **2.1×** — the signature of a call chain whose
hot methods never compile. `SimpleDateFormat.format` runs through
`StringBuffer` (every method `synchronized`) and `DecimalFormat`, so this is
most likely a consumer of [31](31-synchronized-code-never-jit-compiled.md);
confirm with `CRATONVM_DBG_JITC` before treating it as its own bug.

---

## Reproduction

```
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Exe <exe> -Tag <tag>
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Vm hotspot -Tag hotspot
cratonvm.exe -Xmx2g -cp <probes-out> DateFmtProbe
```
