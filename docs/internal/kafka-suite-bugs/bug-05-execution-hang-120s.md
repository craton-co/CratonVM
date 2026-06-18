# Bug 05 — Interpreter hang (120 s watchdog) during test *execution*

**Severity:** High — these packages discover fine but hang while *running* tests.
`--nojit`, so interpreter-path (not the JIT discovery hang of bug 01).
HotSpot runs all of them clean.

**Affected packages (CVM `--nojit` HANG; HotSpot DONE):**
- `org.apache.kafka.common`
- `org.apache.kafka.common.compress`
- `org.apache.kafka.common.metrics`
- `org.apache.kafka.common.network`
- `org.apache.kafka.common.security.authenticator`

**Symptom:**
```
=== T19.H1 watchdog: deadline of 120s elapsed; requesting thread stack dumps ===
tid=0 depth=0 class=RunPkg method=main ...
tid=0 depth=1 ...SessionPerRequestLauncher.execute ...   <- execution phase, not discovery
```
The top frame is `Launcher.execute`, i.e. discovery finished and a test body (or
a `@BeforeAll`/setup) is spinning or blocked. Likely candidates by package:
- `common.compress` — codec round-trips (zstd/lz4/snappy) in a loop.
- `common.metrics` — time-driven/`MockTime` sensor windows.
- `common.network` — selector/socket loops (cf. existing ServerSocket gaps).
- `common.security.authenticator` — SASL handshake loops.

These may be distinct root causes; each needs its own bisection to the test class
and the spinning frame (re-run the package with a shorter watchdog and capture the
deepest interpreter frame).

## Update (2026-06-12) — same class as bug-06: throughput + thread-lifecycle

Investigating the sibling package `consumer.internals` (bug-06) showed its
"execution hang" is **not** an infinite loop: the test class *completes* with the
watchdog disabled (CratonVM >120s vs HotSpot ~13s) and then the JVM stays alive on
**non-daemon background threads** (consumer/network/metrics threads that HotSpot
exits past) until the 120s watchdog kills it. The bug-05 packages
(`common`, `common.network`, `common.metrics`, `common.compress`,
`common.security.authenticator`) match the same `Launcher.execute` watchdog
signature and almost certainly share the two root causes:

1. **Throughput** — interpreter is ~10–50× slower than HotSpot's JIT; large/looping
   tests exceed the 120s watchdog (same family as bug-01).
2. **Thread lifecycle** — tests spawn background threads CratonVM treats as
   non-daemon, so `main` returning doesn't end the JVM.

A general perf contributor was fixed: the heap `get_field` OOB-read guard allocated
a `String` + `warn!` on **every** benign speculative collection-layout probe (a hot
path during discovery/execution); now rate-limited (see bug-06 / `gc/src/gen_heap.rs`).

## Status
- [x] Reproduced; confirmed CVM-only (HotSpot completes).
- [x] Re-classified as throughput + thread-lifecycle (via bug-06), not infinite loops.
- [ ] Throughput (JIT/interpreter speed) + non-daemon-thread lifecycle: open workstreams.
